// agv-avf-runner — per-VM Apple Virtualization supervisor.
//
// This commit: control socket + JSON-RPC. The runner now binds a unix
// socket at the path supplied in the config and accepts line-delimited
// JSON commands (`stop`, `force_stop`, `status`). The Rust agv parent
// will use this to drive lifecycle without resorting to signals;
// signals still work as a fallback.

import Foundation
import Virtualization
import Darwin

// ---------------------------------------------------------------------------
// JSON config sent by the Rust side
// ---------------------------------------------------------------------------

struct RunnerConfig: Codable {
    let name: String
    let memoryBytes: UInt64
    let cpuCount: Int
    let diskPath: String
    let seedIsoPath: String
    let efiVariableStorePath: String
    let serialLogPath: String
    let controlSocketPath: String
}

// ---------------------------------------------------------------------------
// Control-socket protocol (line-delimited JSON, one request per line)
// ---------------------------------------------------------------------------

/// Request shape on the control socket.
///
/// Only `op` is required today; future commands may add typed argument
/// payloads (e.g. snapshot names for suspend). Encode as snake_case so
/// the wire format matches what Rust serde produces by default.
struct ControlRequest: Codable {
    let op: String
}

/// Response shape on the control socket.
///
/// `ok` is the only mandatory field. `error` is set when `ok = false`.
/// For the `status` op, `state` and `guestIp` are populated; for other
/// ops they're `nil`.
struct ControlResponse: Codable {
    let ok: Bool
    var error: String? = nil
    var state: String? = nil
    var guestIp: String? = nil
}

/// VM state tracked by the runner and surfaced via `status` queries.
enum VMState: String {
    case starting
    case running
    case stopping
    case stopped
    case errored
}

// ---------------------------------------------------------------------------
// CLI entry
// ---------------------------------------------------------------------------

let version = "0.0.0"

let args = CommandLine.arguments
var configPath: String? = nil
var validateOnly = false

var i = 1
while i < args.count {
    let arg = args[i]
    switch arg {
    case "--version", "-V":
        print("agv-avf-runner \(version)")
        exit(0)
    case "--help", "-h":
        printHelp()
        exit(0)
    case "--config":
        guard i + 1 < args.count else {
            die("--config requires a path argument")
        }
        configPath = args[i + 1]
        i += 2
        continue
    case "--validate-only":
        validateOnly = true
        i += 1
        continue
    default:
        die("unrecognized argument '\(arg)'")
    }
}

guard let configPath else {
    die("missing required --config <path>")
}

let config: RunnerConfig
let vmConfig: VZVirtualMachineConfiguration
do {
    config = try loadConfig(from: configPath)
    vmConfig = try buildVMConfiguration(from: config)
    try vmConfig.validate()
} catch {
    die("\(error)")
}

if validateOnly {
    print("agv-avf-runner: config validates for VM '\(config.name)'")
    exit(0)
}

let runner = VMRunner(configuration: vmConfig, vmName: config.name)

// Bind the control socket before booting so the parent agv process can
// connect immediately. If this fails, treat it as fatal — there's no
// point booting a VM we can't control.
let controlServer = ControlServer(path: config.controlSocketPath, runner: runner)
do {
    try controlServer.start()
} catch {
    die("failed to bind control socket at \(config.controlSocketPath): \(error)")
}

let exitCode = runner.runUntilStopped()
controlServer.stop()
exit(exitCode)

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

func loadConfig(from path: String) throws -> RunnerConfig {
    let url = URL(fileURLWithPath: path)
    let data = try Data(contentsOf: url)
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(RunnerConfig.self, from: data)
}

func buildVMConfiguration(from config: RunnerConfig) throws -> VZVirtualMachineConfiguration {
    let vm = VZVirtualMachineConfiguration()
    vm.cpuCount = config.cpuCount
    vm.memorySize = config.memoryBytes

    vm.platform = VZGenericPlatformConfiguration()

    let bootLoader = VZEFIBootLoader()
    let efiURL = URL(fileURLWithPath: config.efiVariableStorePath)
    let efiStore: VZEFIVariableStore
    if FileManager.default.fileExists(atPath: efiURL.path) {
        efiStore = VZEFIVariableStore(url: efiURL)
    } else {
        efiStore = try VZEFIVariableStore(creatingVariableStoreAt: efiURL)
    }
    bootLoader.variableStore = efiStore
    vm.bootLoader = bootLoader

    let diskURL = URL(fileURLWithPath: config.diskPath)
    let diskAttachment = try VZDiskImageStorageDeviceAttachment(
        url: diskURL,
        readOnly: false
    )
    let diskDevice = VZVirtioBlockDeviceConfiguration(attachment: diskAttachment)

    let seedURL = URL(fileURLWithPath: config.seedIsoPath)
    let seedAttachment = try VZDiskImageStorageDeviceAttachment(
        url: seedURL,
        readOnly: true
    )
    let seedDevice = VZVirtioBlockDeviceConfiguration(attachment: seedAttachment)

    vm.storageDevices = [diskDevice, seedDevice]

    let netDevice = VZVirtioNetworkDeviceConfiguration()
    netDevice.attachment = VZNATNetworkDeviceAttachment()
    vm.networkDevices = [netDevice]

    let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
    let serialURL = URL(fileURLWithPath: config.serialLogPath)
    FileManager.default.createFile(atPath: serialURL.path, contents: nil, attributes: nil)
    let serialHandle = try FileHandle(forWritingTo: serialURL)
    serial.attachment = VZFileHandleSerialPortAttachment(
        fileHandleForReading: nil,
        fileHandleForWriting: serialHandle
    )
    vm.serialPorts = [serial]

    vm.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

    return vm
}

func printHelp(to handle: FileHandle = FileHandle.standardOutput) {
    let lines = [
        "agv-avf-runner — Apple Virtualization supervisor for agv VMs",
        "",
        "Usage:",
        "  agv-avf-runner --config <path>            Boot a VM",
        "  agv-avf-runner --config <path> --validate-only",
        "                                            Validate config without booting",
        "  agv-avf-runner --version                  Print version",
        "  agv-avf-runner --help                     Print this help",
        "",
        "The runner is normally spawned by the agv Rust binary; manual",
        "invocation is intended for development and config validation.",
        "",
    ].joined(separator: "\n")
    handle.write(Data(lines.utf8))
}

func die(_ msg: String) -> Never {
    FileHandle.standardError.write(Data("agv-avf-runner: \(msg)\n".utf8))
    exit(2)
}

// ---------------------------------------------------------------------------
// VM runner: boot the configured VM and block until it stops.
// ---------------------------------------------------------------------------

final class VMRunner: NSObject, VZVirtualMachineDelegate {
    let vmName: String
    private let queue: DispatchQueue
    private let vm: VZVirtualMachine
    private let exitSemaphore = DispatchSemaphore(value: 0)
    private var exitCode: Int32 = 0
    private var hasExited = false

    /// Coarse-grained VM state tracked for `status` queries. Updated
    /// from the VM queue and from the start completion handler; read
    /// from the control queue under `stateLock`.
    private var _state: VMState = .starting
    private let stateLock = NSLock()

    init(configuration: VZVirtualMachineConfiguration, vmName: String) {
        self.vmName = vmName
        self.queue = DispatchQueue(label: "agv-avf-runner.vm.\(vmName)")
        self.vm = VZVirtualMachine(configuration: configuration, queue: self.queue)
        super.init()
        queue.sync {
            self.vm.delegate = self
        }
    }

    var state: VMState {
        stateLock.lock()
        defer { stateLock.unlock() }
        return _state
    }

    private func setState(_ new: VMState) {
        stateLock.lock()
        _state = new
        stateLock.unlock()
    }

    func runUntilStopped() -> Int32 {
        let signalSources = installSignalHandlers()
        defer { for s in signalSources { s.cancel() } }

        queue.async { [weak self] in
            guard let self else { return }
            self.vm.start { [weak self] result in
                guard let self else { return }
                switch result {
                case .success:
                    self.setState(.running)
                case .failure(let err):
                    FileHandle.standardError.write(
                        Data("agv-avf-runner: VM '\(self.vmName)' failed to start: \(err)\n".utf8)
                    )
                    self.setState(.errored)
                    self.exitCode = 1
                    self.signalExitOnce()
                }
            }
        }

        exitSemaphore.wait()
        return exitCode
    }

    private func installSignalHandlers() -> [DispatchSourceSignal] {
        var sources: [DispatchSourceSignal] = []
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let src = DispatchSource.makeSignalSource(
                signal: sig,
                queue: DispatchQueue.global()
            )
            src.setEventHandler { [weak self] in
                self?.requestGracefulStop()
            }
            src.resume()
            sources.append(src)
        }
        return sources
    }

    func requestGracefulStop() {
        queue.async { [weak self] in
            guard let self else { return }
            if self.hasExited { return }
            self.setState(.stopping)
            do {
                try self.vm.requestStop()
            } catch {
                FileHandle.standardError.write(
                    Data("agv-avf-runner: requestStop failed for '\(self.vmName)': \(error); forcing stop\n".utf8)
                )
                self.vm.stop { _ in
                    self.signalExitOnce()
                }
            }
        }
    }

    func forceStop() {
        queue.async { [weak self] in
            guard let self else { return }
            if self.hasExited { return }
            self.setState(.stopping)
            self.vm.stop { _ in
                self.signalExitOnce()
            }
        }
    }

    private func signalExitOnce() {
        if hasExited { return }
        hasExited = true
        if state != .errored {
            setState(.stopped)
        }
        exitSemaphore.signal()
    }

    // MARK: - VZVirtualMachineDelegate

    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        signalExitOnce()
    }

    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        FileHandle.standardError.write(
            Data("agv-avf-runner: VM '\(vmName)' stopped with error: \(error)\n".utf8)
        )
        setState(.errored)
        exitCode = 1
        signalExitOnce()
    }
}

// ---------------------------------------------------------------------------
// Control server: unix-socket JSON-RPC for parent-process control.
// ---------------------------------------------------------------------------

/// Per-VM JSON-RPC server bound to the unix socket the parent (Rust
/// agv) uses to drive lifecycle. Each connection is one-shot: read one
/// line of JSON, write one line back, close.
///
/// Why a fresh connection per command rather than a long-lived one:
/// matches the call sites on the parent side (each `agv stop` /
/// `agv ssh` reads-modifies-state independently) and keeps the
/// protocol stateless. The runner stays running across many
/// connections until the VM exits (signal, `stop` op, or guest
/// shutdown).
final class ControlServer {
    private let path: String
    private let runner: VMRunner
    private let queue: DispatchQueue
    private var fd: Int32 = -1
    private var listenSource: DispatchSourceRead?

    init(path: String, runner: VMRunner) {
        self.path = path
        self.runner = runner
        self.queue = DispatchQueue(label: "agv-avf-runner.control.\(runner.vmName)")
    }

    func start() throws {
        // Clean up a stale socket from a previous unclean exit.
        unlink(path)

        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw posixError("socket", errno)
        }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathMaxLen = MemoryLayout.size(ofValue: addr.sun_path) - 1
        let pathBytes = Array(path.utf8)
        guard pathBytes.count <= pathMaxLen else {
            Darwin.close(fd)
            fd = -1
            throw posixError("socket path too long (\(pathBytes.count) > \(pathMaxLen))", EINVAL)
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: UInt8.self, capacity: pathMaxLen + 1) { dst in
                _ = pathBytes.withUnsafeBufferPointer { src in
                    memcpy(dst, src.baseAddress, src.count)
                }
                dst[pathBytes.count] = 0
            }
        }

        let addrLen = socklen_t(MemoryLayout<sockaddr_un>.size)
        let bindRet = withUnsafePointer(to: &addr) { addrPtr in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                bind(fd, sockPtr, addrLen)
            }
        }
        guard bindRet == 0 else {
            let err = errno
            Darwin.close(fd)
            fd = -1
            throw posixError("bind", err)
        }

        // Tighten permissions: only the owner of the agv data dir
        // should be able to control the VM.
        chmod(path, 0o600)

        guard listen(fd, 4) == 0 else {
            let err = errno
            Darwin.close(fd)
            fd = -1
            throw posixError("listen", err)
        }

        let src = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
        src.setEventHandler { [weak self] in
            self?.acceptOne()
        }
        src.resume()
        listenSource = src
    }

    func stop() {
        listenSource?.cancel()
        listenSource = nil
        if fd >= 0 {
            Darwin.close(fd)
            fd = -1
        }
        unlink(path)
    }

    private func acceptOne() {
        var clientAddr = sockaddr_un()
        var clientAddrLen = socklen_t(MemoryLayout<sockaddr_un>.size)
        let clientFd = withUnsafeMutablePointer(to: &clientAddr) { addrPtr in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                accept(fd, sockPtr, &clientAddrLen)
            }
        }
        guard clientFd >= 0 else { return }
        // Handle on the same queue — connections are short-lived; one
        // command, one response, close. No need for a worker pool.
        queue.async { [weak self] in
            self?.handleClient(clientFd)
        }
    }

    private func handleClient(_ clientFd: Int32) {
        defer { Darwin.close(clientFd) }
        guard let line = readLine(from: clientFd) else { return }

        let response: ControlResponse
        do {
            let req = try JSONDecoder().decode(ControlRequest.self, from: Data(line.utf8))
            response = dispatch(req)
        } catch {
            response = ControlResponse(ok: false, error: "invalid request: \(error)")
        }
        writeResponse(clientFd, response)
    }

    private func dispatch(_ req: ControlRequest) -> ControlResponse {
        switch req.op {
        case "stop":
            runner.requestGracefulStop()
            return ControlResponse(ok: true)
        case "force_stop":
            runner.forceStop()
            return ControlResponse(ok: true)
        case "status":
            return ControlResponse(
                ok: true,
                state: runner.state.rawValue,
                guestIp: nil
            )
        default:
            return ControlResponse(ok: false, error: "unknown op '\(req.op)'")
        }
    }

    private func readLine(from fd: Int32) -> String? {
        // 4 KiB ceiling — protocol messages are tiny; anything bigger
        // is malformed input or an attack.
        var buf = [UInt8]()
        buf.reserveCapacity(256)
        var b: UInt8 = 0
        while buf.count < 4096 {
            let n = read(fd, &b, 1)
            if n != 1 { break }
            if b == 0x0a { break } // \n
            buf.append(b)
        }
        return String(bytes: buf, encoding: .utf8)
    }

    private func writeResponse(_ fd: Int32, _ response: ControlResponse) {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        guard var data = (try? encoder.encode(response)) else { return }
        data.append(0x0a)
        data.withUnsafeBytes { ptr in
            _ = write(fd, ptr.baseAddress, ptr.count)
        }
    }

    private func posixError(_ op: String, _ code: Int32) -> NSError {
        let msg = String(cString: strerror(code))
        return NSError(
            domain: "agv-avf-runner.control",
            code: Int(code),
            userInfo: [NSLocalizedDescriptionKey: "\(op): \(msg)"]
        )
    }
}

// agv-avf-runner — per-VM Apple Virtualization supervisor.
//
// This commit: actually boot the VM and run it until SIGTERM or guest
// shutdown. Adds `--validate-only` so the integration tests can still
// exercise the config-builder path without booting. Subsequent commits
// add the unix-socket JSON-RPC server (so the parent agv process can
// request stop/suspend/force-stop without sending signals) and a PID
// file (so the parent can find this supervisor by name).

import Foundation
import Virtualization

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

// Boot the VM and block until it stops.
let runner = VMRunner(configuration: vmConfig, vmName: config.name)
exit(runner.runUntilStopped())

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

/// Build a `VZVirtualMachineConfiguration` from the agv runner config.
///
/// Storage layout:
/// - virtio-blk disk0 ← raw disk image (read-write)
/// - virtio-blk disk1 ← seed.iso (read-only; cloud-init NoCloud finds
///   it by `cidata` volume label, no CDROM device needed under AVF)
///
/// Boot: `VZEFIBootLoader` with a per-instance NVRAM file. AVF lazily
/// creates the file on first boot and reuses it on subsequent boots.
///
/// Network: NAT attached to a virtio-net adapter. The guest gets a
/// private 192.168.64.x DHCP lease that's reachable from the host on
/// AVF's bridge interface — no `hostfwd` plumbing needed.
///
/// Serial: virtio-console writing to `<instance>/serial.log`.
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

/// Owns the `VZVirtualMachine` for one VM and runs it until either the
/// guest shuts down on its own or we receive SIGTERM/SIGINT.
///
/// VZ requires every method on a `VZVirtualMachine` to be invoked on
/// the queue passed to its constructor (the "VM queue"). Delegate
/// callbacks also fire on that queue. Signals dispatch on the global
/// queue and hop onto the VM queue before touching `vm`.
final class VMRunner: NSObject, VZVirtualMachineDelegate {
    private let vmName: String
    private let queue: DispatchQueue
    private let vm: VZVirtualMachine
    private let exitSemaphore = DispatchSemaphore(value: 0)
    /// Atomic-ish: only ever written from the VM queue or signal-source
    /// queue; only read after the semaphore wait returns.
    private var exitCode: Int32 = 0
    /// Set to `true` once the guest has been observed to stop (via
    /// `guestDidStop` or `didStopWithError`) so we don't double-signal.
    private var hasExited = false

    init(configuration: VZVirtualMachineConfiguration, vmName: String) {
        self.vmName = vmName
        self.queue = DispatchQueue(label: "agv-avf-runner.vm.\(vmName)")
        self.vm = VZVirtualMachine(configuration: configuration, queue: self.queue)
        super.init()
        queue.sync {
            self.vm.delegate = self
        }
    }

    /// Boot the VM and block until it stops or a signal arrives.
    /// Returns the process exit code (0 on clean shutdown, non-zero on
    /// VM error).
    func runUntilStopped() -> Int32 {
        // Install signal handlers before start() so we never miss a
        // signal that arrives during boot.
        let signalSources = installSignalHandlers()
        defer { for s in signalSources { s.cancel() } }

        queue.async { [weak self] in
            guard let self else { return }
            self.vm.start { [weak self] result in
                guard let self else { return }
                if case .failure(let err) = result {
                    FileHandle.standardError.write(
                        Data("agv-avf-runner: VM '\(self.vmName)' failed to start: \(err)\n".utf8)
                    )
                    self.exitCode = 1
                    self.signalExitOnce()
                }
                // Success path: wait for guestDidStop or a signal.
            }
        }

        exitSemaphore.wait()
        return exitCode
    }

    /// Suppress the default disposition for SIGTERM/SIGINT and route
    /// them to dispatch sources that initiate a graceful stop.
    private func installSignalHandlers() -> [DispatchSourceSignal] {
        var sources: [DispatchSourceSignal] = []
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN) // dispatch source needs default disposition off
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

    /// Send the guest an ACPI shutdown request. The guest has up to a
    /// few seconds to react; on success we'll exit through
    /// `guestDidStop`. If the guest is wedged or already gone, fall
    /// back to a force stop.
    private func requestGracefulStop() {
        queue.async { [weak self] in
            guard let self else { return }
            if self.hasExited { return }
            do {
                try self.vm.requestStop()
                // requestStop just sends the request; the actual stop
                // arrives later via the delegate. Don't signal exit yet.
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

    /// Signal the main thread to wake and exit, exactly once. Safe to
    /// call from multiple paths (delegate, force-stop fallback, start
    /// failure).
    private func signalExitOnce() {
        if hasExited { return }
        hasExited = true
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
        exitCode = 1
        signalExitOnce()
    }
}

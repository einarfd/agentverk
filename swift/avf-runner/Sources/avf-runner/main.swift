// agv-avf-runner — per-VM Apple Virtualization supervisor.
//
// This commit: parse a JSON config from `--config <path>` and build +
// validate a VZVirtualMachineConfiguration from it. No boot, no socket,
// no signal handling — those land in subsequent commits. The point of
// this skeleton is to prove the AVF API integration before tying it to
// the lifecycle on the Rust side.

import Foundation
import Virtualization

// ---------------------------------------------------------------------------
// JSON config sent by the Rust side
// ---------------------------------------------------------------------------

/// On-disk shape of the agv-avf-runner config file. The Rust side writes
/// this to `<instance>/avf-config.json` before spawning the runner.
///
/// All paths are absolute. Memory is in bytes. CPU count is an Int per
/// Apple's `VZVirtualMachineConfiguration.cpuCount` type.
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
    default:
        die("unrecognized argument '\(arg)'")
    }
}

guard let configPath else {
    die("missing required --config <path>")
}

do {
    let config = try loadConfig(from: configPath)
    let vmConfig = try buildVMConfiguration(from: config)
    try vmConfig.validate()
    print("agv-avf-runner: config validates for VM '\(config.name)'")
    exit(0)
} catch {
    die("\(error)")
}

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

    // Platform: generic ARM virt platform (Linux guest on Apple Silicon).
    vm.platform = VZGenericPlatformConfiguration()

    // Boot: EFI with a writable variable store.
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

    // Storage: disk + seed ISO.
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

    // Network: NAT.
    let netDevice = VZVirtioNetworkDeviceConfiguration()
    netDevice.attachment = VZNATNetworkDeviceAttachment()
    vm.networkDevices = [netDevice]

    // Serial: virtio-console → serial.log on the host.
    let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
    let serialURL = URL(fileURLWithPath: config.serialLogPath)
    // Truncate previous boot's log; matches QEMU's `-serial file:` semantics.
    FileManager.default.createFile(atPath: serialURL.path, contents: nil, attributes: nil)
    let serialHandle = try FileHandle(forWritingTo: serialURL)
    serial.attachment = VZFileHandleSerialPortAttachment(
        fileHandleForReading: nil,
        fileHandleForWriting: serialHandle
    )
    vm.serialPorts = [serial]

    // Entropy: virtio-rng. Avoids guest stalls during boot / SSH key
    // generation (matches our QEMU `-device virtio-rng-pci`).
    vm.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

    return vm
}

func printHelp(to handle: FileHandle = FileHandle.standardOutput) {
    let lines = [
        "agv-avf-runner — Apple Virtualization supervisor for agv VMs",
        "",
        "Usage:",
        "  agv-avf-runner --config <path>   Validate a runner config JSON",
        "  agv-avf-runner --version         Print version",
        "  agv-avf-runner --help            Print this help",
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

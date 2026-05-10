// agv-avf-runner — per-VM Apple Virtualization supervisor.
//
// This is the skeleton commit: just enough to validate the toolchain
// and establish the binary's CLI entry point. Subsequent commits add
// the JSON-RPC server, VZVirtualMachine config, and lifecycle plumbing.

import Foundation

let version = "0.0.0"

let args = CommandLine.arguments

if args.count >= 2 {
    switch args[1] {
    case "--version", "-V":
        print("agv-avf-runner \(version)")
        exit(0)
    case "--help", "-h":
        printHelp()
        exit(0)
    default:
        FileHandle.standardError.write(Data("agv-avf-runner: unrecognized argument '\(args[1])'\n".utf8))
        printHelp(to: FileHandle.standardError)
        exit(2)
    }
}

// No-arg invocation: print help and exit non-zero so accidental runs
// are obvious. Real invocation will eventually take a control-socket
// path and a VM-config JSON path; not implemented yet.
printHelp(to: FileHandle.standardError)
exit(2)

func printHelp(to handle: FileHandle = FileHandle.standardOutput) {
    let lines = [
        "agv-avf-runner — Apple Virtualization supervisor for agv VMs",
        "",
        "Usage:",
        "  agv-avf-runner --version    Print version",
        "  agv-avf-runner --help       Print this help",
        "",
        "The runner is normally spawned by the agv Rust binary; manual",
        "invocation is not supported yet.",
        "",
    ].joined(separator: "\n")
    handle.write(Data(lines.utf8))
}

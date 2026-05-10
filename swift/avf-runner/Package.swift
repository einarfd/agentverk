// swift-tools-version:5.9
//
// agv-avf-runner — per-VM Apple Virtualization (AVF) supervisor.
//
// One process per running VM; spawned by the Rust agv binary on macOS
// when a VM has `backend = "avf"`. Owns the Apple Virtualization
// `VZVirtualMachine` instance, accepts JSON-RPC commands on a unix
// socket, exposes lifecycle (start/stop/suspend/resume/force_stop) and
// state queries (guest IP discovery, status).
//
// The Rust side is kept ignorant of the Swift API: the protocol is
// JSON-RPC over a unix socket so we can rewrite the runner in pure
// Rust later (`objc2` + `block2`) without touching any of the Rust
// lifecycle code.

import PackageDescription

let package = Package(
    name: "agv-avf-runner",
    // AVF (`Virtualization` framework) is macOS 11+; we target macOS 13
    // (Ventura) so we can use the modern API surface (Linux guest support,
    // virtiofs, snapshots) without per-version conditionals.
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "agv-avf-runner",
            path: "Sources/avf-runner"
        )
    ]
)

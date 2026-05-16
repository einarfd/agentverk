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
    // AVF (`Virtualization` framework) is macOS 11+; we target macOS 14
    // (Sonoma) because we use `saveMachineStateTo` /
    // `restoreMachineStateFrom` for suspend/resume — both 14.0+. The
    // older surface (start, pause, resume) is 11+, but pinning the
    // whole package to 14 keeps the code free of per-API
    // `@available` shims.
    platforms: [.macOS(.v14)],
    targets: [
        // Pure-logic helpers split into a library target so we can
        // unit-test them without main.swift's top-level boot code
        // running on import. Keep this target free of Virtualization
        // framework references — those belong with the runner exe.
        .target(
            name: "AvfRunnerCore",
            path: "Sources/AvfRunnerCore"
        ),
        .executableTarget(
            name: "agv-avf-runner",
            dependencies: ["AvfRunnerCore"],
            path: "Sources/avf-runner"
        ),
        .testTarget(
            name: "AvfRunnerCoreTests",
            dependencies: ["AvfRunnerCore"],
            path: "Tests/AvfRunnerCoreTests"
        )
    ]
)

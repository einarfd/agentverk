//! Build script: embed a `git describe` string into the binary so
//! `agv --version` reflects the actual build (commits past the tag, dirty
//! working tree). Falls back to `CARGO_PKG_VERSION` when no git repo is
//! available (e.g. installs from a published tarball).

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let cargo_version =
        std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is always set by cargo");

    let version = git_describe()
        .map_or(cargo_version, |d| {
            d.strip_prefix('v').unwrap_or(&d).to_string()
        });

    println!("cargo:rustc-env=AGV_VERSION={version}");
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty", "--tags"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

//! Image download, caching, and checksum verification.
//!
//! Base images are downloaded once and cached in the image cache directory.
//! Each VM gets a copy-on-write qcow2 overlay backed by the cached image,
//! keeping disk usage low.

use std::path::{Path, PathBuf};

/// Ensure the base image is available in the local cache.
///
/// If the image is already cached, verify its checksum (if provided) and
/// return the cached path. Otherwise, download it.
pub async fn ensure_cached(
    _url: &str,
    _checksum: Option<&str>,
) -> anyhow::Result<PathBuf> {
    todo!("download image if not cached, verify checksum, return cached path")
}

/// Create a qcow2 overlay disk backed by the given base image.
pub async fn create_overlay(_base_image: &Path, _output: &Path, _size: &str) -> anyhow::Result<()> {
    todo!("qemu-img create -f qcow2 -b <base> -F qcow2 <output> <size>")
}

/// Return the default base image URL for the current architecture.
#[must_use]
pub fn default_image_url() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"
    } else {
        "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
    }
}

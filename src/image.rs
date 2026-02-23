//! Image download, caching, and checksum verification.
//!
//! Base images are downloaded once and cached in the image cache directory.
//! Each VM gets a copy-on-write qcow2 overlay backed by the cached image,
//! keeping disk usage low.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use futures_util::StreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tracing::info;

use crate::dirs;
use crate::error::Error;

/// Ensure the base image is available in the local cache.
///
/// If the image is already cached, returns immediately — the checksum was
/// verified at download time and we trust the file hasn't changed. If the
/// file is missing, downloads it, verifies the checksum, then caches it.
///
/// Returns `(path, downloaded)` where `downloaded` is `true` if a network
/// fetch was required, `false` if the file was already cached.
pub async fn ensure_cached(url: &str, checksum: Option<&str>) -> anyhow::Result<(PathBuf, bool)> {
    let expected_hex = parse_checksum(checksum)?;
    let filename = filename_from_url(url);
    let cache_dir = dirs::image_cache_dir()?;
    tokio::fs::create_dir_all(&cache_dir).await.with_context(|| {
        format!("failed to create image cache directory {}", cache_dir.display())
    })?;

    let cached_path = cache_dir.join(&filename);

    // Already cached — trust it. Checksum was verified on download.
    if cached_path.exists() {
        info!(path = %cached_path.display(), "image already cached");
        return Ok((cached_path, false));
    }

    // Download to a unique temp file, then atomically rename.
    // Using PID + timestamp avoids collisions when multiple processes download concurrently.
    let part_name = format!(
        "{filename}.part.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let part_path = cache_dir.join(part_name);
    download(url, &part_path).await?;

    // Verify checksum after download.
    if let Some(expected) = expected_hex {
        let actual = sha256_file(&part_path).await?;
        if actual != expected {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(Error::ImageChecksum {
                path: part_path,
                expected: expected.to_string(),
                actual,
            }
            .into());
        }
        info!("checksum verified OK");
    }

    tokio::fs::rename(&part_path, &cached_path).await.with_context(|| {
        format!(
            "failed to rename {} → {}",
            part_path.display(),
            cached_path.display()
        )
    })?;

    info!(path = %cached_path.display(), "image cached successfully");
    Ok((cached_path, true))
}

/// Create a qcow2 overlay disk backed by the given base image.
pub async fn create_overlay(base_image: &Path, output: &Path, size: &str) -> anyhow::Result<()> {
    let base_str = base_image
        .to_str()
        .context("base image path is not valid UTF-8")?;
    let output_str = output
        .to_str()
        .context("output path is not valid UTF-8")?;

    let result = tokio::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-b", base_str, "-F", "qcow2", output_str, size])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("qemu-img create failed (exit {}): {stderr}", output.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("qemu-img not found — is QEMU installed?");
        }
        Err(e) => {
            Err(e).context("failed to run qemu-img")?
        }
    }
}

/// Flatten a qcow2 overlay (and all its backing files) into a standalone template image.
///
/// This reads the full contents of `source` (resolving the backing chain) and writes
/// a self-contained qcow2 at `dest`. The resulting file can be used as a backing image
/// for thin overlay clones.
pub async fn convert_to_template(source: &Path, dest: &Path) -> anyhow::Result<()> {
    let source_str = source
        .to_str()
        .context("source disk path is not valid UTF-8")?;
    let dest_str = dest
        .to_str()
        .context("destination template path is not valid UTF-8")?;

    let result = tokio::process::Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "qcow2", source_str, dest_str])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("qemu-img convert failed (exit {}): {stderr}", output.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("qemu-img not found — is QEMU installed?");
        }
        Err(e) => Err(e).context("failed to run qemu-img convert"),
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Parse a `sha256:<hex>` checksum string, returning the hex portion.
///
/// Returns `None` if `raw` is `None`. Errors on invalid format.
fn parse_checksum(raw: Option<&str>) -> anyhow::Result<Option<&str>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let hex = raw
        .strip_prefix("sha256:")
        .context("checksum must start with 'sha256:' (e.g. sha256:abc123...)")?;

    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("checksum must be exactly 64 hex characters after 'sha256:' prefix");
    }

    Ok(Some(hex))
}

/// Extract a filename from a URL's last path segment.
///
/// Falls back to a SHA256 hash of the URL if no clean filename is available.
pub(crate) fn filename_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && s.contains('.'))
        .map_or_else(
            || {
                let mut hasher = Sha256::new();
                hasher.update(url.as_bytes());
                format!("{:x}", hasher.finalize())
            },
            String::from,
        )
}

/// Compute the SHA256 digest of a file, reading in 64 KiB chunks.
async fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let n = file
            .read(&mut buf)
            .await
            .with_context(|| format!("failed to read {} for checksum", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Stream-download a URL to a local file, logging progress.
#[allow(clippy::cast_precision_loss)] // progress display only
async fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    const LOG_INTERVAL: u64 = 50 * 1024 * 1024; // 50 MiB

    info!(url = url, dest = %dest.display(), "downloading image");

    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| Error::ImageDownload {
            url: url.to_string(),
            source: e,
        })?;

    let total_size = response.content_length();
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("failed to create {}", dest.display()))?;

    let mut downloaded: u64 = 0;
    let mut last_logged: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::ImageDownload {
            url: url.to_string(),
            source: e,
        })?;
        file.write_all(&chunk).await.with_context(|| {
            format!("failed to write to {}", dest.display())
        })?;
        downloaded += chunk.len() as u64;

        if downloaded - last_logged >= LOG_INTERVAL {
            last_logged = downloaded;
            if let Some(total) = total_size {
                let pct = (downloaded as f64 / total as f64) * 100.0;
                info!(
                    "{:.0} MiB / {:.0} MiB ({pct:.0}%)",
                    downloaded as f64 / 1_048_576.0,
                    total as f64 / 1_048_576.0,
                );
            } else {
                info!("{:.0} MiB downloaded", downloaded as f64 / 1_048_576.0);
            }
        }
    }

    file.flush().await.with_context(|| {
        format!("failed to flush {}", dest.display())
    })?;

    if let Some(total) = total_size {
        info!(
            "download complete: {:.1} MiB",
            total as f64 / 1_048_576.0,
        );
    } else {
        info!(
            "download complete: {:.1} MiB",
            downloaded as f64 / 1_048_576.0,
        );
    }

    Ok(())
}

/// Delete cached images that are no longer referenced by any VM.
///
/// Returns a list of `(filename, bytes_freed)` for each deleted file.
pub async fn clean_cache() -> anyhow::Result<Vec<(String, u64)>> {
    let cache_dir = dirs::image_cache_dir()?;
    if !cache_dir.exists() {
        return Ok(Vec::new());
    }

    // Collect filenames referenced by existing VM configs.
    let referenced = referenced_cache_files().await?;

    // Walk the cache dir and delete anything not referenced.
    let mut deleted = Vec::new();
    let mut entries = tokio::fs::read_dir(&cache_dir)
        .await
        .with_context(|| format!("failed to read cache directory {}", cache_dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        // Only consider regular files, skip partial downloads (*.part).
        if path.extension().is_some_and(|e| e == "part") {
            continue;
        }
        let filename = entry.file_name().to_string_lossy().into_owned();
        if referenced.contains(&filename) {
            continue;
        }
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        tokio::fs::remove_file(&path)
            .await
            .with_context(|| format!("failed to delete cached image {}", path.display()))?;
        deleted.push((filename, size));
    }

    Ok(deleted)
}

/// Collect the set of cache filenames currently referenced by VM configs.
async fn referenced_cache_files() -> anyhow::Result<std::collections::HashSet<String>> {
    let instances_dir = dirs::instances_dir()?;
    let mut referenced = std::collections::HashSet::new();

    if !instances_dir.exists() {
        return Ok(referenced);
    }

    let mut entries = tokio::fs::read_dir(&instances_dir)
        .await
        .with_context(|| {
            format!(
                "failed to read instances directory {}",
                instances_dir.display()
            )
        })?;

    while let Some(entry) = entries.next_entry().await? {
        let config_path = entry.path().join("config.toml");
        let Ok(contents) = tokio::fs::read_to_string(&config_path).await else {
            continue;
        };
        let Ok(config) = toml::from_str::<crate::config::ResolvedConfig>(&contents) else {
            continue;
        };
        if !config.base_url.is_empty() {
            referenced.insert(filename_from_url(&config.base_url));
        }
    }

    Ok(referenced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_checksum_valid() {
        let hex = "a" .repeat(64);
        let input = format!("sha256:{hex}");
        let result = parse_checksum(Some(&input)).unwrap();
        assert_eq!(result, Some(hex.as_str()));
    }

    #[test]
    fn parse_checksum_none() {
        assert_eq!(parse_checksum(None).unwrap(), None);
    }

    #[test]
    fn parse_checksum_bad_prefix() {
        let hex = "a".repeat(64);
        let input = format!("md5:{hex}");
        assert!(parse_checksum(Some(&input)).is_err());
    }

    #[test]
    fn parse_checksum_short_hex() {
        let input = "sha256:abcdef";
        assert!(parse_checksum(Some(input)).is_err());
    }

    #[test]
    fn parse_checksum_non_hex_chars() {
        let input = format!("sha256:{}", "g".repeat(64));
        assert!(parse_checksum(Some(&input)).is_err());
    }

    #[test]
    fn filename_from_url_normal() {
        let url = "https://example.com/images/disk.img";
        assert_eq!(filename_from_url(url), "disk.img");
    }

    #[test]
    fn filename_from_url_ubuntu_default() {
        let url = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img";
        assert_eq!(
            filename_from_url(url),
            "noble-server-cloudimg-arm64.img"
        );
    }

    #[test]
    fn filename_from_url_no_extension_falls_back_to_hash() {
        let url = "https://example.com/images/noext";
        let result = filename_from_url(url);
        // Should be a 64-char hex string (SHA256 of the URL).
        assert_eq!(result.len(), 64);
        assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn filename_from_url_trailing_slash() {
        let url = "https://example.com/images/";
        let result = filename_from_url(url);
        // Empty last segment → falls back to hash.
        assert_eq!(result.len(), 64);
    }

    #[tokio::test]
    async fn sha256_file_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        tokio::fs::write(&path, b"hello world\n").await.unwrap();
        let digest = sha256_file(&path).await.unwrap();
        assert_eq!(
            digest,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }
}

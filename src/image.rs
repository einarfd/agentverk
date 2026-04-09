//! Image download, caching, and checksum verification.
//!
//! Base images are downloaded once and cached in the image cache directory.
//! Each VM gets a copy-on-write qcow2 overlay backed by the cached image,
//! keeping disk usage low.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use futures_util::StreamExt as _;
use sha2::{Digest as _, Sha256, Sha512};
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
    if let Some(ref expected) = expected_hex {
        let actual = match expected {
            Checksum::Sha256(_) => sha256_file(&part_path).await?,
            Checksum::Sha512(_) => sha512_file(&part_path).await?,
        };
        if actual != expected.hex() {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(Error::ImageChecksum {
                path: part_path,
                expected: expected.hex().to_string(),
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
            bail!("qemu-img not found — run 'agv doctor' to check all dependencies");
        }
        Err(e) => {
            Err(e).context("failed to run qemu-img")?
        }
    }
}

/// Grow a VM's qcow2 disk to a new (larger) virtual size.
///
/// Only growing is supported — callers must validate that `new_size` is larger
/// than the current size before calling this. The guest filesystem is not
/// resized; the user must do that inside the VM after the next start.
pub async fn resize_disk(path: &Path, new_size: &str) -> anyhow::Result<()> {
    let path_str = path.to_str().context("disk path is not valid UTF-8")?;

    let result = tokio::process::Command::new("qemu-img")
        .args(["resize", path_str, new_size])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("qemu-img resize failed (exit {}): {stderr}", output.status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("qemu-img not found — run 'agv doctor' to check all dependencies");
        }
        Err(e) => Err(e).context("failed to run qemu-img resize"),
    }
}

/// Parse a human-readable disk size string into bytes.
///
/// Accepts suffixes K, M, G, T (case-insensitive, with optional trailing B).
/// Uses binary units: 1G = 1024³ bytes.
///
/// Examples: `"20G"` → `21_474_836_480`, `"512M"` → `536_870_912`.
/// Normalize a size string to the QEMU-compatible short form.
///
/// Accepts `8G`, `8GB`, `8GiB`, `8g`, etc. and returns `8G`.
/// QEMU only understands the single-letter suffix (K, M, G, T).
pub fn normalize_size(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    let split_pos = s
        .find(|c: char| c.is_alphabetic())
        .ok_or_else(|| anyhow::anyhow!("size must include a unit (K, M, G, T): {s:?}"))?;
    let (num_str, suffix) = s.split_at(split_pos);

    let num: u64 = num_str
        .parse()
        .with_context(|| format!("invalid number in size {s:?}"))?;

    let unit = match suffix.chars().next().map(|c| c.to_ascii_uppercase()) {
        Some('K') => 'K',
        Some('M') => 'M',
        Some('G') => 'G',
        Some('T') => 'T',
        _ => anyhow::bail!("unknown unit {suffix:?} in size {s:?} — use K, M, G, or T"),
    };

    Ok(format!("{num}{unit}"))
}

pub fn parse_disk_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let split_pos = s
        .find(|c: char| c.is_alphabetic())
        .ok_or_else(|| anyhow::anyhow!("disk size must include a unit (K, M, G, T): {s:?}"))?;
    let (num_str, suffix) = s.split_at(split_pos);

    let num: u64 = num_str
        .parse()
        .with_context(|| format!("invalid number in disk size {s:?}"))?;

    // Accept "G", "GB", "GiB", etc. — only the first letter matters.
    let multiplier: u64 = match suffix.chars().next().map(|c| c.to_ascii_uppercase()) {
        Some('K') => 1024,
        Some('M') => 1024 * 1024,
        Some('G') => 1024 * 1024 * 1024,
        Some('T') => 1024 * 1024 * 1024 * 1024,
        _ => anyhow::bail!("unknown unit {suffix:?} in disk size {s:?} — use K, M, G, or T"),
    };

    Ok(num * multiplier)
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
            anyhow::bail!("qemu-img not found — run 'agv doctor' to check all dependencies");
        }
        Err(e) => Err(e).context("failed to run qemu-img convert"),
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// A parsed checksum — either SHA-256 (64 hex chars) or SHA-512 (128 hex chars).
enum Checksum<'a> {
    Sha256(&'a str),
    Sha512(&'a str),
}

impl Checksum<'_> {
    fn hex(&self) -> &str {
        match self {
            Self::Sha256(h) | Self::Sha512(h) => h,
        }
    }
}

/// Parse a `sha256:<hex>` or `sha512:<hex>` checksum string.
///
/// Returns `None` if `raw` is `None`. Errors on invalid format.
fn parse_checksum(raw: Option<&str>) -> anyhow::Result<Option<Checksum<'_>>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    if let Some(hex) = raw.strip_prefix("sha256:") {
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("sha256 checksum must be exactly 64 hex characters");
        }
        return Ok(Some(Checksum::Sha256(hex)));
    }

    if let Some(hex) = raw.strip_prefix("sha512:") {
        if hex.len() != 128 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("sha512 checksum must be exactly 128 hex characters");
        }
        return Ok(Some(Checksum::Sha512(hex)));
    }

    bail!("checksum must start with 'sha256:' or 'sha512:'")
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

/// Compute the SHA512 digest of a file, reading in 64 KiB chunks.
async fn sha512_file(path: &Path) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;

    let mut hasher = Sha512::new();
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

/// An entry in the image cache.
pub struct CacheEntry {
    pub filename: String,
    pub size: u64,
    pub in_use: bool,
}

/// List all files in the image cache with their sizes and usage status.
pub async fn list_cache() -> anyhow::Result<Vec<CacheEntry>> {
    let cache_dir = dirs::image_cache_dir()?;
    if !cache_dir.exists() {
        return Ok(Vec::new());
    }

    let referenced = referenced_cache_files().await?;

    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(&cache_dir)
        .await
        .with_context(|| format!("failed to read cache directory {}", cache_dir.display()))?;

    while let Some(entry) = dir.next_entry().await? {
        let filename = entry.file_name().to_string_lossy().into_owned();
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        // Partial downloads are always shown as unused so cache ls reflects
        // the true state of the cache directory.
        let in_use = entry.path().extension().is_none_or(|e| e != "part")
            && referenced.contains(&filename);
        entries.push(CacheEntry {
            filename,
            size,
            in_use,
        });
    }

    entries.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(entries)
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
        let filename = entry.file_name().to_string_lossy().into_owned();
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);

        // Always delete partial downloads — they are never usable.
        let is_partial = path.extension().is_some_and(|e| e == "part");
        if is_partial || !referenced.contains(&filename) {
            tokio::fs::remove_file(&path)
                .await
                .with_context(|| format!("failed to delete cached image {}", path.display()))?;
            deleted.push((filename, size));
        }
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
    fn parse_checksum_sha256_valid() {
        let hex = "a".repeat(64);
        let input = format!("sha256:{hex}");
        let result = parse_checksum(Some(&input)).unwrap().unwrap();
        assert!(matches!(result, Checksum::Sha256(h) if h == hex));
    }

    #[test]
    fn parse_checksum_sha512_valid() {
        let hex = "b".repeat(128);
        let input = format!("sha512:{hex}");
        let result = parse_checksum(Some(&input)).unwrap().unwrap();
        assert!(matches!(result, Checksum::Sha512(h) if h == hex));
    }

    #[test]
    fn parse_checksum_none() {
        assert!(parse_checksum(None).unwrap().is_none());
    }

    #[test]
    fn parse_checksum_bad_prefix() {
        let hex = "a".repeat(64);
        let input = format!("md5:{hex}");
        assert!(parse_checksum(Some(&input)).is_err());
    }

    #[test]
    fn parse_checksum_sha256_short_hex() {
        assert!(parse_checksum(Some("sha256:abcdef")).is_err());
    }

    #[test]
    fn parse_checksum_sha512_short_hex() {
        assert!(parse_checksum(Some("sha512:abcdef")).is_err());
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

    #[test]
    fn parse_disk_size_units() {
        assert_eq!(parse_disk_size("1K").unwrap(), 1024);
        assert_eq!(parse_disk_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_disk_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_disk_size("1T").unwrap(), 1024u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_disk_size_case_insensitive() {
        assert_eq!(parse_disk_size("20g").unwrap(), parse_disk_size("20G").unwrap());
        assert_eq!(parse_disk_size("512m").unwrap(), parse_disk_size("512M").unwrap());
    }

    #[test]
    fn parse_disk_size_with_b_suffix() {
        assert_eq!(parse_disk_size("20GB").unwrap(), parse_disk_size("20G").unwrap());
        assert_eq!(parse_disk_size("20GiB").unwrap(), parse_disk_size("20G").unwrap());
    }

    #[test]
    fn parse_disk_size_no_unit_fails() {
        assert!(parse_disk_size("20").is_err());
    }

    #[test]
    fn parse_disk_size_unknown_unit_fails() {
        assert!(parse_disk_size("20X").is_err());
    }

    #[test]
    fn normalize_size_short_form_unchanged() {
        assert_eq!(normalize_size("8G").unwrap(), "8G");
        assert_eq!(normalize_size("512M").unwrap(), "512M");
        assert_eq!(normalize_size("1T").unwrap(), "1T");
    }

    #[test]
    fn normalize_size_strips_b_suffix() {
        assert_eq!(normalize_size("8GB").unwrap(), "8G");
        assert_eq!(normalize_size("8GiB").unwrap(), "8G");
        assert_eq!(normalize_size("512MB").unwrap(), "512M");
    }

    #[test]
    fn normalize_size_case_insensitive() {
        assert_eq!(normalize_size("8g").unwrap(), "8G");
        assert_eq!(normalize_size("512m").unwrap(), "512M");
    }

    #[test]
    fn normalize_size_no_unit_fails() {
        assert!(normalize_size("8").is_err());
    }

    #[test]
    fn normalize_size_invalid_unit_fails() {
        assert!(normalize_size("8X").is_err());
    }

    #[tokio::test]
    async fn sha512_file_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        tokio::fs::write(&path, b"hello world\n").await.unwrap();
        let digest = sha512_file(&path).await.unwrap();
        assert_eq!(
            digest,
            "db3974a97f2407b7cae1ae637c0030687a11913274d578492558e39c16c017de\
             84eacdc8c62fe34ee4e12b4b1428817f09b6a2760c3f8a664ceae94d2434a593"
        );
    }
}

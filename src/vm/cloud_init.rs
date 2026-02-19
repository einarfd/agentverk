//! Cloud-init seed image generation.
//!
//! Builds an ISO seed image containing user-data and meta-data for
//! first-boot configuration: SSH keys, hostname, files, and user setup.

use std::path::Path;

/// Generate a cloud-init seed ISO at the given output path.
///
/// The seed image contains:
/// - `authorized_keys` with the generated SSH public key
/// - `hostname` set to the VM name
/// - `write_files` entries for all files to inject
/// - Basic user setup (default user with sudo access)
pub async fn generate_seed(
    _output: &Path,
    _vm_name: &str,
    _ssh_pub_key: &str,
    _files: &[(String, String)],
) -> anyhow::Result<()> {
    todo!("generate cloud-init seed.iso with user-data and meta-data")
}

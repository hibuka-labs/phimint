//! Self-update apply layer for standalone installs.
//!
//! `phimint update` runs the check → download → verify → replace cycle against
//! the release archive named by the manifest. Verification is SHA-256 from the
//! manifest entry (older manifests without it are accepted but flagged);
//! replacement is atomic on Unix (sibling temp + rename). Installs owned by a
//! package manager are never written to — the CLI prints that channel's
//! upgrade command instead (see [`super::install_source`]).

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::error::UpdateError;

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Verify `bytes` against an expected lowercase-hex SHA-256.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), UpdateError> {
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(UpdateError::ChecksumMismatch {
            expected: expected.to_string(),
            actual,
        })
    }
}

/// Extract the `bin_name` executable from a gzipped tar release archive.
///
/// Accepts both a flat archive (`phimint` at the root) and a single top-level
/// directory (`phimint-0.2.0-darwin-aarch64/phimint`) — both layouts ship.
pub fn extract_binary(archive: &[u8], bin_name: &str) -> Result<Vec<u8>, UpdateError> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar
        .entries()
        .map_err(|e| UpdateError::Archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| UpdateError::Archive(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| UpdateError::Archive(e.to_string()))?
            .into_owned();
        if path.file_name().map(|n| n == bin_name).unwrap_or(false) {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| UpdateError::Archive(e.to_string()))?;
            return Ok(buf);
        }
    }
    Err(UpdateError::Archive(format!(
        "no `{bin_name}` entry in release archive"
    )))
}

/// Atomically replace the executable at `target` with `new_bytes`.
///
/// Writes a sibling temp file first, then renames over — rename is atomic on
/// Unix and safe to do while the old binary is still running (the process
/// keeps its inode).
pub fn replace_executable(target: &Path, new_bytes: &[u8]) -> Result<(), UpdateError> {
    let tmp = target.with_extension("new");
    std::fs::write(&tmp, new_bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, target)?;
    Ok(())
}

/// Download the release archive at `url`, verify its SHA-256 (when the
/// manifest entry carries one), extract the `phimint` binary and atomically
/// replace the running executable. Returns the replaced path.
///
/// Call only for standalone installs: package-manager installs are owned by
/// their manager (brew checksums break if we overwrite their binary).
pub async fn download_and_replace(
    client: &reqwest::Client,
    url: &str,
    sha256: Option<&str>,
) -> Result<PathBuf, UpdateError> {
    // Large download: binary archives are multi-MB. Not the checker's 3s probe.
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;

    match sha256 {
        Some(expected) => verify_sha256(&bytes, expected)?,
        None => {
            tracing::warn!(
                %url,
                "manifest entry has no sha256; installing without checksum verification"
            );
        }
    }

    let new_binary = extract_binary(&bytes, "phimint")?;
    let exe = std::env::current_exe().map_err(|e| UpdateError::Archive(e.to_string()))?;
    replace_executable(&exe, &new_binary)?;
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tgz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for (name, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, name, *data).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut out = Vec::new();
        let mut enc = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        enc.finish().unwrap();
        out
    }

    #[test]
    fn sha256_known_vector() {
        // sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_accepts_case_insensitive() {
        assert!(
            verify_sha256(
                b"abc",
                "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
            )
            .is_ok()
        );
    }

    #[test]
    fn verify_rejects_mismatch() {
        let err = verify_sha256(b"abc", "deadbeef").unwrap_err();
        assert!(matches!(err, UpdateError::ChecksumMismatch { .. }));
    }

    #[test]
    fn extract_flat_archive() {
        let tgz = make_tgz(&[("phimint", b"binary-v2")]);
        assert_eq!(extract_binary(&tgz, "phimint").unwrap(), b"binary-v2");
    }

    #[test]
    fn extract_nested_archive() {
        let tgz = make_tgz(&[
            ("phimint-0.2.0-darwin-aarch64/README", b"hi"),
            ("phimint-0.2.0-darwin-aarch64/phimint", b"nested-binary"),
        ]);
        assert_eq!(extract_binary(&tgz, "phimint").unwrap(), b"nested-binary");
    }

    #[test]
    fn extract_missing_binary_errors() {
        let tgz = make_tgz(&[("README", b"hi")]);
        assert!(matches!(
            extract_binary(&tgz, "phimint"),
            Err(UpdateError::Archive(_))
        ));
    }

    #[test]
    fn replace_executable_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("phimint");
        std::fs::write(&target, b"old").unwrap();
        replace_executable(&target, b"new-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "replaced binary must stay executable");
        }
    }
}

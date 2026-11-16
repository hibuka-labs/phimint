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

/// How long a single archive download may take.
///
/// 120s (the old value) is not enough for a link running at 41 KB/s — the
/// measured GitHub rate from a China-hosted machine — which transfers only
/// 5 MB of the 25 MB archive in that time. 300s covers a slow-but-working
/// link, and the rate floor catches a stalled one much sooner.
pub const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The rate below which a download is considered stalled, in bytes/sec.
///
/// 32 KiB/s sits below the measured 41 KB/s of a merely-degraded link, so a
/// slow-but-working mirror is not abandoned. Only a link that has genuinely
/// stopped moving trips this.
pub const RATE_FLOOR: u64 = 32 * 1024;

/// How many bytes must arrive before the rate is judged.
///
/// Without this a slow start would be mistaken for a slow link.
pub const RATE_EVIDENCE: u64 = 64 * 1024;

/// Whether a measured transfer rate is too slow to keep going.
///
/// `bytes` is what arrived, `elapsed_secs` the time it took.
pub fn rate_is_below_floor(bytes: u64, elapsed_secs: u64) -> bool {
    if bytes < RATE_EVIDENCE {
        return false;
    }
    let elapsed = elapsed_secs.max(1);
    bytes / elapsed < RATE_FLOOR
}

/// The platform entry to fall back to, from the other mirror's manifest.
pub fn fallback_entry(
    manifest: &crate::update::manifest::Manifest,
    platform_key: &str,
) -> Option<crate::update::manifest::PlatformEntry> {
    manifest.platforms.get(platform_key).cloned()
}

/// Download the release archive at `url`, verify its SHA-256 (when the
/// manifest entry carries one), extract the `phimint` binary and atomically
/// replace the running executable. Returns the replaced path.
///
/// `fallback` is the other mirror's entry for the same version and platform,
/// used when the primary download fails. The fallback bytes are verified
/// against `sha256` — the *winner's* expected digest — because both mirrors
/// publish the same archive: a mirror serving something else is caught by
/// the checksum rather than silently installed.
pub async fn download_and_replace(
    client: &reqwest::Client,
    url: &str,
    sha256: Option<&str>,
    fallback: Option<(&str, &str)>,
) -> Result<PathBuf, UpdateError> {
    let bytes = match download_bytes(client, url).await {
        Ok(bytes) => bytes,
        Err(primary_err) => {
            let Some((fallback_url, fallback_sha)) = fallback else {
                return Err(primary_err);
            };
            tracing::warn!(
                error = %primary_err,
                fallback = %fallback_url,
                "primary download failed, retrying on the other mirror"
            );
            download_bytes(client, fallback_url).await?
        }
    };

    // Verify against the winner's digest in both cases: the fallback archive
    // is the same artifact, so the same digest applies.
    let expected = sha256.or(fallback.map(|(_, sha)| sha));
    if let Some(expected) = expected {
        verify_sha256(&bytes, expected)?;
    } else {
        tracing::warn!(%url, "manifest entry has no sha256; installing without checksum verification");
    }

    let new_binary = extract_binary(&bytes, "phimint")?;
    let exe = std::env::current_exe().map_err(|e| UpdateError::Archive(e.to_string()))?;
    replace_executable(&exe, &new_binary)?;
    Ok(exe)
}

/// Download an archive, reading it in chunks.
///
/// The rate floor abandons a stalled download early instead of waiting out
/// the full timeout.
async fn download_bytes(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<u8>, UpdateError> {
    let resp = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await?
        .error_for_status()?;

    let mut body = Vec::new();
    let mut resp = resp;
    let start = std::time::Instant::now();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if rate_is_below_floor(body.len() as u64, start.elapsed().as_secs()) {
            return Err(UpdateError::Archive(format!(
                "download stalled at {} bytes from {url}",
                body.len()
            )));
        }
    }
    Ok(body)
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

    #[test]
    fn fallback_entry_comes_from_the_runner_up_manifest() {
        use crate::update::manifest::{Manifest, PlatformEntry};
        use std::collections::HashMap;

        let mut platforms = HashMap::new();
        platforms.insert(
            "linux-x86_64".to_string(),
            PlatformEntry {
                signature: String::new(),
                url: "https://gitee.com/.../phimint.tar.gz".to_string(),
                sha256: None,
            },
        );
        let runner_up = Manifest {
            version: "0.11.0".to_string(),
            notes: None,
            pub_date: None,
            channel: "stable".to_string(),
            mandatory: false,
            platforms,
        };

        let entry = fallback_entry(&runner_up, "linux-x86_64");
        assert_eq!(
            entry.map(|e| e.url),
            Some("https://gitee.com/.../phimint.tar.gz".to_string())
        );
    }

    #[test]
    fn fallback_entry_is_none_for_an_unknown_platform() {
        let runner_up = crate::update::manifest::Manifest::empty();
        assert!(fallback_entry(&runner_up, "linux-x86_64").is_none());
    }

    #[test]
    fn the_rate_floor_lets_a_slow_link_through() {
        // 41 KB/s is slow but above the 32 KiB/s floor: keep going.
        assert!(!rate_is_below_floor(41 * 1024, 1));
    }

    #[test]
    fn the_rate_floor_stops_a_stalled_link() {
        // 4 KiB/s sustained: after the 64 KiB of evidence has arrived (16 s
        // at that rate), the measured rate is still below the floor.
        assert!(rate_is_below_floor(64 * 1024, 16));
    }

    #[test]
    fn the_rate_floor_waits_for_enough_bytes() {
        // Below 64 KiB of evidence the rate is not yet meaningful, so a slow
        // start is not mistaken for a slow link.
        assert!(!rate_is_below_floor(4 * 1024, 16));
    }

    #[test]
    fn the_rate_floor_divides_by_at_least_one_second() {
        // Zero elapsed time must not divide by zero: 64 KiB in under a
        // second is fast, not stalled.
        assert!(!rate_is_below_floor(64 * 1024, 0));
    }
}

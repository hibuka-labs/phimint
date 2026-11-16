//! Self-update apply layer for standalone installs.
//!
//! `phimint update` runs the check → download → verify → replace cycle against
//! the release archive named by the manifest. Verification is SHA-256 from the
//! manifest entry (older manifests without it are accepted but flagged);
//! replacement is a sibling temp + rename on Unix. Windows allows no
//! overwrite of a running image, so it renames the running binary to
//! `<exe>.old` first and lets the next launch delete it. Installs owned by a
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

/// Extract the `bin_name` executable from a release archive.
///
/// The release ships two archive layouts and both must land here: a gzipped
/// tar on unix, and a zip on windows (the archive GitHub's runner builds with
/// `Compress-Archive`). The format is sniffed from the bytes rather than
/// named by the caller, so the manifest stays format-agnostic.
pub fn extract_binary(archive: &[u8], bin_name: &str) -> Result<Vec<u8>, UpdateError> {
    if archive.starts_with(b"PK\x03\x04") {
        return extract_from_zip(archive, bin_name);
    }
    extract_from_tar(archive, bin_name)
}

/// The executable's name inside the archive: the windows archive carries
/// `phimint.exe`, the unix ones `phimint`.
fn binary_name() -> &'static str {
    if cfg!(windows) {
        "phimint.exe"
    } else {
        "phimint"
    }
}

/// Regex-free zip read: the first entry whose file name matches wins.
fn extract_from_zip(archive: &[u8], bin_name: &str) -> Result<Vec<u8>, UpdateError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|e| UpdateError::Archive(e.to_string()))?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| UpdateError::Archive(e.to_string()))?;
        let name = Path::new(entry.name())
            .file_name()
            .map(|n| n == bin_name)
            .unwrap_or(false);
        if name {
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

/// Extract from a gzipped tar. Accepts both a flat archive (`phimint` at the
/// root) and a single top-level directory
/// (`phimint-0.2.0-darwin-aarch64/phimint`) — both layouts ship.
fn extract_from_tar(archive: &[u8], bin_name: &str) -> Result<Vec<u8>, UpdateError> {
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

/// The path a self-update moves the running binary to: a sibling named after
/// the executable plus `.old`.
fn old_binary_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".old");
    target.with_file_name(name)
}

/// Remove the binary a previous self-update moved aside.
///
/// Windows cannot delete the image it is executing, so the replaced binary
/// survives as `<exe>.old` until a later launch. Best-effort by design: a
/// missing file, or one another process still holds, is not an update
/// failure.
pub fn cleanup_old_binary() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::fs::remove_file(old_binary_path(&exe));
}

/// Replace the executable at `target` with `new_bytes`.
///
/// Writes a sibling temp file first, then renames over. On Unix rename is
/// atomic and safe while the old binary is still running (the process keeps
/// its inode). On Windows a running image cannot be overwritten or deleted,
/// but it can be renamed: the running binary is moved to `<exe>.old`, the
/// temp file takes its place, and the leftover is removed by
/// [`cleanup_old_binary`] next launch.
pub fn replace_executable(target: &Path, new_bytes: &[u8]) -> Result<(), UpdateError> {
    let tmp = target.with_extension("new");
    std::fs::write(&tmp, new_bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, target)?;
    }
    #[cfg(windows)]
    {
        // Move the running image aside. If that fails (another process holds
        // the file without delete sharing, or permissions), nothing is
        // changed and the temp file is dropped.
        let old = old_binary_path(target);
        let _ = std::fs::remove_file(&old);
        if let Err(e) = std::fs::rename(target, &old) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        // Put the new binary in place; if that fails, put the old one back
        // so an update never leaves the path without a binary.
        if let Err(e) = std::fs::rename(&tmp, target) {
            let _ = std::fs::rename(&old, target);
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    Ok(())
}

/// Download the release archive at `url`, verify its SHA-256 (when the
/// manifest entry carries one), extract the binary and replace the running
/// executable. Returns the replaced path.
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

    let new_binary = extract_binary(&bytes, binary_name())?;
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

    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in files {
                writer.start_file(*name, options).unwrap();
                std::io::Write::write_all(&mut writer, *data).unwrap();
            }
            writer.finish().unwrap();
        }
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
    fn extract_zip_archive() {
        // The windows layout: a zip with the `.exe` at its root.
        let zip = make_zip(&[("phimint.exe", b"windows-binary")]);
        assert_eq!(
            extract_binary(&zip, "phimint.exe").unwrap(),
            b"windows-binary"
        );
    }

    #[test]
    fn extract_zip_missing_binary_errors() {
        let zip = make_zip(&[("README", b"hi")]);
        assert!(matches!(
            extract_binary(&zip, "phimint.exe"),
            Err(UpdateError::Archive(_))
        ));
    }

    #[test]
    fn old_binary_path_is_a_sibling() {
        let target = Path::new("/tmp/phimint.exe");
        assert_eq!(
            old_binary_path(target),
            PathBuf::from("/tmp/phimint.exe.old")
        );
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

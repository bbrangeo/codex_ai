//! Blocking access to the native OAuth credential store. Call from a blocking thread.
//! All operations require the same canonical cache directory.

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::{Result, anyhow};
use keyring::{Entry, Error};
use sha2::{Digest as _, Sha256};

const SERVICE: &str = "codex_ai.oauth";

pub(super) fn load(directory: &Path) -> Result<Option<Vec<u8>>> {
    match entry(directory)?.get_secret() {
        Ok(secret) => Ok(Some(secret)),
        Err(Error::NoEntry) => Ok(None),
        Err(error) => Err(store_error("read", error)),
    }
}

pub(super) fn save(directory: &Path, secret: &[u8]) -> Result<()> {
    #[cfg(windows)]
    validate_windows_secret_size(secret.len())?;

    entry(directory)?
        .set_secret(secret)
        .map_err(|error| store_error("write", error))
}

pub(super) fn remove(directory: &Path) -> Result<()> {
    match entry(directory)?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(error) => Err(store_error("delete", error)),
    }
}

#[cfg(any(windows, test))]
fn validate_windows_secret_size(len: usize) -> Result<()> {
    const CRED_MAX_CREDENTIAL_BLOB_SIZE: usize = 2560;
    anyhow::ensure!(
        len <= CRED_MAX_CREDENTIAL_BLOB_SIZE,
        "OAuth credentials exceed the Windows Credential Manager limit of {CRED_MAX_CREDENTIAL_BLOB_SIZE} bytes"
    );
    Ok(())
}

fn entry(directory: &Path) -> Result<Entry> {
    Entry::new(SERVICE, &cache_id(directory)).map_err(|error| store_error("open", error))
}

fn cache_id(directory: &Path) -> String {
    // Hash the lossless native path representation with a fixed encoding across builds.
    let path = directory.as_os_str();
    #[cfg(unix)]
    let bytes = path.as_bytes();
    #[cfg(windows)]
    let bytes: Vec<u8> = path.encode_wide().flat_map(u16::to_le_bytes).collect();
    format!("{:x}", Sha256::digest(bytes))
}

fn store_error(operation: &str, error: Error) -> anyhow::Error {
    // Do not retain the backend error as a source: it may carry secret bytes.
    let reason = match error {
        Error::NoStorageAccess(_) => "credential store is locked or inaccessible",
        Error::Ambiguous(_) => "multiple credentials match this cache",
        _ => "credential store operation failed",
    };
    anyhow!("could not {operation} OAuth credentials in the native keyring: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn cache_ids_preserve_non_utf8_path_distinctions() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let first = Path::new(OsStr::from_bytes(b"/cache/\xfe"));
        let second = Path::new(OsStr::from_bytes(b"/cache/\xff"));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(cache_id(first), cache_id(second));
    }

    #[test]
    fn backend_error_payload_is_not_exposed() {
        let error = store_error("read", Error::BadEncoding(b"secret bytes".to_vec()));
        assert_eq!(
            error.to_string(),
            "could not read OAuth credentials in the native keyring: credential store operation failed"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn windows_secret_size_limit_is_inclusive() {
        assert!(validate_windows_secret_size(2560).is_ok());
        let error = validate_windows_secret_size(2561).expect_err("oversized secret accepted");
        assert!(error.to_string().contains("limit of 2560 bytes"));
    }
}

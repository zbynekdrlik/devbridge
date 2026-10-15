//! Files only the service may read (#101): the LAN API's private key on a
//! store PC and pz-server's ACME account key.
//!
//! [`write_private`] restricts the file BEFORE its content is written, so the
//! secret is never readable by others, not even for a moment: SYSTEM +
//! Administrators on Windows (the data dir under ProgramData is readable by
//! every local user by default), owner-only (0600) elsewhere.

use std::io::Write;
use std::path::Path;

#[cfg(windows)]
#[path = "file_acl_windows.rs"]
mod platform;

#[cfg(not(windows))]
mod platform {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// Owner read/write only.
    pub fn restrict(path: &Path) -> std::io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
}

/// Restrict `path` to the service (see the module docs); it must exist.
pub fn restrict(path: &Path) -> std::io::Result<()> {
    platform::restrict(path)
}

/// Write `contents` to `path` (created or truncated), restricted to the
/// service before the first byte is written.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::File::create(path)?;
    restrict(path)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_private_writes_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.pem");
        write_private(&path, b"secret one").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"secret one");
        // an existing (longer) file is replaced, not appended to
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[test]
    fn test_write_private_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn test_write_private_into_a_missing_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_private(&dir.path().join("no/such/dir/key.pem"), b"x").is_err());
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Defense-in-depth file permissions for router state.
//!
//! The router state directory contains the bot token (`config.json`),
//! session registry, and inbox/outbox payloads — all of which should
//! only be readable by the owning user. On Unix we enforce `0700` on
//! directories and `0600` on files; on Windows the helpers are no-ops
//! (NTFS ACLs default to user-private under the profile directory).

use std::io;
use std::path::Path;

/// Apply owner-only permissions to a directory (Unix: `0o700`).
#[cfg(unix)]
pub fn secure_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, perms)
}

/// No-op on Windows — inherited ACLs from the user profile directory
/// already keep these private.
#[cfg(not(unix))]
pub fn secure_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Apply owner-only permissions to a file (Unix: `0o600`).
#[cfg(unix)]
pub fn secure_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
}

/// No-op on Windows.
#[cfg(not(unix))]
pub fn secure_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secure_dir_sets_0700() {
        let tmp = tempfile::tempdir().unwrap();
        secure_dir(tmp.path()).unwrap();
        let mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn secure_file_sets_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x");
        std::fs::write(&f, b"x").unwrap();
        secure_file(&f).unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

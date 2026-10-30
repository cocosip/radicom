//! Temporary files that are safe against the classic temp-file race (CORE-17).
//!
//! A predictable name in a shared directory lets another local user pre-create
//! the path as a symlink, so a later `File::create` writes through it. Every
//! temporary file in the workspace goes through [`create_unique_file`], which
//! combines a random name with `create_new` (an existing path is never followed)
//! and `0600` on Unix (a spilled value is often PHI).

use std::io;
use std::path::{Path, PathBuf};

/// Creates `{prefix}-{uuid}.{extension}` in `dir`, never following an existing path.
///
/// Returns the path and the open handle; the caller owns deletion. On Unix the
/// file is created `0600`, so a spilled value is not readable by other local
/// users.
pub fn create_unique_file(
    dir: &Path,
    prefix: &str,
    extension: &str,
) -> io::Result<(PathBuf, std::fs::File)> {
    let path = dir.join(format!("{prefix}-{}.{extension}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(&path).map(|file| (path, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_files_get_fresh_private_paths() {
        let dir = std::env::temp_dir().join(format!("radicom-temp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let (first, handle) = create_unique_file(&dir, "unit", "tmp").unwrap();
        drop(handle);
        let (second, handle) = create_unique_file(&dir, "unit", "tmp").unwrap();
        drop(handle);
        assert_ne!(first, second, "every call gets its own path");
        assert_eq!(std::fs::metadata(&first).unwrap().len(), 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a spilled value is not world-readable");
        }

        std::fs::remove_file(&first).ok();
        std::fs::remove_file(&second).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}

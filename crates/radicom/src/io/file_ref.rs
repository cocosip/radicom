//! File and directory references, and temporary files (IO-01; docs/01-architecture.md §7).
//!
//! A [`PathFile`] is a file that can be opened, created, moved, deleted and
//! range-read. A [`DirectoryRef`] enumerates a directory's entries with an
//! optional glob filter (reusing the same `Glob` as the media scanner).
//! [`TempFileProvider`] hands out UUID-named temporary files whose deletions are
//! deferred and retried on Windows' "still locked" semantics, with a final sweep
//! at process exit.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::Error;
use crate::media::scanner::Glob;

/// A path-backed file reference: open, create, delete, move and range-read
/// (IO-01).
///
/// Deliberately a plain struct, not a trait: the only file abstraction in the
/// crate is a path, so an interface with a single implementation would add
/// indirection without polymorphism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFile {
    path: PathBuf,
}

impl PathFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        PathFile { path: path.into() }
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's current length in bytes.
    pub fn len(&self) -> Result<u64, Error> {
        Ok(fs::metadata(self.path())?.len())
    }

    /// True when the file exists and has zero length.
    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    /// True when the underlying file exists.
    pub fn exists(&self) -> bool {
        self.path().exists()
    }

    /// Opens the file for reading.
    pub fn open_read(&self) -> Result<fs::File, Error> {
        Ok(fs::File::open(self.path())?)
    }

    /// Opens (creating or truncating) the file for writing.
    pub fn open_create(&self) -> Result<fs::File, Error> {
        Ok(fs::File::create(self.path())?)
    }

    /// Reads `len` bytes starting at `offset`.
    pub fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        let mut f = self.open_read()?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Moves (renames) this file to `to`. Fails if `to` already exists on
    /// platforms where rename does not replace (Windows).
    pub fn move_to(&self, to: &Path) -> Result<(), Error> {
        fs::rename(self.path(), to)?;
        Ok(())
    }

    /// Deletes the file; missing is not an error.
    pub fn delete(&self) -> Result<(), Error> {
        match fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// A directory reference that enumerates its entries (IO-01, MEDIA-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryRef {
    path: PathBuf,
}

impl DirectoryRef {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        DirectoryRef { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fully qualified paths of the regular files in this directory, sorted by
    /// name. When `glob` is given, only names matching it are kept. Subdirectories
    /// are not descended (use the media scanner for recursive walks).
    pub fn files(&self, glob: Option<&Glob>) -> Result<Vec<PathBuf>, Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if !ft.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(g) = glob {
                if !g.matches(&name) {
                    continue;
                }
            }
            out.push(entry.path());
        }
        out.sort();
        Ok(out)
    }

    /// Fully qualified paths of the subdirectories in this directory, sorted by name.
    pub fn subdirs(&self) -> Result<Vec<PathBuf>, Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Hands out UUID-named temporary files and cleans them up (IO-01).
///
/// Deletions go through a shared queue: [`delete`](Self::delete) tries immediately
/// and, on failure (a file still open on Windows), retains the path for a later
/// retry; [`sweep`](Self::sweep) retries every queued path, and `Drop` performs a
/// final sweep so nothing outlives the provider in the common case.
pub struct TempFileProvider {
    dir: PathBuf,
    pending: Mutex<Vec<PathBuf>>,
}

impl TempFileProvider {
    /// A provider that creates files in the system temp directory.
    pub fn new() -> Self {
        Self::with_dir(std::env::temp_dir())
    }

    /// A provider that creates files in `dir` (created if missing).
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        // Best-effort: a failure here surfaces on the first create().
        let _ = fs::create_dir_all(&dir);
        TempFileProvider {
            dir,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// The directory in which this provider creates files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates a new empty temporary file with a UUID-based name and returns its
    /// reference. The file is *not* auto-queued for deletion; call
    /// [`delete`](Self::delete) (or `Drop` of the provider for queued paths).
    pub fn create(&self) -> Result<PathFile, Error> {
        let name = format!("radicom-{}.tmp", uuid::Uuid::new_v4());
        let path = self.dir.join(name);
        fs::File::create(&path)?;
        Ok(PathFile::new(path))
    }

    /// Deletes `file`, queuing it for a retry if it is currently locked.
    pub fn delete(&self, file: &PathFile) -> Result<(), Error> {
        match fs::remove_file(file.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => {
                self.pending.lock().unwrap().push(file.path().to_path_buf());
                Ok(())
            }
        }
    }

    /// Retries every queued deletion, keeping paths that are still locked.
    /// Returns the number of files successfully removed this call.
    pub fn sweep(&self) -> usize {
        let mut pending = self.pending.lock().unwrap();
        let mut removed = 0;
        pending.retain(|path| match fs::remove_file(path) {
            Ok(()) => {
                removed += 1;
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                removed += 1;
                false
            }
            Err(_) => true,
        });
        removed
    }
}

impl Default for TempFileProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempFileProvider {
    fn drop(&mut self) {
        self.sweep();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radicom-io01-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn path_file_create_range_move_delete() {
        let dir = unique_dir("fref");
        let f = PathFile::new(dir.join("a.bin"));
        assert!(!f.exists());
        {
            use std::io::Write;
            let mut h = f.open_create().unwrap();
            h.write_all(b"0123456789").unwrap();
        }
        assert!(f.exists());
        assert_eq!(f.len().unwrap(), 10);
        assert_eq!(f.read_range(3, 4).unwrap(), b"3456");

        f.move_to(&dir.join("b.bin")).unwrap();
        let moved = PathFile::new(dir.join("b.bin"));
        assert!(moved.exists());
        assert!(!f.exists());

        moved.delete().unwrap();
        assert!(!moved.exists());
        // deleting again is not an error
        moved.delete().unwrap();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_ref_enumerates_and_filters() {
        let dir = unique_dir("dirref");
        for name in ["x.dcm", "y.dcm", "notes.txt"] {
            fs::write(dir.join(name), b"data").unwrap();
        }
        fs::create_dir(dir.join("sub")).unwrap();

        let d = DirectoryRef::new(&dir);
        let all = d.files(None).unwrap();
        assert_eq!(all.len(), 3);

        let only_dcm = d.files(Some(&Glob::new("*.dcm").unwrap())).unwrap();
        assert_eq!(only_dcm.len(), 2);
        assert!(only_dcm.iter().all(|p| p.extension().unwrap() == "dcm"));

        assert_eq!(d.subdirs().unwrap().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn temp_provider_creates_and_deletes() {
        let dir = unique_dir("temp");
        let tp = TempFileProvider::with_dir(&dir);
        let a = tp.create().unwrap();
        let b = tp.create().unwrap();
        assert!(a.exists() && b.exists());
        assert_ne!(a.path(), b.path(), "names are unique per file");

        tp.delete(&a).unwrap();
        assert!(!a.exists());
        // a successful delete queues nothing
        assert_eq!(tp.sweep(), 0);
        tp.delete(&b).unwrap();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn temp_provider_drop_sweeps_queued_files() {
        let dir = unique_dir("tempdrop");
        let path = {
            let tp = TempFileProvider::with_dir(&dir);
            let f = tp.create().unwrap();
            let p = f.path().to_path_buf();
            // queue it manually to simulate a locked file that could not be removed
            tp.pending.lock().unwrap().push(p.clone());
            assert!(p.exists());
            p
        }; // provider dropped here -> Drop sweeps the queue
        assert!(!path.exists(), "Drop must remove queued temp files");
        fs::remove_dir_all(&dir).ok();
    }
}

//! Files written whole or not at all. The content goes to a temporary file
//! beside the target, which is fsynced and then renamed over the target, so
//! a reader sees the old file or the new one and never half of one. The
//! directory is fsynced after the rename where the platform allows it, so
//! the rename survives a crash right after it. A temporary file that is not
//! renamed into place is removed when the `AtomicFile` is dropped.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{PipesError, Result};
use crate::fhir::ndjson::with_suffix;

/// Writes `bytes` to `path` through `<path>.tmp`.
pub fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let mut f = AtomicFile::create(path)?;
    f.write_all(bytes.as_ref())
        .map_err(|e| PipesError::io(f.tmp(), e))?;
    f.commit()
}

/// Why a state file could not be read.
#[derive(Debug)]
pub enum StateError {
    /// Not JSON of the expected shape, or not UTF-8 text.
    Invalid(String),
    /// Permission denied.
    Unreadable(std::io::Error),
    /// Any other I/O error.
    Io(std::io::Error),
}

/// The one way a JSON state file is read (snapshot `state.json`, export
/// `.export-state.json`, sync `.sync-state.json`): `None` when it does not
/// exist, the value when it parses, and otherwise why not. What a caller
/// does with an error is its own choice.
pub fn read_state<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> std::result::Result<Option<T>, StateError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StateError::Invalid(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            Err(StateError::Invalid(e.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(StateError::Unreadable(e))
        }
        Err(e) => Err(StateError::Io(e)),
    }
}

/// A file being written to a temporary path and renamed to its target by
/// `commit`. It writes unbuffered; wrap it in a `BufWriter` for many small
/// writes.
#[derive(Debug)]
pub struct AtomicFile {
    path: PathBuf,
    tmp: PathBuf,
    /// None when the temporary file is written by something else (`adopt`).
    file: Option<File>,
    /// The temporary file is left in place on drop: renamed, or kept for a
    /// later run to continue.
    keep: bool,
}

impl AtomicFile {
    /// Creates `<path>.tmp`, truncating any left by an earlier run.
    pub fn create(path: &Path) -> Result<Self> {
        Self::create_at(path, with_suffix(path, ".tmp"))
    }

    /// Creates the temporary file at `tmp`, which must be on the same
    /// filesystem as `path` (in practice: in the same directory).
    pub fn create_at(path: &Path, tmp: PathBuf) -> Result<Self> {
        let file = File::create(&tmp).map_err(|e| PipesError::io(&tmp, e))?;
        Ok(Self::with_file(path, tmp, file))
    }

    /// A temporary file already open, such as one reopened to continue an
    /// interrupted run.
    pub fn with_file(path: &Path, tmp: PathBuf, file: File) -> Self {
        Self {
            path: path.to_path_buf(),
            tmp,
            file: Some(file),
            keep: false,
        }
    }

    /// A temporary file written by something else (another function or
    /// program): `commit` opens it to fsync it before the rename, and drop
    /// removes it as for any other.
    pub fn adopt(path: &Path, tmp: PathBuf) -> Self {
        Self {
            path: path.to_path_buf(),
            tmp,
            file: None,
            keep: false,
        }
    }

    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    pub fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("an AtomicFile created, not adopted")
    }

    fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("an AtomicFile created, not adopted")
    }

    /// Leave the temporary file in place when dropped without `commit`.
    pub fn keep_on_drop(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// Fsyncs the temporary file, renames it over the target and fsyncs the
    /// directory (best effort: not every platform or filesystem supports
    /// fsync on a directory, and that is not worth failing a finished write
    /// over).
    pub fn commit(mut self) -> Result<()> {
        let synced = match self.file.take() {
            Some(file) => file.sync_all(),
            None => File::open(&self.tmp).and_then(|f| f.sync_all()),
        };
        synced.map_err(|e| PipesError::io(&self.tmp, e))?;
        std::fs::rename(&self.tmp, &self.path).map_err(|e| PipesError::io(&self.path, e))?;
        self.keep = true;
        if let Some(dir) = self.path.parent()
            && let Ok(d) = File::open(if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            })
        {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

impl Write for AtomicFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file_mut().write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.file_mut().write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file_mut().flush()
    }
}

impl Seek for AtomicFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file_mut().seek(pos)
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.keep {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!dir.path().join("a.json.tmp").exists());
    }

    #[test]
    fn dropped_uncommitted_removes_the_tmp_and_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ndjson");
        std::fs::write(&path, "old").unwrap();
        let mut f = AtomicFile::create(&path).unwrap();
        f.write_all(b"half").unwrap();
        assert!(f.tmp().exists());
        drop(f);
        assert!(!dir.path().join("a.ndjson.tmp").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }

    #[test]
    fn keep_on_drop_leaves_the_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.ndjson");
        let mut f = AtomicFile::create(&path).unwrap();
        f.write_all(b"partial").unwrap();
        f.keep_on_drop(true);
        drop(f);
        let tmp = dir.path().join("a.ndjson.tmp");
        assert_eq!(std::fs::read_to_string(tmp).unwrap(), "partial");
        assert!(!path.exists());
    }

    #[test]
    fn adopt_renames_a_file_written_elsewhere_or_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (path, tmp) = (dir.path().join("out"), dir.path().join(".out.part"));
        std::fs::write(&tmp, "done").unwrap();
        AtomicFile::adopt(&path, tmp.clone()).commit().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "done");
        assert!(!tmp.exists());

        std::fs::write(&tmp, "failed").unwrap();
        drop(AtomicFile::adopt(&path, tmp.clone()));
        assert!(!tmp.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "done");
    }

    #[test]
    fn read_state_tells_missing_invalid_and_valid_apart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(matches!(read_state::<serde_json::Value>(&path), Ok(None)));
        std::fs::write(&path, "{nope").unwrap();
        assert!(matches!(
            read_state::<serde_json::Value>(&path),
            Err(StateError::Invalid(_))
        ));
        std::fs::write(&path, b"\xff\xfe").unwrap();
        assert!(matches!(
            read_state::<serde_json::Value>(&path),
            Err(StateError::Invalid(_))
        ));
        std::fs::write(&path, "{\"a\":1}").unwrap();
        let v: Option<serde_json::Value> = read_state(&path).unwrap();
        assert_eq!(v.unwrap()["a"], 1);
    }

    #[test]
    fn read_state_of_a_directory_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_state::<serde_json::Value>(dir.path()),
            Err(StateError::Io(_))
        ));
    }
}

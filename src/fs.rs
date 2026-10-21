//! Files written whole or not at all. The content goes to a temporary file
//! beside the target, which is fsynced and then renamed over the target, so
//! a reader sees the old file or the new one and never half of one. The
//! directory is fsynced after the rename where the platform allows it, so
//! the rename survives a crash right after it. A temporary file that is not
//! renamed into place is removed when the `AtomicFile` is dropped.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{PipesError, Result};
use crate::fhir::ndjson::with_suffix;

/// The paths in `dir`, sorted. An entry the directory listing fails on is
/// skipped.
pub fn sorted_entries(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    paths.sort();
    Ok(paths)
}

/// As `sorted_entries`, but an entry the listing fails on is an error.
#[cfg(feature = "postgres")]
pub fn sorted_entries_strict(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

/// Writes `bytes` to `path` through `<path>.tmp`.
pub fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let mut f = AtomicFile::create(path)?;
    f.write_all(bytes.as_ref())
        .map_err(|e| PipesError::io(f.tmp(), e))?;
    f.commit()
}

/// Puts the directory `staged` in place of `target`: `target` is renamed to
/// `backup`, `staged` renamed in, then `backup` removed and the parent
/// fsynced. A reader sees the old directory or the new one, never a mix;
/// between the two renames there is none, which `recover_swap` undoes.
/// A backup that cannot be removed afterwards is left for the next run: the
/// new directory is in place.
pub fn swap_dir(staged: &Path, target: &Path, backup: &Path) -> Result<()> {
    if backup.exists() {
        std::fs::remove_dir_all(backup).map_err(|e| PipesError::io(backup, e))?;
    }
    if target.exists() {
        std::fs::rename(target, backup).map_err(|e| PipesError::io(target, e))?;
    }
    std::fs::rename(staged, target).map_err(|e| PipesError::io(staged, e))?;
    let _ = std::fs::remove_dir_all(backup);
    sync_dir(target.parent());
    Ok(())
}

/// After a `swap_dir` cut short between its two renames: the backup is put
/// back where `target` was. A backup left beside a `target` is removed.
pub fn recover_swap(target: &Path, backup: &Path) -> Result<()> {
    if backup.exists() && !target.exists() {
        std::fs::rename(backup, target).map_err(|e| PipesError::io(backup, e))?;
    } else if backup.exists() {
        std::fs::remove_dir_all(backup).map_err(|e| PipesError::io(backup, e))?;
    }
    Ok(())
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

/// Opening options for a file only its owner can read, on Unix (0600 when
/// the file is created). Elsewhere, the defaults.
fn private() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Creates or truncates `path` as `File::create` does, readable only by its
/// owner on Unix when it is created: for exported FHIR resources, which can
/// be patient data.
pub fn create_private(path: &Path) -> std::io::Result<File> {
    private().write(true).create(true).truncate(true).open(path)
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
    /// filesystem as `path` (in practice: in the same directory). One left
    /// by an earlier run is removed first and a new file made, so a symlink
    /// at `tmp` is removed rather than written through.
    pub fn create_at(path: &Path, tmp: PathBuf) -> Result<Self> {
        Self::create_with(path, tmp, OpenOptions::new())
    }

    /// `create_at`, readable only by its owner on Unix: for exported FHIR
    /// resources, which can be patient data.
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub fn create_private_at(path: &Path, tmp: PathBuf) -> Result<Self> {
        Self::create_with(path, tmp, private())
    }

    fn create_with(path: &Path, tmp: PathBuf, mut options: OpenOptions) -> Result<Self> {
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(PipesError::io(&tmp, e));
            }
            _ => {}
        }
        let file = options
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| PipesError::io(&tmp, e))?;
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
    /// directory.
    pub fn commit(mut self) -> Result<()> {
        let synced = match self.file.take() {
            Some(file) => file.sync_all(),
            None => sync_adopted(&self.tmp),
        };
        synced.map_err(|e| PipesError::io(&self.tmp, e))?;
        std::fs::rename(&self.tmp, &self.path).map_err(|e| PipesError::io(&self.path, e))?;
        self.keep = true;
        sync_dir(self.path.parent());
        Ok(())
    }
}

/// Fsyncs a temporary file something else wrote. It is opened for writing,
/// not truncated: on Windows, sync_all (FlushFileBuffers) needs a handle
/// with write access. A read-only file, such as `fs::copy` makes of a
/// read-only source, keeps its permissions: on Unix it is fsynced through a
/// read-only handle, which fsync allows; on Windows its read-only attribute
/// is cleared for the fsync and set again.
fn sync_adopted(tmp: &Path) -> std::io::Result<()> {
    let sync = || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(tmp)
            .and_then(|f| f.sync_all())
    };
    match sync() {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            #[cfg(not(windows))]
            {
                let _ = e;
                File::open(tmp).and_then(|f| f.sync_all())
            }
            #[cfg(windows)]
            {
                let readonly = std::fs::metadata(tmp)?.permissions();
                if !readonly.readonly() {
                    return Err(e);
                }
                let mut writable = readonly.clone();
                // Windows only: this clears the read-only attribute of our
                // own temporary file, which is set again just after.
                #[allow(clippy::permissions_set_readonly_false)]
                writable.set_readonly(false);
                std::fs::set_permissions(tmp, writable)?;
                let synced = sync();
                std::fs::set_permissions(tmp, readonly)?;
                synced
            }
        }
        other => other,
    }
}

/// Fsyncs a directory, so that a rename in it survives a crash. Best
/// effort: not every filesystem supports it, and that is not worth failing
/// a finished write over. Windows cannot open a directory as a file, and
/// NTFS records a rename in its journal, so there it does nothing.
fn sync_dir(dir: Option<&Path>) {
    if cfg!(windows) {
        return;
    }
    let dir = match dir {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
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
    fn a_directory_is_swapped_in_and_a_cut_short_swap_undone() {
        let dir = tempfile::tempdir().unwrap();
        let (staged, target, backup) = (
            dir.path().join(".t.new"),
            dir.path().join("t"),
            dir.path().join(".t.old"),
        );
        for (d, f) in [(&target, "old"), (&staged, "new")] {
            std::fs::create_dir(d).unwrap();
            std::fs::write(d.join(f), "").unwrap();
        }
        swap_dir(&staged, &target, &backup).unwrap();
        assert!(target.join("new").exists() && !target.join("old").exists());
        assert!(!staged.exists() && !backup.exists());
        // Cut short after the first rename: the backup comes back.
        std::fs::rename(&target, &backup).unwrap();
        recover_swap(&target, &backup).unwrap();
        assert!(target.join("new").exists() && !backup.exists());
        // A backup beside the target is left over: removed.
        std::fs::create_dir(&backup).unwrap();
        recover_swap(&target, &backup).unwrap();
        assert!(target.exists() && !backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_temporary_path_is_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("other.txt");
        std::fs::write(&target, "keep").unwrap();
        let out = dir.path().join("out.txt");
        std::os::unix::fs::symlink(&target, dir.path().join("out.txt.tmp")).unwrap();
        atomic_write(&out, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_readable_by_their_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.ndjson");
        create_private(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let out = dir.path().join("q.ndjson");
        AtomicFile::create_private_at(&out, dir.path().join("q.tmp"))
            .unwrap()
            .commit()
            .unwrap();
        let mode = std::fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

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
    fn adopt_commits_a_read_only_file_and_keeps_it_read_only() {
        // `fs::copy` of a read-only source makes a read-only copy; main kept
        // it so, and the commit must not need it writable.
        let dir = tempfile::tempdir().unwrap();
        let (path, tmp) = (dir.path().join("out.ndjson"), dir.path().join(".out.tmp"));
        std::fs::write(&tmp, "{}\n").unwrap();
        let mut perms = std::fs::metadata(&tmp).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&tmp, perms).unwrap();
        AtomicFile::adopt(&path, tmp.clone()).commit().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        assert!(!tmp.exists());
        make_writable(&path);
    }

    /// So the temporary directory can be removed on Windows.
    fn make_writable(path: &Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn adopt_commits_a_closed_file_on_every_platform() {
        // On Windows the fsync of an adopted file needs a handle opened for
        // writing; a read-only one fails and the drop then removes the file.
        let dir = tempfile::tempdir().unwrap();
        let (path, tmp) = (dir.path().join("out.ndjson"), dir.path().join(".out.tmp"));
        {
            let mut f = File::create(&tmp).unwrap();
            f.write_all(b"{}\n").unwrap();
        }
        AtomicFile::adopt(&path, tmp.clone()).commit().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
        assert!(!tmp.exists());
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
    fn read_state_of_a_directory_is_an_error() {
        // An I/O error on Unix; Windows says access is denied.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_state::<serde_json::Value>(dir.path()),
            Err(StateError::Io(_) | StateError::Unreadable(_))
        ));
    }
}

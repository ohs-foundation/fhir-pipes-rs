//! Staging: a run writes its tables, its copies of the views, its tiles, its
//! manifest and its catalog beside the live warehouse, and swaps them in only
//! once every step before the swap has succeeded. A run that fails before then
//! leaves the previous run's warehouse as it was.
//!
//!   parquet/.staging/<table>/   each table the run writes, Locations included
//!   .staging/views/             the views step's copies of the views
//!   .staging/tiles/             the tiles step's archives
//!   .staging/manifest.json      the manifest and catalog.sql
//!   .staging/catalog.sql
//!
//! While the run goes on, a table is read from the staging directory where the
//! run wrote it, else from parquet/, and likewise views/. `publish` renames
//! each staged table into parquet/, then views/ and tiles/ into the warehouse
//! directory, each replacing the whole directory, then the manifest and
//! catalog.sql, so a reader can see two runs' files only during those renames.
//!
//! A run holds an exclusive lock on `.refresh.lock` in the warehouse directory
//! from start to end (`Lock`), so two refreshes of one warehouse take turns
//! instead of removing each other's staging. A run waits for the lock as long
//! as `--lock-timeout` allows, by default for as long as it takes.
//!
//! `publish` writes `.staging/.publishing` before its first rename and removes
//! it after its last. A run that finds it, because the publish before it
//! failed or was killed part way, finishes that publish before anything else:
//! the database already holds that run's rows, and the warehouse would
//! otherwise be left with some tables from each run.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::config::Settings;
use super::partition;

const DIR: &str = ".staging";
const LOCK: &str = ".refresh.lock";
/// In `.staging` while a publish is under way.
const PUBLISHING: &str = ".publishing";
/// The warehouse directories a run stages and swaps in whole.
const DIRS: [&str; 2] = ["views", "tiles"];

/// The warehouse's `.refresh.lock`, held exclusively until dropped.
pub struct Lock(#[allow(dead_code)] fs::File);

/// How often a run waiting with a timeout tries the lock again.
const LOCK_RETRY: Duration = Duration::from_millis(250);

impl Lock {
    /// Take the lock on warehouse `data`, once any other refresh of it has
    /// finished. With `timeout`, give up after waiting that long.
    pub fn acquire(data: &Path, timeout: Option<Duration>) -> Result<Lock> {
        fs::create_dir_all(data).with_context(|| format!("creating {}", data.display()))?;
        let path = data.join(LOCK);
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let locking = || format!("locking {}", path.display());
        match lock.try_lock() {
            Ok(()) => return Ok(Lock(lock)),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(e)) => return Err(e).with_context(locking),
        }
        let Some(timeout) = timeout else {
            println!(
                "   waiting for another refresh of {} to finish",
                data.display()
            );
            lock.lock().with_context(locking)?;
            return Ok(Lock(lock));
        };
        println!(
            "   waiting up to {} s for another refresh of {} to finish",
            timeout.as_secs(),
            data.display()
        );
        let deadline = Instant::now() + timeout;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(Lock(lock)),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(e)) => return Err(e).with_context(locking),
            }
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "another refresh of {} still holds {} after {} s (--lock-timeout)",
                    data.display(),
                    LOCK,
                    timeout.as_secs()
                );
            }
            std::thread::sleep(LOCK_RETRY.min(deadline - now));
        }
    }
}

pub struct Stage {
    data: PathBuf,
    parquet: PathBuf,
    /// parquet/.staging: the run's tables.
    tables: PathBuf,
    /// .staging: the run's manifest and catalog.
    files: PathBuf,
}

impl Stage {
    /// Start a run's staging. The caller holds the warehouse's lock, `_held`,
    /// until the run ends. What a killed run left behind is removed first,
    /// and a table (or views/, tiles/) a killed swap left only as
    /// `.<name>.old` is put back.
    pub fn new(s: &Settings, _held: &Lock) -> Result<Stage> {
        Stage::open(&s.data, &s.parquet_dir())
    }

    fn open(data: &Path, parquet: &Path) -> Result<Stage> {
        let stage = Stage {
            data: data.to_path_buf(),
            parquet: parquet.to_path_buf(),
            tables: parquet.join(DIR),
            files: data.join(DIR),
        };
        let unfinished = stage.publishing();
        if !unfinished {
            stage.clear()?;
        }
        recover(&stage.parquet, "parquet/", |_| true)?;
        recover(&stage.data, "", |name| DIRS.contains(&name))?;
        if unfinished {
            println!("   finishing the publish of a refresh that stopped part way");
            // Another run's publish: this run has published nothing yet.
            stage.publish(&mut false)?;
        }
        for dir in [&stage.tables, &stage.files] {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(stage)
    }

    /// Whether a publish has started and not finished.
    fn publishing(&self) -> bool {
        self.files.join(PUBLISHING).exists()
    }

    fn clear(&self) -> Result<()> {
        for dir in [&self.tables, &self.files] {
            if dir.exists() {
                fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
            }
        }
        Ok(())
    }

    /// The directory the run writes its tables in.
    pub fn tables_dir(&self) -> &Path {
        &self.tables
    }

    /// Where the run writes table `name`.
    pub fn table(&self, name: &str) -> PathBuf {
        self.tables.join(name)
    }

    /// Where the run writes `name` (manifest.json, catalog.sql) until it is
    /// published to the warehouse directory.
    pub fn file(&self, name: &str) -> PathBuf {
        self.files.join(name)
    }

    /// Where the run writes the warehouse directory `name` (views, tiles)
    /// until it is published in place of the live one.
    pub fn dir(&self, name: &str) -> PathBuf {
        debug_assert!(DIRS.contains(&name));
        self.files.join(name)
    }

    /// Where to read the warehouse directory `name` now: the run's own copy
    /// if it wrote one, else the live one.
    pub fn read_dir(&self, name: &str) -> PathBuf {
        let staged = self.dir(name);
        if staged.is_dir() {
            staged
        } else {
            self.data.join(name)
        }
    }

    /// Where to read table `name` now: the run's own copy if it wrote one,
    /// else the live table.
    pub fn read(&self, name: &str) -> PathBuf {
        let staged = self.table(name);
        if staged.is_dir() {
            staged
        } else {
            self.parquet.join(name)
        }
    }

    /// The tables the warehouse will have once the run is published, sorted,
    /// each with the directory to read it from now.
    pub fn tables(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut names = BTreeSet::new();
        for dir in [&self.parquet, &self.tables] {
            names.extend(table_names(dir)?);
        }
        Ok(names
            .into_iter()
            .map(|n| {
                let dir = self.read(&n);
                (n, dir)
            })
            .collect())
    }

    /// Swap the run's tables in, then views/ and tiles/, then its manifest
    /// and catalog. Returns the number of tables and the directories swapped
    /// in.
    /// `published` is set before the first live table, directory or file is
    /// replaced, and stays set if the publish then fails part way.
    pub fn publish(&self, published: &mut bool) -> Result<(usize, Vec<String>)> {
        let marker = self.files.join(PUBLISHING);
        crate::fs::atomic_write(&marker, "")
            .with_context(|| format!("writing {}", marker.display()))?;
        self.swap_in(published)
            .context("publishing the warehouse stopped part way; the next refresh finishes it")
    }

    fn swap_in(&self, published: &mut bool) -> Result<(usize, Vec<String>)> {
        let mut tables = 0;
        for staged in entries(&self.tables)? {
            let live = self
                .parquet
                .join(staged.file_name().context("no file name")?);
            *published = true;
            if staged.is_dir() {
                partition::replace(&staged, &live)?;
                tables += 1;
            } else {
                // The Location transform's _report.json.
                fs::rename(&staged, &live)
                    .with_context(|| format!("renaming {} into place", staged.display()))?;
            }
        }
        let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) =
            entries(&self.files)?.into_iter().partition(|p| p.is_dir());
        let mut swapped = Vec::new();
        for staged in dirs {
            let name = staged.file_name().context("no file name")?;
            *published = true;
            partition::replace(&staged, &self.data.join(name))?;
            swapped.push(name.to_string_lossy().into_owned());
        }
        for staged in files {
            let live = self.data.join(staged.file_name().context("no file name")?);
            *published = true;
            fs::rename(&staged, &live)
                .with_context(|| format!("renaming {} into place", staged.display()))?;
        }
        fs::remove_file(self.files.join(PUBLISHING))?;
        self.clear()?;
        Ok((tables, swapped))
    }

    /// Whether the run staged anything to publish.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(entries(&self.tables)?.is_empty() && entries(&self.files)?.is_empty())
    }
}

/// A run that stops early leaves nothing staged behind, unless it stopped
/// while publishing: the next run finishes that. The lock is released after
/// this, when the file is closed.
impl Drop for Stage {
    fn drop(&mut self) {
        if !self.publishing() {
            let _ = self.clear();
        }
    }
}

/// The entries of `dir` other than dot-files, sorted.
fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths =
        crate::fs::sorted_entries(dir).with_context(|| format!("reading {}", dir.display()))?;
    paths.retain(|p| {
        p.file_name()
            .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
    });
    Ok(paths)
}

/// The tables in `dir`: its directories, leaving out dot-directories, which
/// hold staging and other tools' state. None when `dir` does not exist.
pub fn table_names(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    Ok(entries(dir)?
        .into_iter()
        .filter(|p| p.is_dir())
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect())
}

/// A swap killed between its two renames leaves the table (or directory)
/// only as `.<name>.old`: put it back. Other `.old` and `.new` directories are
/// what a killed swap or write left behind, and are removed. Only names
/// `managed` accepts are touched; `label` is `dir` as the log names it.
fn recover(dir: &Path, label: &str, managed: impl Fn(&str) -> bool) -> Result<()> {
    let Ok(it) = fs::read_dir(dir) else {
        return Ok(());
    };
    for path in it.filter_map(|e| e.ok().map(|e| e.path())) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(rest) = name.strip_prefix('.') else {
            continue;
        };
        let Some(table) = rest
            .strip_suffix(".old")
            .or_else(|| rest.strip_suffix(".new"))
            .filter(|t| managed(t))
        else {
            continue;
        };
        if rest.ends_with(".old") {
            let live = dir.join(table);
            if live.exists() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            } else {
                fs::rename(&path, &live)
                    .with_context(|| format!("restoring {}", live.display()))?;
                println!("   restored {label}{table} from an interrupted swap");
            }
        } else {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(data: &Path) -> Stage {
        Stage::open(data, &data.join("parquet")).unwrap()
    }

    fn table(dir: &Path, marker: &str) {
        fs::create_dir_all(dir.join("country=SLE")).unwrap();
        fs::write(dir.join("country=SLE/part-0.parquet"), marker).unwrap();
    }

    fn marker(dir: &Path) -> String {
        fs::read_to_string(dir.join("country=SLE/part-0.parquet")).unwrap()
    }

    #[test]
    fn staged_tables_are_read_first_and_published_last() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let s = stage(data);
        table(&data.join("parquet/a"), "old a");
        table(&data.join("parquet/b"), "old b");
        table(&s.table("a"), "new a");
        table(&s.table("c"), "new c");
        fs::write(s.file("manifest.json"), "new").unwrap();
        fs::write(data.join("manifest.json"), "old").unwrap();

        assert_eq!(marker(&s.read("a")), "new a");
        assert_eq!(marker(&s.read("b")), "old b");
        let names: Vec<String> = s.tables().unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["a", "b", "c"]);
        // Nothing live has changed yet.
        assert_eq!(marker(&data.join("parquet/a")), "old a");
        assert!(!data.join("parquet/c").exists());

        assert_eq!(s.publish(&mut false).unwrap(), (2, vec![]));
        assert_eq!(marker(&data.join("parquet/a")), "new a");
        assert_eq!(marker(&data.join("parquet/b")), "old b");
        assert_eq!(marker(&data.join("parquet/c")), "new c");
        assert_eq!(
            fs::read_to_string(data.join("manifest.json")).unwrap(),
            "new"
        );
        assert!(!data.join("parquet/.staging").exists());
        assert!(!data.join(".staging").exists());
        assert!(!data.join("parquet/.a.old").exists());
    }

    #[test]
    fn the_publish_reports_a_live_change() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let s = stage(data);
        let mut published = false;
        s.publish(&mut published).unwrap();
        assert!(!published, "nothing was staged");
        drop(s);
        let s = stage(data);
        fs::write(s.file("manifest.json"), "new").unwrap();
        s.publish(&mut published).unwrap();
        assert!(published);
    }

    #[test]
    fn staged_views_and_tiles_replace_the_live_directories() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let s = stage(data);
        for (d, f) in [("views", "IcrGone.json"), ("tiles", "admin.pmtiles")] {
            fs::create_dir_all(data.join(d)).unwrap();
            fs::write(data.join(d).join(f), "old").unwrap();
        }
        // Only views/ is staged: tiles/ stays as it is.
        assert_eq!(s.read_dir("views"), data.join("views"));
        fs::create_dir_all(s.dir("views")).unwrap();
        fs::write(s.dir("views").join("IcrObs.json"), "new").unwrap();
        assert_eq!(s.read_dir("views"), s.dir("views"));
        assert_eq!(s.read_dir("tiles"), data.join("tiles"));
        assert!(data.join("views/IcrGone.json").exists());

        assert_eq!(
            s.publish(&mut false).unwrap(),
            (0, vec!["views".to_string()])
        );
        assert!(!data.join("views/IcrGone.json").exists());
        assert_eq!(
            fs::read_to_string(data.join("views/IcrObs.json")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(data.join("tiles/admin.pmtiles")).unwrap(),
            "old"
        );
        assert!(!data.join(".views.old").exists());
        assert!(!data.join(".staging").exists());
    }

    #[test]
    fn the_warehouse_lock_is_held_until_it_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let s = Lock::acquire(dir.path(), None).unwrap();
        let other = fs::File::open(dir.path().join(LOCK)).unwrap();
        assert!(matches!(
            other.try_lock_shared(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(s);
        other.try_lock_shared().unwrap();
    }

    #[test]
    fn a_run_with_a_lock_timeout_gives_up_when_another_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let held = Lock::acquire(dir.path(), None).unwrap();
        let started = Instant::now();
        let err = Lock::acquire(dir.path(), Some(Duration::from_millis(300)))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("still holds .refresh.lock after 0 s"), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(300));
        drop(held);
        Lock::acquire(dir.path(), Some(Duration::ZERO)).unwrap();
    }

    #[test]
    fn a_publish_that_stopped_part_way_is_finished_by_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        table(&data.join("parquet/a"), "old a");
        table(&data.join("parquet/b"), "old b");
        fs::write(data.join("manifest.json"), "old").unwrap();
        let s = stage(data);
        table(&s.table("a"), "new a");
        table(&s.table("b"), "new b");
        fs::write(s.file("manifest.json"), "new").unwrap();
        // The publish stopped after its first table, as a failed rename or
        // a kill would leave it.
        fs::write(s.file(PUBLISHING), "").unwrap();
        partition::replace(&s.table("a"), &data.join("parquet/a")).unwrap();
        drop(s);
        assert_eq!(marker(&data.join("parquet/a")), "new a");
        assert_eq!(marker(&data.join("parquet/b")), "old b");
        assert!(
            data.join("parquet/.staging/b").exists(),
            "kept for the next run"
        );

        let s = stage(data);
        assert_eq!(marker(&data.join("parquet/a")), "new a");
        assert_eq!(marker(&data.join("parquet/b")), "new b");
        assert_eq!(
            fs::read_to_string(data.join("manifest.json")).unwrap(),
            "new"
        );
        assert!(!s.publishing());
        assert!(s.is_empty().unwrap());
    }

    #[test]
    fn dropping_a_stage_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = stage(dir.path());
        table(&s.table("a"), "new a");
        drop(s);
        assert!(!dir.path().join("parquet/.staging").exists());
        assert!(!dir.path().join(".staging").exists());
    }

    #[test]
    fn what_a_killed_run_left_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        table(&data.join("parquet/.staging/a"), "staged");
        fs::create_dir_all(data.join(".staging")).unwrap();
        fs::write(data.join(".staging/manifest.json"), "staged").unwrap();
        // A swap killed between its renames: b only as .b.old.
        table(&data.join("parquet/.b.old"), "old b");
        // One killed after its first rename was undone: c and .c.old both.
        table(&data.join("parquet/c"), "c");
        table(&data.join("parquet/.c.old"), "older c");
        table(&data.join("parquet/.d.new"), "half-written d");
        fs::create_dir_all(data.join("parquet/.portolan")).unwrap();
        // views/ swapped aside and not back; tiles/ half swapped.
        fs::create_dir_all(data.join(".views.old")).unwrap();
        fs::write(data.join(".views.old/IcrObs.json"), "old views").unwrap();
        fs::create_dir_all(data.join("tiles")).unwrap();
        fs::create_dir_all(data.join(".tiles.old")).unwrap();
        // Something else's dot-directory in the warehouse directory.
        fs::create_dir_all(data.join(".cache.old")).unwrap();

        let s = stage(data);
        assert!(entries(&s.tables).unwrap().is_empty());
        assert!(s.is_empty().unwrap());
        assert_eq!(marker(&data.join("parquet/b")), "old b");
        assert_eq!(marker(&data.join("parquet/c")), "c");
        assert!(!data.join("parquet/.c.old").exists());
        assert!(!data.join("parquet/.d.new").exists());
        // Other tools' dot-directories are left alone.
        assert!(data.join("parquet/.portolan").exists());
        assert_eq!(
            fs::read_to_string(data.join("views/IcrObs.json")).unwrap(),
            "old views"
        );
        assert!(!data.join(".views.old").exists());
        assert!(data.join("tiles").exists());
        assert!(!data.join(".tiles.old").exists());
        assert!(data.join(".cache.old").exists());
    }
}

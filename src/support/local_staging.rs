//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Durable local writes, synced as a group.
//!
//! Each file is written under a staging name readers skip. [`LocalStaging::finalize`] syncs
//! every staged file, renames them into place and syncs their directories, so one round of
//! syncs covers many files. Until then the route holds the source commit.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

/// Where a file waits until it is finalized.
#[derive(Clone, Copy)]
pub(crate) enum StagedName {
    /// `<root>/<name>#<n>`, which `object_store` listings skip.
    #[cfg_attr(not(feature = "object-store"), allow(dead_code))]
    Suffixed,
    /// `<target dir>/.stage-<uuid>`, which the `file` sink's recovery skips and reaps.
    Hidden,
}

struct Staged {
    staged: PathBuf,
    target: PathBuf,
    /// An existing target is kept and this file dropped, instead of replaced.
    create_only: bool,
}

pub(crate) struct LocalStaging {
    root: PathBuf,
    naming: StagedName,
    interval: Duration,
    pending: StdMutex<Vec<Staged>>,
    next_id: AtomicU64,
    finalizing: tokio::sync::Mutex<()>,
}

impl LocalStaging {
    /// Stages files whose targets lie under `root` and finalizes them every `interval`.
    pub(crate) fn new(root: PathBuf, naming: StagedName, interval: Duration) -> Arc<Self> {
        let staging = Arc::new(Self {
            root,
            naming,
            interval,
            pending: StdMutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            finalizing: tokio::sync::Mutex::new(()),
        });
        spawn_finalizer(Arc::downgrade(&staging), interval);
        staging
    }

    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Writes `body` under a staging name; it appears at `target` after `finalize`.
    pub(crate) async fn write(self: &Arc<Self>, target: PathBuf, body: Vec<u8>) -> io::Result<()> {
        self.stage(target, body, false).await
    }

    /// Like [`Self::write`], but a `target` that exists by then is left as it is.
    #[cfg_attr(not(feature = "object-store"), allow(dead_code))]
    pub(crate) async fn write_new(
        self: &Arc<Self>,
        target: PathBuf,
        body: Vec<u8>,
    ) -> io::Result<()> {
        self.stage(target, body, true).await
    }

    async fn stage(
        self: &Arc<Self>,
        target: PathBuf,
        body: Vec<u8>,
        create_only: bool,
    ) -> io::Result<()> {
        let (dir, staged_name) = match self.naming {
            StagedName::Suffixed => {
                let name = target.file_name().ok_or_else(|| {
                    io::Error::other(format!("'{}' has no file name", target.display()))
                })?;
                let mut staged = name.to_os_string();
                staged.push(format!("#{}", self.next_id.fetch_add(1, Ordering::Relaxed)));
                (self.root.clone(), staged)
            }
            StagedName::Hidden => {
                let dir = target.parent().unwrap_or(&self.root).to_path_buf();
                (dir, format!(".stage-{}", fast_uuid_v7::gen_id_str()).into())
            }
        };
        let staged = dir.join(staged_name);
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            File::create(&staged)?.write_all(&body)?;
            this.pending
                .lock()
                .expect("staging list mutex poisoned")
                .push(Staged {
                    staged,
                    target,
                    create_only,
                });
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Makes every file staged before this call durable under its final name.
    pub(crate) async fn finalize(self: &Arc<Self>) -> io::Result<()> {
        let _finalizing = self.finalizing.lock().await;
        let batch = std::mem::take(&mut *self.pending.lock().expect("staging list mutex poisoned"));
        if batch.is_empty() {
            return Ok(());
        }
        let root = self.root.clone();
        let outcome = tokio::task::spawn_blocking(move || match finalize_all(&root, &batch) {
            Ok(()) => Ok(()),
            Err(error) => Err((error, batch)),
        })
        .await
        .map_err(io::Error::other)?;
        outcome.map_err(|(error, batch)| {
            // A renamed one is kept too: its directory may still be unsynced.
            let mut pending = self.pending.lock().expect("staging list mutex poisoned");
            pending.extend(
                batch
                    .into_iter()
                    .filter(|s| s.staged.exists() || s.target.exists()),
            );
            error
        })
    }
}

/// A publisher dropped without a flush still leaves its files in place.
impl Drop for LocalStaging {
    fn drop(&mut self) {
        let pending = self.pending.get_mut().unwrap_or_else(|e| e.into_inner());
        let batch = std::mem::take(pending);
        if batch.is_empty() {
            return;
        }
        if let Err(error) = finalize_all(&self.root, &batch) {
            tracing::warn!(dir = %self.root.display(), %error, "Finalizing staged files on close failed; they stay hidden and are removed on a later start");
        }
    }
}

fn spawn_finalizer(staging: Weak<LocalStaging>, interval: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let Some(staging) = staging.upgrade() else {
                break;
            };
            if let Err(error) = staging.finalize().await {
                tracing::warn!(dir = %staging.root.display(), %error, "Finalizing staged files failed; retrying");
            }
        }
    });
}

fn finalize_all(root: &Path, batch: &[Staged]) -> io::Result<()> {
    let mut moved = Vec::with_capacity(batch.len());
    for entry in batch {
        // Windows syncs only a handle opened for writing.
        match std::fs::OpenOptions::new().write(true).open(&entry.staged) {
            Ok(file) => {
                push_to_device(&file)?;
                moved.push(entry);
            }
            // Staged twice, or renamed by a finalize that failed later: already in place.
            Err(error) if error.kind() == io::ErrorKind::NotFound && entry.target.exists() => {}
            Err(error) => return Err(error),
        }
    }
    flush_device(root)?;
    for entry in moved {
        if let Some(parent) = entry.target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if entry.create_only {
            match std::fs::hard_link(&entry.staged, &entry.target) {
                Ok(()) => {}
                // A duplicate: the target already holds what this write would have.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            std::fs::remove_file(&entry.staged)?;
        } else {
            std::fs::rename(&entry.staged, &entry.target)?;
        }
    }
    let mut dirs = BTreeSet::new();
    for entry in batch {
        let mut dir = entry.target.parent();
        while let Some(current) = dir.filter(|d| d.starts_with(root)) {
            dirs.insert(current.to_path_buf());
            dir = current.parent();
        }
    }
    if let Some(parent) = root.parent() {
        dirs.insert(parent.to_path_buf());
    }
    for dir in &dirs {
        sync_dir(dir)?;
    }
    flush_device(root)
}

/// Staged files younger than this may belong to a concurrent writer sharing the directory.
#[cfg_attr(not(feature = "object-store"), allow(dead_code))]
pub(crate) const STAGING_REAP_AGE: Duration = Duration::from_secs(60);

/// Removes `<name>#<digits>` files in `root` older than `min_age` whose `name` is one of this
/// sink's own.
#[cfg_attr(not(feature = "object-store"), allow(dead_code))]
pub(crate) fn remove_suffixed_leftovers(
    root: &Path,
    min_age: Duration,
    is_ours: impl Fn(&str) -> bool,
) -> io::Result<usize> {
    let now = std::time::SystemTime::now();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some((name, suffix)) = file_name.to_str().and_then(|n| n.rsplit_once('#')) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) || !is_ours(name) {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if age.is_some_and(|age| age >= min_age) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}

/// Hands the file's data to the drive. On Apple platforms that is a plain `fsync`, made
/// durable by one [`flush_device`] for the whole group.
#[cfg(target_vendor = "apple")]
fn push_to_device(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `file` owns a valid open descriptor for the duration of the call.
    if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_vendor = "apple"))]
fn push_to_device(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Flushes the drive's write cache (`F_FULLFSYNC`); elsewhere each fsync already is durable.
#[cfg(target_vendor = "apple")]
fn flush_device(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(target_vendor = "apple"))]
fn flush_device(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Some network or FUSE mounts cannot sync a directory; that is skipped, as the file sink does.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    match push_to_device(&File::open(dir)?) {
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

/// A directory cannot be opened for syncing here; NTFS journals the rename itself.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_staged_file_is_hidden_until_finalized() {
        let dir = tempfile::tempdir().unwrap();
        let staging = LocalStaging::new(dir.path().to_path_buf(), StagedName::Suffixed, SECOND);
        let key = dir.path().join("2026/a.jsonl");
        let twin = dir.path().join("2027/a.jsonl");
        staging.write(key.clone(), b"x\n".to_vec()).await.unwrap();
        staging.write(twin.clone(), b"y\n".to_vec()).await.unwrap();
        assert_eq!(names(dir.path()), ["a.jsonl#1", "a.jsonl#2"]);

        staging.finalize().await.unwrap();
        assert_eq!(std::fs::read(key).unwrap(), b"x\n");
        assert_eq!(std::fs::read(twin).unwrap(), b"y\n");
        assert_eq!(names(dir.path()), ["2026", "2027"]);
    }

    #[tokio::test]
    async fn a_create_only_write_keeps_an_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let staging = LocalStaging::new(dir.path().to_path_buf(), StagedName::Suffixed, SECOND);
        let kept = dir.path().join("kept.jsonl");
        let fresh = dir.path().join("fresh.jsonl");
        std::fs::write(&kept, b"first\n").unwrap();
        staging
            .write_new(kept.clone(), b"second\n".to_vec())
            .await
            .unwrap();
        staging
            .write_new(fresh.clone(), b"new\n".to_vec())
            .await
            .unwrap();

        staging.finalize().await.unwrap();
        assert_eq!(std::fs::read(&kept).unwrap(), b"first\n");
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new\n");
        assert_eq!(names(dir.path()), ["fresh.jsonl", "kept.jsonl"]);
    }

    #[tokio::test]
    async fn hidden_staging_stays_next_to_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let staging = LocalStaging::new(dir.path().to_path_buf(), StagedName::Hidden, SECOND);
        let target = dir.path().join("part.jsonl");
        staging
            .write(target.clone(), b"x\n".to_vec())
            .await
            .unwrap();
        let staged = names(dir.path());
        assert_eq!(staged.len(), 1);
        assert!(staged[0].starts_with(".stage-"), "{staged:?}");

        staging.finalize().await.unwrap();
        assert_eq!(names(dir.path()), ["part.jsonl"]);
    }

    #[tokio::test]
    async fn dropping_the_staging_finalizes_what_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let staging = LocalStaging::new(dir.path().to_path_buf(), StagedName::Suffixed, SECOND);
        let target = dir.path().join("a.jsonl");
        staging
            .write(target.clone(), b"x\n".to_vec())
            .await
            .unwrap();
        drop(staging);
        assert_eq!(std::fs::read(target).unwrap(), b"x\n");
    }

    #[test]
    fn only_this_sinks_suffixed_leftovers_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["ours.jsonl#1", "ours.jsonl#x", "theirs.csv#1", "ours.jsonl"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let fresh = remove_suffixed_leftovers(dir.path(), STAGING_REAP_AGE, |name| {
            name.ends_with(".jsonl")
        })
        .unwrap();
        assert_eq!(fresh, 0, "a concurrent writer's file is kept");
        let removed =
            remove_suffixed_leftovers(dir.path(), Duration::ZERO, |name| name.ends_with(".jsonl"))
                .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            names(dir.path()),
            ["ours.jsonl", "ours.jsonl#x", "theirs.csv#1"]
        );
    }
}

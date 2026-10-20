//! What is kept on disk so the window has something to show before the
//! network answers: the space's index, the map's points, the favourites,
//! the album and artist pages seen lately.
//!
//! Only ever a head start. Whatever is read from here is asked for again,
//! and every failure is treated as a miss.
//!
//! Kept per pairing, so a laptop handed from one of the family to another
//! does not open on the last person's favourites.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

fn path(name: &str) -> PathBuf {
    let root = crate::client_data_dir().join("cache");
    if crate::backend::wired() {
        root.join(crate::backend::backend().cache_key()).join(name)
    } else {
        root.join(name)
    }
}

pub fn read_bytes(name: &str) -> Option<Vec<u8>> {
    std::fs::read(path(name)).ok()
}

pub fn read<T: DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_slice(&read_bytes(name)?).ok()
}

/// Written beside and renamed, so a process killed mid-write, which on a
/// phone is any process, leaves the old copy rather than half a new one.
pub fn write_bytes(name: &str, bytes: &[u8]) {
    write_to(&path(name), bytes);
}

fn write_to(target: &Path, bytes: &[u8]) {
    let staging = target.with_extension("partial");
    let written = target
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&staging, bytes))
        .and_then(|()| std::fs::rename(&staging, target));
    if let Err(err) = written {
        eprintln!("could not cache {}: {err}", target.display());
    }
}

pub fn write<T: Serialize>(name: &str, value: &T) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        write_bytes(name, &bytes);
    }
}

/// What `write_soon` was handed and has not written yet, by file. A value
/// handed over again before the last one was written replaces it.
type Waiting = HashMap<PathBuf, Box<dyn FnOnce() -> Option<Vec<u8>> + Send>>;
static WAITING: LazyLock<Mutex<Waiting>> = LazyLock::new(Default::default);
/// Held while a file is written, so two writers never share the staging copy.
static WRITING: Mutex<()> = Mutex::new(());

/// `write`, but serialised and written on a thread of its own, for a value
/// that changes while the window is in use. Only the latest of several is
/// written: one handed over while an older one waits takes its place.
pub fn write_soon<T: Serialize + Send + 'static>(name: &str, value: T) {
    write_soon_to(path(name), value, None);
}

/// `write_soon`, as `name` in the folder `dir`, which keeps only the `keep`
/// files written last: the pages seen lately, not every page ever opened.
pub fn write_soon_among<T: Serialize + Send + 'static>(dir: &str, name: &str, keep: usize, value: T) {
    write_soon_to(path(dir).join(name), value, Some(keep));
}

fn write_soon_to<T: Serialize + Send + 'static>(target: PathBuf, value: T, keep: Option<usize>) {
    let encode = Box::new(move || serde_json::to_vec(&value).ok());
    let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
    // Already waiting means a writer is on its way, and it takes this one.
    if waiting.insert(target.clone(), encode).is_some() {
        return;
    }
    drop(waiting);
    std::thread::spawn(move || {
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let encode = WAITING.lock().unwrap_or_else(|e| e.into_inner()).remove(&target);
        if let Some(bytes) = encode.and_then(|encode| encode()) {
            write_to(&target, &bytes);
        }
        if let (Some(keep), Some(dir)) = (keep, target.parent()) {
            keep_latest(dir, keep);
        }
    });
}

/// Delete all but the `keep` files in `dir` written last. A page is written
/// again each time it is opened, so the oldest is the one least lately seen.
fn keep_latest(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then_some((meta.modified().ok()?, entry.path()))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, file) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(file);
    }
}

/// Delete the copy of the space older versions synced, a few hundred MB on a
/// phone that nothing reads any more, and the cache from before it was kept
/// per pairing, which sat loose in `cache/` where the pairings' folders are.
pub fn forget_synced_space() {
    let dir = crate::client_data_dir();
    for name in ["space.bin", "space.json", "semantic_pca.bin", "catalog.db"] {
        for file in [dir.join(name), dir.join(name).with_extension("partial")] {
            let _ = std::fs::remove_file(file);
        }
    }
    let Ok(entries) = std::fs::read_dir(dir.join("cache")) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_latest_is_written() {
        let dir = std::env::temp_dir().join(format!("stelly-cache-test-{}", std::process::id()));
        let target = dir.join("value.json");
        // Held, so every one of these waits and each replaces the last.
        let writing = WRITING.lock().unwrap();
        for value in 0..5 {
            write_soon_to(target.clone(), value, None);
        }
        drop(writing);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while WAITING.lock().unwrap().contains_key(&target) || !target.exists() {
            assert!(std::time::Instant::now() < deadline, "never written");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _written = WRITING.lock().unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "4");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_latest_pages_are_kept() {
        let dir = std::env::temp_dir().join(format!("stelly-cache-pages-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        for (name, age) in [("a", 3), ("b", 1), ("c", 4), ("d", 2)] {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_modified(epoch + std::time::Duration::from_secs(1000 - age)).unwrap();
        }
        keep_latest(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["b", "d"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

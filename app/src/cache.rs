//! What is kept on disk so the window has something to show before the
//! network answers: the space's index, the map's points, the favourites.
//!
//! Only ever a head start. Whatever is read from here is asked for again,
//! and every failure is treated as a miss.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::PathBuf;

fn path(name: &str) -> PathBuf {
    crate::client_data_dir().join("cache").join(name)
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
    let target = path(name);
    let staging = target.with_extension("partial");
    let written = target
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&staging, bytes))
        .and_then(|()| std::fs::rename(&staging, &target));
    if let Err(err) = written {
        eprintln!("could not cache {name}: {err}");
    }
}

pub fn write<T: Serialize>(name: &str, value: &T) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        write_bytes(name, &bytes);
    }
}

/// Delete the copy of the space older versions synced, a few hundred MB on a
/// phone that nothing reads any more.
pub fn forget_synced_space() {
    let dir = crate::client_data_dir();
    for name in ["space.bin", "space.json", "semantic_pca.bin", "catalog.db"] {
        for file in [dir.join(name), dir.join(name).with_extension("partial")] {
            let _ = std::fs::remove_file(file);
        }
    }
}

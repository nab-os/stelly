//! The space and its catalogue, loaded together, with navigation over them.
//!
//! Shared by every request at once, none of which waits on another: the
//! space never changes once loaded, the catalogue is swapped whole when the
//! block list does, and each weighting asked for is built once and kept.

use crate::db::Catalog;
use crate::paths::Navigator;
use crate::space::{Space, WeightedSpace};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// At most this many weightings besides the default are kept, fewer when a
/// copy is large: each is the whole space again, 7.6 MB at 23k tracks and
/// 46 MB at 142k. Sliders being moved build a few in turn, and the last
/// ones asked are the likeliest to be asked again.
const WEIGHTED_MAX: usize = 4;
const WEIGHTED_BUDGET: usize = 128 << 20;

/// A weighting as the space is built from it: each block's weight in the
/// manifest's block order, bit for bit. Two requests with the same key get
/// the same scale on every dimension, so the same copy answers both exactly
/// as a copy of its own would. Rounding would let two weightings that build
/// different copies share one, and change what one of them gets.
type Key = Vec<u32>;

/// A weighting being built or built. Whoever asks for it first builds it,
/// outside any lock; whoever asks meanwhile waits for that build.
type Slot = Arc<OnceLock<Arc<WeightedSpace>>>;

/// Everything navigation needs, loaded once per build of the space.
pub struct Engine {
    pub space: Space,
    /// Swapped whole when the block list changes. A request takes its own
    /// `Arc` once, so it sees one list from its start to its end.
    catalog: RwLock<Arc<Catalog>>,
    /// The space as the manifest weights it, what a request leaving the
    /// sliders alone asks for. Built at load and never dropped.
    default: (Key, Arc<WeightedSpace>),
    /// Other weightings.
    weighted: Weightings,
}

impl Engine {
    pub fn load(data_dir: &Path, db_path: &Path) -> Result<Self> {
        let space = Space::load(data_dir)?;
        let catalog = Catalog::load(db_path, &space.manifest.track_ids)?;
        let weights = space.default_weights();
        let default = (key(&space, &weights), Arc::new(space.weighted(&weights)));
        let copy = space.manifest.n_tracks * space.manifest.n_dims * std::mem::size_of::<f32>();
        let capacity = (WEIGHTED_BUDGET / copy.max(1)).clamp(1, WEIGHTED_MAX);

        Ok(Self {
            space,
            catalog: RwLock::new(Arc::new(catalog)),
            default,
            weighted: Weightings::new(capacity),
        })
    }

    /// The catalogue as it stands.
    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the corpus carries the raw CLAP embeddings text steering
    /// anchors against. Only half the question, the text tower is the other.
    pub fn has_audio_embeddings(&self) -> bool {
        self.catalog().clap.is_some()
    }

    /// The space weighted as asked, any block left out at its default, over
    /// the catalogue as it stands. Costs nothing when the weighting has been
    /// asked for lately; otherwise it is built, holding up no one else.
    pub fn navigator(&self, asked: &HashMap<String, f32>) -> Navigator {
        let mut weights = self.space.default_weights();
        for (name, weight) in asked {
            if weights.contains_key(name) {
                weights.insert(name.clone(), *weight);
            }
        }
        Navigator {
            space: self.weighted(&weights),
            catalog: self.catalog(),
            index_of: self.space.index_of.clone(),
        }
    }

    fn weighted(&self, weights: &HashMap<String, f32>) -> Arc<WeightedSpace> {
        let key = key(&self.space, weights);
        if key == self.default.0 {
            return self.default.1.clone();
        }
        self.weighted.get(key, || self.space.weighted(weights))
    }

    /// Apply a block list. Blocking only filters, so nothing needs
    /// rebuilding: the new catalogue shares the old one's tracks. Requests
    /// already under way finish on the list they started with.
    pub fn set_blocked(&self, blocked: HashSet<i64>) -> Arc<Catalog> {
        let mut catalog = self.catalog.write().unwrap_or_else(|e| e.into_inner());
        *catalog = Arc::new(catalog.with_blocked(blocked));
        catalog.clone()
    }
}

/// The last few weightings asked for, the least recently asked first.
struct Weightings {
    cached: Mutex<Vec<(Key, Slot)>>,
    capacity: usize,
}

impl Weightings {
    fn new(capacity: usize) -> Self {
        Self {
            cached: Mutex::new(Vec::new()),
            capacity,
        }
    }

    /// The copy for `key`, built by `build` unless it is held or being built.
    fn get(&self, key: Key, build: impl FnOnce() -> WeightedSpace) -> Arc<WeightedSpace> {
        let slot = {
            let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            match cached.iter().position(|(cached, _)| *cached == key) {
                Some(at) => {
                    let entry = cached.remove(at);
                    let slot = entry.1.clone();
                    cached.push(entry);
                    slot
                }
                None => {
                    if cached.len() >= self.capacity {
                        cached.remove(0);
                    }
                    let slot = Slot::default();
                    cached.push((key, slot.clone()));
                    slot
                }
            }
        };
        // A request still walking a copy since dropped from the list keeps
        // it until it is done.
        slot.get_or_init(|| Arc::new(build())).clone()
    }
}

/// What `weights` builds the space from, as `Space::weighted` reads it.
fn key(space: &Space, weights: &HashMap<String, f32>) -> Key {
    space
        .manifest
        .blocks
        .iter()
        .map(|block| weights.get(&block.name).copied().unwrap_or(block.default_weight).to_bits())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::Duration;

    fn copy(n: usize) -> WeightedSpace {
        WeightedSpace {
            unit: vec![0.0; n],
            n_tracks: n,
            n_dims: 1,
            scale: vec![1.0],
        }
    }

    /// Requests asking for one weighting at once share a single build.
    #[test]
    fn one_build_per_weighting_however_many_ask() {
        let weightings = Weightings::new(2);
        let builds = AtomicUsize::new(0);
        let start = Barrier::new(8);
        let got: Vec<Arc<WeightedSpace>> = std::thread::scope(|scope| {
            let asks: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        weightings.get(vec![1], || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(50));
                            copy(3)
                        })
                    })
                })
                .collect();
            asks.into_iter().map(|ask| ask.join().unwrap()).collect()
        });
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(got.iter().all(|space| Arc::ptr_eq(space, &got[0])));
    }

    /// Past its capacity, the weighting asked for longest ago goes first.
    #[test]
    fn the_least_recently_asked_weighting_goes_first() {
        let weightings = Weightings::new(2);
        let builds = AtomicUsize::new(0);
        let get = |key: u32| {
            weightings.get(vec![key], || {
                builds.fetch_add(1, Ordering::SeqCst);
                copy(1)
            })
        };
        get(1);
        get(2);
        get(1);
        get(3);
        assert_eq!(builds.load(Ordering::SeqCst), 3);
        get(1);
        assert_eq!(builds.load(Ordering::SeqCst), 3, "1 was asked after 2");
        get(2);
        assert_eq!(builds.load(Ordering::SeqCst), 4, "2 made way for 3");
    }
}

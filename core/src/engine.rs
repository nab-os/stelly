//! The space and its catalogue, loaded together, with navigation over them.

use crate::db::Catalog;
use crate::paths::Navigator;
use crate::space::Space;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Everything navigation needs, loaded once per build of the space.
pub struct Engine {
    pub space: Space,
    pub navigator: Navigator,
    /// What `navigator` is weighted by, so a request with the same weights as
    /// the last one costs nothing.
    weights: HashMap<String, f32>,
}

impl Engine {
    pub fn load(data_dir: &Path, db_path: &Path) -> Result<Self> {
        let space = Space::load(data_dir)?;
        let catalog = Catalog::load(db_path, &space.manifest.track_ids)?;
        let weights = space.default_weights();
        let navigator = Navigator::new(&space, &weights, catalog);

        Ok(Self {
            space,
            navigator,
            weights,
        })
    }

    /// Whether the corpus carries the raw CLAP embeddings text steering
    /// anchors against. Only half the question, the text tower is the other.
    pub fn has_audio_embeddings(&self) -> bool {
        self.navigator.catalog.clap.is_some()
    }

    /// Weight the space as asked, filling any block left out with its default.
    /// Milliseconds when they changed, nothing when they did not.
    pub fn set_weights(&mut self, asked: &HashMap<String, f32>) {
        let mut weights = self.space.default_weights();
        for (name, weight) in asked {
            if weights.contains_key(name) {
                weights.insert(name.clone(), *weight);
            }
        }
        if weights == self.weights {
            return;
        }

        let catalog = Catalog {
            tracks: std::mem::take(&mut self.navigator.catalog.tracks),
            clap: self.navigator.catalog.clap.take(),
            clap_dims: self.navigator.catalog.clap_dims,
            blocked_artists: std::mem::take(&mut self.navigator.catalog.blocked_artists),
        };
        self.navigator = Navigator::new(&self.space, &weights, catalog);
        self.weights = weights;
    }

    /// Apply a block list. Blocking only filters, so nothing needs rebuilding.
    pub fn set_blocked(&mut self, blocked: HashSet<i64>) {
        self.navigator.catalog.blocked_artists = blocked;
        // The kNN graph was built over the old visibility, so drop it.
        self.navigator.invalidate_graph();
    }
}

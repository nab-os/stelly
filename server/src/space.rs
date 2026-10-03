//! The one loaded copy of the space. Every client navigates through it: the
//! apps hold no space of their own, so a phone on a poor connection has
//! nothing to download before it can generate.
//!
//! Loaded at startup and again whenever a rebuild settles. Requests hold an
//! `Arc` to the copy they started on, so a swap never pulls it from under one.

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use stelly_core::api::{
    BlockWeight, GenerateRequest, OrderRequest, Recipe, SpaceAlbum, SpaceArtist, SpaceIndex,
    SpaceInfo, SpaceSearch, SpaceSort, Step, TrackMeta,
};
use stelly_core::engine::Engine;
use stelly_core::map;
use stelly_core::paths::Constraints;

pub struct SpaceService {
    data_dir: PathBuf,
    db_path: PathBuf,
    loaded: RwLock<Option<Arc<Loaded>>>,
}

struct Loaded {
    /// Behind a mutex rather than shared: a request's weights reweight it, and
    /// the kNN graph is built on first use.
    engine: Mutex<Engine>,
    built_at: String,
    /// Each track's searchable text, lowercased once per load rather than per
    /// query. Indexed by catalogue row, blocked rows included.
    haystacks: Vec<Haystack>,
    /// What follows from which artists are hidden, rebuilt when that changes.
    /// Its own lock, so a long radio walk does not hold up the map.
    derived: RwLock<Arc<Derived>>,
}

/// Searchable text, kept per field: terms are split on whitespace, so no term
/// can span two of them.
struct Haystack {
    artist: String,
    title: String,
    album: String,
}

/// Everything a client caches, worked out once per load or block change.
pub struct Derived {
    pub index: SpaceIndex,
    /// `map::payload`, the binary the map's canvas reads.
    pub points: Vec<u8>,
    /// `map::meta`, as JSON.
    pub meta: Vec<u8>,
    pub in_space: i64,
    pub on_map: i64,
}

impl SpaceService {
    pub fn new(data_dir: PathBuf, db_path: PathBuf) -> Self {
        Self {
            data_dir,
            db_path,
            loaded: RwLock::new(None),
        }
    }

    /// Load the space from disk, replacing whatever was loaded. Blocking:
    /// reading the catalogue and its embeddings takes a second or two. A
    /// corpus without a space yet is an error here, and leaves the old copy.
    pub fn reload(&self) -> Result<()> {
        let engine = Engine::load(&self.data_dir, &self.db_path)?;
        let built_at = engine.space.manifest.built_at.clone();
        let catalog = &engine.navigator.catalog;
        let haystacks = (0..catalog.len())
            .map(|i| {
                let track = catalog.get(i);
                Haystack {
                    artist: track.artist.to_lowercase(),
                    title: track.title.to_lowercase(),
                    album: track.album.to_lowercase(),
                }
            })
            .collect();
        let derived = derive(&engine, &built_at);

        let loaded = Loaded {
            engine: Mutex::new(engine),
            built_at,
            haystacks,
            derived: RwLock::new(Arc::new(derived)),
        };
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(loaded));
        Ok(())
    }

    fn loaded(&self) -> Option<Arc<Loaded>> {
        self.loaded.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Apply a changed block list, and redo what depends on it.
    pub fn set_blocked(&self, blocked: HashSet<i64>) {
        let Some(loaded) = self.loaded() else { return };
        let mut engine = loaded.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.set_blocked(blocked);
        let derived = derive(&engine, &loaded.built_at);
        *loaded.derived.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(derived);
    }

    /// What there is, for a client deciding whether its cache still holds.
    /// The default when nothing is built yet: no stamp, no tracks.
    pub fn info(&self, tower: bool) -> SpaceInfo {
        let Some(loaded) = self.loaded() else {
            return SpaceInfo::default();
        };
        let derived = loaded.derived();
        let engine = loaded.engine.lock().unwrap_or_else(|e| e.into_inner());
        SpaceInfo {
            stamp: derived.index.stamp.clone(),
            n_tracks: engine.space.manifest.n_tracks,
            blocks: engine
                .space
                .manifest
                .blocks
                .iter()
                .map(|block| BlockWeight {
                    name: block.name.clone(),
                    default_weight: engine
                        .space
                        .manifest
                        .weights
                        .get(&block.name)
                        .copied()
                        .unwrap_or(block.default_weight),
                })
                .collect(),
            can_steer: tower && engine.has_audio_embeddings(),
            in_space: derived.in_space,
            on_map: derived.on_map,
        }
    }

    pub fn derived(&self) -> Option<Arc<Derived>> {
        Some(self.loaded()?.derived())
    }

    /// The space's rows for these ids, in the order asked, leaving out any it
    /// does not hold.
    pub fn tracks(&self, ids: &[i64]) -> Option<Vec<TrackMeta>> {
        let loaded = self.loaded()?;
        let engine = loaded.engine.lock().unwrap_or_else(|e| e.into_inner());
        let navigator = &engine.navigator;
        Some(
            ids.iter()
                .filter_map(|id| navigator.index_of.get(id))
                .map(|&row| navigator.catalog.get(row).clone())
                .collect(),
        )
    }

    pub fn search(&self, query: &str, sort: SpaceSort, descending: bool, limit: usize) -> Option<SpaceSearch> {
        let loaded = self.loaded()?;
        let engine = loaded.engine.lock().unwrap_or_else(|e| e.into_inner());
        let catalog = &engine.navigator.catalog;
        let terms: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();

        // Tracks, on their titles. Something starting with what was typed is
        // far likelier to be the thing meant than something merely containing
        // it, so those go first.
        let mut found: Vec<(u8, usize)> = Vec::new();
        for i in catalog.visible() {
            let Some(haystack) = loaded.haystacks.get(i) else { continue };
            if !terms.iter().all(|term| haystack.title.contains(term)) {
                continue;
            }
            let rank = match terms.first() {
                Some(first) => u8::from(!(haystack.artist.starts_with(first) || haystack.title.starts_with(first))),
                None => 0,
            };
            found.push((rank, i));
        }
        let total = found.len();
        found.sort_by_key(|entry| entry.0);

        let mut rows: Vec<usize> = found.into_iter().map(|(_, i)| i).collect();
        let field = |i: usize| -> &str {
            let haystack = &loaded.haystacks[i];
            match sort {
                SpaceSort::Title => &haystack.title,
                SpaceSort::Artist => &haystack.artist,
                SpaceSort::Album => &haystack.album,
                SpaceSort::Rank => "",
            }
        };
        // Stable either way round, so ties keep the rank order they came in.
        match (sort, descending) {
            (SpaceSort::Rank, false) => {}
            (SpaceSort::Rank, true) => rows.reverse(),
            (_, false) => rows.sort_by(|&a, &b| field(a).cmp(field(b))),
            (_, true) => rows.sort_by(|&a, &b| field(b).cmp(field(a))),
        }
        rows.truncate(limit);
        let tracks = rows.into_iter().map(|i| catalog.get(i).clone()).collect();

        let (albums, artists) = groups(&loaded.haystacks, &engine, &terms);
        Some(SpaceSearch {
            total,
            tracks,
            albums,
            artists,
        })
    }

    /// Run a recipe. Drift and mood need the phrase embedded first, which is
    /// the caller's to do: it is async, and this holds the engine.
    pub fn generate(&self, request: &GenerateRequest, embedding: Option<&[f32]>) -> Option<Vec<Step>> {
        let loaded = self.loaded()?;
        let mut engine = loaded.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.set_weights(&request.weights);
        let navigator = &mut engine.navigator;
        let count = request.count;
        let constraints = Constraints::default();

        Some(match (&request.recipe, embedding) {
            (Recipe::Neighbours { seed }, _) => navigator.neighbours(*seed, count, false),
            (Recipe::Radio { seed }, _) => navigator.radio_nearest(*seed, count, request.penalty, &constraints),
            (Recipe::Path { a, b, even: true }, _) => navigator.interpolate(*a, *b, count, &constraints),
            (Recipe::Path { a, b, even: false }, _) => navigator.graph_path(*a, *b, 16),
            (Recipe::Drift { seed, .. }, Some(embedding)) => {
                navigator.drift_to_text(*seed, embedding, count, 5, &constraints)
            }
            (Recipe::Mood { .. }, Some(embedding)) => navigator.mood(embedding, count, request.penalty, &constraints),
            (Recipe::Drift { .. } | Recipe::Mood { .. }, None) => Vec::new(),
        })
    }

    pub fn order(&self, request: &OrderRequest) -> Option<Vec<i64>> {
        let loaded = self.loaded()?;
        let mut engine = loaded.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.set_weights(&request.weights);
        Some(engine.navigator.shortest_path_order(&request.ids, request.from))
    }
}

impl Loaded {
    fn derived(&self) -> Arc<Derived> {
        self.derived.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn derive(engine: &Engine, built_at: &str) -> Derived {
    let catalog = &engine.navigator.catalog;

    // The build and the hidden artists are all a client's copy depends on.
    // Hashed rather than counted, so it survives a server restart.
    let mut blocked: Vec<i64> = catalog.blocked_artists.iter().copied().collect();
    blocked.sort_unstable();
    let mut hasher = DefaultHasher::new();
    blocked.hash(&mut hasher);
    let stamp = format!("{built_at}/{:016x}", hasher.finish());

    let (albums, artists) = catalog.reach();
    let ids: Vec<i64> = catalog.visible().map(|i| catalog.get(i).track_id).collect();
    let in_space = ids.len() as i64;
    let on_map = catalog.visible().filter(|&i| catalog.get(i).x.is_some()).count() as i64;

    Derived {
        index: SpaceIndex {
            stamp,
            ids,
            albums: albums.into_iter().collect(),
            artists: artists.into_iter().collect(),
        },
        points: map::payload(catalog),
        meta: serde_json::to_vec(&map::meta(catalog)).unwrap_or_default(),
        in_space,
        on_map,
    }
}

/// The albums and artists behind the tracks matching every term somewhere in
/// artist, title or album, best match first.
fn groups(haystacks: &[Haystack], engine: &Engine, terms: &[String]) -> (Vec<SpaceAlbum>, Vec<SpaceArtist>) {
    let Some(first) = terms.first() else {
        return Default::default();
    };
    let catalog = &engine.navigator.catalog;

    let mut albums: Vec<(u8, SpaceAlbum)> = Vec::new();
    let mut artists: Vec<(u8, SpaceArtist)> = Vec::new();
    let mut album_at: HashMap<String, usize> = HashMap::new();
    let mut artist_at: HashMap<i64, usize> = HashMap::new();

    for i in catalog.visible() {
        let Some(haystack) = haystacks.get(i) else { continue };
        if !terms.iter().all(|term| {
            haystack.artist.contains(term) || haystack.title.contains(term) || haystack.album.contains(term)
        }) {
            continue;
        }
        let track = catalog.get(i);

        // Named after what was typed first, the same preference a track gets,
        // but on the album's own title here.
        let rank = u8::from(!(haystack.artist.starts_with(first) || haystack.album.starts_with(first)));
        let artist_id = Some(track.artist_id).filter(|id| *id >= 0);

        if !track.album_id.is_empty() {
            let at = *album_at.entry(track.album_id.clone()).or_insert_with(|| {
                albums.push((
                    rank,
                    SpaceAlbum {
                        id: track.album_id.clone(),
                        title: track.album.clone(),
                        artist: track.artist.clone(),
                        artist_id,
                    },
                ));
                albums.len() - 1
            });
            albums[at].0 = albums[at].0.min(rank);
        }

        if let Some(id) = artist_id {
            let rank = u8::from(!haystack.artist.starts_with(first));
            let at = *artist_at.entry(id).or_insert_with(|| {
                artists.push((rank, SpaceArtist { id, name: track.artist.clone() }));
                artists.len() - 1
            });
            artists[at].0 = artists[at].0.min(rank);
        }
    }

    albums.sort_by_key(|entry| entry.0);
    artists.sort_by_key(|entry| entry.0);
    (
        albums.into_iter().map(|(_, album)| album).collect(),
        artists.into_iter().map(|(_, artist)| artist).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::Haystack;

    impl Haystack {
        fn contains(&self, term: &str) -> bool {
            self.artist.contains(term) || self.title.contains(term) || self.album.contains(term)
        }
    }

    fn haystack(artist: &str, title: &str, album: &str) -> Haystack {
        Haystack {
            artist: artist.to_lowercase(),
            title: title.to_lowercase(),
            album: album.to_lowercase(),
        }
    }

    /// The filter used to test terms against `format!("{artist} {title} {album}")`.
    /// Testing each field separately is the same predicate only because terms
    /// are split on whitespace and so can never span the joins, if that ever
    /// stops being true, this test is where it shows up.
    #[test]
    fn per_field_matches_the_joined_string() {
        let cases = [
            ("Aphex Twin", "Xtal", "Selected Ambient Works"),
            ("Burial", "Near Dark", "Untrue"),
            ("", "Untitled", ""),
        ];

        for (artist, title, album) in cases {
            let subject = haystack(artist, title, album);
            let joined = format!(
                "{} {} {}",
                artist.to_lowercase(),
                title.to_lowercase(),
                album.to_lowercase()
            );

            for term in ["aph", "xtal", "works", "near", "untrue", "zzz", "twin"] {
                assert_eq!(
                    subject.contains(term),
                    joined.contains(term),
                    "{term:?} against {artist:?}/{title:?}/{album:?}"
                );
            }
        }
    }

    /// The one case where the two differ, and why splitting on whitespace
    /// before matching keeps the difference unreachable.
    #[test]
    fn a_term_spanning_two_fields_cannot_be_produced_by_splitting() {
        let subject = haystack("Aphex Twin", "Xtal", "Ambient");
        // The joined string contains "twin xtal"; no single field does.
        assert!(!subject.contains("twin xtal"));
        assert!("aphex twin xtal ambient".contains("twin xtal"));
        // But a query is split first, so "twin xtal" is never one term.
        let terms: Vec<&str> = "twin xtal".split_whitespace().collect();
        assert_eq!(terms, ["twin", "xtal"]);
        assert!(terms.iter().all(|term| subject.contains(term)));
    }
}

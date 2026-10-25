//! The one loaded copy of the space. Every client navigates through it: the
//! apps hold no space of their own, so a phone on a poor connection has
//! nothing to download before it can generate.
//!
//! Loaded at startup and again whenever a rebuild settles. Requests hold an
//! `Arc` to the copy they started on, so a swap never pulls it from under one,
//! and share it without a lock: one person's long walk holds up no one's
//! search.

use anyhow::Result;
use axum::body::Bytes;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use stelly_core::api::{
    BlockWeight, GenerateRequest, OrderRequest, Recipe, SpaceAlbum, SpaceArtist, SpaceIndex,
    SpaceInfo, SpaceSearch, SpaceSort, Step, TrackMeta, Word,
};
use stelly_core::db::Catalog;
use stelly_core::engine::Engine;
use stelly_core::map;
use stelly_core::paths::Constraints;
use stelly_core::vectors::{quantise, Key, VectorBlock, Vectors, VectorsHead};

/// How long a graph path may take before the wider corridor is given up on
/// for the even path. Most get through the first corridor in tens of
/// milliseconds, and never come near it.
const PATH_BUDGET: Duration = Duration::from_millis(500);

pub struct SpaceService {
    data_dir: PathBuf,
    db_path: PathBuf,
    loaded: RwLock<Option<Arc<Loaded>>>,
    /// The describing words, once embedded. Each load works out its baseline
    /// against them in the background as soon as both are there.
    vocabulary: OnceLock<Arc<Vocabulary>>,
}

struct Loaded {
    engine: Engine,
    built_at: String,
    /// Each track's searchable text, lowercased once per load rather than per
    /// query. Indexed by catalogue row, blocked rows included.
    haystacks: Vec<Haystack>,
    /// What follows from which artists are hidden, rebuilt when that changes.
    derived: RwLock<Arc<Derived>>,
    /// Held while a block list is applied and derived from, so two changes
    /// close together land in the order they came, the last one's derived
    /// last. Requests never take it.
    blocking: Mutex<()>,
    /// Where each describing word sits across this corpus, worked out in the
    /// background after the load. The words never change while the server
    /// runs, so the corpus is all it depends on.
    baseline: OnceLock<Vec<(f32, f32)>>,
    /// `/api/space/vectors`, once an explorer has asked, for the stamp it was
    /// built at. Only stelly-explorer asks, so it is built then rather than
    /// in `derive`, which every hidden artist waits on.
    vectors: Mutex<Option<Arc<Prepared>>>,
}

/// The words a track can be described with, and their CLAP text embeddings,
/// row-aligned.
#[derive(Default)]
pub struct Vocabulary {
    pub words: Vec<String>,
    pub vectors: Vec<Vec<f32>>,
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
    /// `index` as JSON.
    pub index_body: Prepared,
    /// `map::payload`, the binary the map's canvas reads.
    pub points: Prepared,
    /// `map::meta`, as JSON, for apps from before `meta_binary`.
    pub meta: Prepared,
    /// `map::meta_binary`.
    pub meta_binary: Prepared,
    pub in_space: i64,
    pub on_map: i64,
}

/// One of the bodies a client caches, serialised and compressed once here
/// rather than per request: megabytes each, asked for by every device after
/// every rebuild.
pub struct Prepared {
    /// Names this body at this stamp, for `If-None-Match`.
    pub tag: String,
    pub content_type: &'static str,
    pub raw: Bytes,
    pub zstd: Bytes,
}

/// The level the compression layer used. Level 19 takes a fifth off the
/// labels but adds a second to every derive, which hiding an artist waits on.
const ZSTD_LEVEL: i32 = 3;

impl Prepared {
    fn new(stamp: &str, name: &str, content_type: &'static str, raw: Vec<u8>) -> Self {
        // Only fails on a bad level or out of memory.
        let zstd = zstd::bulk::compress(&raw, ZSTD_LEVEL).unwrap_or_default();
        Prepared {
            tag: stelly_core::api::space_tag(stamp, name),
            content_type,
            raw: Bytes::from(raw),
            zstd: Bytes::from(zstd),
        }
    }
}

impl SpaceService {
    pub fn new(data_dir: PathBuf, db_path: PathBuf) -> Self {
        Self {
            data_dir,
            db_path,
            loaded: RwLock::new(None),
            vocabulary: OnceLock::new(),
        }
    }

    /// Load the space from disk, replacing whatever was loaded. Blocking:
    /// reading the catalogue and its embeddings takes a second or two. A
    /// corpus without a space yet is an error here, and leaves the old copy.
    pub fn reload(&self) -> Result<()> {
        let engine = Engine::load(&self.data_dir, &self.db_path)?;
        let built_at = engine.space.manifest.built_at.clone();
        let catalog = engine.catalog();
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
        let derived = derive(&catalog, &built_at);

        let loaded = Loaded {
            engine,
            built_at,
            haystacks,
            derived: RwLock::new(Arc::new(derived)),
            blocking: Mutex::new(()),
            baseline: OnceLock::new(),
            vectors: Mutex::new(None),
        };
        let loaded = Arc::new(loaded);
        // Under the lock `set_vocabulary` reads the load through, so
        // whichever of the two comes last starts the baseline.
        let vocabulary = {
            let mut slot = self.loaded.write().unwrap_or_else(|e| e.into_inner());
            *slot = Some(loaded.clone());
            self.vocabulary.get().cloned()
        };
        if let Some(vocabulary) = vocabulary {
            loaded.prepare(vocabulary);
        }
        Ok(())
    }

    /// Hand over the describing words, once embedded, and start the loaded
    /// space's baseline against them.
    pub fn set_vocabulary(&self, vocabulary: Arc<Vocabulary>) {
        let loaded = {
            let slot = self.loaded.read().unwrap_or_else(|e| e.into_inner());
            let _ = self.vocabulary.set(vocabulary.clone());
            slot.clone()
        };
        if let Some(loaded) = loaded {
            loaded.prepare(vocabulary);
        }
    }

    fn loaded(&self) -> Option<Arc<Loaded>> {
        self.loaded.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Apply a changed block list, and redo what depends on it.
    pub fn set_blocked(&self, blocked: HashSet<i64>) {
        let Some(loaded) = self.loaded() else { return };
        let _blocking = loaded.blocking.lock().unwrap_or_else(|e| e.into_inner());
        let catalog = loaded.engine.set_blocked(blocked);
        let derived = derive(&catalog, &loaded.built_at);
        *loaded.derived.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(derived);
    }

    /// What there is, for a client deciding whether its cache still holds.
    /// The default when nothing is built yet: no stamp, no tracks.
    pub fn info(&self, tower: bool) -> SpaceInfo {
        let Some(loaded) = self.loaded() else {
            return SpaceInfo::default();
        };
        let derived = loaded.derived();
        let engine = &loaded.engine;
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

    /// Every visible track with its row of the space, see `stelly_core::vectors`.
    /// Built under the lock, so explorers asking at once wait for one build
    /// rather than each making their own.
    pub fn vectors(&self) -> Option<Arc<Prepared>> {
        let loaded = self.loaded()?;
        let stamp = loaded.derived().index.stamp.clone();
        let tag = stelly_core::api::space_tag(&stamp, "vectors");
        let mut slot = loaded.vectors.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(body) = slot.as_ref().filter(|body| body.tag == tag) {
            return Some(body.clone());
        }
        // Without them every track is drawn keyless, which is no reason to
        // send nothing.
        let keys = read_keys(&self.db_path).unwrap_or_else(|err| {
            eprintln!("vectors: reading the keys failed: {err:#}");
            HashMap::new()
        });
        let vectors = vectors(&loaded.engine, &keys, &stamp);
        let body = Arc::new(Prepared::new(&stamp, "vectors", "application/octet-stream", vectors.encode()));
        *slot = Some(body.clone());
        Some(body)
    }

    pub fn derived(&self) -> Option<Arc<Derived>> {
        Some(self.loaded()?.derived())
    }

    /// The space's rows for these ids, in the order asked, leaving out any it
    /// does not hold.
    pub fn tracks(&self, ids: &[i64]) -> Option<Vec<TrackMeta>> {
        let loaded = self.loaded()?;
        let index_of = &loaded.engine.space.index_of;
        let catalog = loaded.engine.catalog();
        Some(
            ids.iter()
                .filter_map(|id| index_of.get(id))
                .map(|&row| catalog.get(row).clone())
                .collect(),
        )
    }

    pub fn search(&self, query: &str, sort: SpaceSort, descending: bool, limit: usize) -> Option<SpaceSearch> {
        let loaded = self.loaded()?;
        let catalog = loaded.engine.catalog();
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

        let (albums, artists) = groups(&loaded.haystacks, &catalog, &terms);
        Some(SpaceSearch {
            total,
            tracks,
            albums,
            artists,
        })
    }

    /// Run a recipe. Drift and mood need the phrase embedded first, which is
    /// the caller's to do: it is async, and this blocks.
    pub fn generate(&self, request: &GenerateRequest, embedding: Option<&[f32]>) -> Option<Vec<Step>> {
        let loaded = self.loaded()?;
        let navigator = loaded.engine.navigator(&request.weights);
        let count = request.count;
        let constraints = Constraints::default();

        Some(match (&request.recipe, embedding) {
            // Led by the seed itself, as radio and path are: the list then
            // plays as "this, and what sounds like it" rather than starting
            // somewhere other than the track that was asked about.
            (Recipe::Neighbours { seed }, _) => match navigator.index_of.get(seed) {
                Some(&i) => {
                    let mut steps = vec![Step {
                        track: navigator.catalog.get(i).clone(),
                        similarity: None,
                    }];
                    steps.extend(navigator.neighbours(*seed, count.saturating_sub(1), false));
                    steps
                }
                None => Vec::new(),
            },
            (Recipe::Radio { seed }, _) => navigator.radio_nearest(*seed, count, request.penalty, &constraints),
            (Recipe::Path { a, b, even: true }, _) => navigator.interpolate(*a, *b, count, &constraints),
            (Recipe::Path { a, b, even: false }, _) => navigator.graph_path(*a, *b, 16, count, PATH_BUDGET),
            (Recipe::Drift { seed, .. }, Some(embedding)) => {
                navigator.drift_to_text(*seed, embedding, count, 5, &constraints)
            }
            (Recipe::Mood { .. }, Some(embedding)) => navigator.mood(embedding, count, request.penalty, &constraints),
            (Recipe::Drift { .. } | Recipe::Mood { .. }, None) => Vec::new(),
        })
    }

    /// The words a track sounds like more than the rest of the space does,
    /// best first. Empty when the space holds no CLAP vector for it.
    pub fn describe(&self, track_id: i64, vocabulary: &Vocabulary, count: usize) -> Option<Vec<Word>> {
        let loaded = self.loaded()?;
        // When it is still being worked out this waits for it, holding up no
        // one else. The weights do not matter to a description.
        let baseline = loaded.baseline(vocabulary);
        let navigator = loaded.engine.navigator(&HashMap::new());
        let Some(scores) = navigator.describe(track_id, &vocabulary.vectors, baseline) else {
            return Some(Vec::new());
        };
        let mut words: Vec<Word> = vocabulary
            .words
            .iter()
            .zip(scores)
            .map(|(word, score)| Word { word: word.clone(), score })
            .collect();
        words.sort_by(|a, b| b.score.total_cmp(&a.score));
        words.truncate(count);
        Some(words)
    }

    pub fn order(&self, request: &OrderRequest) -> Option<Vec<i64>> {
        let loaded = self.loaded()?;
        let navigator = loaded.engine.navigator(&request.weights);
        Some(navigator.shortest_path_order(&request.ids, request.from))
    }
}

impl Loaded {
    fn derived(&self) -> Arc<Derived> {
        self.derived.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The baseline, worked out by whoever asks first while the others wait.
    /// Half a second on 23k tracks, seconds on a large corpus.
    fn baseline(&self, vocabulary: &Vocabulary) -> &[(f32, f32)] {
        self.baseline.get_or_init(|| {
            let catalog = self.engine.catalog();
            let clap = catalog.clap.as_deref().map_or(&[][..], Vec::as_slice);
            stelly_core::paths::text_baseline(clap, catalog.clap_dims, &vocabulary.vectors)
        })
    }

    /// Work the baseline out in the background, so the first description
    /// asked of this load does not.
    fn prepare(self: &Arc<Self>, vocabulary: Arc<Vocabulary>) {
        let loaded = self.clone();
        std::thread::spawn(move || {
            loaded.baseline(&vocabulary);
        });
    }
}

fn derive(catalog: &Catalog, built_at: &str) -> Derived {
    // The build and the hidden artists are all a client's copy depends on.
    // Hashed rather than counted, so it survives a server restart.
    let mut blocked: Vec<i64> = catalog.blocked_artists().iter().copied().collect();
    blocked.sort_unstable();
    let mut hasher = DefaultHasher::new();
    blocked.hash(&mut hasher);
    let stamp = format!("{built_at}/{:016x}", hasher.finish());

    let (albums, artists) = catalog.reach();
    let ids: Vec<i64> = catalog.visible().map(|i| catalog.get(i).track_id).collect();
    let in_space = ids.len() as i64;
    let on_map = catalog.visible().filter(|&i| catalog.get(i).x.is_some()).count() as i64;

    let index = SpaceIndex {
        stamp,
        ids,
        albums: albums.into_iter().collect(),
        artists: artists.into_iter().collect(),
    };
    let stamp = &index.stamp;
    let map_meta = map::meta(catalog);
    // Side by side: the labels take the longest to compress, in both forms.
    let (index_body, points, meta, meta_binary) = std::thread::scope(|scope| {
        let points = scope.spawn(|| {
            Prepared::new(stamp, "points", "application/octet-stream", map::payload(catalog))
        });
        let meta = scope.spawn(|| {
            let meta = serde_json::to_vec(&map_meta).unwrap_or_default();
            Prepared::new(stamp, "meta", "application/json", meta)
        });
        let meta_binary = scope.spawn(|| {
            Prepared::new(stamp, "meta-binary", "application/octet-stream", map::meta_binary(&map_meta))
        });
        let index_body = serde_json::to_vec(&index).unwrap_or_default();
        let index_body = Prepared::new(stamp, "index", "application/json", index_body);
        (index_body, points.join(), meta.join(), meta_binary.join())
    });
    let joined = |body: std::thread::Result<Prepared>| body.unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    Derived {
        index_body,
        points: joined(points),
        meta: joined(meta),
        meta_binary: joined(meta_binary),
        index,
        in_space,
        on_map,
    }
}

/// Each analysed track's key and how sure the analyser was of it, from what
/// `analyse` stored. The space keeps the key only as a direction on the
/// circle of fifths, normalised past reading back.
fn read_keys(db_path: &std::path::Path) -> Result<HashMap<i64, (u8, u8)>> {
    use diesel::prelude::*;
    use stelly_core::schema::features;
    let mut conn = crate::db::connect(db_path, Duration::from_secs(5))?;
    let rows: Vec<(i64, Option<String>)> = features::table
        .filter(features::descriptors_json.is_not_null())
        .select((features::track_id, features::descriptors_json))
        .load(&mut conn)?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, json)| {
            let descriptors: serde_json::Value = serde_json::from_str(&json?).ok()?;
            let key = Key::parse(descriptors.get("key")?.as_str()?, descriptors.get("scale").and_then(|v| v.as_str()))?;
            let strength = descriptors.get("key_strength").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Some((id, (Key::encode(Some(key)), (strength.clamp(0.0, 1.0) * 255.0).round() as u8)))
        })
        .collect())
}

/// The visible tracks, in catalogue order, which is the space's row order.
fn vectors(engine: &Engine, keys: &HashMap<i64, (u8, u8)>, stamp: &str) -> Vectors {
    let catalog = engine.catalog();
    let space = &engine.space;
    let rows: Vec<usize> = catalog.visible().collect();
    let dims = space.manifest.n_dims;

    let mut genres: Vec<String> = Vec::new();
    let mut genre_of: HashMap<&str, u16> = HashMap::new();
    let mut v = Vectors {
        head: VectorsHead {
            stamp: stamp.to_string(),
            n: rows.len(),
            dims,
            blocks: space
                .manifest
                .blocks
                .iter()
                .map(|b| VectorBlock {
                    name: b.name.clone(),
                    start: b.start,
                    end: b.end,
                    columns: b.columns.clone(),
                    default_weight: b.default_weight,
                })
                .collect(),
            genres: Vec::new(),
        },
        rows: Vec::with_capacity(rows.len() * dims),
        ..Vectors::default()
    };
    for &i in &rows {
        let track = catalog.get(i);
        v.ids.push(track.track_id);
        v.xy.push([track.x.unwrap_or(f32::NAN), track.y.unwrap_or(f32::NAN)]);
        let genre = *genre_of.entry(track.genre.as_str()).or_insert_with(|| {
            genres.push(track.genre.clone());
            (genres.len() - 1) as u16
        });
        v.genre.push(genre);
        v.artist.push(track.artist_id);
        v.rows.extend(space.row(i).iter().map(|x| quantise(*x)));
        v.bpm.push(track.bpm.unwrap_or(f32::NAN));
        let (key, strength) = keys.get(&track.track_id).copied().unwrap_or((Key::NONE, 0));
        v.key.push(key);
        v.key_strength.push(strength);
        v.titles.push(track.title.clone());
        v.artists.push(track.artist.clone());
        v.albums.push(track.album.clone());
    }
    v.head.genres = genres;
    v
}

/// The albums and artists behind the tracks matching every term somewhere in
/// artist, title or album, best match first.
fn groups(haystacks: &[Haystack], catalog: &Catalog, terms: &[String]) -> (Vec<SpaceAlbum>, Vec<SpaceArtist>) {
    let Some(first) = terms.first() else {
        return Default::default();
    };

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
                        image: track.image.clone(),
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

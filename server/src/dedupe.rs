//! Tracks that are one recording, heard rather than read: two whose CLAP
//! embeddings all but coincide, and whose lengths agree, are one track.
//!
//! Catalogues file the same audio under several entries, a single and the
//! album it is on, a compilation, a deluxe reissue, a clean and an explicit
//! edition, and providers give the same recording different ids and
//! sometimes no ISRC at all. Measured on the production catalogue, the same
//! song from a FLAC file and from Qobuz's MP3 scores 0.9986 at the median,
//! 0.993 at worst, and different songs start passing 0.99: two tracks with
//! no ISRC to tell them apart are one at 0.995, with lengths within two
//! seconds. Different ISRCs are different recordings unless the audio is the
//! same master, a compilation's or a reissue's, which scores 0.9999 and up:
//! two tunes from one jazz session, a song and its dub, an audiobook's
//! chapters in one voice all reach 0.995 to 0.998, so those want 0.999.
//!
//! Merging keeps the track closest to a like, the oldest if they tie: the
//! others' providers' items, likes and crawl distance become its, and their
//! rows go. `track_merges` remembers each, so a device still holding a merged
//! id is pointed at the one kept. Only tracks analysed since the last run are
//! compared, each against every analysed track of about its length.

use crate::pipeline::{Job, Paths};
use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Binary, Double, Nullable, Text};
use std::collections::HashMap;

/// The least similarity two embeddings of one recording have, when at
/// least one of them has no ISRC.
pub const MIN_SIMILARITY: f32 = 0.995;
/// The same, between two tracks whose ISRCs differ: one master released
/// twice.
pub const MIN_SIMILARITY_OTHER_ISRC: f32 = 0.999;
/// The most two lengths of one recording differ, in seconds.
pub const LENGTH_SLACK: i64 = 2;

#[derive(Debug, Clone)]
pub struct Options {
    pub min_similarity: f32,
    pub min_similarity_other_isrc: f32,
    /// Say what would be merged, and merge nothing.
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { min_similarity: MIN_SIMILARITY, min_similarity_other_isrc: MIN_SIMILARITY_OTHER_ISRC, dry_run: false }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub compared: usize,
    pub merged: usize,
}

#[derive(QueryableByName)]
struct Row {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    duration: Option<i64>,
    #[diesel(sql_type = BigInt)]
    seed_distance: i64,
    #[diesel(sql_type = Nullable<Text>)]
    isrc: Option<String>,
    #[diesel(sql_type = Binary)]
    clap: Vec<u8>,
    #[diesel(sql_type = BigInt)]
    new: i64,
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Analysed tracks not compared yet.
pub fn pending_count(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(diesel::sql_query(
        "SELECT COUNT(*) AS n FROM features f WHERE f.clap_f32 IS NOT NULL
         AND f.track_id NOT IN (SELECT track_id FROM dedupe_seen)",
    )
    .get_result::<Count>(conn)?
    .n)
}

struct Track {
    id: i64,
    seconds: i64,
    seed_distance: i64,
    isrc: Option<String>,
    unit: Vec<f32>,
    new: bool,
}

fn unit(bytes: &[u8]) -> Vec<f32> {
    let mut v: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Every pair of tracks, at least one of them new, that are one recording,
/// with how alike they are. `tracks` sorted by length.
fn pairs(tracks: &[Track], options: &Options) -> Vec<(usize, usize, f32)> {
    // All the cores but one, behind requests, as layout's neighbours.
    let threads = crate::pipeline::stage_threads();
    let chunk = tracks.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..tracks.len())
            .step_by(chunk)
            .map(|start| {
                scope.spawn(move || {
                    crate::pipeline::step_back();
                    let mut found = Vec::new();
                    for i in start..(start + chunk).min(tracks.len()) {
                        let a = &tracks[i];
                        // Forwards only, so each pair is looked at once.
                        for (j, b) in tracks.iter().enumerate().skip(i + 1) {
                            if b.seconds - a.seconds > LENGTH_SLACK {
                                break;
                            }
                            if !(a.new || b.new) {
                                continue;
                            }
                            let similarity = dot(&a.unit, &b.unit);
                            let min = match (&a.isrc, &b.isrc) {
                                (Some(x), Some(y)) if x != y => options.min_similarity_other_isrc,
                                _ => options.min_similarity,
                            };
                            if similarity >= min {
                                found.push((i, j, similarity));
                            }
                        }
                    }
                    found
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("a comparing thread panicked")).collect()
    })
}

/// Groups of indices that are one recording: around the one `keep` picks,
/// those alike enough to it themselves. Not through a chain of pairs, each
/// alike enough to the next: an audiobook's chapters, read by one voice,
/// would be one track by the end of it.
fn groups(n: usize, pairs: &[(usize, usize, f32)], keep: impl Fn(usize) -> (i64, i64)) -> Vec<Vec<usize>> {
    let mut alike: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(a, b, _) in pairs {
        alike.entry(a).or_default().push(b);
        alike.entry(b).or_default().push(a);
    }
    let mut taken = vec![false; n];
    // The tracks to keep first, so each group forms around the one it keeps.
    let mut order: Vec<usize> = alike.keys().copied().collect();
    order.sort_by_key(|&i| keep(i));
    let mut out = Vec::new();
    for i in order {
        if taken[i] {
            continue;
        }
        let members: Vec<usize> = alike[&i].iter().copied().filter(|&j| !taken[j]).collect();
        if members.is_empty() {
            continue;
        }
        taken[i] = true;
        let mut group = vec![i];
        for j in members {
            taken[j] = true;
            group.push(j);
        }
        out.push(group);
    }
    out
}

/// Make `from` `into`: its providers' items, likes and crawl distance, then
/// let its rows go.
pub fn merge(conn: &mut SqliteConnection, from: i64, into: i64, similarity: f32) -> Result<()> {
    let (from_s, into_s) = (from.to_string(), into.to_string());
    let text = |sql: &str| diesel::sql_query(sql.to_string());
    text("UPDATE refs SET id = ? WHERE kind = 'track' AND id = ?")
        .bind::<Text, _>(&into_s)
        .bind::<Text, _>(&from_s)
        .execute(conn)?;
    // A like of both stays one like, the earlier.
    text(
        "INSERT OR IGNORE INTO likes (user_id, kind, ref_id, item_json, liked_at)
         SELECT user_id, kind, ?, item_json, liked_at FROM likes WHERE kind = 'track' AND ref_id = ?",
    )
    .bind::<Text, _>(&into_s)
    .bind::<Text, _>(&from_s)
    .execute(conn)
    .ok();
    text("DELETE FROM likes WHERE kind = 'track' AND ref_id = ?").bind::<Text, _>(&from_s).execute(conn).ok();
    text(
        "UPDATE tracks SET seed_distance = MIN(seed_distance, (SELECT seed_distance FROM tracks WHERE id = ?)),
         isrc = COALESCE(isrc, (SELECT isrc FROM tracks WHERE id = ?)) WHERE id = ?",
    )
    .bind::<BigInt, _>(from)
    .bind::<BigInt, _>(from)
    .bind::<BigInt, _>(into)
    .execute(conn)?;
    // Why it was waiting for its mix analysis is `into`'s now, the more
    // urgent reason of the two when both were.
    text(
        "INSERT INTO mix_queue (track_id, priority, reason, queued_at)
         SELECT ?, priority, reason, queued_at FROM mix_queue WHERE track_id = ?
         ON CONFLICT(track_id) DO UPDATE SET priority = excluded.priority, reason = excluded.reason
         WHERE excluded.priority < mix_queue.priority",
    )
    .bind::<BigInt, _>(into)
    .bind::<BigInt, _>(from)
    .execute(conn)?;
    // The same recording, so its mix analysis is `into`'s when `into` has
    // none, renamed for it: the entry names the track it was made for.
    text(
        "INSERT OR IGNORE INTO mix (track_id, version, entry_json, analysed_at)
         SELECT ?, version, json_set(entry_json, '$.origin.item.id', ?), analysed_at FROM mix WHERE track_id = ?",
    )
    .bind::<BigInt, _>(into)
    .bind::<Text, _>(&into_s)
    .bind::<BigInt, _>(from)
    .execute(conn)?;
    for table in ["features", "layout", "failures", "mix", "mix_failures", "mix_queue", "dedupe_seen"] {
        text(&format!("DELETE FROM {table} WHERE track_id = ?")).bind::<BigInt, _>(from).execute(conn)?;
    }
    text("DELETE FROM tracks WHERE id = ?").bind::<BigInt, _>(from).execute(conn)?;
    // What was merged into `from` before is `into`'s now.
    text("UPDATE track_merges SET into_id = ? WHERE into_id = ?")
        .bind::<BigInt, _>(into)
        .bind::<BigInt, _>(from)
        .execute(conn)?;
    text("INSERT OR REPLACE INTO track_merges (from_id, into_id, similarity, merged_at) VALUES (?, ?, ?, ?)")
        .bind::<BigInt, _>(from)
        .bind::<BigInt, _>(into)
        .bind::<Double, _>(f64::from(similarity))
        .bind::<Text, _>(crate::db::utc_now())
        .execute(conn)?;
    Ok(())
}

/// How many tracks are marked compared a statement.
const MARKED_AT_ONCE: i64 = 5_000;

/// Every track with an embedding marked compared, `MARKED_AT_ONCE` at a
/// time in the order of the index that lists them.
fn mark_compared(chunks: &mut crate::db::Chunked) -> Result<()> {
    #[derive(QueryableByName)]
    struct Last {
        #[diesel(sql_type = Nullable<BigInt>)]
        id: Option<i64>,
    }
    let mut after = i64::MIN;
    loop {
        let conn = chunks.conn()?;
        let last = diesel::sql_query(
            "SELECT MAX(track_id) AS id FROM
             (SELECT track_id FROM features WHERE clap_f32 IS NOT NULL AND track_id > ? ORDER BY track_id LIMIT ?)",
        )
        .bind::<BigInt, _>(after)
        .bind::<BigInt, _>(MARKED_AT_ONCE)
        .get_result::<Last>(conn)?
        .id;
        let Some(last) = last else { return Ok(()) };
        diesel::sql_query(
            "INSERT OR IGNORE INTO dedupe_seen (track_id)
             SELECT track_id FROM features WHERE clap_f32 IS NOT NULL AND track_id > ? AND track_id <= ?",
        )
        .bind::<BigInt, _>(after)
        .bind::<BigInt, _>(last)
        .execute(conn)?;
        after = last;
    }
}

/// Compare what was analysed since the last run with everything analysed,
/// and merge what is one recording.
pub fn run(paths: &Paths, options: &Options, job: &Job) -> Result<Stats> {
    let conn = &mut crate::db::open_for_write(&paths.db_path)?;
    let rows = diesel::sql_query(
        "SELECT t.id AS id, t.duration AS duration, t.seed_distance AS seed_distance, UPPER(TRIM(t.isrc)) AS isrc, f.clap_f32 AS clap,
                (f.track_id NOT IN (SELECT track_id FROM dedupe_seen)) AS new
         FROM tracks t JOIN features f ON f.track_id = t.id
         WHERE f.clap_f32 IS NOT NULL AND t.duration IS NOT NULL",
    )
    .load::<Row>(conn)
    .context("loading the embeddings")?;
    let mut tracks: Vec<Track> = rows
        .into_iter()
        .map(|r| Track {
            id: r.id,
            seconds: r.duration.unwrap_or(0),
            seed_distance: r.seed_distance,
            isrc: r.isrc.filter(|s| !s.is_empty()),
            unit: unit(&r.clap),
            new: r.new != 0,
        })
        .collect();
    let new = tracks.iter().filter(|t| t.new).count();
    if new == 0 {
        job.log("nothing new to compare");
        return Ok(Stats::default());
    }
    tracks.sort_by_key(|t| (t.seconds, t.id));
    job.log(format!("comparing {new} newly analysed tracks with {} analysed", tracks.len()));
    job.advance("dedupe", 0, 1);

    let found = pairs(&tracks, options);
    let best: HashMap<(usize, usize), f32> = found.iter().map(|&(a, b, s)| ((a, b), s)).collect();
    let mut stats = Stats { compared: new, merged: 0 };
    let all = groups(tracks.len(), &found, |i| (tracks[i].seed_distance, tracks[i].id));
    // Committed a few groups at a time, so requests' writes go in between;
    // each group whole in one. Stopped halfway, the groups merged stay
    // merged, nothing is marked compared yet, and the next run finds the
    // rest again: the groups left are the ones this run had left.
    let mut chunks = (!options.dry_run).then(|| crate::db::Chunked::new(conn));
    for group in &all {
        // The first is the one kept: closest to a like, the oldest if they tie.
        let keep = group[0];
        let mut conn = match &mut chunks {
            Some(chunks) => Some(chunks.conn()?),
            None => None,
        };
        for &i in group.iter().filter(|&&i| i != keep) {
            let similarity = best
                .get(&(keep.min(i), keep.max(i)))
                .copied()
                .unwrap_or_else(|| dot(&tracks[keep].unit, &tracks[i].unit));
            let (from, into) = (tracks[i].id, tracks[keep].id);
            match conn.as_deref_mut() {
                Some(conn) => merge(conn, from, into, similarity)?,
                None => job.log(format!("  would merge {from} into {into} ({similarity:.4})")),
            }
            stats.merged += 1;
        }
    }
    if let Some(mut chunks) = chunks {
        mark_compared(&mut chunks)?;
        chunks.finish()?;
    }
    job.advance("dedupe", 1, 1);
    let verb = if options.dry_run { "would merge" } else { "merged" };
    job.log(format!("{verb} {} tracks into {} others", stats.merged, all.len()));
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::connection::SimpleConnection;

    fn track(id: i64, seconds: i64, unit: Vec<f32>, new: bool) -> Track {
        Track { id, seconds, seed_distance: 1, isrc: None, unit, new }
    }

    #[test]
    fn different_isrcs_want_the_same_master() {
        let near = |isrc: &str, unit: Vec<f32>| Track { isrc: Some(isrc.into()), ..track(1, 200, unit, true) };
        let a = vec![1.0, 0.0];
        // 0.997 alike: one recording without ISRCs, two with different ones.
        let close = vec![0.997, (1.0f32 - 0.997 * 0.997).sqrt()];
        let pair = |x: Track, y: Track| pairs(&[x, y], &Options::default()).len();
        assert_eq!(pair(track(1, 200, a.clone(), true), track(2, 200, close.clone(), true)), 1);
        assert_eq!(pair(near("A", a.clone()), near("B", close.clone())), 0);
        assert_eq!(pair(near("A", a.clone()), near("B", a.clone())), 1);
    }

    #[test]
    fn one_recording_is_what_is_alike_to_the_one_kept_and_only_at_its_length() {
        let a = vec![1.0, 0.0];
        let close = vec![0.9999, 0.0141];
        let far = vec![0.0, 1.0];
        let tracks = vec![
            track(1, 200, a.clone(), false),
            track(2, 201, close.clone(), true),
            track(3, 202, a.clone(), false),
            // The same sound, far too long to be it.
            track(4, 300, a.clone(), true),
            track(5, 300, far, true),
        ];
        let found = pairs(&tracks, &Options::default());
        let mut seen: Vec<(i64, i64)> = found.iter().map(|&(i, j, _)| (tracks[i].id, tracks[j].id)).collect();
        seen.sort();
        // 1 and 3 are both old, so never compared with each other.
        assert_eq!(seen, [(1, 2), (2, 3)]);
        let mut all: Vec<Vec<i64>> = groups(tracks.len(), &found, |i| (1, tracks[i].id))
            .into_iter()
            .map(|g| {
                let mut ids: Vec<i64> = g.into_iter().map(|i| tracks[i].id).collect();
                ids.sort();
                ids
            })
            .collect();
        all.sort();
        // Kept, 1; 2 is alike to it. 3 is alike only to 2, so stays its own.
        assert_eq!(all, [vec![1, 2]]);
    }

    #[test]
    fn a_merge_keeps_what_the_merged_track_had() {
        let dir = std::env::temp_dir().join(format!("stelly-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = &mut crate::db::open_for_write(&dir.join("stelly.db")).unwrap();
        conn.batch_execute(
            "INSERT INTO tracks (id, title, seed_distance, isrc) VALUES (1, 'One', 2, NULL), (2, 'One again', 0, 'X');
             INSERT INTO refs (kind, id, provider, ref) VALUES ('track', '1', 'qobuz', '1'), ('track', '2', 'local', '9');
             INSERT INTO features (track_id, extractor_version) VALUES (1, 'v'), (2, 'v');
             INSERT INTO mix_queue (track_id, priority, reason, queued_at) VALUES (2, 1, 'liked', '');
             INSERT INTO mix (track_id, version, entry_json, analysed_at)
               VALUES (2, 1, '{\"origin\":{\"source\":\"stelly\",\"item\":{\"id\":\"2\"}}}', 'then');",
        )
        .unwrap();
        merge(conn, 2, 1, 0.999).unwrap();
        let mut routes = crate::refs::routes(conn, crate::refs::Kind::Track, "1").unwrap();
        routes.sort();
        assert_eq!(routes, [("local".to_string(), "9".to_string()), ("qobuz".to_string(), "1".to_string())]);
        // The merged id still finds its way.
        assert_eq!(crate::refs::routes(conn, crate::refs::Kind::Track, "2").unwrap().len(), 2);
        #[derive(QueryableByName)]
        struct Kept {
            #[diesel(sql_type = BigInt)]
            seed_distance: i64,
            #[diesel(sql_type = Nullable<Text>)]
            isrc: Option<String>,
        }
        let kept = diesel::sql_query("SELECT seed_distance, isrc FROM tracks WHERE id = 1").get_result::<Kept>(conn).unwrap();
        assert_eq!((kept.seed_distance, kept.isrc.as_deref()), (0, Some("X")));
        assert_eq!(diesel::sql_query("SELECT COUNT(*) AS n FROM tracks").get_result::<Count>(conn).unwrap().n, 1);
        // Liked as 2, so 1 is analysed for mixing in its place.
        assert_eq!(diesel::sql_query("SELECT COUNT(*) AS n FROM mix_queue WHERE track_id = 1 AND reason = 'liked'").get_result::<Count>(conn).unwrap().n, 1);
        assert_eq!(diesel::sql_query("SELECT COUNT(*) AS n FROM mix_queue").get_result::<Count>(conn).unwrap().n, 1);
        // And 2's mix analysis is 1's, named for it.
        #[derive(QueryableByName)]
        struct Entry {
            #[diesel(sql_type = Text)]
            id: String,
        }
        let moved = diesel::sql_query("SELECT json_extract(entry_json, '$.origin.item.id') AS id FROM mix WHERE track_id = 1")
            .get_result::<Entry>(conn)
            .unwrap();
        assert_eq!(moved.id, "1");
        assert_eq!(diesel::sql_query("SELECT COUNT(*) AS n FROM mix").get_result::<Count>(conn).unwrap().n, 1);
    }
}

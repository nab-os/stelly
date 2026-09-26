//! Catalogue crawler.
//!
//! Seeds from favourites at seed_distance 0, then expands through
//! `artist/getSimilarArtists`. The frontier lives in SQLite, so the crawl is
//! interruptible.

use crate::db::{coalesce, least};
use crate::qobuz::QobuzClient;
use crate::schema::{albums, artists, blocked_artists, frontier, tracks};
use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::upsert::excluded;
use serde_json::Value;
use std::collections::HashSet;


#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub artists_expanded: usize,
    pub albums_expanded: usize,
    pub tracks_added: i64,
    pub errors: usize,
    pub blocked_skipped: usize,
}

fn now() -> String {
    crate::db::utc_now()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn as_i64(value: &Value, key: &str) -> Option<i64> {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn as_id_string(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

// ------------------------------------------------------------------ upserts
//
// COALESCE keeps stored values when a sparser payload for the same entity
// arrives, a stub artist inside a track versus a full artist/get.

pub fn upsert_artist(conn: &mut SqliteConnection, artist: &Value) -> Result<Option<i64>> {
    let Some(id) = as_i64(artist, "id") else {
        return Ok(None);
    };
    diesel::insert_into(artists::table)
        .values((
            artists::id.eq(id),
            artists::name.eq(text(artist, "name").unwrap_or_else(|| "Unknown Artist".into())),
            artists::qobuz_json.eq(artist.to_string()),
        ))
        .on_conflict(artists::id)
        .do_update()
        .set((
            artists::name.eq(excluded(artists::name)),
            artists::qobuz_json.eq(coalesce(excluded(artists::qobuz_json), artists::qobuz_json)),
        ))
        .execute(conn)?;
    Ok(Some(id))
}

pub fn upsert_album(conn: &mut SqliteConnection, album: &Value) -> Result<Option<String>> {
    let Some(id) = as_id_string(album, "id") else {
        return Ok(None);
    };
    let artist_id = match album.get("artist") {
        Some(artist) => upsert_artist(conn, artist)?,
        None => None,
    };

    diesel::insert_into(albums::table)
        .values((
            albums::id.eq(&id),
            albums::artist_id.eq(artist_id),
            albums::title.eq(text(album, "title").unwrap_or_else(|| "Unknown Album".into())),
            albums::release_date
                .eq(text(album, "release_date_original").or_else(|| text(album, "released_at"))),
            albums::label.eq(album.get("label").and_then(|l| l.get("name")).and_then(|v| v.as_str())),
            albums::genre.eq(album.get("genre").and_then(|g| g.get("name")).and_then(|v| v.as_str())),
            albums::qobuz_json.eq(album.to_string()),
        ))
        .on_conflict(albums::id)
        .do_update()
        .set((
            albums::artist_id.eq(coalesce(excluded(albums::artist_id), albums::artist_id)),
            albums::title.eq(excluded(albums::title)),
            albums::release_date.eq(coalesce(excluded(albums::release_date), albums::release_date)),
            albums::label.eq(coalesce(excluded(albums::label), albums::label)),
            albums::genre.eq(coalesce(excluded(albums::genre), albums::genre)),
            albums::qobuz_json.eq(coalesce(excluded(albums::qobuz_json), albums::qobuz_json)),
        ))
        .execute(conn)?;
    Ok(Some(id))
}

pub fn upsert_track(
    conn: &mut SqliteConnection,
    track: &Value,
    album_id: Option<&str>,
    artist_id: Option<i64>,
    seed_distance: i64,
) -> Result<Option<i64>> {
    let Some(id) = as_i64(track, "id") else {
        return Ok(None);
    };

    let album_id = match (album_id, track.get("album")) {
        (Some(given), _) => Some(given.to_string()),
        (None, Some(album)) => upsert_album(conn, album)?,
        (None, None) => None,
    };

    let artist_id = match artist_id {
        Some(given) => Some(given),
        None => {
            let performer = track.get("performer").or_else(|| track.get("artist"));
            match performer {
                Some(value) => upsert_artist(conn, value)?,
                None => None,
            }
        }
    };

    diesel::insert_into(tracks::table)
        .values((
            tracks::id.eq(id),
            tracks::album_id.eq(album_id),
            tracks::artist_id.eq(artist_id),
            tracks::title.eq(text(track, "title").unwrap_or_else(|| "Unknown Track".into())),
            tracks::duration.eq(as_i64(track, "duration")),
            tracks::isrc.eq(text(track, "isrc")),
            tracks::qobuz_json.eq(track.to_string()),
            tracks::seed_distance.eq(seed_distance),
        ))
        .on_conflict(tracks::id)
        .do_update()
        .set((
            tracks::album_id.eq(coalesce(excluded(tracks::album_id), tracks::album_id)),
            tracks::artist_id.eq(coalesce(excluded(tracks::artist_id), tracks::artist_id)),
            tracks::title.eq(excluded(tracks::title)),
            tracks::duration.eq(coalesce(excluded(tracks::duration), tracks::duration)),
            tracks::isrc.eq(coalesce(excluded(tracks::isrc), tracks::isrc)),
            tracks::qobuz_json.eq(coalesce(excluded(tracks::qobuz_json), tracks::qobuz_json)),
            // Keep the shortest known distance to a favourite.
            tracks::seed_distance.eq(least(tracks::seed_distance, excluded(tracks::seed_distance))),
        ))
        .execute(conn)?;
    Ok(Some(id))
}

// ----------------------------------------------------------------- frontier

/// Add to the frontier, keeping the lowest priority (closest to a seed).
pub fn enqueue(conn: &mut SqliteConnection, kind: &str, ref_id: &str, priority: i64) -> Result<()> {
    diesel::insert_into(frontier::table)
        .values((
            frontier::kind.eq(kind),
            frontier::ref_id.eq(ref_id),
            frontier::priority.eq(priority),
            frontier::state.eq("pending"),
        ))
        .on_conflict((frontier::kind, frontier::ref_id))
        .do_update()
        .set(frontier::priority.eq(least(frontier::priority, excluded(frontier::priority))))
        .execute(conn)?;
    Ok(())
}

fn mark(conn: &mut SqliteConnection, kind: &str, ref_id: &str, state: &str) -> Result<()> {
    diesel::update(frontier::table.find((kind, ref_id)))
        .set(frontier::state.eq(state))
        .execute(conn)?;
    Ok(())
}

#[derive(Queryable)]
struct Item {
    kind: String,
    ref_id: String,
    priority: i64,
}

fn next_batch(conn: &mut SqliteConnection, limit: usize) -> Result<Vec<Item>> {
    Ok(frontier::table
        .filter(frontier::state.eq("pending"))
        .order((frontier::priority.asc(), frontier::kind.desc()))
        .limit(limit as i64)
        .select((frontier::kind, frontier::ref_id, frontier::priority))
        .load(conn)?)
}

fn track_count(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(tracks::table.count().get_result(conn)?)
}

fn blocked_ids(conn: &mut SqliteConnection) -> Result<HashSet<i64>> {
    Ok(blocked_artists::table
        .select(blocked_artists::artist_id)
        .load::<i64>(conn)?
        .into_iter()
        .collect())
}

/// Whether a frontier item belongs to a blocked artist.
fn is_blocked(conn: &mut SqliteConnection, kind: &str, ref_id: &str, blocked: &HashSet<i64>) -> bool {
    if blocked.is_empty() {
        return false;
    }
    match kind {
        "artist" => ref_id.parse::<i64>().is_ok_and(|id| blocked.contains(&id)),
        "album" => albums::table
            .find(ref_id)
            .select(albums::artist_id)
            .first::<Option<i64>>(conn)
            .ok()
            .flatten()
            .is_some_and(|id| blocked.contains(&id)),
        _ => false,
    }
}

// --------------------------------------------------------------------- seed

/// Load every favourite into the catalogue at seed_distance 0.
pub async fn seed(conn: &mut SqliteConnection, client: &mut QobuzClient, cap: usize) -> Result<Stats> {
    let mut stats = Stats::default();

    for track in client.favorites_raw("tracks", cap).await? {
        if upsert_track(conn, &track, None, None, 0)?.is_some() {
            stats.tracks_added += 1;
            if let Some(id) = track.get("performer").and_then(|p| as_i64(p, "id")) {
                enqueue(conn, "artist", &id.to_string(), 0)?;
            }
        }
    }

    for album in client.favorites_raw("albums", cap).await? {
        if let Some(album_id) = upsert_album(conn, &album)? {
            stats.albums_expanded += 1;
            enqueue(conn, "album", &album_id, 0)?;
            if let Some(id) = album.get("artist").and_then(|a| as_i64(a, "id")) {
                enqueue(conn, "artist", &id.to_string(), 0)?;
            }
        }
    }

    for artist in client.favorites_raw("artists", cap).await? {
        if let Some(id) = upsert_artist(conn, &artist)? {
            stats.artists_expanded += 1;
            enqueue(conn, "artist", &id.to_string(), 0)?;
        }
    }

    Ok(stats)
}

// -------------------------------------------------------------------- crawl

/// Pull an artist's albums, and enqueue their similar artists one hop further out.
pub async fn expand_artist(
    conn: &mut SqliteConnection,
    client: &mut QobuzClient,
    artist_id: i64,
    distance: i64,
    max_distance: i64,
) -> Result<()> {
    for album in client.artist_albums_raw(artist_id, 1000).await? {
        if let Some(album_id) = upsert_album(conn, &album)? {
            enqueue(conn, "album", &album_id, distance)?;
        }
    }

    diesel::update(artists::table.find(artist_id))
        .set(artists::similar_fetched_at.eq(now()))
        .execute(conn)?;

    if distance < max_distance {
        let blocked = blocked_ids(conn)?;
        for similar in client.similar_artists_raw(artist_id, 50).await? {
            if let Some(id) = upsert_artist(conn, &similar)? {
                // A blocked artist is a dead end, not just a hidden one:
                // following them would pull their whole neighbourhood in.
                if !blocked.contains(&id) {
                    enqueue(conn, "artist", &id.to_string(), distance + 1)?;
                }
            }
        }
    }

    Ok(())
}

/// Pull an album's tracklist. Returns how many tracks were written.
pub async fn expand_album(
    conn: &mut SqliteConnection,
    client: &mut QobuzClient,
    album_id: &str,
    distance: i64,
) -> Result<usize> {
    let album = client.album_raw(album_id).await?;
    upsert_album(conn, &album)?;
    let artist_id = album.get("artist").and_then(|a| as_i64(a, "id"));

    let mut written = 0;
    let tracks = album
        .get("tracks")
        .and_then(|block| block.get("items"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for track in &tracks {
        if upsert_track(conn, track, Some(album_id), artist_id, distance)?.is_some() {
            written += 1;
        }
    }
    Ok(written)
}

/// The outcome of one unit of crawl work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    Expanded { kind: String, ref_id: String },
    Skipped { kind: String, ref_id: String },
    Failed { kind: String, ref_id: String, error: String },
    /// The track budget is reached; nothing was done.
    BudgetReached,
    /// The frontier is empty; nothing was done.
    Exhausted,
}

/// Take one item off the frontier and expand it.
///
/// One entry rather than the whole crawl, because the app runs this while you
/// browse: the caller takes the client for one step and gives it back.
pub async fn step(
    conn: &mut SqliteConnection,
    client: &mut QobuzClient,
    max_tracks: i64,
    max_distance: i64,
) -> Result<StepResult> {
    if track_count(conn)? >= max_tracks {
        return Ok(StepResult::BudgetReached);
    }

    let Some(item) = next_batch(conn, 1)?.into_iter().next() else {
        return Ok(StepResult::Exhausted);
    };
    let (kind, ref_id) = (item.kind.clone(), item.ref_id.clone());

    let blocked = blocked_ids(conn)?;
    if item.priority > max_distance || is_blocked(conn, &kind, &ref_id, &blocked) {
        mark(conn, &kind, &ref_id, "skipped")?;
        return Ok(StepResult::Skipped { kind, ref_id });
    }

    // A dead id or a region-locked album must not stop the crawl.
    let outcome = match kind.as_str() {
        "artist" => match ref_id.parse::<i64>() {
            Ok(id) => expand_artist(conn, client, id, item.priority, max_distance)
                .await
                .map(|_| ()),
            Err(err) => Err(err).context("artist id"),
        },
        "album" => expand_album(conn, client, &ref_id, item.priority)
            .await
            .map(|_| ()),
        _ => Ok(()),
    };

    match outcome {
        Ok(()) => {
            mark(conn, &kind, &ref_id, "done")?;
            Ok(StepResult::Expanded { kind, ref_id })
        }
        Err(err) => {
            mark(conn, &kind, &ref_id, "failed")?;
            Ok(StepResult::Failed {
                kind,
                ref_id,
                error: format!("{err:#}"),
            })
        }
    }
}

impl Stats {
    /// Fold one step's outcome in. Returns false once the crawl should stop.
    pub fn absorb(&mut self, result: &StepResult) -> bool {
        match result {
            StepResult::Expanded { kind, .. } => {
                if kind == "artist" {
                    self.artists_expanded += 1;
                } else {
                    self.albums_expanded += 1;
                }
                true
            }
            StepResult::Skipped { .. } => {
                self.blocked_skipped += 1;
                true
            }
            StepResult::Failed { .. } => {
                self.errors += 1;
                true
            }
            StepResult::BudgetReached | StepResult::Exhausted => false,
        }
    }
}

/// What to report while a crawl runs. The CLI prints; the app updates a signal.
pub type Progress<'a> = &'a (dyn Fn(&Stats, i64) + Send + Sync);

/// Work the frontier until the track budget or the distance limit is reached.
pub async fn crawl(
    conn: &mut SqliteConnection,
    client: &mut QobuzClient,
    max_tracks: i64,
    max_distance: i64,
    progress: Option<Progress<'_>>,
) -> Result<Stats> {
    client.login().await?;
    let mut stats = Stats::default();
    let start_count = track_count(conn)?;
    let mut since_report = 0;

    loop {
        let result = step(conn, client, max_tracks, max_distance).await?;
        if let StepResult::Failed { kind, ref_id, error } = &result {
            eprintln!("  ! {kind} {ref_id}: {error}");
        }
        if !stats.absorb(&result) {
            break;
        }

        since_report += 1;
        if since_report >= 32 {
            since_report = 0;
            if let Some(report) = progress {
                report(&stats, track_count(conn)?);
            }
        }
    }

    stats.tracks_added = track_count(conn)? - start_count;
    Ok(stats)
}

/// Pull an artist's discography into the catalogue and queue their albums.
///
/// Deliberately does not fetch every tracklist: a prolific artist is hundreds
/// of albums, which at 2/s is minutes of a frozen button. Returns how many
/// albums were queued.
pub async fn discover_artist(
    conn: &mut SqliteConnection,
    client: &mut QobuzClient,
    artist_id: i64,
) -> Result<usize> {
    client.login().await?;

    let albums = client.artist_albums_raw(artist_id, 1000).await?;
    let mut queued = 0;
    for album in &albums {
        if let Some(album_id) = upsert_album(conn, album)? {
            enqueue(conn, "album", &album_id, 0)?;
            queued += 1;
        }
    }

    // Queue the artist too, so a later crawl still takes the similar-artist
    // hop from here.
    enqueue(conn, "artist", &artist_id.to_string(), 0)?;
    Ok(queued)
}

/// Crawl a single album's tracklist.
pub async fn crawl_one_album(
    conn: &mut SqliteConnection,
    client: &mut QobuzClient,
    album_id: &str,
) -> Result<usize> {
    client.login().await?;
    expand_album(conn, client, album_id, 0).await
}

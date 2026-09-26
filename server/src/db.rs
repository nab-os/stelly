//! The database, from the side that writes it: the schema, the block list,
//! and the helpers every stage shares. `schema.sql` at the repo root is the
//! contract, and `schema` its typed mirror; the app only ever reads the slim
//! copy `catalog` builds from it.

use crate::schema::{albums, artists, blocked_artists, failures, features, frontier, layout, tracks};
use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::dsl::count;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, SingleValue};
use diesel::upsert::excluded;
use std::path::Path;
use std::time::Duration;
use two_khz::api::BlockedArtist;

/// The schema, embedded at compile time.
const SCHEMA: &str = include_str!("../../schema.sql");

#[diesel::declare_sql_function]
extern "SQL" {
    /// The first of two values that is not null. What the upserts use to keep
    /// a stored value when a sparser payload arrives.
    fn coalesce<ST: SingleValue>(first: Nullable<ST>, second: Nullable<ST>) -> Nullable<ST>;

    /// The smaller of two. SQLite's `MIN` is scalar when given two arguments.
    #[sql_name = "MIN"]
    fn least(a: BigInt, b: BigInt) -> BigInt;
}

/// Create any missing tables and indexes, after migrating an older layout.
/// Cheap and idempotent.
pub fn ensure_schema(conn: &mut SqliteConnection) -> Result<()> {
    migrate(conn)?;
    conn.batch_execute(SCHEMA).context("applying schema.sql")?;
    Ok(())
}

#[derive(QueryableByName)]
struct Found {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Bring an older database up to `schema.sql`.
///
/// The one migration so far: `features` as the Python pipeline wrote it, with
/// Essentia's descriptors and EffNet embeddings. None of it is comparable with
/// what the Rust analyser produces, and every row would be re-analysed anyway
/// for its extractor version, so the table is dropped and made again.
fn migrate(conn: &mut SqliteConnection) -> Result<()> {
    let python_features = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM pragma_table_info('features') WHERE name = 'essentia_json'",
    )
    .get_result::<Found>(conn)?
    .n
        > 0;
    if python_features {
        conn.batch_execute("DROP TABLE features")
            .context("dropping the Python-era features table")?;
    }
    Ok(())
}

/// Open the database, waiting up to `busy` for a lock another connection
/// holds. No schema: see `open_for_write`.
pub fn connect(db_path: &Path, busy: Duration) -> Result<SqliteConnection> {
    let url = db_path
        .to_str()
        .with_context(|| format!("{} is not a UTF-8 path", db_path.display()))?;
    let mut conn = SqliteConnection::establish(url)
        .with_context(|| format!("opening {}", db_path.display()))?;
    conn.batch_execute(&format!("PRAGMA busy_timeout = {}", busy.as_millis()))?;
    Ok(conn)
}

/// Open the database for writing, with the schema applied.
pub fn open_for_write(db_path: &Path) -> Result<SqliteConnection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // A stage may be writing while a request reads, or the other way round.
    let mut conn = connect(db_path, Duration::from_secs(30))?;
    ensure_schema(&mut conn)?;
    Ok(conn)
}

/// The filter every stage applies to tracks, so the definition of "blocked"
/// cannot drift between the crawl, the analyser and the space.
#[diesel::dsl::auto_type]
pub fn not_blocked() -> _ {
    let blocked = blocked_artists::table.select(blocked_artists::artist_id.nullable());
    tracks::artist_id.is_null().or(tracks::artist_id.ne_all(blocked))
}

// ------------------------------------------------------------ writing back
//
// The block list. Hiding filters rather than deletes, so a block takes effect
// at once and can be lifted; see `purge` for the destructive version.

/// Hide an artist everywhere, and drop them from the frontier so an in-flight
/// crawl stops expanding them.
pub fn block_artist(
    db_path: &Path,
    artist_id: i64,
    name: &str,
    reason: Option<&str>,
) -> Result<()> {
    let conn = &mut connect(db_path, Duration::from_secs(5))?;

    diesel::insert_into(blocked_artists::table)
        .values((
            blocked_artists::artist_id.eq(artist_id),
            blocked_artists::name.eq(name),
            blocked_artists::reason.eq(reason),
            blocked_artists::blocked_at.eq(utc_now()),
        ))
        .on_conflict(blocked_artists::artist_id)
        .do_update()
        .set((
            blocked_artists::name.eq(coalesce(excluded(blocked_artists::name), blocked_artists::name)),
            blocked_artists::reason.eq(coalesce(excluded(blocked_artists::reason), blocked_artists::reason)),
        ))
        .execute(conn)
        .with_context(|| format!("blocking artist {artist_id}"))?;

    diesel::delete(
        frontier::table
            .filter(frontier::kind.eq("artist"))
            .filter(frontier::ref_id.eq(artist_id.to_string())),
    )
    .execute(conn)?;

    Ok(())
}

pub fn unblock_artist(db_path: &Path, artist_id: i64) -> Result<()> {
    let conn = &mut connect(db_path, Duration::from_secs(5))?;
    diesel::delete(blocked_artists::table.find(artist_id)).execute(conn)?;
    Ok(())
}

pub fn blocked_artists(db_path: &Path) -> Result<Vec<BlockedArtist>> {
    let conn = &mut connect(db_path, Duration::from_secs(5))?;
    let mut rows: Vec<BlockedArtist> = blocked_artists::table
        .select((blocked_artists::artist_id, blocked_artists::name, blocked_artists::reason))
        .load::<(i64, Option<String>, Option<String>)>(conn)?
        .into_iter()
        .map(|(artist_id, name, reason)| BlockedArtist {
            artist_id,
            name: name.unwrap_or_default(),
            reason,
        })
        .collect();
    rows.sort_by_cached_key(|b| b.name.to_lowercase());
    Ok(rows)
}

/// An artist as the command line lists them: id, name, stored tracks.
#[derive(Debug, Clone, Queryable)]
pub struct ArtistMatch {
    pub id: i64,
    pub name: String,
    pub tracks: i64,
}

/// Candidate artists for an id or a name fragment. Every match, not a guess:
/// Qobuz sometimes files one person under several ids, and all of them need
/// blocking.
pub fn resolve_artist(conn: &mut SqliteConnection, needle: &str) -> Result<Vec<ArtistMatch>> {
    let tracks_of = || count(tracks::id.nullable());
    let with_counts = || {
        artists::table
            .left_join(tracks::table.on(tracks::artist_id.eq(artists::id.nullable())))
            .group_by((artists::id, artists::name))
            .select((artists::id, artists::name, tracks_of()))
            .into_boxed()
    };
    if let Ok(id) = needle.parse::<i64>() {
        let found = with_counts().filter(artists::id.eq(id)).load(conn)?;
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(with_counts()
        .filter(artists::name.like(format!("%{needle}%")))
        .order((tracks_of().desc(), artists::name))
        .limit(25)
        .load(conn)?)
}

/// What `purge` removed.
#[derive(Debug, Default)]
pub struct Purged {
    pub tracks: usize,
    pub features: usize,
    pub albums: usize,
}

/// Delete a blocked artist's stored data outright, rather than hiding it. The
/// space keeps its own copy of every vector, so `build-space` and `layout`
/// still need re-running for the map to forget them.
///
/// One transaction: a purge interrupted halfway would leave features for
/// tracks that no longer exist.
pub fn purge(conn: &mut SqliteConnection, artist_id: i64) -> Result<Purged> {
    conn.transaction(|conn| {
        let owned = || tracks::table.filter(tracks::artist_id.eq(artist_id)).select(tracks::id);
        let features =
            diesel::delete(features::table.filter(features::track_id.eq_any(owned()))).execute(conn)?;
        diesel::delete(layout::table.filter(layout::track_id.eq_any(owned()))).execute(conn)?;
        diesel::delete(failures::table.filter(failures::track_id.eq_any(owned()))).execute(conn)?;
        let tracks = diesel::delete(tracks::table.filter(tracks::artist_id.eq(artist_id))).execute(conn)?;
        let albums = diesel::delete(albums::table.filter(albums::artist_id.eq(artist_id))).execute(conn)?;
        diesel::delete(
            frontier::table
                .filter(frontier::kind.eq("artist"))
                .filter(frontier::ref_id.eq(artist_id.to_string())),
        )
        .execute(conn)?;
        Ok(Purged {
            tracks,
            features,
            albums,
        })
    })
}

pub fn utc_now() -> String {
    // UTC, seconds precision: sorts as text.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Cheaper than pulling in chrono
/// for the one timestamp this crate writes.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::analyse;
    use crate::pipeline::descriptors::Descriptors;
    use crate::{catalog, crawl, stages};
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("two-khz-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn analysed(conn: &mut SqliteConnection, track: i64) {
        let descriptors = Descriptors {
            bpm: Some(120.0),
            key: Some("A".into()),
            ..Default::default()
        };
        analyse::store(conn, track, &descriptors, &[0.5; 512]).unwrap();
    }

    /// The queries every stage leans on, end to end on a scratch database:
    /// upserts, the frontier, blocking, purging and the slim catalogue.
    #[test]
    fn catalogue_round_trip() {
        let dir = scratch("round-trip");
        let db_path = dir.join("two_khz.db");
        let conn = &mut open_for_write(&db_path).unwrap();

        let album = json!({
            "id": "a1", "title": "A", "genre": {"name": "Jazz"},
            "artist": {"id": 7, "name": "Seven"},
        });
        let full = json!({
            "id": 1, "title": "One", "isrc": "X", "duration": 200,
            "album": album, "performer": {"id": 7, "name": "Seven"},
        });
        crawl::upsert_track(conn, &full, None, None, 2).unwrap();
        // Sparser and closer, then fuller but further: stored values survive
        // a payload without them, and the shortest distance wins.
        crawl::upsert_track(conn, &json!({"id": 1, "title": "One"}), None, None, 1).unwrap();
        crawl::upsert_track(conn, &full, None, None, 3).unwrap();
        crawl::upsert_track(
            conn,
            &json!({"id": 2, "title": "Two", "performer": {"id": 8, "name": "Eight"}}),
            None,
            None,
            0,
        )
        .unwrap();

        let one: (Option<String>, Option<String>, Option<i64>, Option<i64>, i64) = tracks::table
            .find(1)
            .select((
                tracks::isrc,
                tracks::album_id,
                tracks::artist_id,
                tracks::duration,
                tracks::seed_distance,
            ))
            .first(conn)
            .unwrap();
        assert_eq!(one, (Some("X".into()), Some("a1".into()), Some(7), Some(200), 1));
        let genre: Option<String> = albums::table.find("a1").select(albums::genre).first(conn).unwrap();
        assert_eq!(genre.as_deref(), Some("Jazz"));

        crawl::enqueue(conn, "artist", "7", 2).unwrap();
        crawl::enqueue(conn, "artist", "7", 1).unwrap();
        crawl::enqueue(conn, "artist", "7", 3).unwrap();
        let priority: i64 = frontier::table
            .find(("artist", "7"))
            .select(frontier::priority)
            .first(conn)
            .unwrap();
        assert_eq!(priority, 1);

        assert_eq!(analyse::pending_count(conn).unwrap(), 2);

        let found = resolve_artist(conn, "sev").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].id, found[0].tracks), (7, 1));
        assert_eq!(resolve_artist(conn, "7").unwrap()[0].name, "Seven");
        assert!(resolve_artist(conn, "nobody").unwrap().is_empty());

        block_artist(&db_path, 7, "Seven", Some("too loud")).unwrap();
        block_artist(&db_path, 7, "Seven", None).unwrap();
        let blocked = blocked_artists(&db_path).unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].reason.as_deref(), Some("too loud"));
        let queued: i64 = frontier::table.count().get_result(conn).unwrap();
        assert_eq!(queued, 0, "blocking drops the artist from the frontier");

        // Hidden, not gone: only track 2 is still due, and only it is
        // buildable once both are analysed.
        assert_eq!(analyse::pending_count(conn).unwrap(), 1);
        analysed(conn, 1);
        analysed(conn, 2);
        assert_eq!(analyse::pending_count(conn).unwrap(), 0);
        let corpus = stages::corpus(&db_path).unwrap();
        assert_eq!((corpus.tracks, corpus.analysed, corpus.buildable), (2, 2, 1));

        let purged = purge(conn, 7).unwrap();
        assert_eq!((purged.tracks, purged.features, purged.albums), (1, 1, 1));

        let target = dir.join("catalog.db");
        catalog::build(&db_path, &target).unwrap();
        let slim = &mut connect(&target, Duration::from_secs(5)).unwrap();
        let json: Option<String> = features::table
            .find(2)
            .select(features::descriptors_json)
            .first(slim)
            .unwrap();
        assert_eq!(json.as_deref(), Some(r#"{"bpm":120.0}"#));
        let qobuz: Option<String> = tracks::table.find(2).select(tracks::qobuz_json).first(slim).unwrap();
        assert_eq!(qobuz, None, "the slim copy leaves the payloads behind");

        // And the client reads it back, in the space's row order.
        let loaded = two_khz::db::Catalog::load(&target, &[99, 2]).unwrap();
        assert_eq!(loaded.get(0).title, "<missing 99>");
        let two = loaded.get(1);
        assert_eq!((two.title.as_str(), two.artist.as_str(), two.bpm), ("Two", "Eight", Some(120.0)));
        assert_eq!(loaded.clap_dims, 512);
        assert!(loaded.blocked_artists.contains(&7));

        unblock_artist(&db_path, 7).unwrap();
        assert!(blocked_artists(&db_path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

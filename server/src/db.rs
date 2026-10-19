//! The database, from the side that writes it: the schema, the block list,
//! and the helpers every stage shares. `schema.sql` at the repo root is the
//! contract, and `schema` its typed mirror; `stelly_core::db` reads it back
//! when the space is loaded.

use crate::schema::{albums, artists, blocked_artists, failures, features, frontier, layout, tracks};
use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::dsl::count;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, SingleValue};
use diesel::upsert::excluded;
use std::path::Path;
use std::time::Duration;
use stelly_core::api::BlockedArtist;

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
/// `features` as the Python pipeline wrote it, with Essentia's descriptors
/// and EffNet embeddings: none of it is comparable with what the Rust
/// analyser produces, and every row would be re-analysed anyway for its
/// extractor version, so the table is dropped and made again.
///
/// The catalogue from before providers, which kept each item as Qobuz
/// described it in `qobuz_json`: that is the provider's own shape, so it
/// becomes `legacy_json`, for `import` to convert, beside an empty
/// `meta_json`. Albums gain their cover.
fn migrate(conn: &mut SqliteConnection) -> Result<()> {
    if has_column(conn, "features", "essentia_json")? {
        conn.batch_execute("DROP TABLE features")
            .context("dropping the Python-era features table")?;
    }
    for table in ["artists", "albums", "tracks"] {
        if has_column(conn, table, "qobuz_json")? {
            conn.batch_execute(&format!(
                "ALTER TABLE {table} RENAME COLUMN qobuz_json TO legacy_json;
                 ALTER TABLE {table} ADD COLUMN meta_json TEXT;"
            ))
            .with_context(|| format!("moving {table}'s stored items aside for import"))?;
        }
    }
    if has_column(conn, "albums", "title")? && !has_column(conn, "albums", "image")? {
        conn.batch_execute("ALTER TABLE albums ADD COLUMN image TEXT")?;
    }
    Ok(())
}

/// Whether `table` exists and has `column`.
pub fn has_column(conn: &mut SqliteConnection, table: &str, column: &str) -> Result<bool> {
    let found = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM pragma_table_info('{table}') WHERE name = '{column}'"
    ))
    .get_result::<Found>(conn)?;
    Ok(found.n > 0)
}

/// Open the database, waiting up to `busy` for a lock another connection
/// holds. No schema: see `open_for_write`.
pub fn connect(db_path: &Path, busy: Duration) -> Result<SqliteConnection> {
    let url = db_path
        .to_str()
        .with_context(|| format!("{} is not a UTF-8 path", db_path.display()))?;
    let mut conn = SqliteConnection::establish(url)
        .with_context(|| format!("opening {}", db_path.display()))?;
    // WAL's own advice: a commit waits for no disk sync, only a checkpoint
    // does. The database stays whole whatever happens; a power cut can lose
    // the last few commits, not corrupt it. A write a request makes, a new
    // provider item's id, no longer waits on the disk.
    conn.batch_execute(&format!("PRAGMA busy_timeout = {}; PRAGMA synchronous = NORMAL;", busy.as_millis()))?;
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

/// Whether the pipeline has already been at an album: one of its tracks
/// analysed, or given up on. Adding it again would only rebuild the space
/// for nothing.
pub fn album_known(conn: &mut SqliteConnection, album_id: &str) -> Result<bool> {
    let owned = || tracks::table.filter(tracks::album_id.eq(album_id)).select(tracks::id);
    let analysed = diesel::select(diesel::dsl::exists(
        features::table.filter(features::track_id.eq_any(owned())),
    ))
    .get_result::<bool>(conn)?;
    let failed = diesel::select(diesel::dsl::exists(
        failures::table.filter(failures::track_id.eq_any(owned())),
    ))
    .get_result::<bool>(conn)?;
    Ok(analysed || failed)
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
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    utc_at(seconds)
}

/// UTC, seconds precision: sorts as text.
pub fn utc_at(seconds: u64) -> String {
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
    use crate::provider::{RemoteAlbum, RemoteTrack};
    use crate::{crawl, stages};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stelly-db-{name}-{}", std::process::id()));
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

    /// A database from before providers keeps what it stored, aside, for
    /// `import`; likes included, whose old column could not be empty.
    #[test]
    fn the_catalogue_from_before_providers_is_kept_for_import() {
        use crate::auth::AuthStore;
        use diesel::connection::SimpleConnection;

        let dir = scratch("before-providers");
        let db_path = dir.join("stelly.db");
        {
            let conn = &mut connect(&db_path, Duration::from_secs(1)).unwrap();
            conn.batch_execute(
                "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT NOT NULL, qobuz_json TEXT, similar_fetched_at TEXT);
                 CREATE TABLE albums (id TEXT PRIMARY KEY, artist_id INTEGER, title TEXT NOT NULL, release_date TEXT,
                                      label TEXT, genre TEXT, qobuz_json TEXT);
                 CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id TEXT, artist_id INTEGER, title TEXT NOT NULL,
                                      duration INTEGER, isrc TEXT, qobuz_json TEXT, seed_distance INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO albums (id, title, qobuz_json) VALUES ('a1', 'A', '{\"id\":\"a1\"}');
                 CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
                 INSERT INTO users (name, created_at) VALUES ('sam', 'now');
                 CREATE TABLE likes (user_id INTEGER NOT NULL REFERENCES users(id), kind TEXT NOT NULL, ref_id TEXT NOT NULL,
                                     qobuz_json TEXT NOT NULL, liked_at INTEGER NOT NULL, PRIMARY KEY (user_id, kind, ref_id));
                 INSERT INTO likes VALUES (1, 'track', '5', '{\"id\":5}', 10);",
            )
            .unwrap();
        }
        let conn = &mut open_for_write(&db_path).unwrap();
        AuthStore::new(&db_path).unwrap();

        let (legacy, meta, image): (Option<String>, Option<String>, Option<String>) = albums::table
            .find("a1")
            .select((albums::legacy_json, albums::meta_json, albums::image))
            .first(conn)
            .unwrap();
        assert_eq!((legacy.as_deref(), meta, image), (Some("{\"id\":\"a1\"}"), None, None));
        assert!(crate::legacy::pending(conn).unwrap());
        // Not listed until imported, and new likes need no legacy at all.
        let listed: Vec<(crate::provider::RemoteTrack, i64)> = crate::likes::list(&db_path, 1, "track", 10).unwrap();
        assert!(listed.is_empty());
        let track = crate::likes::Liked::Track(RemoteTrack { id: 6, ..Default::default() });
        crate::likes::record(&db_path, 1, "6", &track, 20).unwrap();
        assert_eq!(crate::likes::list::<RemoteTrack>(&db_path, 1, "track", 10).unwrap().len(), 1);

        // And all of it again is nothing.
        ensure_schema(conn).unwrap();
        AuthStore::new(&db_path).unwrap();
    }

    /// Catalogued is not enough to skip an album: only analysed or given up on.
    #[test]
    fn an_album_is_known_once_analysed_or_failed() {
        let dir = scratch("album-known");
        let conn = &mut open_for_write(&dir.join("stelly.db")).unwrap();
        let track = |id, album: &str| RemoteTrack {
            id,
            title: "T".into(),
            album: "A".into(),
            album_id: Some(album.into()),
            ..Default::default()
        };
        crawl::upsert_track(conn, &track(1, "fresh"), None, 0).unwrap();
        crawl::upsert_track(conn, &track(2, "done"), None, 0).unwrap();
        crawl::upsert_track(conn, &track(3, "broken"), None, 0).unwrap();
        analysed(conn, 2);
        diesel::insert_into(failures::table)
            .values((failures::track_id.eq(3), failures::stage.eq("analyse")))
            .execute(conn)
            .unwrap();

        assert!(!album_known(conn, "fresh").unwrap());
        assert!(!album_known(conn, "unseen").unwrap());
        assert!(album_known(conn, "done").unwrap());
        assert!(album_known(conn, "broken").unwrap());
    }

    /// The queries every stage leans on, end to end on a scratch database:
    /// upserts, the frontier, blocking, purging and the catalogue the space loads.
    #[test]
    fn catalogue_round_trip() {
        let dir = scratch("round-trip");
        let db_path = dir.join("stelly.db");
        let conn = &mut open_for_write(&db_path).unwrap();

        let album = RemoteAlbum {
            id: "a1".into(),
            title: "A".into(),
            genre: Some("Jazz".into()),
            artist: "Seven".into(),
            artist_id: Some(7),
            image: Some("https://covers/a1.jpg".into()),
            ..Default::default()
        };
        crawl::upsert_album(conn, &album, true).unwrap();
        let full = RemoteTrack {
            id: 1,
            title: "One".into(),
            isrc: Some("X".into()),
            duration: Some(200),
            album: "A".into(),
            album_id: Some("a1".into()),
            artist: "Seven".into(),
            artist_id: Some(7),
            ..Default::default()
        };
        crawl::upsert_track(conn, &full, None, 2).unwrap();
        // Sparser and closer, then fuller but further: stored values survive
        // a description without them, and the shortest distance wins.
        let sparse = RemoteTrack { id: 1, title: "One".into(), ..Default::default() };
        crawl::upsert_track(conn, &sparse, None, 1).unwrap();
        crawl::upsert_track(conn, &full, None, 3).unwrap();
        let two = RemoteTrack {
            id: 2,
            title: "Two".into(),
            artist: "Eight".into(),
            artist_id: Some(8),
            ..Default::default()
        };
        crawl::upsert_track(conn, &two, None, 0).unwrap();

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
        // A track's stub of its album keeps what the whole album said.
        let (genre, image): (Option<String>, Option<String>) =
            albums::table.find("a1").select((albums::genre, albums::image)).first(conn).unwrap();
        assert_eq!(genre.as_deref(), Some("Jazz"));
        assert_eq!(image.as_deref(), Some("https://covers/a1.jpg"));

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

        // And the space reads it back, in its own row order.
        let loaded = stelly_core::db::Catalog::load(&db_path, &[99, 2]).unwrap();
        assert_eq!(loaded.get(0).title, "<missing 99>");
        let two = loaded.get(1);
        assert_eq!((two.title.as_str(), two.artist.as_str(), two.bpm), ("Two", "Eight", Some(120.0)));
        assert_eq!(loaded.clap_dims, 512);
        assert!(loaded.blocked_artists().contains(&7));

        unblock_artist(&db_path, 7).unwrap();
        assert!(blocked_artists(&db_path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

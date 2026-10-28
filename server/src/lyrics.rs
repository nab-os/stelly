//! The language a track is sung in, from its lyrics.
//!
//! LRCLIB, an open lyrics database, is asked for each track of the space by
//! its title, artist, album and duration, and `whatlang` tells the language
//! of what comes back. Only that is kept, never the lyrics. LRCLIB also says
//! when a track is instrumental, which is worth as much: such a track has no
//! language, rather than an unknown one.
//!
//! One request a second, the space's own tracks before the rest, and longer
//! pauses whenever LRCLIB says it is busy, which it often is: a whole library
//! takes days, and nobody waits on it. A track LRCLIB knew nothing of is asked
//! again after `RECHECK_DAYS`, by when someone may have added it.
//!
//! So a library is better started from a dump of LRCLIB's database, `import`,
//! which matches every track the way `/api/get` would, in minutes, and leaves
//! the requests to the tracks analysed since.

use crate::db;
use crate::pipeline::Job;
use anyhow::{bail, ensure, Context, Result};
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Double, Nullable, Text};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use stelly_core::api::Lyrics;
use stelly_core::logbuffer::LogBuffer;

const API: &str = "https://lrclib.net/api/get";
/// Between two requests.
const PACE: Duration = Duration::from_secs(1);
/// After a busy answer that says not how long to wait, doubling up to
/// `BUSY_MAX` while it stays busy.
const BUSY_FIRST: Duration = Duration::from_secs(30);
const BUSY_MAX: Duration = Duration::from_secs(30 * 60);
/// Waiting on an unreachable LRCLIB, or on new tracks once every one is done.
const IDLE: Duration = Duration::from_secs(6 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(30);
/// Tracks read from the database at a time.
const BATCH: i64 = 200;
const RECHECK_DAYS: u64 = 90;

/// The statuses a row is stored with.
const SUNG: &str = "lyrics";
const INSTRUMENTAL: &str = "instrumental";
const NONE: &str = "none";

/// Look up every track that needs it, for as long as the server runs.
pub async fn watch(db_path: PathBuf, log: Arc<LogBuffer>) {
    let client = match reqwest::Client::builder()
        .user_agent(concat!("Stelly/", env!("CARGO_PKG_VERSION"), " (https://github.com/nab-os/stelly-releases)"))
        .timeout(TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => return log.push(format!("lyrics: no HTTP client: {err:#}")),
    };
    // Behind the start's own work.
    tokio::time::sleep(Duration::from_secs(120)).await;

    let mut busy = BUSY_FIRST;
    loop {
        let path = db_path.clone();
        let batch = match tokio::task::spawn_blocking(move || pending(&path)).await {
            Ok(Ok(batch)) => batch,
            Ok(Err(err)) => {
                log.push(format!("lyrics: reading the tracks to look up failed: {err:#}"));
                tokio::time::sleep(IDLE).await;
                continue;
            }
            Err(_) => return,
        };
        if batch.is_empty() {
            tokio::time::sleep(IDLE).await;
            continue;
        }

        let mut found = 0;
        for track in &batch {
            let outcome = loop {
                match lookup(&client, track).await {
                    Ok(outcome) => {
                        busy = BUSY_FIRST;
                        break Some(outcome);
                    }
                    Err(Lookup::Busy(Some(after))) => tokio::time::sleep(jittered(after)).await,
                    Err(Lookup::Busy(None)) => {
                        tokio::time::sleep(jittered(busy)).await;
                        busy = (busy * 2).min(BUSY_MAX);
                    }
                    Err(Lookup::Failed(err)) => {
                        log.push(format!("lyrics: LRCLIB unreachable, trying again later: {err:#}"));
                        break None;
                    }
                }
            };
            let Some(outcome) = outcome else {
                tokio::time::sleep(IDLE).await;
                break;
            };
            found += usize::from(outcome.status == SUNG);
            let path = db_path.clone();
            let id = track.id;
            let stored = move || store(&mut db::open_for_write(&path)?, id, &outcome);
            if let Ok(Err(err)) = tokio::task::spawn_blocking(stored).await {
                log.push(format!("lyrics: storing track {id} failed: {err:#}"));
            }
            tokio::time::sleep(PACE).await;
        }
        log.push(format!("lyrics: {} tracks looked up, {found} with lyrics", batch.len()));
    }
}

/// Several answers at once, as LRCLIB asks, so that clients turned away
/// together do not all come back together: up to 30% longer.
fn jittered(wait: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    wait + wait.mul_f64(f64::from(nanos % 300) / 1000.0)
}

#[derive(QueryableByName)]
struct Pending {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Text)]
    artist: String,
    #[diesel(sql_type = Nullable<Text>)]
    album: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    duration: Option<i64>,
}

/// The tracks a lookup is for: analysed ones, with an artist to ask by.
const TO_LOOK_UP: &str = "
    SELECT t.id, t.title, a.name AS artist, al.title AS album, NULLIF(t.duration, 0) AS duration
    FROM tracks t
    JOIN features f ON f.track_id = t.id AND f.clap_f32 IS NOT NULL
    JOIN artists a ON a.id = t.artist_id
    LEFT JOIN albums al ON al.id = t.album_id
    LEFT JOIN lyrics l ON l.track_id = t.id";

/// The next tracks to look up: never looked up, or found nothing a while
/// ago. The space's, the user's own library first.
fn pending(db_path: &Path) -> Result<Vec<Pending>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let before = db::utc_at(now.saturating_sub(RECHECK_DAYS * 86_400));
    let mut conn = db::open_for_write(db_path)?;
    Ok(diesel::sql_query(format!(
        "{TO_LOOK_UP}
         WHERE l.track_id IS NULL OR (l.status = 'none' AND l.checked_at < ?)
         ORDER BY t.seed_distance, t.id
         LIMIT ?"
    ))
    .bind::<Text, _>(before)
    .bind::<BigInt, _>(BATCH)
    .load(&mut conn)?)
}

struct Outcome {
    status: &'static str,
    language: Option<&'static str>,
    confidence: Option<f64>,
    reliable: bool,
}

enum Lookup {
    /// Asked to come back later, after this long when it says.
    Busy(Option<Duration>),
    Failed(anyhow::Error),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Found {
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
}

async fn lookup(client: &reqwest::Client, track: &Pending) -> Result<Outcome, Lookup> {
    // The album and the duration narrow the match, when there are any.
    let mut query = vec![("track_name", track.title.clone()), ("artist_name", track.artist.clone())];
    if let Some(album) = &track.album {
        query.push(("album_name", album.clone()));
    }
    if let Some(duration) = track.duration {
        query.push(("duration", duration.to_string()));
    }
    let response = client.get(API).query(&query).send().await.map_err(|err| Lookup::Failed(err.into()))?;
    let status = response.status();
    if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
        // Which LRCLIB asks to be honoured, on pain of a ban.
        let after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(Lookup::Busy(after));
    }
    if status.is_server_error() {
        return Err(Lookup::Failed(anyhow::anyhow!("LRCLIB answered {status}")));
    }
    // Anything else it will answer the same next time: a track it has not,
    // or one it cannot take, is one it has nothing for.
    match response.json::<Found>().await {
        Ok(found) if status.is_success() => Ok(language_of(&found)),
        _ => Ok(Outcome { status: NONE, language: None, confidence: None, reliable: false }),
    }
}

fn language_of(found: &Found) -> Outcome {
    if found.instrumental {
        return Outcome { status: INSTRUMENTAL, language: None, confidence: None, reliable: false };
    }
    let lyrics = found.plain_lyrics.as_deref().unwrap_or("").trim();
    match whatlang::detect(lyrics).filter(|_| !lyrics.is_empty()) {
        Some(info) => Outcome {
            status: SUNG,
            language: Some(info.lang().code()),
            confidence: Some(info.confidence()),
            reliable: info.is_reliable(),
        },
        None => Outcome { status: NONE, language: None, confidence: None, reliable: false },
    }
}

fn store(conn: &mut SqliteConnection, id: i64, outcome: &Outcome) -> Result<()> {
    diesel::sql_query(
        "INSERT INTO lyrics (track_id, status, language, confidence, reliable, checked_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(track_id) DO UPDATE SET
           status = excluded.status, language = excluded.language,
           confidence = excluded.confidence, reliable = excluded.reliable,
           checked_at = excluded.checked_at",
    )
    .bind::<BigInt, _>(id)
    .bind::<Text, _>(outcome.status)
    .bind::<Nullable<Text>, _>(outcome.language)
    .bind::<Nullable<Double>, _>(outcome.confidence)
    .bind::<BigInt, _>(i64::from(outcome.reliable))
    .bind::<Text, _>(db::utc_now())
    .execute(conn)
    .context("writing the lyrics row")?;
    Ok(())
}

#[derive(QueryableByName)]
struct Stored {
    #[diesel(sql_type = BigInt)]
    track_id: i64,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Nullable<Text>)]
    language: Option<String>,
    #[diesel(sql_type = BigInt)]
    reliable: i64,
}

/// What is known of a track's lyrics.
pub fn of(db_path: &Path, id: i64) -> Result<Lyrics> {
    let mut conn = db::open_for_write(db_path)?;
    let row: Option<Stored> = diesel::sql_query(
        "SELECT track_id, status, language, reliable FROM lyrics WHERE track_id = ?",
    )
    .bind::<BigInt, _>(id)
    .get_result(&mut conn)
    .optional()?;
    Ok(match row {
        None => Lyrics::Unknown,
        Some(row) => match (row.status.as_str(), row.language) {
            (INSTRUMENTAL, _) => Lyrics::Instrumental,
            (SUNG, Some(code)) => {
                let name = whatlang::Lang::from_code(&code).map_or_else(|| code.clone(), |l| l.eng_name().to_string());
                Lyrics::Sung { language: code, name, reliable: row.reliable != 0 }
            }
            (SUNG | NONE, _) => Lyrics::None,
            (other, _) => bail!("unknown lyrics status {other:?}"),
        },
    })
}

/// Each track's language, where its lyrics told it reliably: what the
/// space's `language` block is made from.
pub fn languages(conn: &mut SqliteConnection) -> Result<HashMap<i64, String>> {
    let rows: Vec<Stored> = diesel::sql_query(
        "SELECT track_id, status, language, reliable FROM lyrics
         WHERE status = 'lyrics' AND reliable = 1 AND language IS NOT NULL",
    )
    .load(conn)?;
    Ok(rows.into_iter().filter_map(|r| Some((r.track_id, r.language?))).collect())
}

// ------------------------------------------------------------------ import

/// What `import` did.
#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    pub tracks: usize,
    pub sung: usize,
    pub instrumental: usize,
    /// Not in the dump, or in it without lyrics.
    pub none: usize,
}

/// Tracks written between two commits, so a running server's own writes are
/// held up for moments rather than for the whole import.
const IMPORT_CHUNK: usize = 1000;

/// Match every analysed track without a language yet against `dump`, a copy
/// of LRCLIB's database from https://lrclib.net/db-dumps: gzipped, as it is
/// downloaded, and then it is unpacked next to itself first, or already
/// unpacked. Tracks it has nothing for are stored as such, and asked of the
/// API again only after `RECHECK_DAYS`, so run it soon after the download.
pub fn import(db_path: &Path, dump: &Path, job: &Job) -> Result<Imported> {
    let dump = unpacked(dump, job)?;
    let url = format!("file:{}?mode=ro&immutable=1", dump.display());
    let mut lrclib = SqliteConnection::establish(&url).with_context(|| format!("opening {}", dump.display()))?;
    ensure!(
        diesel::sql_query("SELECT name_lower, artist_name_lower, duration, last_lyrics_id FROM tracks LIMIT 1")
            .execute(&mut lrclib)
            .is_ok(),
        "{} is not a dump of LRCLIB's database",
        dump.display()
    );

    let mut conn = db::open_for_write(db_path)?;
    let ours: Vec<Pending> = diesel::sql_query(format!(
        "{TO_LOOK_UP}
         WHERE l.track_id IS NULL OR l.status = 'none'
         ORDER BY t.seed_distance, t.id"
    ))
    .load(&mut conn)?;
    job.log(format!("lyrics: matching {} tracks against {}", ours.len(), dump.display()));

    let mut imported = Imported { tracks: ours.len(), ..Default::default() };
    for (done, chunk) in ours.chunks(IMPORT_CHUNK).enumerate() {
        if job.cancelled() {
            break;
        }
        let outcomes = chunk
            .iter()
            .map(|track| Ok((track.id, find(&mut lrclib, track)?.map_or_else(nothing, |found| language_of(&found)))))
            .collect::<Result<Vec<_>>>()?;
        conn.transaction(|conn| {
            for (id, outcome) in &outcomes {
                store(conn, *id, outcome)?;
            }
            anyhow::Ok(())
        })?;
        for (_, outcome) in &outcomes {
            match outcome.status {
                SUNG => imported.sung += 1,
                INSTRUMENTAL => imported.instrumental += 1,
                _ => imported.none += 1,
            }
        }
        let matched = (done * IMPORT_CHUNK + chunk.len()) as u64;
        job.advance("lyrics-import", matched, ours.len() as u64);
        if (done + 1) % 10 == 0 {
            job.log(format!("lyrics: {matched} of {} matched", ours.len()));
        }
    }
    Ok(imported)
}

fn nothing() -> Outcome {
    Outcome { status: NONE, language: None, confidence: None, reliable: false }
}

/// The dump, unpacked next to itself when it is gzipped, unless that was
/// done before. Written aside and renamed, so an interrupted run leaves no
/// half a database to be taken for a whole one.
fn unpacked(dump: &Path, job: &Job) -> Result<PathBuf> {
    let Some(name) = dump.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".gz")) else {
        return Ok(dump.to_path_buf());
    };
    let target = dump.with_file_name(name);
    if target.exists() {
        job.log(format!("lyrics: using {}, unpacked before", target.display()));
        return Ok(target);
    }
    let partial = dump.with_file_name(format!("{name}.partial"));
    let file = std::fs::File::open(dump).with_context(|| format!("opening {}", dump.display()))?;
    let total = file.metadata()?.len();
    let read = Counted { inner: file, count: 0 };
    let mut gz = flate2::read::MultiGzDecoder::new(std::io::BufReader::with_capacity(1 << 20, read));
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&partial)?);
    job.log(format!("lyrics: unpacking {} to {}", dump.display(), target.display()));

    let mut buffer = vec![0u8; 1 << 20];
    let mut logged = 0;
    loop {
        if job.cancelled() {
            bail!("unpacking stopped");
        }
        let n = std::io::Read::read(&mut gz, &mut buffer).context("unpacking the dump")?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut out, &buffer[..n]).context("writing the unpacked dump")?;
        let at = gz.get_ref().get_ref().count;
        job.advance("lyrics-unpack", at, total);
        if at - logged >= 1 << 30 {
            logged = at;
            job.log(format!("lyrics: unpacked {:.0}%", at as f64 * 100.0 / total.max(1) as f64));
        }
    }
    std::io::Write::flush(&mut out)?;
    drop(out);
    std::fs::rename(&partial, &target)?;
    Ok(target)
}

/// A reader that counts what it has read, for the unpacking's progress.
struct Counted<R> {
    inner: R,
    count: u64,
}

impl<R: std::io::Read> std::io::Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        Ok(n)
    }
}

#[derive(QueryableByName)]
struct Candidate {
    #[diesel(sql_type = Nullable<Bool>)]
    instrumental: Option<bool>,
    #[diesel(sql_type = Nullable<Text>)]
    plain_lyrics: Option<String>,
}

/// The dump's record for a track, as `/api/get` finds it: its title and
/// artist as LRCLIB writes them, and its duration within two seconds. Of
/// several, one on the same album, then one with lyrics or marked
/// instrumental. A title with a version after it, "(Remastered 2011)" or
/// " - Live", is tried bare after.
fn find(lrclib: &mut SqliteConnection, track: &Pending) -> Result<Option<Found>> {
    let artist = prepare(&track.artist);
    let album = track.album.as_deref().map(prepare).unwrap_or_default();
    let mut titles = vec![prepare(&track.title)];
    let bare = prepare(bare_title(&track.title));
    if !bare.is_empty() && bare != titles[0] {
        titles.push(bare);
    }
    for title in titles {
        let duration = track.duration.map_or(String::new(), |_| " AND t.duration BETWEEN ? AND ?".into());
        let query = diesel::sql_query(format!(
            "SELECT l.instrumental, l.plain_lyrics
             FROM tracks t LEFT JOIN lyrics l ON l.id = t.last_lyrics_id
             WHERE t.name_lower = ? AND t.artist_name_lower = ?{duration}
             ORDER BY t.album_name_lower = ? DESC,
                      (COALESCE(l.plain_lyrics, '') != '' OR COALESCE(l.instrumental, 0)) DESC,
                      t.id
             LIMIT 1"
        ))
        .bind::<Text, _>(title)
        .bind::<Text, _>(&artist)
        .into_boxed::<diesel::sqlite::Sqlite>();
        let query = match track.duration {
            Some(d) => query.bind::<Double, _>(d as f64 - 2.0).bind::<Double, _>(d as f64 + 2.0),
            None => query,
        };
        let found: Option<Candidate> = query.bind::<Text, _>(&album).get_result(lrclib).optional()?;
        if let Some(found) = found.filter(|f| f.instrumental == Some(true) || f.plain_lyrics.as_deref().is_some_and(|l| !l.trim().is_empty())) {
            return Ok(Some(Found { instrumental: found.instrumental == Some(true), plain_lyrics: found.plain_lyrics }));
        }
    }
    Ok(None)
}

/// A title without the version after it: "(Remastered 2011)", "[Live]",
/// " - Radio Edit".
fn bare_title(title: &str) -> &str {
    let cut = [" (", " [", " - "].iter().filter_map(|mark| title.find(mark)).min();
    cut.map_or(title, |at| &title[..at]).trim()
}

/// A name as LRCLIB keeps it to match on, its `prepare_input`: lowercase
/// without diacritics, punctuation as spaces, apostrophes dropped, spaces
/// collapsed.
pub fn prepare(input: &str) -> String {
    const TO_SPACE: &[char] = &[
        '`', '~', '!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '|', '+', '-', '=', '?', ';', ':', '"',
        ',', '.', '<', '>', '{', '}', '[', ']', '\\', '/', '\0', '\n',
    ];
    let lay: String = secular::lower_lay_string(input)
        .chars()
        .filter(|c| !matches!(c, '\'' | '\u{2019}'))
        .map(|c| if TO_SPACE.contains(&c) { ' ' } else { c })
        .collect();
    collapse::collapse(&lay.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::connection::SimpleConnection;

    fn found(instrumental: bool, lyrics: Option<&str>) -> Found {
        Found { instrumental, plain_lyrics: lyrics.map(str::to_string) }
    }

    #[test]
    fn lyrics_tell_their_language() {
        let outcome = language_of(&found(
            false,
            Some("J'suis dans les airs toutes les heures, je ne compte plus les jours\nOn a grandi dans la rue, on n'avait rien du tout"),
        ));
        assert_eq!(outcome.status, SUNG);
        assert_eq!(outcome.language, Some("fra"));
        assert!(outcome.reliable);
    }

    #[test]
    fn an_instrumental_has_no_language() {
        let outcome = language_of(&found(true, None));
        assert_eq!(outcome.status, INSTRUMENTAL);
        assert_eq!(outcome.language, None);
    }

    #[test]
    fn the_spaces_tracks_are_looked_up_once_and_read_back() {
        let dir = std::env::temp_dir().join(format!("stelly-lyrics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        let mut conn = db::open_for_write(&db_path).unwrap();
        conn.batch_execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Heuss L''enfoiré');
             INSERT INTO albums (id, artist_id, title) VALUES ('a', 1, 'En esprit');
             INSERT INTO tracks (id, album_id, artist_id, title, duration, seed_distance) VALUES
               (1, 'a', 1, 'Similar', 180, 1),
               (2, 'a', 1, 'Adios', 200, 0),
               (3, 'a', 1, 'Not analysed', 200, 0),
               (4, NULL, 1, 'No album', 200, 0);
             INSERT INTO features (track_id, extractor_version, clap_f32) VALUES
               (1, 'v', x'00'), (2, 'v', x'00'), (4, 'v', x'00');",
        )
        .unwrap();

        // Analysed, the user's own first; an album is not needed.
        let ids: Vec<i64> = pending(&db_path).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, [2, 4, 1]);

        let sung = Outcome { status: SUNG, language: Some("fra"), confidence: Some(0.9), reliable: true };
        store(&mut conn, 2, &sung).unwrap();
        store(&mut conn, 1, &language_of(&found(false, None))).unwrap();
        store(&mut conn, 4, &language_of(&found(true, None))).unwrap();
        assert!(pending(&db_path).unwrap().is_empty());

        assert_eq!(
            of(&db_path, 2).unwrap(),
            Lyrics::Sung { language: "fra".into(), name: "French".into(), reliable: true }
        );
        assert_eq!(of(&db_path, 1).unwrap(), Lyrics::None);
        assert_eq!(of(&db_path, 3).unwrap(), Lyrics::Unknown);
        assert_eq!(languages(&mut conn).unwrap(), HashMap::from([(2, "fra".to_string())]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_prepared_as_lrclib_prepares_them() {
        assert_eq!(prepare("Heuss L'enfoiré"), "heuss lenfoire");
        assert_eq!(prepare("  Don\u{2019}t  Stop Me Now "), "dont stop me now");
        assert_eq!(prepare("AC/DC"), "ac dc");
        assert_eq!(prepare("Bad Decisions (LOFI) [feat. Dahlia]"), "bad decisions lofi feat dahlia");
        assert_eq!(prepare("BEYONCÉ"), "beyonce");
    }

    #[test]
    fn a_version_comes_off_the_title() {
        assert_eq!(bare_title("Let It Be (Remastered 2009)"), "Let It Be");
        assert_eq!(bare_title("Song - Radio Edit"), "Song");
        assert_eq!(bare_title("Plain"), "Plain");
    }

    #[test]
    fn a_dump_is_unpacked_and_matched_as_the_api_would() {
        let dir = std::env::temp_dir().join(format!("stelly-lyrics-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // LRCLIB's own tables, as far as a match reads them.
        let plain = dir.join("dump.sqlite3");
        let mut dump = SqliteConnection::establish(plain.to_str().unwrap()).unwrap();
        dump.batch_execute(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, name_lower TEXT, artist_name_lower TEXT,
                                  album_name_lower TEXT, duration FLOAT, last_lyrics_id INTEGER);
             CREATE TABLE lyrics (id INTEGER PRIMARY KEY, plain_lyrics TEXT, instrumental BOOLEAN);
             INSERT INTO lyrics VALUES
               (1, 'J''suis dans les airs toutes les heures, je ne compte plus les jours\nOn a grandi dans la rue, on n''avait rien du tout', 0),
               (2, NULL, 1),
               (3, 'Everything you ever told me could have been a lie, and I would never know it', 0),
               (4, '', 0);
             INSERT INTO tracks VALUES
               (1, 'adios', 'heuss lenfoire', 'en esprit', 201.4, 1),
               (2, 'interlude', 'heuss lenfoire', 'en esprit', 90, 2),
               (3, 'expectation', 'tame impala', 'innerspeaker', 362, 3),
               (4, 'empty', 'heuss lenfoire', 'en esprit', 100, 4);",
        )
        .unwrap();
        drop(dump);
        let gz = dir.join("dump.sqlite3.gz");
        let mut encoder = flate2::write::GzEncoder::new(std::fs::File::create(&gz).unwrap(), flate2::Compression::fast());
        std::io::copy(&mut std::fs::File::open(&plain).unwrap(), &mut encoder).unwrap();
        encoder.finish().unwrap();
        std::fs::remove_file(&plain).unwrap();

        let db_path = dir.join("stelly.db");
        let mut conn = db::open_for_write(&db_path).unwrap();
        conn.batch_execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Heuss L''enfoiré'), (2, 'Tame Impala');
             INSERT INTO albums (id, artist_id, title) VALUES ('a', 1, 'En Esprit');
             INSERT INTO tracks (id, album_id, artist_id, title, duration) VALUES
               (1, 'a', 1, 'Adiós', 200),
               (2, 'a', 1, 'Interlude', 91),
               (3, NULL, 2, 'Expectation (Remastered)', 361),
               (4, 'a', 1, 'Empty', 100),
               (5, 'a', 1, 'Not in the dump', 120),
               (6, 'a', 1, 'Adios', 260);
             INSERT INTO features (track_id, extractor_version, clap_f32) VALUES
               (1, 'v', x'00'), (2, 'v', x'00'), (3, 'v', x'00'), (4, 'v', x'00'), (5, 'v', x'00'), (6, 'v', x'00');",
        )
        .unwrap();

        let imported = import(&db_path, &gz, &Job::stderr()).unwrap();
        assert_eq!(imported, Imported { tracks: 6, sung: 2, instrumental: 1, none: 3 });
        assert!(dir.join("dump.sqlite3").exists(), "unpacked next to the download");
        assert_eq!(of(&db_path, 1).unwrap(), Lyrics::Sung { language: "fra".into(), name: "French".into(), reliable: true });
        assert_eq!(of(&db_path, 2).unwrap(), Lyrics::Instrumental);
        // Only by its bare title, and without an album.
        assert!(matches!(of(&db_path, 3).unwrap(), Lyrics::Sung { ref language, .. } if language == "eng"));
        // Empty lyrics, not there, and a minute longer than the dump's.
        assert_eq!(of(&db_path, 4).unwrap(), Lyrics::None);
        assert_eq!(of(&db_path, 5).unwrap(), Lyrics::None);
        assert_eq!(of(&db_path, 6).unwrap(), Lyrics::None);

        // Again, from what was unpacked: only what found nothing is matched.
        let again = import(&db_path, &gz, &Job::stderr()).unwrap();
        assert_eq!(again.tracks, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_lyrics_are_none() {
        assert_eq!(language_of(&found(false, Some("  \n"))).status, NONE);
        assert_eq!(language_of(&found(false, None)).status, NONE);
    }
}

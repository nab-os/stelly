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

use crate::db;
use anyhow::{bail, Context, Result};
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Double, Nullable, Text};
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
/// After a busy answer, doubling up to `BUSY_MAX` while it stays busy.
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
                    Err(Lookup::Busy) => {
                        tokio::time::sleep(busy).await;
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
            if let Ok(Err(err)) = tokio::task::spawn_blocking(move || store(&path, id, &outcome)).await {
                log.push(format!("lyrics: storing track {id} failed: {err:#}"));
            }
            tokio::time::sleep(PACE).await;
        }
        log.push(format!("lyrics: {} tracks looked up, {found} with lyrics", batch.len()));
    }
}

#[derive(QueryableByName)]
struct Pending {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Text)]
    artist: String,
    #[diesel(sql_type = Text)]
    album: String,
    #[diesel(sql_type = BigInt)]
    duration: i64,
}

/// The next tracks to look up: never looked up, or found nothing a while
/// ago. The space's, the user's own library first. LRCLIB matches on all
/// four fields, so a track missing one is left out.
fn pending(db_path: &Path) -> Result<Vec<Pending>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let before = db::utc_at(now.saturating_sub(RECHECK_DAYS * 86_400));
    let mut conn = db::open_for_write(db_path)?;
    Ok(diesel::sql_query(
        "SELECT t.id, t.title, a.name AS artist, al.title AS album, t.duration
         FROM tracks t
         JOIN features f ON f.track_id = t.id AND f.clap_f32 IS NOT NULL
         JOIN artists a ON a.id = t.artist_id
         JOIN albums al ON al.id = t.album_id
         LEFT JOIN lyrics l ON l.track_id = t.id
         WHERE t.duration > 0
           AND (l.track_id IS NULL OR (l.status = 'none' AND l.checked_at < ?))
         ORDER BY t.seed_distance, t.id
         LIMIT ?",
    )
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
    /// Asked to come back later.
    Busy,
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
    let response = client
        .get(API)
        .query(&[
            ("track_name", track.title.as_str()),
            ("artist_name", track.artist.as_str()),
            ("album_name", track.album.as_str()),
            ("duration", &track.duration.to_string()),
        ])
        .send()
        .await
        .map_err(|err| Lookup::Failed(err.into()))?;
    let status = response.status();
    if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
        return Err(Lookup::Busy);
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

fn store(db_path: &Path, id: i64, outcome: &Outcome) -> Result<()> {
    let mut conn = db::open_for_write(db_path)?;
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
    .execute(&mut conn)
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

        // Analysed, with all four fields, the user's own first.
        let ids: Vec<i64> = pending(&db_path).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, [2, 1]);

        let sung = Outcome { status: SUNG, language: Some("fra"), confidence: Some(0.9), reliable: true };
        store(&db_path, 2, &sung).unwrap();
        store(&db_path, 1, &language_of(&found(false, None))).unwrap();
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
    fn empty_lyrics_are_none() {
        assert_eq!(language_of(&found(false, Some("  \n"))).status, NONE);
        assert_eq!(language_of(&found(false, None)).status, NONE);
    }
}

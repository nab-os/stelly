//! What an earlier Stelly stored in the provider's own shapes, converted.
//!
//! Before providers, the catalogue and the likes kept each item exactly as
//! Qobuz described it. The migration moved that aside as `legacy_json`;
//! this has the provider turn it into `stelly_core::catalogue`'s shapes,
//! which become `meta_json` (`item_json` for a like), and an album's cover.
//! The provider makes sense of its own JSON, so nothing here has to, and it
//! asks the catalogue behind it for nothing.
//!
//! Each row converted, or that the provider makes nothing of, loses its
//! `legacy_json`, so this resumes where it stopped; a start that finds
//! nothing left says so in `id_counters`, so it costs nothing once done. The
//! server runs it at start; `stelly-server convert-legacy` runs it by hand,
//! as `stelly-server convert-legacy`.

use crate::provider::{Provider, RemoteAlbum, RemoteArtist, RemoteTrack};
use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, Text};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use stelly_provider::Kind;

/// Rows sent to the provider at once: a few megabytes of JSON.
const BATCH: i64 = 200;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    pub converted: usize,
    /// Rows the provider made nothing of, given up on.
    pub dropped: usize,
}

#[derive(QueryableByName)]
struct Row {
    #[diesel(sql_type = BigInt)]
    rowid: i64,
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Text)]
    legacy: String,
}

/// Kept in `id_counters` once a start finds nothing left to convert. No
/// index covers `legacy_json`, so without it every start reads four whole
/// tables to learn there is nothing to do.
const CONVERTED: &str = "legacy-converted";

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn converted(conn: &mut SqliteConnection) -> Result<bool> {
    let marks = diesel::sql_query("SELECT COUNT(*) AS n FROM id_counters WHERE kind = ?")
        .bind::<Text, _>(CONVERTED)
        .get_result::<Count>(conn)?;
    Ok(marks.n > 0)
}

/// Whether anything is still waiting to be converted.
pub fn pending(conn: &mut SqliteConnection) -> Result<bool> {
    for table in ["likes", "artists", "albums", "tracks"] {
        if !crate::db::has_column(conn, table, "legacy_json")? {
            continue;
        }
        let rows = diesel::sql_query(format!("SELECT rowid AS rowid, '' AS kind, '' AS legacy FROM {table} WHERE legacy_json IS NOT NULL LIMIT 1"))
            .load::<Row>(conn)?;
        if !rows.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Convert everything still in `legacy_json`, telling `report` how it goes.
pub async fn run(db_path: &Path, provider: &Provider, report: &(dyn Fn(String) + Sync)) -> Result<Imported> {
    let mut conn = crate::db::open_for_write(db_path)?;
    // Likes are brought up to date with the users they belong to, which the
    // command line may not have touched yet.
    if crate::db::has_column(&mut conn, "likes", "qobuz_json")? {
        crate::likes::ensure_schema(&mut conn)?;
    }
    if converted(&mut conn)? {
        return Ok(Imported::default());
    }
    if !pending(&mut conn)? {
        // Nothing brings legacy rows back: the migration that made them
        // runs once, on a database older than this.
        diesel::sql_query("INSERT OR IGNORE INTO id_counters (kind, next) VALUES (?, 1)")
            .bind::<Text, _>(CONVERTED)
            .execute(&mut conn)?;
        return Ok(Imported::default());
    }
    // Only the provider that catalogue came from can make sense of it.
    provider
        .legacy_capabilities()
        .await
        .context("asking the music providers what they can do")?
        .context("no configured music provider can convert what an earlier Stelly stored; it stays as it is")?;

    let mut total = Imported::default();
    for (table, kind) in [("likes", None), ("artists", Some(Kind::Artist)), ("albums", Some(Kind::Album)), ("tracks", Some(Kind::Track))] {
        if !crate::db::has_column(&mut conn, table, "legacy_json")? {
            continue;
        }
        let done = table_pass(&mut conn, provider, table, kind).await?;
        if done.converted + done.dropped > 0 {
            report(format!("imported {table}: {} converted, {} dropped", done.converted, done.dropped));
        }
        total.converted += done.converted;
        total.dropped += done.dropped;
    }
    Ok(total)
}

async fn table_pass(conn: &mut SqliteConnection, provider: &Provider, table: &str, kind: Option<Kind>) -> Result<Imported> {
    let mut done = Imported::default();
    let kind_column = if kind.is_none() { "kind" } else { "''" };
    let mut after = 0i64;
    loop {
        let rows = diesel::sql_query(format!(
            "SELECT rowid AS rowid, {kind_column} AS kind, legacy_json AS legacy FROM {table}
             WHERE legacy_json IS NOT NULL AND rowid > ? ORDER BY rowid LIMIT {BATCH}"
        ))
        .bind::<BigInt, _>(after)
        .load::<Row>(conn)?;
        let Some(last) = rows.last() else { break };
        after = last.rowid;

        // A like's kind is its own; a whole table's is the table's.
        let kinds: Vec<Kind> = match kind {
            Some(kind) => vec![kind; rows.len()],
            None => rows.iter().map(|row| row.kind.parse().unwrap_or(Kind::Track)).collect(),
        };
        let mut converted: Vec<Option<String>> = vec![None; rows.len()];
        let mut images: Vec<Option<String>> = vec![None; rows.len()];
        for each in [Kind::Track, Kind::Album, Kind::Artist] {
            let at: Vec<usize> = (0..rows.len()).filter(|&i| kinds[i] == each).collect();
            if at.is_empty() {
                continue;
            }
            let items: Vec<Value> = at.iter().map(|&i| serde_json::from_str(&rows[i].legacy).unwrap_or(Value::Null)).collect();
            let found = match each {
                Kind::Track => fill::<RemoteTrack>(provider, each, &items, &at, &mut converted, |_| None).await?,
                Kind::Album => fill::<RemoteAlbum>(provider, each, &items, &at, &mut converted, |album| album.image.clone()).await?,
                Kind::Artist => fill::<RemoteArtist>(provider, each, &items, &at, &mut converted, |_| None).await?,
            };
            for (i, image) in found {
                images[i] = image;
            }
        }

        let target = if kind.is_none() { "item_json" } else { "meta_json" };
        conn.batch_execute("BEGIN")?;
        for (i, row) in rows.iter().enumerate() {
            diesel::sql_query(format!(
                "UPDATE {table} SET {target} = COALESCE({target}, ?), legacy_json = NULL WHERE rowid = ?"
            ))
            .bind::<Nullable<Text>, _>(&converted[i])
            .bind::<BigInt, _>(row.rowid)
            .execute(conn)?;
            if table == "albums" && images[i].is_some() {
                diesel::sql_query("UPDATE albums SET image = COALESCE(image, ?) WHERE rowid = ?")
                    .bind::<Nullable<Text>, _>(&images[i])
                    .bind::<BigInt, _>(row.rowid)
                    .execute(conn)?;
            }
            match converted[i] {
                Some(_) => done.converted += 1,
                None => done.dropped += 1,
            }
        }
        conn.batch_execute("COMMIT")?;
    }
    Ok(done)
}

/// The provider's conversion of `items`, the rows at `at`, into
/// `converted`; and for each, what `extra` takes from it.
async fn fill<T: DeserializeOwned + Serialize>(
    provider: &Provider,
    kind: Kind,
    items: &[Value],
    at: &[usize],
    converted: &mut [Option<String>],
    extra: impl Fn(&T) -> Option<String>,
) -> Result<Vec<(usize, Option<String>)>> {
    let answers: Vec<Option<T>> = provider.import(kind, items).await.context("converting stored items")?;
    anyhow::ensure!(answers.len() == items.len(), "the provider converted {} of {} items", answers.len(), items.len());
    let mut extras = Vec::with_capacity(at.len());
    for (&i, answer) in at.iter().zip(answers) {
        if let Some(item) = answer {
            converted[i] = serde_json::to_string(&item).ok();
            extras.push((i, extra(&item)));
        }
    }
    Ok(extras)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nothing_left_is_remembered() {
        let dir = std::env::temp_dir().join(format!("stelly-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        let conn = &mut crate::db::open_for_write(&db_path).unwrap();
        // No provider configured: never asked while nothing is pending.
        let provider = Provider::from_env(&dir, &db_path);

        assert!(!converted(conn).unwrap());
        assert_eq!(run(&db_path, &provider, &|_| {}).await.unwrap(), Imported::default());
        assert!(converted(conn).unwrap());

        // From then on the tables are not read at all.
        conn.batch_execute("INSERT INTO artists (id, name, legacy_json) VALUES (1, 'a', '{}')").unwrap();
        assert!(pending(conn).unwrap());
        assert_eq!(run(&db_path, &provider, &|_| {}).await.unwrap(), Imported::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

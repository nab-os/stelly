//! Likes: each person's own, kept here and nowhere else.
//!
//! The provider is never told, and an account's own favourites there are
//! not read: the two are separate lists. Nobody sees anyone else's likes.
//! What they are shared into is the crawl, which seeds from the whole
//! family's.
//!
//! Each like keeps the item as it was fetched, so listing them needs no
//! request, and the crawl can store it the way it stores anything else.

use crate::db::has_column;
use crate::provider::{RemoteAlbum, RemoteArtist, RemoteTrack};
use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::time::Duration;

// Created by `AuthStore::ensure_schema`, with the users it points at.
diesel::table! {
    likes (user_id, kind, ref_id) {
        user_id -> BigInt,
        kind -> Text,
        ref_id -> Text,
        item_json -> Nullable<Text>,
        legacy_json -> Nullable<Text>,
        liked_at -> BigInt,
    }
}

const TABLE: &str = "CREATE TABLE IF NOT EXISTS likes (
    user_id     INTEGER NOT NULL REFERENCES users(id),
    -- 'track' | 'album' | 'artist'
    kind        TEXT NOT NULL,
    ref_id      TEXT NOT NULL,
    -- The item, in `stelly_core::catalogue`'s shape. NULL for a like from
    -- before providers until `import` has converted its `legacy_json`.
    item_json   TEXT,
    legacy_json TEXT,
    -- Unix seconds.
    liked_at    INTEGER NOT NULL,
    PRIMARY KEY (user_id, kind, ref_id)
);";

const INDEX: &str = "CREATE INDEX IF NOT EXISTS likes_listing ON likes(user_id, kind, liked_at);";

/// The table, made or brought up to date. A table from before providers
/// kept Qobuz's objects in a `qobuz_json` that could not be empty: it is
/// made again, those objects kept as `legacy_json` for `import`.
pub fn ensure_schema(conn: &mut SqliteConnection) -> Result<()> {
    if has_column(conn, "likes", "qobuz_json")? {
        conn.batch_execute(&format!(
            "BEGIN;
             ALTER TABLE likes RENAME TO likes_before_providers;
             {TABLE}
             INSERT INTO likes (user_id, kind, ref_id, legacy_json, liked_at)
                 SELECT user_id, kind, ref_id, qobuz_json, liked_at FROM likes_before_providers;
             DROP TABLE likes_before_providers;
             COMMIT;"
        ))
        .context("moving likes' stored items aside for import")?;
    }
    conn.batch_execute(&format!("{TABLE}\n{INDEX}"))?;
    Ok(())
}

/// One liked item, whichever kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Liked {
    Track(RemoteTrack),
    Album(RemoteAlbum),
    Artist(RemoteArtist),
}

impl Liked {
    pub fn kind(&self) -> &'static str {
        match self {
            Liked::Track(_) => "track",
            Liked::Album(_) => "album",
            Liked::Artist(_) => "artist",
        }
    }

    fn parse(kind: &str, json: &str) -> Option<Self> {
        Some(match kind {
            "track" => Liked::Track(serde_json::from_str(json).ok()?),
            "album" => Liked::Album(serde_json::from_str(json).ok()?),
            "artist" => Liked::Artist(serde_json::from_str(json).ok()?),
            _ => return None,
        })
    }

    fn json(&self) -> String {
        match self {
            Liked::Track(track) => serde_json::to_string(track),
            Liked::Album(album) => serde_json::to_string(album),
            Liked::Artist(artist) => serde_json::to_string(artist),
        }
        .expect("an item serialises")
    }
}

fn open(db_path: &Path) -> Result<SqliteConnection> {
    crate::db::connect(db_path, Duration::from_secs(5))
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Liking twice keeps the first time.
pub fn record(db_path: &Path, user_id: i64, ref_id: &str, item: &Liked, liked_at: i64) -> Result<()> {
    diesel::insert_or_ignore_into(likes::table)
        .values((
            likes::user_id.eq(user_id),
            likes::kind.eq(item.kind()),
            likes::ref_id.eq(ref_id),
            likes::item_json.eq(item.json()),
            likes::liked_at.eq(liked_at),
        ))
        .execute(&mut open(db_path)?)?;
    Ok(())
}

pub fn forget(db_path: &Path, user_id: i64, kind: &str, ref_id: &str) -> Result<()> {
    diesel::delete(
        likes::table
            .filter(likes::user_id.eq(user_id))
            .filter(likes::kind.eq(kind))
            .filter(likes::ref_id.eq(ref_id)),
    )
    .execute(&mut open(db_path)?)?;
    Ok(())
}

/// One person's likes of one kind, newest first, with when each was liked.
pub fn list<T: DeserializeOwned>(db_path: &Path, user_id: i64, kind: &str, cap: usize) -> Result<Vec<(T, i64)>> {
    let rows = likes::table
        .filter(likes::user_id.eq(user_id))
        .filter(likes::kind.eq(kind))
        .filter(likes::item_json.is_not_null())
        .order(likes::liked_at.desc())
        .limit(cap as i64)
        .select((likes::item_json.assume_not_null(), likes::liked_at))
        .load::<(String, i64)>(&mut open(db_path)?)?;
    Ok(rows
        .into_iter()
        .filter_map(|(json, liked_at)| Some((serde_json::from_str(&json).ok()?, liked_at)))
        .collect())
}

/// Everything anyone in the family likes, once each, for the crawl to seed
/// from. Whose it was does not matter there.
pub fn family(conn: &mut SqliteConnection) -> Result<Vec<Liked>> {
    let rows = likes::table
        .filter(likes::item_json.is_not_null())
        .group_by((likes::kind, likes::ref_id))
        .select((likes::kind, diesel::dsl::max(likes::item_json)))
        .load::<(String, Option<String>)>(conn)?;
    Ok(rows
        .into_iter()
        .filter_map(|(kind, json)| Liked::parse(&kind, &json?))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStore;

    fn track(id: i64, title: &str) -> Liked {
        Liked::Track(RemoteTrack { id, title: title.into(), ..Default::default() })
    }

    #[test]
    fn private_but_seeded_together() {
        let dir = std::env::temp_dir().join(format!("stelly-likes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        let store = AuthStore::new(&db_path).unwrap();
        let sasha = store.add_user("sasha").unwrap();
        let sam = store.add_user("sam").unwrap();

        let (one, two) = (track(1, "one"), track(2, "two"));
        record(&db_path, sasha.id, "1", &one, 10).unwrap();
        record(&db_path, sasha.id, "1", &one, 20).unwrap();
        record(&db_path, sasha.id, "2", &two, 30).unwrap();
        record(&db_path, sam.id, "1", &one, 40).unwrap();
        let album = Liked::Album(RemoteAlbum { id: "a".into(), ..Default::default() });
        record(&db_path, sam.id, "a", &album, 50).unwrap();

        // Newest first, first like kept, nobody else's.
        let mine: Vec<(RemoteTrack, i64)> = list(&db_path, sasha.id, "track", 10).unwrap();
        let mine: Vec<(&str, i64)> = mine.iter().map(|(track, at)| (track.title.as_str(), *at)).collect();
        assert_eq!(mine, vec![("two", 30), ("one", 10)]);
        assert_eq!(list::<RemoteAlbum>(&db_path, sasha.id, "album", 10).unwrap().len(), 0);

        // The crawl sees each item once, whoever liked it.
        let conn = &mut open(&db_path).unwrap();
        let mut seeds: Vec<String> = family(conn)
            .unwrap()
            .into_iter()
            .map(|liked| match liked {
                Liked::Track(track) => format!("track:{}", track.id),
                Liked::Album(album) => format!("album:{}", album.id),
                Liked::Artist(artist) => format!("artist:{}", artist.id),
            })
            .collect();
        seeds.sort();
        assert_eq!(seeds, vec!["album:a", "track:1", "track:2"]);

        forget(&db_path, sasha.id, "track", "1").unwrap();
        assert_eq!(list::<RemoteTrack>(&db_path, sasha.id, "track", 10).unwrap().len(), 1);
        assert_eq!(list::<RemoteTrack>(&db_path, sam.id, "track", 10).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A like reaches the catalogue at distance 0, and its artist the frontier.
    #[test]
    fn seeds_the_crawl() {
        use crate::schema::{frontier, tracks};

        let dir = std::env::temp_dir().join(format!("stelly-likes-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        let conn = &mut crate::db::open_for_write(&db_path).unwrap();
        let sam = AuthStore::new(&db_path).unwrap().add_user("sam").unwrap();

        let track = Liked::Track(RemoteTrack {
            id: 1,
            title: "One".into(),
            album: "A".into(),
            album_id: Some("a1".into()),
            artist: "Seven".into(),
            artist_id: Some(7),
            ..Default::default()
        });
        record(&db_path, sam.id, "1", &track, 10).unwrap();

        let seeded = crate::crawl::seed(conn).unwrap();
        assert_eq!(seeded.tracks_added, 1);
        let distance: i64 = tracks::table.find(1).select(tracks::seed_distance).first(conn).unwrap();
        assert_eq!(distance, 0);
        let queued: i64 = frontier::table
            .filter(frontier::kind.eq("artist"))
            .filter(frontier::ref_id.eq("7"))
            .count()
            .get_result(conn)
            .unwrap();
        assert_eq!(queued, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

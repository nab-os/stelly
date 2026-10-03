//! Likes: each person's own, kept here and nowhere else.
//!
//! Qobuz is never told. The account's favourites are read once, when someone
//! asks to import them, and copied into that person's likes; after that the
//! two drift apart freely. Nobody sees anyone else's likes. What they are
//! shared into is the crawl, which seeds from the whole family's.
//!
//! Each like keeps the Qobuz object as it was fetched, so listing them needs
//! no request, and the crawl can store it the way it stores anything else.

use anyhow::Result;
use diesel::prelude::*;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

// Created by `AuthStore::ensure_schema`, with the users it points at.
diesel::table! {
    likes (user_id, kind, ref_id) {
        user_id -> BigInt,
        kind -> Text,
        ref_id -> Text,
        qobuz_json -> Text,
        liked_at -> BigInt,
    }
}

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS likes (
    user_id    INTEGER NOT NULL REFERENCES users(id),
    -- 'track' | 'album' | 'artist'
    kind       TEXT NOT NULL,
    ref_id     TEXT NOT NULL,
    qobuz_json TEXT NOT NULL,
    -- Unix seconds, the shape Qobuz reports favorited_at in.
    liked_at   INTEGER NOT NULL,
    PRIMARY KEY (user_id, kind, ref_id)
);
CREATE INDEX IF NOT EXISTS likes_listing ON likes(user_id, kind, liked_at);";

fn open(db_path: &Path) -> Result<SqliteConnection> {
    crate::db::connect(db_path, Duration::from_secs(5))
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Whether it was new. Liking twice keeps the first time.
pub fn record(db_path: &Path, user_id: i64, kind: &str, ref_id: &str, item: &Value, liked_at: i64) -> Result<bool> {
    let added = diesel::insert_or_ignore_into(likes::table)
        .values((
            likes::user_id.eq(user_id),
            likes::kind.eq(kind),
            likes::ref_id.eq(ref_id),
            likes::qobuz_json.eq(item.to_string()),
            likes::liked_at.eq(liked_at),
        ))
        .execute(&mut open(db_path)?)?;
    Ok(added > 0)
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
pub fn list(db_path: &Path, user_id: i64, kind: &str, cap: usize) -> Result<Vec<(Value, i64)>> {
    let rows = likes::table
        .filter(likes::user_id.eq(user_id))
        .filter(likes::kind.eq(kind))
        .order(likes::liked_at.desc())
        .limit(cap as i64)
        .select((likes::qobuz_json, likes::liked_at))
        .load::<(String, i64)>(&mut open(db_path)?)?;
    Ok(rows
        .into_iter()
        .filter_map(|(json, liked_at)| Some((serde_json::from_str(&json).ok()?, liked_at)))
        .collect())
}

/// Everything anyone in the family likes, once each, for the crawl to seed
/// from. Whose it was does not matter there.
pub fn family(conn: &mut SqliteConnection) -> Result<Vec<(String, Value)>> {
    let rows = likes::table
        .group_by((likes::kind, likes::ref_id))
        .select((likes::kind, diesel::dsl::max(likes::qobuz_json)))
        .load::<(String, Option<String>)>(conn)?;
    Ok(rows
        .into_iter()
        .filter_map(|(kind, json)| Some((kind, serde_json::from_str(&json?).ok()?)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStore;
    use serde_json::json;

    #[test]
    fn private_but_seeded_together() {
        let dir = std::env::temp_dir().join(format!("stelly-likes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        let store = AuthStore::new(&db_path).unwrap();
        let sasha = store.add_user("sasha").unwrap();
        let sam = store.add_user("sam").unwrap();

        let one = json!({"id": 1, "title": "one"});
        let two = json!({"id": 2, "title": "two"});
        assert!(record(&db_path, sasha.id, "track", "1", &one, 10).unwrap());
        assert!(!record(&db_path, sasha.id, "track", "1", &one, 20).unwrap());
        record(&db_path, sasha.id, "track", "2", &two, 30).unwrap();
        record(&db_path, sam.id, "track", "1", &one, 40).unwrap();
        record(&db_path, sam.id, "album", "a", &json!({"id": "a"}), 50).unwrap();

        // Newest first, first like kept, nobody else's.
        let mine: Vec<i64> = list(&db_path, sasha.id, "track", 10).unwrap().iter().map(|(_, at)| *at).collect();
        assert_eq!(mine, vec![30, 10]);
        assert_eq!(list(&db_path, sasha.id, "album", 10).unwrap().len(), 0);

        // The crawl sees each item once, whoever liked it.
        let conn = &mut open(&db_path).unwrap();
        let mut seeds: Vec<String> = family(conn).unwrap().into_iter().map(|(kind, item)| format!("{kind}:{}", item["id"])).collect();
        seeds.sort();
        assert_eq!(seeds, vec!["album:\"a\"", "track:1", "track:2"]);

        forget(&db_path, sasha.id, "track", "1").unwrap();
        assert_eq!(list(&db_path, sasha.id, "track", 10).unwrap().len(), 1);
        assert_eq!(list(&db_path, sam.id, "track", 10).unwrap().len(), 1);

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

        let track = json!({
            "id": 1, "title": "One",
            "album": {"id": "a1", "title": "A", "artist": {"id": 7, "name": "Seven"}},
            "performer": {"id": 7, "name": "Seven"},
        });
        record(&db_path, sam.id, "track", "1", &track, 10).unwrap();

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

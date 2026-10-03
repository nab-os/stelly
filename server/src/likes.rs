//! Who in the family liked what.
//!
//! The favourites are the account's and everyone shares them, so Qobuz cannot
//! say whose a like was. Every like made through here is written down, one
//! row per item: whoever liked it first. An unlike drops the row with the
//! favourite, for everyone, as it always did.
//!
//! What has no row was liked before there were users, which makes it the
//! first user's, or in another Qobuz client, which makes it nobody's we know.

use crate::auth::users;
use anyhow::Result;
use diesel::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

// Created by `AuthStore::ensure_schema`, with the users it points at.
diesel::table! {
    likes (kind, ref_id) {
        kind -> Text,
        ref_id -> Text,
        user_id -> BigInt,
        liked_at -> Text,
    }
}

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS likes (
    kind     TEXT NOT NULL,
    ref_id   TEXT NOT NULL,
    user_id  INTEGER NOT NULL REFERENCES users(id),
    liked_at TEXT NOT NULL,
    PRIMARY KEY (kind, ref_id)
);";

fn open(db_path: &Path) -> Result<SqliteConnection> {
    crate::db::connect(db_path, Duration::from_secs(5))
}

/// Keeps the first liker: liking what is already a favourite changes nothing
/// on Qobuz either.
pub fn record(db_path: &Path, kind: &str, ref_id: &str, user_id: i64) -> Result<()> {
    diesel::insert_or_ignore_into(likes::table)
        .values((
            likes::kind.eq(kind),
            likes::ref_id.eq(ref_id),
            likes::user_id.eq(user_id),
            likes::liked_at.eq(crate::db::utc_now()),
        ))
        .execute(&mut open(db_path)?)?;
    Ok(())
}

pub fn forget(db_path: &Path, kind: &str, ref_id: &str) -> Result<()> {
    diesel::delete(likes::table.filter(likes::kind.eq(kind)).filter(likes::ref_id.eq(ref_id)))
        .execute(&mut open(db_path)?)?;
    Ok(())
}

/// Every like and every name, read once per favourites listing. A family's
/// likes run to thousands of rows at most.
pub struct Likers {
    by_item: HashMap<(String, String), i64>,
    names: HashMap<i64, String>,
    /// The first user, and when they were made: what was already liked by
    /// then is theirs.
    first: Option<(i64, String)>,
}

impl Likers {
    pub fn load(db_path: &Path) -> Result<Self> {
        let conn = &mut open(db_path)?;
        let by_item = likes::table
            .select((likes::kind, likes::ref_id, likes::user_id))
            .load::<(String, String, i64)>(conn)?
            .into_iter()
            .map(|(kind, ref_id, user_id)| ((kind, ref_id), user_id))
            .collect();
        let everyone = users::table
            .order(users::id)
            .select((users::id, users::name, users::created_at))
            .load::<(i64, String, String)>(conn)?;
        let first = everyone.first().map(|(id, _, created_at)| (*id, created_at.clone()));
        let names = everyone.into_iter().map(|(id, name, _)| (id, name)).collect();
        Ok(Self { by_item, names, first })
    }

    /// `liked_at` is Qobuz's, in Unix seconds.
    pub fn of(&self, kind: &str, ref_id: &str, liked_at: Option<i64>) -> Option<String> {
        let user_id = match self.by_item.get(&(kind.to_string(), ref_id.to_string())) {
            Some(&user_id) => user_id,
            None => {
                let (first, since) = self.first.as_ref()?;
                let liked_at = crate::db::utc_at(u64::try_from(liked_at?).ok()?);
                if liked_at > *since {
                    return None;
                }
                *first
            }
        };
        self.names.get(&user_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStore;

    #[test]
    fn whose_like() {
        let dir = std::env::temp_dir().join(format!("stelly-likes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        let store = AuthStore::new(&db_path).unwrap();
        store.add_user("sasha").unwrap();
        let sam = store.add_user("sam").unwrap();

        let before = Some(1_000_000_000);
        let after = Some(4_000_000_000);
        let names = || Likers::load(&db_path).unwrap();

        // Already liked when the family arrived: the first user's.
        assert_eq!(names().of("track", "1", before).as_deref(), Some("sasha"));
        // Liked since, but not through here: nobody's we know.
        assert_eq!(names().of("track", "1", after), None);

        record(&db_path, "track", "1", sam.id).unwrap();
        record(&db_path, "track", "1", 1).unwrap();
        assert_eq!(names().of("track", "1", after).as_deref(), Some("sam"));
        // Kinds do not collide.
        assert_eq!(names().of("album", "1", after), None);

        forget(&db_path, "track", "1").unwrap();
        assert_eq!(names().of("track", "1", after), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

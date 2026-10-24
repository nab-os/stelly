//! "Tune the space": each person's weights on the space's blocks, kept so
//! they follow the account from device to device and across restarts.
//!
//! The client sends only the blocks moved off the space's defaults, and gets
//! them back as sent: the defaults are the space's, and a rebuild can move
//! them. No row is the defaults.

use anyhow::Result;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use std::collections::HashMap;

// Created by `AuthStore::ensure_schema`, with the users it points at.
diesel::table! {
    tunings (user_id) {
        user_id -> BigInt,
        weights_json -> Text,
        updated_at -> BigInt,
    }
}

const TABLE: &str = "CREATE TABLE IF NOT EXISTS tunings (
    user_id      INTEGER PRIMARY KEY REFERENCES users(id),
    -- Block name to weight, as a JSON object.
    weights_json TEXT NOT NULL,
    -- Unix seconds.
    updated_at   INTEGER NOT NULL
);";

pub fn ensure_schema(conn: &mut SqliteConnection) -> Result<()> {
    conn.batch_execute(TABLE)?;
    Ok(())
}

/// One person's weights, empty when they never tuned or reset since. A
/// stored set that no longer reads is the same as none.
pub fn load(conn: &mut SqliteConnection, user_id: i64) -> Result<HashMap<String, f32>> {
    let json: Option<String> = tunings::table
        .find(user_id)
        .select(tunings::weights_json)
        .first(conn)
        .optional()?;
    Ok(json.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
}

/// Keep one person's weights, or forget them when there are none.
pub fn save(conn: &mut SqliteConnection, user_id: i64, weights: &HashMap<String, f32>) -> Result<()> {
    if weights.is_empty() {
        return forget(conn, user_id);
    }
    // A slider cannot send these, but a request can.
    let weights: HashMap<&String, f32> =
        weights.iter().filter(|(_, w)| w.is_finite()).map(|(name, w)| (name, w.clamp(0.0, 10.0))).collect();
    let json = serde_json::to_string(&weights)?;
    diesel::replace_into(tunings::table)
        .values((
            tunings::user_id.eq(user_id),
            tunings::weights_json.eq(json),
            tunings::updated_at.eq(crate::likes::now()),
        ))
        .execute(conn)?;
    Ok(())
}

pub fn forget(conn: &mut SqliteConnection, user_id: i64) -> Result<()> {
    diesel::delete(tunings::table.find(user_id)).execute(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> SqliteConnection {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.batch_execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             INSERT INTO users (name, created_at) VALUES ('sam', 'now'), ('alex', 'now');",
        )
        .unwrap();
        ensure_schema(&mut conn).unwrap();
        conn
    }

    #[test]
    fn weights_come_back_per_person() {
        let conn = &mut conn();
        let mut sam = HashMap::new();
        sam.insert("timbre".to_string(), 2.0);
        save(conn, 1, &sam).unwrap();

        assert_eq!(load(conn, 1).unwrap(), sam);
        assert!(load(conn, 2).unwrap().is_empty());

        sam.insert("timbre".to_string(), 0.5);
        save(conn, 1, &sam).unwrap();
        assert_eq!(load(conn, 1).unwrap()["timbre"], 0.5);
    }

    #[test]
    fn saving_none_is_a_reset() {
        let conn = &mut conn();
        save(conn, 1, &HashMap::from([("mood".to_string(), 1.5)])).unwrap();
        save(conn, 1, &HashMap::new()).unwrap();
        assert!(load(conn, 1).unwrap().is_empty());
    }

    #[test]
    fn nonsense_is_not_kept() {
        let conn = &mut conn();
        let odd = HashMap::from([("a".to_string(), f32::NAN), ("b".to_string(), 99.0)]);
        save(conn, 1, &odd).unwrap();
        assert_eq!(load(conn, 1).unwrap(), HashMap::from([("b".to_string(), 10.0)]));
    }
}

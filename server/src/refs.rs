//! Stelly's own ids, and which providers' items each stands for.
//!
//! Every track, album, artist and playlist the server knows has an id of
//! Stelly's, what the catalogue, the space, likes and the app all speak in,
//! and one row here for each provider item it stands for. One recording on
//! two providers is one track when their ISRCs agree; names are not enough,
//! and what else is the same recording is the `dedupe` stage's to find, by
//! what it sounds like. Albums are merged by artist and title, artists by
//! name. Within one provider nothing is merged here: a catalogue's own single
//! and album versions stay its business, until `dedupe` hears them as one.
//!
//! A catalogue from before several providers was keyed by its one
//! provider's ids; `bootstrap` takes those for Stelly's own, so nothing in it
//! has to change.

use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, Text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Playlist,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Track => "track",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "playlist",
        }
    }
}

/// One provider's item, as it is matched.
#[derive(Debug, Clone, Default)]
pub struct Item<'a> {
    /// The provider's id for it.
    pub external: String,
    pub isrc: Option<&'a str>,
    /// What it is called, `key` normalised: an artist's name, `artist|title`
    /// for a track or an album.
    pub key: Option<String>,
    pub seconds: Option<i64>,
}

/// Names as they are compared: case, punctuation and spacing make no
/// second artist or title.
pub fn key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| {
            part.chars()
                .flat_map(char::to_lowercase)
                .map(|c| if c.is_alphanumeric() { c } else { ' ' })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// What a track or an album is matched on: its main artist, as the first
/// of several credited is, and its title without who it features. Catalogues
/// write "Bad Omens, Dahlia" and "Bad Omens feat. Dahlia" for one credit.
pub fn credit_key(artist: &str, title: &str) -> String {
    let lower = artist.to_lowercase();
    let main = [",", " & ", " feat", " ft.", " ft ", " featuring ", " x ", ";", " / ", " with "]
        .iter()
        .filter_map(|separator| lower.find(separator))
        .min()
        .map_or(lower.as_str(), |at| &lower[..at]);
    let title = title.to_lowercase();
    let bare = ["(feat", "[feat", "(ft.", "[ft.", "(with ", " feat. ", " ft. "]
        .iter()
        .filter_map(|marker| title.find(marker))
        .min()
        .map_or(title.as_str(), |at| &title[..at]);
    key(&[main, bare])
}

#[derive(QueryableByName)]
struct Found {
    #[diesel(sql_type = Text)]
    id: String,
}

#[derive(QueryableByName)]
struct Known {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = BigInt)]
    no_isrc: i64,
    #[diesel(sql_type = BigInt)]
    no_key: i64,
    #[diesel(sql_type = BigInt)]
    no_seconds: i64,
}

#[derive(QueryableByName)]
struct Route {
    #[diesel(sql_type = Text)]
    provider: String,
    #[diesel(sql_type = Text)]
    external: String,
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Stelly's id for `provider`'s `item` when it has one and `item` tells it
/// nothing new: a read alone, which waits on no writer. `None` when only
/// `resolve` can give it.
pub fn known(conn: &mut SqliteConnection, provider: &str, kind: Kind, item: &Item) -> Result<Option<String>> {
    Ok(stored(conn, provider, kind, item)?.and_then(|(id, learns)| (!learns).then_some(id)))
}

/// The id stored for `item`, and whether `item` fills in what it lacks: a
/// stub, an artist named in a track say, learns what it is called when the
/// item itself arrives.
fn stored(conn: &mut SqliteConnection, provider: &str, kind: Kind, item: &Item) -> Result<Option<(String, bool)>> {
    let known = diesel::sql_query(
        "SELECT id, isrc IS NULL AS no_isrc, match_key IS NULL AS no_key, seconds IS NULL AS no_seconds
         FROM refs WHERE provider = ? AND kind = ? AND ref = ?",
    )
    .bind::<Text, _>(provider)
    .bind::<Text, _>(kind.as_str())
    .bind::<Text, _>(&item.external)
    .load::<Known>(conn)?;
    Ok(known.into_iter().next().map(|found| {
        let learns = (found.no_isrc != 0 && item.isrc.is_some())
            || (found.no_key != 0 && item.key.is_some())
            || (found.no_seconds != 0 && item.seconds.is_some());
        (found.id, learns)
    }))
}

/// Stelly's id for `provider`'s `item`, made if it has none yet. Writes, so
/// in a transaction that took the write lock first: a read that turned into
/// a write would fail at once had another connection written meanwhile, and
/// two could take the same new id.
pub fn resolve(conn: &mut SqliteConnection, provider: &str, kind: Kind, item: &Item) -> Result<String> {
    if let Some((id, learns)) = stored(conn, provider, kind, item)? {
        if !learns {
            return Ok(id);
        }
        diesel::sql_query(
            "UPDATE refs SET isrc = COALESCE(isrc, ?), match_key = COALESCE(match_key, ?), seconds = COALESCE(seconds, ?)
             WHERE provider = ? AND kind = ? AND ref = ?",
        )
        .bind::<Nullable<Text>, _>(item.isrc.map(str::to_ascii_uppercase))
        .bind::<Nullable<Text>, _>(&item.key)
        .bind::<Nullable<BigInt>, _>(item.seconds)
        .bind::<Text, _>(provider)
        .bind::<Text, _>(kind.as_str())
        .bind::<Text, _>(&item.external)
        .execute(conn)?;
        return Ok(id);
    }

    let id = match same_as(conn, provider, kind, item)? {
        Some(id) => id,
        None => allocate(conn, kind)?,
    };
    diesel::sql_query("INSERT INTO refs (kind, id, provider, ref, isrc, match_key, seconds) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind::<Text, _>(kind.as_str())
        .bind::<Text, _>(&id)
        .bind::<Text, _>(provider)
        .bind::<Text, _>(&item.external)
        .bind::<Nullable<Text>, _>(item.isrc.map(str::to_ascii_uppercase))
        .bind::<Nullable<Text>, _>(&item.key)
        .bind::<Nullable<BigInt>, _>(item.seconds)
        .execute(conn)?;
    Ok(id)
}

/// Another provider's item that is this one: an id `provider` has nothing
/// under yet.
fn same_as(conn: &mut SqliteConnection, provider: &str, kind: Kind, item: &Item) -> Result<Option<String>> {
    if kind == Kind::Playlist {
        return Ok(None);
    }
    // Another provider's, under an id this one has nothing under: looked up
    // by that id, not by listing every id this provider has.
    let elsewhere = "provider <> ? AND NOT EXISTS (SELECT 1 FROM refs o WHERE o.kind = refs.kind AND o.id = refs.id AND o.provider = ?)";
    if let Some(isrc) = item.isrc.map(str::trim).filter(|s| !s.is_empty()) {
        let found = diesel::sql_query(format!("SELECT id FROM refs WHERE kind = ? AND isrc = ? AND {elsewhere} LIMIT 1"))
            .bind::<Text, _>(kind.as_str())
            .bind::<Text, _>(isrc.to_ascii_uppercase())
            .bind::<Text, _>(provider)
            .bind::<Text, _>(provider)
            .load::<Found>(conn)?;
        if let Some(found) = found.into_iter().next() {
            return Ok(Some(found.id));
        }
    }
    // A track is its ISRC or nothing: names and a length mislead too often.
    if kind == Kind::Track {
        return Ok(None);
    }
    let Some(key) = item.key.as_deref().filter(|k| !k.replace('|', "").trim().is_empty()) else {
        return Ok(None);
    };
    let found = diesel::sql_query(format!("SELECT id FROM refs WHERE kind = ? AND match_key = ? AND {elsewhere}"))
        .bind::<Text, _>(kind.as_str())
        .bind::<Text, _>(key)
        .bind::<Text, _>(provider)
        .bind::<Text, _>(provider)
        .load::<Found>(conn)?;
    Ok(found.into_iter().next().map(|f| f.id))
}

/// A new id: a number, but for albums, whose ids are text.
fn allocate(conn: &mut SqliteConnection, kind: Kind) -> Result<String> {
    let next = diesel::sql_query("UPDATE id_counters SET next = next + 1 WHERE kind = ? RETURNING next - 1 AS n")
        .bind::<Text, _>(kind.as_str())
        .load::<Count>(conn)?;
    let n = match next.into_iter().next() {
        Some(count) => count.n,
        None => {
            let start = first_free(conn, kind)?;
            diesel::sql_query("INSERT INTO id_counters (kind, next) VALUES (?, ?)")
                .bind::<Text, _>(kind.as_str())
                .bind::<BigInt, _>(start + 1)
                .execute(conn)?;
            start
        }
    };
    Ok(match kind {
        Kind::Album => format!("s-{n}"),
        _ => n.to_string(),
    })
}

/// Past every number already in use for `kind`.
fn first_free(conn: &mut SqliteConnection, kind: Kind) -> Result<i64> {
    if kind == Kind::Album {
        return Ok(1);
    }
    let table = match kind {
        Kind::Track => "SELECT MAX(id) AS n FROM tracks",
        Kind::Artist => "SELECT MAX(id) AS n FROM artists",
        _ => "SELECT 0 AS n",
    };
    #[derive(QueryableByName)]
    struct Max {
        #[diesel(sql_type = Nullable<BigInt>)]
        n: Option<i64>,
    }
    let in_table = diesel::sql_query(table).get_result::<Max>(conn)?.n.unwrap_or(0);
    let in_refs = diesel::sql_query("SELECT MAX(CAST(id AS INTEGER)) AS n FROM refs WHERE kind = ?")
        .bind::<Text, _>(kind.as_str())
        .get_result::<Max>(conn)?
        .n
        .unwrap_or(0);
    Ok(in_table.max(in_refs) + 1)
}

/// The providers' items Stelly's `id` stands for, as (provider, their id).
/// A track `dedupe` merged into another stands for that one's.
pub fn routes(conn: &mut SqliteConnection, kind: Kind, id: &str) -> Result<Vec<(String, String)>> {
    let rows = diesel::sql_query(
        "SELECT provider, ref AS external FROM refs WHERE kind = ?1 AND id = COALESCE(
             (SELECT CAST(into_id AS TEXT) FROM track_merges WHERE ?1 = 'track' AND from_id = CAST(?2 AS INTEGER)), ?2)",
    )
    .bind::<Text, _>(kind.as_str())
    .bind::<Text, _>(id)
    .load::<Route>(conn)?;
    Ok(rows.into_iter().map(|r| (r.provider, r.external)).collect())
}

/// Take a catalogue from before several providers, keyed by `provider`'s
/// ids, for Stelly's own: each item it holds, liked or blocked, that no
/// provider stands for yet becomes `provider`'s item of the same id. Once;
/// `provider` is the one that catalogue came from, the one that can convert
/// what it stored, never merely the first configured.
///
/// A build before this credited such a catalogue to the first provider,
/// whatever it was: the ids it gave away, each the provider's own id as it
/// was, are given back.
pub fn bootstrap(conn: &mut SqliteConnection, provider: &str) -> Result<usize> {
    let done = diesel::sql_query("SELECT COUNT(*) AS n FROM id_counters WHERE kind = 'bootstrapped'").get_result::<Count>(conn)?;
    if done.n > 0 {
        return Ok(0);
    }

    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        id: String,
        #[diesel(sql_type = Nullable<Text>)]
        artist: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        title: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        isrc: Option<String>,
        #[diesel(sql_type = Nullable<BigInt>)]
        seconds: Option<i64>,
    }
    let unclaimed = |kind: &str, id: &str| format!("NOT EXISTS (SELECT 1 FROM refs r WHERE r.kind = '{kind}' AND r.id = {id})");
    let queries = [
        (
            Kind::Track,
            format!(
                "SELECT CAST(t.id AS TEXT) AS id, a.name AS artist, t.title AS title, t.isrc AS isrc, t.duration AS seconds
                 FROM tracks t LEFT JOIN artists a ON a.id = t.artist_id WHERE {}",
                unclaimed("track", "CAST(t.id AS TEXT)")
            ),
        ),
        (
            Kind::Album,
            format!(
                "SELECT b.id AS id, a.name AS artist, b.title AS title, NULL AS isrc, NULL AS seconds
                 FROM albums b LEFT JOIN artists a ON a.id = b.artist_id WHERE {}",
                unclaimed("album", "b.id")
            ),
        ),
        (
            Kind::Artist,
            format!(
                "SELECT CAST(id AS TEXT) AS id, name AS artist, NULL AS title, NULL AS isrc, NULL AS seconds
                 FROM artists WHERE {}",
                unclaimed("artist", "CAST(id AS TEXT)")
            ),
        ),
    ];

    // The write lock first: a read that turns into a write fails at once
    // if anyone wrote in between.
    conn.batch_execute("BEGIN IMMEDIATE").context("bootstrapping refs")?;
    // Given away by an earlier build: an item whose id is the provider's own
    // id is the catalogue's, and the catalogue is `provider`'s.
    let mut written = diesel::sql_query(
        "UPDATE OR IGNORE refs SET provider = ?1
         WHERE ref = id AND provider <> ?1 AND kind IN ('track', 'album', 'artist')",
    )
    .bind::<Text, _>(provider)
    .execute(conn)?;
    for (kind, query) in queries {
        for row in diesel::sql_query(query).load::<Row>(conn)? {
            let key = match (&row.artist, &row.title) {
                (Some(artist), Some(title)) => Some(credit_key(artist, title)),
                (Some(name), None) => Some(key(&[name])),
                _ => None,
            };
            written += diesel::sql_query(
                "INSERT OR IGNORE INTO refs (kind, id, provider, ref, isrc, match_key, seconds) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind::<Text, _>(kind.as_str())
            .bind::<Text, _>(&row.id)
            .bind::<Text, _>(provider)
            .bind::<Text, _>(&row.id)
            .bind::<Nullable<Text>, _>(row.isrc.map(|s| s.to_ascii_uppercase()))
            .bind::<Nullable<Text>, _>(key)
            .bind::<Nullable<BigInt>, _>(row.seconds)
            .execute(conn)?;
        }
    }
    // Liked or hidden but never catalogued: theirs too, unnamed until seen.
    let identity = [
        format!(
            "INSERT OR IGNORE INTO refs (kind, id, provider, ref) SELECT kind, ref_id, ?, ref_id FROM likes l WHERE {}",
            "NOT EXISTS (SELECT 1 FROM refs r WHERE r.kind = l.kind AND r.id = l.ref_id)"
        ),
        format!(
            "INSERT OR IGNORE INTO refs (kind, id, provider, ref)
             SELECT 'artist', CAST(artist_id AS TEXT), ?, CAST(artist_id AS TEXT) FROM blocked_artists WHERE {}",
            unclaimed("artist", "CAST(artist_id AS TEXT)")
        ),
        // Merged by `dedupe` before there were refs: the one it joined.
        "INSERT OR IGNORE INTO refs (kind, id, provider, ref)
         SELECT 'track', CAST(into_id AS TEXT), ?, CAST(from_id AS TEXT) FROM track_merges"
            .to_string(),
    ];
    for query in identity {
        if let Ok(n) = diesel::sql_query(query).bind::<Text, _>(provider).execute(conn) {
            written += n;
        }
    }
    conn.batch_execute("INSERT INTO id_counters (kind, next) VALUES ('bootstrapped', 1); COMMIT;")?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> SqliteConnection {
        let dir = std::env::temp_dir().join(format!("stelly-refs-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::db::open_for_write(&dir.join("stelly.db")).unwrap()
    }

    fn track<'a>(external: &str, isrc: Option<&'a str>, artist: &str, title: &str, seconds: i64) -> Item<'a> {
        Item { external: external.into(), isrc, key: Some(credit_key(artist, title)), seconds: Some(seconds) }
    }

    #[test]
    fn a_credit_is_its_main_artist_and_its_bare_title() {
        let one = credit_key("Bad Omens", "BAD DECISIONS (LOFI)");
        assert_eq!(credit_key("Bad Omens, Dahlia", "Bad Decisions (LOFI)"), one);
        assert_eq!(credit_key("Bad Omens feat. Dahlia", "bad decisions (lofi)"), one);
        assert_eq!(credit_key("Bad Omens", "Bad Decisions (LOFI) (feat. Dahlia)"), one);
        assert_ne!(credit_key("Bad Omens", "Bad Decisions"), one);
    }

    #[test]
    fn names_compare_without_case_or_punctuation() {
        assert_eq!(key(&["Bad Omens", "Bad Decisions!"]), key(&["BAD  omens", "bad decisions"]));
        assert_eq!(key(&["Dañs"]), key(&["DAÑS"]));
        assert_ne!(key(&["Lun"]), key(&["Lún"]));
    }

    #[test]
    fn one_recording_on_two_providers_is_one_track() {
        let conn = &mut scratch();
        let qobuz = resolve(conn, "qobuz", Kind::Track, &track("111", Some("GBAAA1"), "Lun", "Gwerz", 200)).unwrap();
        // Asked again, the same.
        assert_eq!(resolve(conn, "qobuz", Kind::Track, &track("111", None, "Lun", "Gwerz", 200)).unwrap(), qobuz);
        // By ISRC, whatever the names.
        assert_eq!(resolve(conn, "local", Kind::Track, &track("9", Some("gbaaa1"), "LUN", "Gwerz (remaster)", 205)).unwrap(), qobuz);
        // Names and a length are not enough: that is for `dedupe` to hear.
        let yt = resolve(conn, "ytmusic", Kind::Track, &track("abc", None, "lun", "GWERZ", 200)).unwrap();
        assert_ne!(yt, qobuz);
        // The same provider's second edition stays its own.
        let edition = resolve(conn, "qobuz", Kind::Track, &track("222", Some("GBAAA1"), "Lun", "Gwerz", 200)).unwrap();
        assert_ne!(edition, qobuz);

        let mut routes = routes(conn, Kind::Track, &qobuz).unwrap();
        routes.sort();
        assert_eq!(routes, [("local".into(), "9".into()), ("qobuz".into(), "111".into())]);
    }

    /// Read alone only what needs no write: an item never seen, or a stub
    /// that would learn what it is called, is left for `resolve`.
    #[test]
    fn known_is_only_what_needs_no_write() {
        let conn = &mut scratch();
        let stub = Item { external: "7".into(), ..Default::default() };
        let named = Item { key: Some(key(&["Lun"])), ..stub.clone() };
        assert_eq!(known(conn, "qobuz", Kind::Artist, &stub).unwrap(), None);
        let id = resolve(conn, "qobuz", Kind::Artist, &stub).unwrap();
        assert_eq!(known(conn, "qobuz", Kind::Artist, &stub).unwrap(), Some(id.clone()));
        assert_eq!(known(conn, "qobuz", Kind::Artist, &named).unwrap(), None);
        assert_eq!(resolve(conn, "qobuz", Kind::Artist, &named).unwrap(), id);
        assert_eq!(known(conn, "qobuz", Kind::Artist, &named).unwrap(), Some(id));
    }

    #[test]
    fn a_catalogue_given_to_the_wrong_provider_is_given_back() {
        let conn = &mut scratch();
        conn.batch_execute(
            "INSERT INTO tracks (id, title) VALUES (500, 'Gwerz');
             INSERT INTO refs (kind, id, provider, ref) VALUES ('track', '500', 'ytmusic', '500'),
                                                               ('track', '501', 'ytmusic', '8796093022207');",
        )
        .unwrap();
        bootstrap(conn, "qobuz").unwrap();
        assert_eq!(routes(conn, Kind::Track, "500").unwrap(), [("qobuz".to_string(), "500".to_string())]);
        // Its own item, under an id of Stelly's, stays its own.
        assert_eq!(routes(conn, Kind::Track, "501").unwrap(), [("ytmusic".to_string(), "8796093022207".to_string())]);
    }

    #[test]
    fn a_catalogue_from_before_keeps_its_ids() {
        let conn = &mut scratch();
        conn.batch_execute(
            "INSERT INTO artists (id, name) VALUES (7, 'Lun');
             INSERT INTO albums (id, artist_id, title) VALUES ('a1', 7, 'Ifern');
             INSERT INTO tracks (id, album_id, artist_id, title, duration, isrc) VALUES (500, 'a1', 7, 'Gwerz', 200, 'X1');",
        )
        .unwrap();
        assert_eq!(bootstrap(conn, "qobuz").unwrap(), 3);
        assert_eq!(bootstrap(conn, "qobuz").unwrap(), 0);
        let item = |external: &str| Item { external: external.into(), ..Default::default() };
        assert_eq!(resolve(conn, "qobuz", Kind::Track, &item("500")).unwrap(), "500");
        assert_eq!(resolve(conn, "qobuz", Kind::Album, &item("a1")).unwrap(), "a1");
        // A new one comes after them, and an album gets a name of its own.
        assert_eq!(resolve(conn, "qobuz", Kind::Track, &item("501")).unwrap(), "501");
        assert_eq!(resolve(conn, "local", Kind::Track, &item("x")).unwrap(), "502");
        assert_eq!(resolve(conn, "local", Kind::Album, &item("l1")).unwrap(), "s-1");
        // The folder's copy of the track is the catalogue's.
        let copy = Item { external: "y".into(), isrc: Some("x1"), ..Default::default() };
        assert_eq!(resolve(conn, "local", Kind::Track, &copy).unwrap(), "500");
    }
}

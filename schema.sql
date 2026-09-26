-- Schema for two_khz.db, the server's database, and for the slim catalog.db it
-- builds for clients from it.
--
-- Executed by `server/src/db.rs::ensure_schema`, which embeds this file at
-- compile time, on every connect. CREATE TABLE IF NOT EXISTS does nothing to a
-- table that already exists, so a changed table needs a migration there too.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS artists (
    id                  INTEGER PRIMARY KEY,
    name                TEXT NOT NULL,
    qobuz_json          TEXT,
    similar_fetched_at  TEXT
);

CREATE TABLE IF NOT EXISTS albums (
    id            TEXT PRIMARY KEY,
    artist_id     INTEGER,
    title         TEXT NOT NULL,
    release_date  TEXT,
    label         TEXT,
    genre         TEXT,
    qobuz_json    TEXT
);

CREATE TABLE IF NOT EXISTS tracks (
    id             INTEGER PRIMARY KEY,
    album_id       TEXT,
    artist_id      INTEGER,
    title          TEXT NOT NULL,
    duration       INTEGER,
    isrc           TEXT,
    qobuz_json     TEXT,
    -- Hops from a favourited item. 0 = the user's own library.
    seed_distance  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_album  ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_seed   ON tracks(seed_distance);
CREATE INDEX IF NOT EXISTS idx_albums_artist ON albums(artist_id);

CREATE TABLE IF NOT EXISTS features (
    track_id           INTEGER PRIMARY KEY REFERENCES tracks(id),
    extractor_version  TEXT NOT NULL,
    -- Tempo, key, loudness and spectral shape; see pipeline/descriptors.rs.
    descriptors_json   TEXT,
    -- 512 floats, the CLAP audio embedding. Moods and styles are scored from
    -- it at build-space time, so changing a label never means re-analysing.
    clap_f32           BLOB,
    analysed_at        TEXT
);

CREATE INDEX IF NOT EXISTS idx_features_version ON features(extractor_version);

-- Resumable crawl queue. kind is 'artist' | 'album' | 'track'.
CREATE TABLE IF NOT EXISTS frontier (
    kind      TEXT NOT NULL,
    ref_id    TEXT NOT NULL,
    priority  INTEGER NOT NULL DEFAULT 0,
    state     TEXT NOT NULL DEFAULT 'pending',
    PRIMARY KEY (kind, ref_id)
);

CREATE INDEX IF NOT EXISTS idx_frontier_work ON frontier(state, priority);

CREATE TABLE IF NOT EXISTS layout (
    track_id  INTEGER PRIMARY KEY REFERENCES tracks(id),
    x         REAL NOT NULL,
    y         REAL NOT NULL
);

-- Tracks that failed analysis, so the pipeline stops retrying them forever.
CREATE TABLE IF NOT EXISTS failures (
    track_id  INTEGER PRIMARY KEY,
    stage     TEXT NOT NULL,
    reason    TEXT,
    failed_at TEXT
);

-- Artists to leave out of everything, honoured by every stage and every client.
-- A filter rather than a delete, so it is reversible and needs no rebuild; see
-- `block --purge` for the destructive version.
CREATE TABLE IF NOT EXISTS blocked_artists (
    artist_id   INTEGER PRIMARY KEY,
    name        TEXT,
    reason      TEXT,
    blocked_at  TEXT NOT NULL
);

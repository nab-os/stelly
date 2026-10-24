-- Schema for stelly.db, the server's database, and for the slim catalog.db it
-- builds for clients from it.
--
-- Executed by `server/src/db.rs::ensure_schema`, which embeds this file at
-- compile time, on every connect. CREATE TABLE IF NOT EXISTS does nothing to a
-- table that already exists, so a changed table needs a migration there too.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- Ids are the music provider's own. `meta_json` is the item as the provider
-- described it, in `stelly_core::catalogue`'s shape. `legacy_json` is what an
-- earlier Stelly stored instead, in the provider's own shape, until `import`
-- has the provider convert it; then it is NULL.
CREATE TABLE IF NOT EXISTS artists (
    id                  INTEGER PRIMARY KEY,
    name                TEXT NOT NULL,
    meta_json           TEXT,
    legacy_json         TEXT,
    similar_fetched_at  TEXT
);

CREATE TABLE IF NOT EXISTS albums (
    id            TEXT PRIMARY KEY,
    artist_id     INTEGER,
    title         TEXT NOT NULL,
    release_date  TEXT,
    label         TEXT,
    genre         TEXT,
    -- The cover's URL, what the space's rows show.
    image         TEXT,
    meta_json     TEXT,
    legacy_json   TEXT
);

CREATE TABLE IF NOT EXISTS tracks (
    id             INTEGER PRIMARY KEY,
    album_id       TEXT,
    artist_id      INTEGER,
    title          TEXT NOT NULL,
    duration       INTEGER,
    isrc           TEXT,
    meta_json      TEXT,
    legacy_json    TEXT,
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
-- The tracks with an embedding, for the counts that ask for them: without
-- it they read every row, embedding and all.
CREATE INDEX IF NOT EXISTS idx_features_clap ON features(track_id) WHERE clap_f32 IS NOT NULL;

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

-- Each track's whole-length analysis for mixing, from stelly-mix: its beats,
-- bars, phrases and each phrase's CLAP embedding, as stelly-mix's library
-- entry. Beside `features`, the 90s excerpt the space is built from.
CREATE TABLE IF NOT EXISTS mix (
    track_id     INTEGER PRIMARY KEY REFERENCES tracks(id),
    -- stelly_mix::library::VERSION; rows with another are redone.
    version      INTEGER NOT NULL,
    entry_json   TEXT NOT NULL,
    analysed_at  TEXT NOT NULL
);

-- Tracks the mix analysis failed on, apart from `failures`, which stops
-- `analyse` retrying: a track can fail one and not the other.
CREATE TABLE IF NOT EXISTS mix_failures (
    track_id   INTEGER PRIMARY KEY,
    reason     TEXT,
    failed_at  TEXT
);

-- Tracks waiting for their mix analysis, taken most urgent first, then in the
-- order they came, which is the rowid: so `track_id` is not the primary key.
-- See server/src/mix.rs.
CREATE TABLE IF NOT EXISTS mix_queue (
    track_id   INTEGER NOT NULL UNIQUE,
    -- 0 asked for and waited on, 1 liked, 2 a selection, 3 played through.
    priority   INTEGER NOT NULL,
    reason     TEXT NOT NULL,
    queued_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_mix_queue_order ON mix_queue(priority);

-- Every track played, one row each once it stopped being the one playing,
-- kept raw for whatever is wanted of it later. See server/src/listens.rs.
CREATE TABLE IF NOT EXISTS listens (
    id                INTEGER PRIMARY KEY,
    user_id           INTEGER NOT NULL,
    -- The output it started on.
    device_id         INTEGER,
    track_id          INTEGER NOT NULL,
    -- Unix seconds.
    started_at        INTEGER NOT NULL,
    -- What the position moved by while playing: a seek is not listening.
    listened_seconds  REAL NOT NULL,
    track_seconds     REAL,
    -- 'finished' | 'skipped' | 'stopped'
    ended             TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_listens_user ON listens(user_id, started_at);
CREATE INDEX IF NOT EXISTS idx_listens_track ON listens(track_id);

-- Which providers' items each of Stelly's own ids stands for: one row per
-- provider item, several for one recording found on several providers.
-- See server/src/refs.rs.
CREATE TABLE IF NOT EXISTS refs (
    -- 'track' | 'album' | 'artist' | 'playlist'
    kind       TEXT NOT NULL,
    -- Stelly's id: a number written out, but for albums.
    id         TEXT NOT NULL,
    -- The provider's name, as its capabilities give it, and its id.
    provider   TEXT NOT NULL,
    ref        TEXT NOT NULL,
    -- What items from different providers are matched on.
    isrc       TEXT,
    match_key  TEXT,
    seconds    INTEGER,
    PRIMARY KEY (provider, kind, ref)
);

CREATE INDEX IF NOT EXISTS idx_refs_id   ON refs(kind, id);
CREATE INDEX IF NOT EXISTS idx_refs_isrc ON refs(kind, isrc);
CREATE INDEX IF NOT EXISTS idx_refs_key  ON refs(kind, match_key);

-- The next of Stelly's own ids for each kind.
CREATE TABLE IF NOT EXISTS id_counters (
    kind  TEXT PRIMARY KEY,
    next  INTEGER NOT NULL
);

-- Tracks `dedupe` has compared with the rest, so the next run compares only
-- what was analysed since.
CREATE TABLE IF NOT EXISTS dedupe_seen (
    track_id  INTEGER PRIMARY KEY
);

-- Tracks `dedupe` heard as another and merged into it: their providers'
-- items, likes and mix analysis are `into_id`'s now. Kept so an id a device
-- still holds keeps working, and so a merge can be looked back on.
CREATE TABLE IF NOT EXISTS track_merges (
    from_id     INTEGER PRIMARY KEY,
    into_id     INTEGER NOT NULL,
    similarity  REAL NOT NULL,
    merged_at   TEXT NOT NULL
);

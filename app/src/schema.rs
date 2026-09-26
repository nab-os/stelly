//! `schema.sql`, as Diesel sees it. Kept by hand: the DDL there is what
//! creates the tables, and this is only its shape, so a column added there
//! needs adding here too, the compiler then finds every query it affects.
//!
//! Here rather than in the server so both sides of the catalogue read it
//! through the same definitions: the server writes `two_khz.db`, the client
//! reads the slim `catalog.db` built from it.
//!
//! SQLite's INTEGER is 64-bit, so every integer column is `BigInt`.

diesel::table! {
    artists (id) {
        id -> BigInt,
        name -> Text,
        qobuz_json -> Nullable<Text>,
        similar_fetched_at -> Nullable<Text>,
    }
}

diesel::table! {
    albums (id) {
        id -> Text,
        artist_id -> Nullable<BigInt>,
        title -> Text,
        release_date -> Nullable<Text>,
        label -> Nullable<Text>,
        genre -> Nullable<Text>,
        qobuz_json -> Nullable<Text>,
    }
}

diesel::table! {
    tracks (id) {
        id -> BigInt,
        album_id -> Nullable<Text>,
        artist_id -> Nullable<BigInt>,
        title -> Text,
        duration -> Nullable<BigInt>,
        isrc -> Nullable<Text>,
        qobuz_json -> Nullable<Text>,
        seed_distance -> BigInt,
    }
}

diesel::table! {
    features (track_id) {
        track_id -> BigInt,
        extractor_version -> Text,
        descriptors_json -> Nullable<Text>,
        clap_f32 -> Nullable<Binary>,
        analysed_at -> Nullable<Text>,
    }
}

diesel::table! {
    frontier (kind, ref_id) {
        kind -> Text,
        ref_id -> Text,
        priority -> BigInt,
        state -> Text,
    }
}

diesel::table! {
    layout (track_id) {
        track_id -> BigInt,
        x -> Double,
        y -> Double,
    }
}

diesel::table! {
    failures (track_id) {
        track_id -> BigInt,
        stage -> Text,
        reason -> Nullable<Text>,
        failed_at -> Nullable<Text>,
    }
}

diesel::table! {
    blocked_artists (artist_id) {
        artist_id -> BigInt,
        name -> Nullable<Text>,
        reason -> Nullable<Text>,
        blocked_at -> Text,
    }
}

diesel::joinable!(tracks -> artists (artist_id));
diesel::joinable!(tracks -> albums (album_id));
diesel::joinable!(features -> tracks (track_id));
diesel::joinable!(failures -> tracks (track_id));
diesel::joinable!(layout -> tracks (track_id));

diesel::allow_tables_to_appear_in_same_query!(
    artists,
    albums,
    tracks,
    features,
    frontier,
    layout,
    failures,
    blocked_artists,
);

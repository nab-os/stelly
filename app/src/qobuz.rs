//! Qobuz catalogue items as the app shows them: the flattened shapes the
//! server sends, and the parsers it builds them with.
//!
//! Only types live here. The client that talks to Qobuz, credentials,
//! signing, the rate limit, is the server's; a device never holds any of it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Qobuz format ids.
pub const FORMAT_MP3_320: u32 = 5;
pub const FORMAT_FLAC_CD: u32 = 6;
pub const FORMAT_FLAC_HIRES: u32 = 7;

// --------------------------------------------------------------- catalogue
//
// Flattened views of the Qobuz JSON. No vector and no row index: a remote item
// links to the space by track id or not at all.

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteTrack {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<i64>,
    pub album: String,
    pub album_id: Option<String>,
    pub duration: Option<i64>,
    /// Qobuz says up front whether a stream exists; greying these out saves a
    /// round trip that would fail with a confusing signature error.
    pub streamable: bool,
    pub hires: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<i64>,
    pub released: Option<String>,
    pub genre: Option<String>,
    pub tracks_count: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteArtist {
    pub id: i64,
    pub name: String,
    pub albums_count: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemotePlaylist {
    pub id: i64,
    pub name: String,
    pub tracks_count: Option<i64>,
    pub owner: Option<String>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Ids arrive as numbers for tracks/artists and as strings for albums, and not
/// always consistently, so accept either shape everywhere.
fn as_i64(value: &Value, key: &str) -> Option<i64> {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn as_id_string(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

impl RemoteTrack {
    /// Parse a track object. Tracks nested in an album omit the album and
    /// often the performer, so the caller passes down `context`.
    pub fn parse(value: &Value, context: Option<&RemoteAlbum>) -> Option<Self> {
        let id = as_i64(value, "id")?;

        let performer = value
            .get("performer")
            .or_else(|| value.get("artist"))
            .cloned()
            .unwrap_or(Value::Null);
        let album = value.get("album").cloned().unwrap_or(Value::Null);

        let mut artist = text(&performer, "name");
        let mut artist_id = as_i64(&performer, "id");
        let mut album_title = text(&album, "title");
        let mut album_id = as_id_string(&album, "id");

        if artist.is_none() {
            let album_artist = album.get("artist").cloned().unwrap_or(Value::Null);
            artist = text(&album_artist, "name");
            artist_id = as_i64(&album_artist, "id");
        }
        if let Some(parent) = context {
            artist = artist.or_else(|| Some(parent.artist.clone()));
            artist_id = artist_id.or(parent.artist_id);
            album_title = album_title.or_else(|| Some(parent.title.clone()));
            album_id = album_id.or_else(|| Some(parent.id.clone()));
        }

        // A missing `streamable` means the endpoint does not report it (album
        // tracklists sometimes do not). Assume playable rather than hiding it.
        let streamable = value
            .get("streamable")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let title = match (text(value, "title"), text(value, "version")) {
            (Some(title), Some(version)) => format!("{title} ({version})"),
            (Some(title), None) => title,
            (None, _) => "Unknown Track".to_string(),
        };

        Some(Self {
            id,
            title,
            artist: artist.unwrap_or_else(|| "Unknown Artist".into()),
            artist_id,
            album: album_title.unwrap_or_default(),
            album_id,
            duration: as_i64(value, "duration"),
            streamable,
            hires: value
                .get("hires_streamable")
                .or_else(|| value.get("hires"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    /// `m:ss`, blank when Qobuz did not report a duration.
    pub fn duration_label(&self) -> String {
        match self.duration {
            Some(seconds) if seconds > 0 => format!("{}:{:02}", seconds / 60, seconds % 60),
            _ => String::new(),
        }
    }
}

impl RemoteAlbum {
    pub fn parse(value: &Value) -> Option<Self> {
        let id = as_id_string(value, "id")?;
        let artist = value
            .get("artist")
            .or_else(|| value.get("performer"))
            .cloned()
            .unwrap_or(Value::Null);

        Some(Self {
            id,
            title: text(value, "title").unwrap_or_else(|| "Unknown Album".into()),
            artist: text(&artist, "name").unwrap_or_else(|| "Unknown Artist".into()),
            artist_id: as_i64(&artist, "id"),
            released: text(value, "release_date_original").or_else(|| text(value, "released_at")),
            genre: value
                .get("genre")
                .and_then(|g| g.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            tracks_count: as_i64(value, "tracks_count"),
        })
    }

    /// Just the year, which is all the list rows have room for.
    pub fn year(&self) -> String {
        self.released
            .as_deref()
            .and_then(|d| d.get(..4))
            .unwrap_or("")
            .to_string()
    }
}

impl RemoteArtist {
    pub fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: as_i64(value, "id")?,
            name: text(value, "name").unwrap_or_else(|| "Unknown Artist".into()),
            albums_count: as_i64(value, "albums_count"),
        })
    }
}

impl RemotePlaylist {
    pub fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: as_i64(value, "id")?,
            name: text(value, "name").unwrap_or_else(|| "Untitled playlist".into()),
            tracks_count: as_i64(value, "tracks_count"),
            owner: value
                .get("owner")
                .and_then(|o| o.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// Everything `catalog/search` returns, in one go.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<RemoteTrack>,
    pub albums: Vec<RemoteAlbum>,
    pub artists: Vec<RemoteArtist>,
}

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
    /// The album's cover; a track has no art of its own. See `image`.
    #[serde(default)]
    pub image: Option<String>,
    /// The recording, as opposed to this particular catalogue entry. One
    /// song reached through a single, an album and a deluxe reissue is three
    /// track ids but one ISRC, which is what `identity` dedupes on.
    #[serde(default)]
    pub isrc: Option<String>,
    /// The release this entry belongs to, not necessarily the track's own
    /// field, see `parse`. Full date where Qobuz reports one, same shape
    /// as `RemoteAlbum::released`.
    #[serde(default)]
    pub released: Option<String>,
    /// Qobuz's own credit string, composer, lyricist, producer and so on,
    /// each tagged with their role, semicolon-separated. Shown as reported
    /// rather than reparsed into a struct: the exact grammar of this field is
    /// not confirmed against a live response (see `parse`'s comment), so
    /// trusting it only as far as "split on `;` and print" is the safe
    /// failure mode if the assumption is wrong.
    #[serde(default)]
    pub performers: Option<String>,
}

impl RemoteTrack {
    /// What makes two queue entries "the same music".
    ///
    /// The ISRC when Qobuz reports one, since that names the recording rather
    /// than the release. Falling back to the track id means an untagged track
    /// is only ever a duplicate of itself, which is the safe direction: a
    /// missed duplicate is a nuisance, a wrongly dropped track is a bug.
    pub fn identity(&self) -> TrackIdentity {
        match self.isrc.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(isrc) => TrackIdentity::Recording(isrc.to_ascii_uppercase()),
            None => TrackIdentity::Entry(self.id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TrackIdentity {
    /// An ISRC, upper-cased, Qobuz is not consistent about the case.
    Recording(String),
    /// A Qobuz track id, for the tracks that carry no ISRC.
    Entry(i64),
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
    #[serde(default)]
    pub image: Option<String>,
    /// crawl.rs has captured this into its own database column since the
    /// crawler was written; `RemoteAlbum` itself never did.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteArtist {
    pub id: i64,
    pub name: String,
    pub albums_count: Option<i64>,
    #[serde(default)]
    pub image: Option<String>,
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

/// Sizes to accept from an image bag, smallest usable first. A row thumbnail
/// and the player's cover are both served by `small` (230px for an album);
/// `thumbnail` is 50px and only worth having when nothing else is offered.
const IMAGE_SIZES: [&str; 6] = ["small", "medium", "large", "thumbnail", "extralarge", "mega"];

/// A cover or portrait URL. Qobuz nests these under `image` as a bag of named
/// sizes, and the names differ between albums (thumbnail/small/large) and
/// artists (small/medium/large/extralarge/mega). Newer artist payloads drop
/// `image` for an `images.portrait` hash, which has to be assembled by hand.
///
/// Everything returned points at `static.qobuz.com`, which serves unsigned,
/// so these go straight into an `<img>` with no proxy and no credentials.
fn image(value: &Value) -> Option<String> {
    match value.get("image") {
        Some(Value::String(url)) if !url.is_empty() => return Some(url.clone()),
        Some(bag @ Value::Object(_)) => {
            for size in IMAGE_SIZES {
                if let Some(url) = text(bag, size) {
                    return Some(url);
                }
            }
        }
        _ => {}
    }

    let hash = text(value.get("images")?.get("portrait")?, "hash")?;
    Some(format!(
        "https://static.qobuz.com/images/artists/covers/medium/{hash}.jpg"
    ))
}

/// The cover for an album id, assembled rather than looked up.
///
/// Qobuz files covers under a path derived from the id: the last two
/// characters, then the two before those, then the id. Worth the guess because
/// a track that came out of the space carries only an album id, generated
/// sequences would otherwise be the one list in the app with no art at all.
/// `Cover` hides an image that fails to load, so guessing wrong costs nothing.
pub fn cover_url(album_id: &str) -> Option<String> {
    let id: String = album_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.len() < 4 {
        return None;
    }
    let tail = &id[id.len() - 2..];
    let mid = &id[id.len() - 4..id.len() - 2];
    Some(format!(
        "https://static.qobuz.com/images/covers/{tail}/{mid}/{id}_230.jpg"
    ))
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

        // A track's own date, if Qobuz put one on it directly; otherwise the
        // release it belongs to, nested or passed down, same order as the
        // cover just above. Unlike the cover, this has no visual cost to
        // getting slightly redundant when a track's own date does exist and
        // happens to match, so the fallback chain is worth the full three
        // steps.
        let released = text(value, "release_date_original")
            .or_else(|| text(value, "released_at"))
            .or_else(|| text(&album, "release_date_original"))
            .or_else(|| context.and_then(|parent| parent.released.clone()));

        Some(Self {
            id,
            title,
            artist: artist.unwrap_or_else(|| "Unknown Artist".into()),
            artist_id,
            album: album_title.unwrap_or_default(),
            album_id,
            duration: as_i64(value, "duration"),
            streamable,
            // A track carries no art; the cover comes from whichever album
            // description reached it, nested or passed down.
            image: image(&album).or_else(|| context.and_then(|parent| parent.image.clone())),
            isrc: text(value, "isrc"),
            hires: value
                .get("hires_streamable")
                .or_else(|| value.get("hires"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            released,
            // Unconfirmed against a live response, see the field's doc
            // comment on `RemoteTrack`.
            performers: text(value, "performers"),
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
            image: image(value),
            label: value
                .get("label")
                .and_then(|l| l.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
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
            image: image(value),
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

/// Keep only the albums whose main artist is `artist_id`.
///
/// `artist/get?extra=albums` is every album the artist is *credited* on, and
/// covers credit the original composer, so a much-covered band's
/// "discography" comes back mostly other people's albums. Measured on Rage
/// Against The Machine: 42 returned, 13 theirs, the rest cover albums named
/// after their songs. An album whose artist Qobuz did not report is kept:
/// nothing says it is someone else's.
///
/// Typed results only, the server's `artist_albums_raw` stays unfiltered
/// for the crawler, where an album credited to the artist is still somewhere
/// worth expanding the frontier to.
pub fn own_releases(artist_id: i64, albums: Vec<RemoteAlbum>) -> Vec<RemoteAlbum> {
    albums
        .into_iter()
        .filter(|album| album.artist_id.is_none_or(|id| id == artist_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(title: &str, artist_id: Option<i64>) -> RemoteAlbum {
        RemoteAlbum {
            title: title.into(),
            artist_id,
            ..Default::default()
        }
    }

    /// The case that motivated `own_releases`: a cover album credits the
    /// original band, so `artist/get` lists it under them.
    #[test]
    fn discography_drops_albums_by_other_artists() {
        let kept = own_releases(
            155699,
            vec![
                album("Evil Empire", Some(155699)),
                album("Killing in the Name", Some(42)),
                album("Untagged", None),
            ],
        );
        let titles: Vec<_> = kept.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Evil Empire", "Untagged"]);
    }

    fn track(id: i64, isrc: Option<&str>) -> RemoteTrack {
        RemoteTrack {
            id,
            isrc: isrc.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn one_recording_across_two_releases_is_one_identity() {
        // The point of the whole exercise: a single and a deluxe reissue are
        // different catalogue entries carrying the same recording.
        assert_eq!(
            track(111, Some("GBAAA9100123")).identity(),
            track(222, Some("GBAAA9100123")).identity()
        );
    }

    #[test]
    fn isrc_case_does_not_make_a_second_identity() {
        assert_eq!(
            track(111, Some("gbaaa9100123")).identity(),
            track(222, Some("GBAAA9100123")).identity()
        );
    }

    #[test]
    fn an_untagged_track_is_only_ever_a_duplicate_of_itself() {
        // The safe direction: a missed duplicate is a nuisance, a wrongly
        // dropped track is a bug.
        assert_eq!(track(111, None).identity(), track(111, None).identity());
        assert_ne!(track(111, None).identity(), track(222, None).identity());
    }

    #[test]
    fn a_blank_isrc_counts_as_absent() {
        // Qobuz sends "" rather than null often enough to matter; taking it at
        // face value would collapse every untagged track into one.
        assert_eq!(track(111, Some("   ")).identity(), TrackIdentity::Entry(111));
        assert_ne!(
            track(111, Some("")).identity(),
            track(222, Some("")).identity()
        );
    }

    #[test]
    fn a_tagged_and_an_untagged_track_never_collide() {
        assert_ne!(
            track(111, Some("GBAAA9100123")).identity(),
            track(111, None).identity()
        );
    }

    #[test]
    fn cover_url_follows_the_two_by_two_tail_convention() {
        // Qobuz nests covers under the last two characters of the id, then the
        // two before those.
        assert_eq!(
            cover_url("3610159663848").as_deref(),
            Some("https://static.qobuz.com/images/covers/48/38/3610159663848_230.jpg")
        );
        // Too short to split, so there is nothing to guess from.
        assert_eq!(cover_url("12"), None);
        assert_eq!(cover_url(""), None);
    }
}

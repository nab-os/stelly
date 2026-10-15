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
    /// When it was liked, in Unix seconds. Only a likes listing has one;
    /// everywhere else it is `None`.
    #[serde(default)]
    pub liked_at: Option<i64>,
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
    /// When it was liked, in Unix seconds. Only a likes listing has one;
    /// everywhere else it is `None`.
    #[serde(default)]
    pub liked_at: Option<i64>,
    /// Album, EP, single... Only the discography listing reports it, so
    /// everywhere else it is `None`.
    #[serde(default)]
    pub kind: Option<ReleaseKind>,
    /// How the artist whose discography this came from is credited on it.
    /// Only a discography has one; see `discography`.
    #[serde(default)]
    pub credit: Option<Credit>,
}

/// What sort of release an album is, as the artist page groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseKind {
    Album,
    Ep,
    Single,
    Live,
    Compilation,
}

impl ReleaseKind {
    /// From `release_type`, which `artist/getReleasesList` and `album/get`
    /// fill in and `artist/get`'s album listing does not. A live album is an
    /// `album` tagged `live`; a live single stays a single.
    fn parse(value: &Value) -> Option<Self> {
        let live = value
            .get("release_tags")
            .and_then(|tags| tags.as_array())
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("live")));
        match text(value, "release_type")?.as_str() {
            "album" | "boxset" if live => Some(Self::Live),
            "album" | "boxset" => Some(Self::Album),
            "epmini" => Some(Self::Ep),
            "single" => Some(Self::Single),
            "bestof" | "compilation" => Some(Self::Compilation),
            _ => None,
        }
    }
}

/// How an artist is credited on a release in their discography.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Credit {
    /// Theirs, alone or as one of several main artists.
    Main,
    /// Someone else's, with them featured.
    Featured,
    /// Credited some other way: remixer, or composer of a song covered on it.
    Appears,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteArtist {
    pub id: i64,
    pub name: String,
    pub albums_count: Option<i64>,
    #[serde(default)]
    pub image: Option<String>,
    /// When it was liked, in Unix seconds. Only a likes listing has one;
    /// everywhere else it is `None`.
    #[serde(default)]
    pub liked_at: Option<i64>,
    /// Qobuz's biography, as plain text. Only `artist/get` sends one, so only
    /// the artist page ever has it.
    #[serde(default)]
    pub biography: Option<String>,
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
            liked_at: as_i64(value, "favorited_at"),
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
            // `artist/getReleasesList` nests the name as `{"display": ...}`.
            artist: text(&artist, "name")
                .or_else(|| artist.get("name").and_then(|name| text(name, "display")))
                .unwrap_or_else(|| "Unknown Artist".into()),
            artist_id: as_i64(&artist, "id"),
            released: text(value, "release_date_original")
                .or_else(|| text(value, "released_at"))
                .or_else(|| value.get("dates").and_then(|dates| text(dates, "original"))),
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
            liked_at: as_i64(value, "favorited_at"),
            kind: ReleaseKind::parse(value),
            credit: None,
        })
    }

    /// How `artist_id` is credited on this album. A server from before
    /// `credit` existed sent only the artist's own releases, so an album
    /// without one is theirs unless it names someone else as its artist.
    pub fn credit_on(&self, artist_id: i64) -> Credit {
        self.credit.unwrap_or(match self.artist_id {
            Some(id) if id != artist_id => Credit::Appears,
            _ => Credit::Main,
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
            liked_at: as_i64(value, "favorited_at"),
            biography: value
                .get("biography")
                .and_then(|bio| text(bio, "content"))
                .map(|html| plain_text(&html))
                .filter(|bio| !bio.is_empty()),
        })
    }
}

/// Qobuz biographies are HTML. Tags are dropped, a `<br>` or a closing `</p>`
/// becomes a line break, and the handful of entities it uses are decoded.
fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        if tag.starts_with("br") || tag == "/p" {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);

    let out = out
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");

    // One paragraph per line; the blank ones nested block tags leave behind
    // are dropped.
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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

/// An artist's discography, each album marked with how they are on it.
///
/// Qobuz lists every album the artist is *credited* on, and covers credit the
/// original composer, so a much-covered band's discography comes back mostly
/// other people's albums. Measured on Rage Against The Machine: 42 returned,
/// 13 theirs, the rest cover albums named after their songs. Those are kept
/// but marked, so the page can put them below the band's own.
///
/// `artists` carries each credited artist's roles; a cover album usually
/// leaves the composer out of it entirely, which is still `Appears`. A
/// collaboration lists every headliner as a main artist, so it is theirs too,
/// except under "Various Artists": a label compilation tags everyone on it
/// that way. An album that reports no artist at all is kept as theirs:
/// nothing says it is someone else's.
pub fn discography(artist_id: i64, raw: &[Value]) -> Vec<RemoteAlbum> {
    raw.iter()
        .filter_map(|value| {
            let mut album = RemoteAlbum::parse(value)?;
            album.credit = Some(credit(value, artist_id, &album));
            Some(album)
        })
        .collect()
}

/// The releases out between `since` and `until`, both `YYYY-MM-DD` and
/// inclusive, newest first, from several artists' discographies at once.
///
/// Only an artist's own: a feature or a remix credit is someone else's
/// release. One album reached through two liked artists shows once, and so
/// does a release Qobuz carries as several same-day editions (explicit and
/// clean, standard and deluxe artwork), which would otherwise fill a row.
pub fn recent_releases(albums: Vec<RemoteAlbum>, since: &str, until: &str) -> Vec<RemoteAlbum> {
    let mut out: Vec<RemoteAlbum> = albums
        .into_iter()
        .filter(|album| album.credit.is_none_or(|credit| credit == Credit::Main))
        .filter(|album| {
            album
                .released
                .as_deref()
                .and_then(|date| date.get(..10))
                .is_some_and(|date| (since..=until).contains(&date))
        })
        .collect();
    out.sort_by(|a, b| b.released.cmp(&a.released).then_with(|| a.title.cmp(&b.title)));

    let mut ids = std::collections::HashSet::new();
    let mut editions = std::collections::HashSet::new();
    out.retain(|album| {
        ids.insert(album.id.clone())
            && editions.insert((album.artist_id, album.title.to_lowercase(), album.released.clone()))
    });
    out
}

fn credit(value: &Value, artist_id: i64, album: &RemoteAlbum) -> Credit {
    if album.artist_id.is_none_or(|id| id == artist_id) {
        return Credit::Main;
    }
    if album.artist.eq_ignore_ascii_case("various artists") {
        return Credit::Appears;
    }
    let roles: Vec<&str> = value
        .get("artists")
        .and_then(|artists| artists.as_array())
        .into_iter()
        .flatten()
        .filter(|artist| as_i64(artist, "id") == Some(artist_id))
        .filter_map(|artist| artist.get("roles")?.as_array())
        .flatten()
        .filter_map(|role| role.as_str())
        .collect();
    if roles.contains(&"main-artist") {
        Credit::Main
    } else if roles.contains(&"featured-artist") {
        Credit::Featured
    } else {
        Credit::Appears
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes `artist/getReleasesList` sends, trimmed to what matters.
    fn releases() -> Vec<Value> {
        let json = serde_json::json!([
            {"id": "a", "title": "Evil Empire", "release_type": "album",
             "artist": {"id": 155699, "name": {"display": "Rage Against The Machine"}},
             "artists": [{"id": 155699, "roles": ["main-artist"]}],
             "dates": {"original": "1996-04-16"}},
            {"id": "b", "title": "Live at the Grand Olympic Auditorium",
             "release_type": "album", "release_tags": ["live"],
             "artist": {"id": 155699}},
            {"id": "c", "title": "Bulls On Parade", "release_type": "single",
             "artist": {"id": 155699}},
            {"id": "d", "title": "Collab", "release_type": "epmini",
             "artist": {"id": 7},
             "artists": [{"id": 7, "roles": ["main-artist"]},
                         {"id": 155699, "roles": ["main-artist"]}]},
            {"id": "e", "title": "Someone's Single", "release_type": "single",
             "artist": {"id": 8},
             "artists": [{"id": 8, "roles": ["main-artist"]},
                         {"id": 155699, "roles": ["featured-artist"]}]},
            // A cover: the band is credited as composer, which leaves them
            // out of `artists` altogether.
            {"id": "f", "title": "Killing in the Name", "release_type": "bestof",
             "artist": {"id": 42}, "artists": [{"id": 42, "roles": ["main-artist"]}]},
            {"id": "g", "title": "Untagged"},
            {"id": "h", "title": "Label Sampler", "release_type": "album",
             "artist": {"id": 145383, "name": {"display": "Various Artists"}},
             "artists": [{"id": 155699, "roles": ["main-artist"]}]},
        ]);
        json.as_array().unwrap().clone()
    }

    /// The case that motivated `credit`: a cover album credits the original
    /// band, so Qobuz lists it under them.
    #[test]
    fn discography_marks_how_the_artist_is_credited() {
        let albums = discography(155699, &releases());
        let credits: Vec<_> = albums.iter().map(|a| a.credit.unwrap()).collect();
        use Credit::*;
        assert_eq!(credits, [Main, Main, Main, Main, Featured, Appears, Main, Appears]);
    }

    #[test]
    fn release_kinds_and_the_nested_name_parse() {
        let albums = discography(155699, &releases());
        let kinds: Vec<_> = albums.iter().map(|a| a.kind).collect();
        use ReleaseKind::*;
        assert_eq!(
            kinds,
            [
                Some(Album),
                Some(Live),
                Some(Single),
                Some(Ep),
                Some(Single),
                Some(Compilation),
                None,
                Some(Album),
            ]
        );
        assert_eq!(albums[0].artist, "Rage Against The Machine");
        assert_eq!(albums[0].year(), "1996");
    }

    /// An album from a server that predates `credit`.
    #[test]
    fn credit_falls_back_to_the_album_artist() {
        let album = |artist_id| RemoteAlbum { artist_id, ..Default::default() };
        assert_eq!(album(Some(1)).credit_on(1), Credit::Main);
        assert_eq!(album(None).credit_on(1), Credit::Main);
        assert_eq!(album(Some(2)).credit_on(1), Credit::Appears);
    }

    #[test]
    fn recent_releases_keep_the_artists_own_inside_the_window_newest_first() {
        let album = |id: &str, title: &str, artist_id, released: &str, credit| RemoteAlbum {
            id: id.into(),
            title: title.into(),
            artist_id: Some(artist_id),
            released: Some(released.into()),
            credit: Some(credit),
            ..Default::default()
        };
        let albums = vec![
            album("old", "Old", 1, "2026-01-01", Credit::Main),
            album("a", "Single", 1, "2026-09-01", Credit::Main),
            album("feat", "Featuring", 2, "2026-09-02", Credit::Featured),
            album("b", "Album", 2, "2026-10-01", Credit::Main),
            // The same album, reached through its second main artist.
            album("b", "Album", 2, "2026-10-01", Credit::Main),
            // A clean edition of the single, out the same day.
            album("a2", "single", 1, "2026-09-01", Credit::Main),
            album("future", "Pre-order", 1, "2026-12-01", Credit::Main),
            RemoteAlbum { id: "undated".into(), ..Default::default() },
        ];
        let ids: Vec<_> = recent_releases(albums, "2026-07-17", "2026-10-15")
            .into_iter()
            .map(|album| album.id)
            .collect();
        assert_eq!(ids, ["b", "a"]);
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

    #[test]
    fn a_biography_loses_its_markup_but_keeps_its_paragraphs() {
        let html = "<p>Born in <b>Leeds</b>&nbsp;in 1971.</p><p></p><p>Tom &amp; Jerry<br/>again</p>";
        assert_eq!(plain_text(html), "Born in Leeds in 1971.\nTom & Jerry\nagain");
    }
}

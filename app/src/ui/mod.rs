//! The Qobuz half: browsing, playing, and crawling more of it. Live, so it
//! sees the whole catalogue rather than just the analysed corpus.
//!
//! Panels go through `crate::backend`, the space included: it lives on the
//! server, and what the client knows of it is what it has been told.

pub mod crawler;
pub mod generate;
pub mod icons;
pub mod library;
pub mod menu;
#[cfg(target_os = "android")]
mod now_playing;
pub mod pipeline;
pub mod player;
pub mod prefs;
pub mod queue;
pub mod release;
pub mod screens;
pub mod settings;
pub mod side;

pub use crawler::Crawler;
pub use generate::Generator;
pub use library::{open_initial, Library, MainScreen, View};
pub use menu::{menu_button, ContextMenu, ContextMenuView, MenuState, MenuTarget};
pub use pipeline::{Family, Pipeline};
pub use player::{use_session, use_transport, Player, PlayerBar};
pub use queue::QueueView;
pub use release::{ReleaseNotice, Releases};
pub use side::{GeneratePanel, PathPill};

use dioxus::prelude::*;
use crate::api::{BlockedArtist, Device, SpaceIndex, SpaceInfo, TrackMeta};
use crate::backend::backend;
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemoteTrack};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Mutex;

/// Qobuz pages at 100. These caps stop a four-thousand-track favourites list
/// from stalling the panel the first time it is opened.
pub(crate) const LIST_CAP: usize = 500;
pub(crate) const SEARCH_LIMIT: usize = 50;

/// How often views watching a background job re-read it. A server can't push,
/// so the crawl and pipeline publish status and the views poll. 200ms is below
/// where a counter starts to look stuck.
pub(crate) const POLL: std::time::Duration = std::time::Duration::from_millis(200);

// ------------------------------------------------------------------ context

/// Track ids in the analysed space and not hidden. A memo, not a snapshot,
/// hiding an artist changes it and the rows reading it must re-render.
#[derive(Clone, Copy)]
pub struct LocalIds(pub Memo<Rc<HashSet<i64>>>);

/// Album and artist ids with at least one track in `LocalIds`, what an
/// album or artist tile reads to show the in-space mark. Kept apart from
/// `LocalIds` because nearly every reader of that one only wants tracks.
#[derive(Clone, Copy)]
pub struct SpaceReach(pub Memo<Rc<(HashSet<String>, HashSet<i64>)>>);

/// The in-space mark on a tile: a dot over the cover's corner. A list row
/// uses the inline `in-space-dot` instead, beside the title.
pub(crate) fn space_mark(in_space: bool) -> Element {
    if !in_space {
        return rsx! {};
    }
    rsx! { span { class: "space-mark", title: "in your space" } }
}

/// The map/generator's notion of "the track in hand", what a neighbours
/// walk starts from, what the map highlights and pins its card to. Track-only,
/// and only ever a space track: nothing else has coordinates to select.
///
/// Separate from what the main area shows: picking a point on the map selects
/// it without leaving the map, and the generate panel keeps its seed while you
/// browse somewhere else entirely.
#[derive(Clone, Copy)]
pub struct Selection(pub Signal<Option<i64>>);

// -------------------------------------------------------------------- space

/// What the client knows of the server's space: its shape, which tracks it
/// holds, and whether it could be reached. Starts from the disk cache and is
/// refreshed behind the window, so nothing waits on the network to open.
#[derive(Clone, Copy)]
pub struct Space {
    pub info: Signal<SpaceInfo>,
    pub index: Signal<Rc<SpaceIndex>>,
    /// Why the server did not answer, while it does not.
    pub offline: Signal<Option<String>>,
    /// Bumped when rows arrive for `space_row`, to draw again what asked.
    pub seen: Signal<u64>,
}

/// Every space row the server has sent, by id: generated rows, search
/// results, the tracks asked for by id. Rows are small and a session sees a
/// few thousand at most, so nothing is evicted until the space is rebuilt.
///
/// A plain map rather than a signal: it is read from event handlers and
/// tasks with no scope to subscribe. What renders from it goes through
/// `space_row`, which subscribes to `Space::seen` instead.
static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

#[derive(Default)]
struct Known {
    rows: HashMap<i64, TrackMeta>,
    /// Asked for already, found or not, so a track the space does not hold
    /// is not asked for again on every render.
    asked: HashSet<i64>,
}

fn with_known<T>(work: impl FnOnce(&mut Known) -> T) -> T {
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    work(known.get_or_insert_with(Known::default))
}

pub(crate) fn remember<'a>(tracks: impl IntoIterator<Item = &'a TrackMeta>) {
    with_known(|known| {
        for track in tracks {
            known.rows.insert(track.track_id, track.clone());
        }
    });
}

/// Forget every row, after a rebuild moved them.
pub(crate) fn forget_known() {
    *KNOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn known(track_id: i64) -> Option<TrackMeta> {
    with_known(|known| known.rows.get(&track_id).cloned())
}

/// A space row, from what has been seen or else from the server. `None` when
/// the space does not hold it, or the server could not be asked.
pub(crate) async fn space_meta(track_id: i64) -> Option<TrackMeta> {
    if let Some(found) = known(track_id) {
        return Some(found);
    }
    let found = backend().space_tracks(vec![track_id]).await.ok()?;
    remember(&found);
    found.into_iter().next()
}

/// A space row for a component to show. One not seen yet is asked for, and
/// the component drawn again when it arrives; until then, and for a track the
/// space does not hold, `None`.
pub(crate) fn space_row(track_id: i64) -> Option<TrackMeta> {
    let mut space = consume_context::<Space>();
    space.seen.read();
    if let Some(found) = known(track_id) {
        return Some(found);
    }
    if with_known(|known| known.asked.insert(track_id)) {
        spawn(async move {
            match backend().space_tracks(vec![track_id]).await {
                Ok(found) => {
                    remember(&found);
                    *space.seen.write() += 1;
                }
                // Not asked after all: the next render may try again.
                Err(_) => {
                    with_known(|known| known.asked.remove(&track_id));
                }
            }
        });
    }
    None
}

/// A space track, as something to display or play, if it has been seen. The
/// synchronous half of `space_meta`, for an event handler that cannot wait.
pub(crate) fn space_track(track_id: i64) -> Option<RemoteTrack> {
    known(track_id).as_ref().map(generate::as_remote)
}

/// Select a space track for the map and the generator, and open its details
/// in the main area, what a space row, a generated row and the map's card all
/// mean by "look at this track".
pub fn open_track(library: Library, mut selection: Signal<Option<i64>>, track_id: i64) {
    selection.set(Some(track_id));
    match space_track(track_id) {
        Some(track) => library.go(View::Track(track)),
        // A point on the map is only an id until it is asked for.
        None => {
            spawn(async move {
                if let Some(meta) = space_meta(track_id).await {
                    library.go(View::Track(generate::as_remote(&meta)));
                }
            });
        }
    }
}

/// A track's own details page, for a track that did not come from the space.
/// Selects it too when the space holds it, so the generate panel and the map
/// follow along.
pub(crate) fn open_remote_track(library: Library, mut selection: Signal<Option<i64>>, local: &LocalIds, track: RemoteTrack) {
    if local.0.peek().contains(&track.id) {
        selection.set(Some(track.id));
    }
    library.go(View::Track(track));
}

/// The album a track sits on, as much of it as the track knows. The album page
/// fetches the tracklist itself; the cover is assembled from the id.
pub(crate) fn album_of(track: &RemoteTrack) -> Option<RemoteAlbum> {
    let id = track.album_id.clone()?;
    Some(RemoteAlbum {
        image: crate::qobuz::cover_url(&id).or_else(|| track.image.clone()),
        id,
        title: track.album.clone(),
        artist: track.artist.clone(),
        artist_id: track.artist_id,
        released: track.released.clone(),
        ..Default::default()
    })
}

/// The dimension weight sliders' values, keyed by block name. A context
/// rather than a prop: the shell owns the signal and resets it when the
/// space's blocks change, but the sliders themselves live in the panel.
#[derive(Clone, Copy)]
pub struct Weights(pub Signal<std::collections::HashMap<String, f32>>);

/// The one search box. There used to be two, one filtering the analysed
/// space as you typed, one asking Qobuz on Enter, which made "where do I
/// type the name of a song" a question with two answers.
///
/// They stay two *queries*, because they are genuinely different: the local
/// one is a scan of memory and can run per keystroke, the remote one is a
/// network round trip and must not. What they no longer are is two inputs.
#[derive(Clone, Copy)]
pub struct Search {
    /// What is in the box. The local filter reads this directly.
    pub text: Signal<String>,
    /// What the remote has actually been asked for. Compared against `text`
    /// to decide whether a round trip is owed; also what stops the debounce
    /// from re-firing a query it has already run.
    pub submitted: Signal<String>,
}

impl Search {
    pub fn new() -> Self {
        Self {
            text: Signal::new(String::new()),
            submitted: Signal::new(String::new()),
        }
    }
}

impl Default for Search {
    fn default() -> Self {
        Self::new()
    }
}

/// The map's state, so anything that wants to show something on the map can
/// open it without the shell threading callbacks down to it. The map covers
/// the main area, never the generate panel beside it.
#[derive(Clone, Copy)]
pub struct MapView {
    pub map_open: Signal<bool>,
    /// Showing a route, and therefore dimming everything that is not on it.
    pub map_route: Signal<bool>,
}

impl MapView {
    /// Open the map to look around. Nothing is dimmed: there is no route in
    /// question, and a corpus at 18% is not a thing you can browse.
    pub fn browse(mut self) {
        self.map_route.set(false);
        self.map_open.set(true);
    }

    /// Open the map to show a produced sequence, dimming the rest so the line
    /// through it can be read.
    pub fn show_route(mut self) {
        self.map_route.set(true);
        self.map_open.set(true);
    }

    /// For the floating map button: open it if it is not showing, close it if
    /// it is. Every other caller wants `browse` or `show_route`'s one-way
    /// "definitely open it", asking to see a track on the map should never
    /// accidentally close a map that was already open on something else.
    pub fn toggle(mut self) {
        if *self.map_open.peek() {
            self.map_open.set(false);
        } else {
            self.browse();
        }
    }

    pub fn close(mut self) {
        self.map_open.set(false);
    }
}

/// A track in the analysed space, reduced to what a row needs. Carries
/// `album_id` because a space track has no stored art and its cover is
/// derived from that id.
#[derive(Clone, PartialEq)]
pub struct SpaceRow {
    pub track_id: i64,
    pub artist: String,
    /// For the artist-name link; `None` where the space stores -1.
    pub artist_id: Option<i64>,
    pub title: String,
    /// The album's title, for sorting by it.
    pub album: String,
    pub album_id: String,
}

/// Open an artist's page from just the id and name a row already carries. The
/// page fetches the portrait and biography itself.
pub(crate) fn open_artist(library: Library, id: i64, name: String) {
    library.go(View::Artist(RemoteArtist {
        id,
        name,
        ..Default::default()
    }));
}

/// An artist's name as a way to their page, wherever one appears, a library
/// row or tile, a queue entry, the player. Almost all of those already have a
/// click of their own (play the row, open the album), hence
/// `stop_propagation`: the name does its own thing and not the row's as well.
/// Plain text when there is no id to go to.
///
/// The link is its own span inside the column one: `.artist` is held to a
/// minimum width so list rows line up, and with the handler on that span the
/// blank space after a short name opened the artist instead of the row.
///
/// A function, not a component, for the same reason as `menu_button`: no
/// hooks, and a props struct per row is not worth it.
pub(crate) fn artist_link(library: Library, id: Option<i64>, name: String, class: &'static str) -> Element {
    match id {
        Some(id) => rsx! {
            span { class: "{class}",
                span {
                    class: "clickable",
                    title: "open {name}",
                    onclick: {
                        let name = name.clone();
                        move |event: Event<MouseData>| {
                            event.stop_propagation();
                            open_artist(library, id, name.clone());
                        }
                    },
                    "{name}"
                }
            }
        },
        None => rsx! { span { class: "{class}", "{name}" } },
    }
}

/// As `artist_link`, for the album a track sits on.
pub(crate) fn album_link(library: Library, track: &RemoteTrack, class: &'static str) -> Element {
    let name = track.album.clone();
    match album_of(track) {
        Some(album) if !name.is_empty() => rsx! {
            span { class: "{class}",
                span {
                    class: "clickable",
                    title: "open {name}",
                    onclick: move |event: Event<MouseData>| {
                        event.stop_propagation();
                        library.go(View::Album(album.clone()));
                    },
                    "{name}"
                }
            }
        },
        _ => rsx! { span { class: "{class}", "{name}" } },
    }
}

/// Tracks in the space matching the search box, and how many matched in all.
///
/// A context rather than a prop: the shell asks the server, but the column that
/// draws the results is `LibraryPanel`, and threading a list through every
/// intervening component to get there is how the two lists ended up in two
/// different columns in the first place.
#[derive(Clone, Copy)]
pub struct SpaceMatches(pub Signal<(Vec<SpaceRow>, usize)>);

/// Artists hidden from the family's space, and the actions that change that.
#[derive(Clone, Copy)]
pub struct Blocklist {
    pub artists: Signal<Vec<BlockedArtist>>,
    /// (artist_id, name)
    pub block: Callback<(i64, String)>,
    pub unblock: Callback<i64>,
    /// Whether this device may: hiding is for `pipeline` devices, so a `play`
    /// one is not offered what the server would refuse.
    pub editable: Memo<bool>,
}

/// This device as the server knows it: its name, whose it is, its scope.
/// `None` until the server says, or for good against one older than users.
#[derive(Clone, Copy)]
pub struct Me(pub Signal<Option<Device>>);

impl Blocklist {
    pub fn contains(&self, artist_id: i64) -> bool {
        self.artists
            .read()
            .iter()
            .any(|entry| entry.artist_id == artist_id)
    }

    /// Matches on the performer's artist id, so a featured credit filed under
    /// someone else's id can still slip through.
    fn hides(&self, track: &RemoteTrack) -> bool {
        track.artist_id.is_some_and(|id| self.contains(id))
    }
}

// -------------------------------------------------------------------- covers

/// Artwork, as a background rather than an `<img>`.
///
/// Deliberate: a URL that 404s, and `qobuz::cover_url` guesses some of them,
/// leaves the placeholder showing instead of a broken-image glyph, with no
/// `onerror` handler to install. Sizing is the caller's, via `class`.
#[component]
pub fn Cover(url: Option<String>, class: Option<String>) -> Element {
    // Single quotes and parens would break out of `url('…')`. Qobuz sends
    // neither, so a URL containing one is corrupt rather than merely unusual.
    let art = url.filter(|u| !u.contains(['\'', '(', ')']));
    let class = class.unwrap_or_default();

    // The URL rides as a data attribute and `covers.js` promotes it to a
    // background when the box nears the viewport. Setting it inline here
    // fetched every cover the moment it painted, which for a 500-row shelf is
    // 500 requests at once, and asking WebKitGTK for that is how scrolling
    // stops being smooth.
    //
    // An `<img loading="lazy">` would get the laziness for free but not the
    // failure behaviour: `cover_url` *guesses* the album art path, so a 404 is
    // expected, and a background leaves the placeholder tint where an `<img>`
    // shows a broken glyph. Recovering that would need an `onerror` hook per
    // cover, thousands per shelf.
    //
    // `eager` opts out, for the few covers that are always on screen.
    let eager = class.split_whitespace().any(|c| c == "eager");

    rsx! {
        div {
            class: "cover {class}",
            "data-cover": match (&art, eager) {
                (Some(url), false) => url.clone(),
                _ => String::new(),
            },
            style: match (&art, eager) {
                (Some(url), true) => format!("background-image:url('{url}')"),
                _ => String::new(),
            },
        }
    }
}

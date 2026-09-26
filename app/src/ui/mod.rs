//! The Qobuz half: browsing, playing, and crawling more of it. Live, so it
//! sees the whole catalogue rather than just the analysed corpus.
//!
//! Panels go through `crate::backend` and cannot tell local from remote. The
//! space is the exception, always answered in-process.

pub mod crawler;
pub mod generate;
pub mod library;
pub mod menu;
pub mod pipeline;
pub mod player;
pub mod queue;
pub mod sheet;

pub use crawler::Crawler;
pub use generate::Generator;
pub use library::{open_initial, Library, LibraryPanel};
pub use menu::{menu_button, ContextMenu, ContextMenuView, MenuState, MenuTarget};
pub use pipeline::{Pipeline, PipelineView};
pub use player::{use_transport, FullPlayer, Player, PlayerBar};
pub use queue::QueueView;
pub use sheet::{DetailSheet, PathPill};
pub(crate) use sheet::{AlbumDetail, ArtistDetail};

use dioxus::prelude::*;
use crate::api::BlockedArtist;
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemoteTrack};
use std::collections::HashSet;
use std::rc::Rc;

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
/// walk starts from, what the map highlights, what a path's A/B buttons
/// name. Track-only, and only ever a space track: nothing else has
/// coordinates to select.
///
/// This used to also be the detail sheet's open flag, which is what made
/// "close the sheet, keep the point highlighted on the map" impossible,
/// the only way to dim the sheet was to deselect, and deselecting is what
/// the map reads to know what to highlight. `Detail` is the sheet's own flag
/// now; the two move together almost everywhere (see `open_track`) except
/// the one place that needed them not to.
#[derive(Clone, Copy)]
pub struct Selection(pub Signal<Option<i64>>);

/// What the detail sheet is showing, if anything, a track (the space's or
/// Qobuz's; nothing here requires the space), an album, or an artist.
/// Independent of `Selection`; see its doc comment for why.
#[derive(Clone, PartialEq)]
pub enum DetailSubject {
    Track(RemoteTrack),
    Album(RemoteAlbum),
    Artist(RemoteArtist),
}

#[derive(Clone, Copy)]
pub struct Detail(pub Signal<Option<DetailSubject>>);

/// A space track, as something to display or play. `None` when the id names
/// nothing the space currently holds, selected, then the space was rebuilt
/// without it, or a row's address outlived its target.
pub(crate) fn space_track(track_id: i64) -> Option<RemoteTrack> {
    let guard = crate::engine().lock().unwrap();
    let row = *guard.navigator.index_of.get(&track_id)?;
    Some(generate::as_remote(guard.navigator.catalog.get(row)))
}

/// Select a space track for the map and the generator, and open its detail
/// sheet, the one action several different surfaces (a row, a map point, a
/// menu item) all mean by "look at this track". Every one of them used to be
/// a bare `selection.set(Some(id))`, back when that alone opened the sheet.
///
/// Not used by the sheet's own "On the map" button, which is the one place
/// `Selection` and `Detail` are meant to move separately, see `Selection`'s
/// doc comment.
pub fn open_track(mut selection: Signal<Option<i64>>, mut detail: Signal<Option<DetailSubject>>, track_id: i64) {
    selection.set(Some(track_id));
    if let Some(track) = space_track(track_id) {
        detail.set(Some(DetailSubject::Track(track)));
    }
}

/// The dimension weight sliders' values, keyed by block name. A context
/// rather than a prop: the shell owns the signal and reacts to the space
/// being rebuilt, but the sliders themselves live in the track sheet.
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

/// The map overlay's state, so anything that wants to show something on the
/// map can open it without the shell threading callbacks down to it.
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

    /// For the one persistent map button (the player bar's): open it if it
    /// is not showing, close it if it is. Every other caller wants `browse`
    /// or `show_route`'s one-way "definitely open it", asking to see a
    /// track on the map should never accidentally close a map that was
    /// already open on something else.
    pub fn toggle(mut self) {
        if *self.map_open.read() {
            self.map_open.set(false);
        } else {
            self.browse();
        }
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
    pub album_id: String,
}

/// Open an artist's detail from just the id and name a row already carries.
/// `albums_count` and a portrait are left `None`; `ArtistDetail` renders fine
/// without them, and the discography it fetches is what the sheet is for.
pub(crate) fn open_artist(mut detail: Signal<Option<DetailSubject>>, id: i64, name: String) {
    detail.set(Some(DetailSubject::Artist(RemoteArtist {
        id,
        name,
        ..Default::default()
    })));
}

/// An artist's name as a way to their detail sheet, wherever one appears,
/// a library row or tile, a queue entry, the player. Almost all of those
/// already have a click of their own (play the row, open the album, open
/// "now playing"), hence `stop_propagation`: the name does its own thing and
/// not the row's as well. Plain text when there is no id to go to.
///
/// A function, not a component, for the same reason as `menu_button`: no
/// hooks, and a props struct per row is not worth it.
pub(crate) fn artist_link(
    detail: Signal<Option<DetailSubject>>,
    id: Option<i64>,
    name: String,
    class: &'static str,
) -> Element {
    match id {
        Some(id) => rsx! {
            span {
                class: "{class} clickable",
                title: "open {name}",
                onclick: {
                    let name = name.clone();
                    move |event: Event<MouseData>| {
                        event.stop_propagation();
                        open_artist(detail, id, name.clone());
                    }
                },
                "{name}"
            }
        },
        None => rsx! { span { class: "{class}", "{name}" } },
    }
}

/// Tracks in the space matching the search box, and how many matched in all.
///
/// A context rather than a prop: the shell owns the scan, but the column that
/// draws the results is `LibraryPanel`, and threading a list through every
/// intervening component to get there is how the two lists ended up in two
/// different columns in the first place.
#[derive(Clone, Copy)]
pub struct SpaceMatches(pub Memo<(Vec<SpaceRow>, usize)>);

/// Artists the user has hidden, and the actions that change that. A signal
/// rather than a per-render read: the engine's copy sits behind a mutex the UI
/// cannot subscribe to.
#[derive(Clone, Copy)]
pub struct Blocklist {
    pub artists: Signal<Vec<BlockedArtist>>,
    /// (artist_id, name)
    pub block: Callback<(i64, String)>,
    pub unblock: Callback<i64>,
}

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

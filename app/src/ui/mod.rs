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

pub use crawler::Crawler;
pub use generate::{GeneratePanel, Generator};
pub use library::{open_initial, Library, LibraryPanel};
pub use menu::{menu_button, ContextMenu, ContextMenuView, MenuState, MenuTarget};
pub use pipeline::{Pipeline, PipelineView};
pub use player::{use_transport, Player, PlayerBar};
pub use queue::QueueView;

use dioxus::prelude::*;
use crate::api::BlockedArtist;
use crate::qobuz::RemoteTrack;
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

/// The map/neighbours selection, owned by the app shell. Browsing can drive it
/// when a Qobuz result turns out to be a track that was analysed.
#[derive(Clone, Copy)]
pub struct Selection(pub Signal<Option<i64>>);

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
}

/// A track in the analysed space, reduced to what a row needs. Carries
/// `album_id` because a space track has no stored art and its cover is
/// derived from that id.
#[derive(Clone, PartialEq)]
pub struct SpaceRow {
    pub track_id: i64,
    pub artist: String,
    pub title: String,
    pub album_id: String,
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

    rsx! {
        div {
            class: "cover {class}",
            style: match art {
                Some(url) => format!("background-image:url('{url}')"),
                None => String::new(),
            },
        }
    }
}

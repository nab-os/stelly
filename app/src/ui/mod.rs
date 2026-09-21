//! The Qobuz half: browsing, playing, and crawling more of it. Live, so it
//! sees the whole catalogue rather than just the analysed corpus.
//!
//! Panels go through `crate::backend` and cannot tell local from remote. The
//! space is the exception, always answered in-process.

pub mod crawler;
pub mod generate;
pub mod library;
pub mod pipeline;
pub mod player;

pub use crawler::Crawler;
pub use generate::{GeneratePanel, Generator};
pub use library::{open_initial, Library, LibraryPanel};
pub use pipeline::{Pipeline, PipelineView};
pub use player::{use_transport, Player, PlayerBar};

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

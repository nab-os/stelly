//! Browsing and searching the provider's catalogue, and the shelf of results.
//!
//! Navigation is explicit rather than reactive: every move sets the view and
//! spawns its own load. One `Shelf` holds whatever the view returned.

use super::icons;
use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, play_next, Player};
use super::prefs::{self, Prefs};
use super::screens::{AlbumScreen, ArtistScreen, TrackScreen};
use super::search_history::SearchHistory;
use super::settings::SettingsScreen;
use super::{
    artist_link, open_remote_track, open_track, Blocklist, Cover, LocalIds, MapView,
    Search, Selection, SpaceMatches, SpaceReach, SpaceRow, row_keys, space_mark, LIST_CAP,
    SEARCH_LIMIT,
};
use dioxus::core::Task;
use dioxus::prelude::*;
use crate::api::Target;
use crate::backend::backend;
use crate::catalogue::{RemoteAlbum, RemoteArtist, RemoteTrack};

/// How many space rows the list opens with. Enough to fill the column on any
/// screen; "show more" triples it.
const FIRST_SHOWN: usize = 80;

/// Which kinds of result a search shows. Every search starts with all of
/// them; the tracks/albums/artists chips narrow it, and tapping the lit one
/// again widens it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Everything,
    Tracks,
    Albums,
    Artists,
}

impl Scope {
    fn tracks(self) -> bool {
        matches!(self, Scope::Everything | Scope::Tracks)
    }

    fn albums(self) -> bool {
        matches!(self, Scope::Everything | Scope::Albums)
    }

    fn artists(self) -> bool {
        matches!(self, Scope::Everything | Scope::Artists)
    }
}

/// What every section of the list is ordered by. One choice for them all:
/// sorting the tracks by title and leaving the albums beside them as they came
/// would read as the sort half working. A key a section has nothing for
/// (duration, for an album) leaves that section as it came.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    /// When it was favourited. Only favourites know, so anywhere else this
    /// leaves the order as it came.
    Liked,
    Default,
    Title,
    Artist,
    Album,
    Released,
    Duration,
}

impl SortKey {
    const ALL: [SortKey; 7] = [
        SortKey::Liked,
        SortKey::Default,
        SortKey::Title,
        SortKey::Artist,
        SortKey::Album,
        SortKey::Released,
        SortKey::Duration,
    ];

    fn label(self) -> &'static str {
        match self {
            SortKey::Liked => "liked date",
            SortKey::Default => "original order",
            SortKey::Title => "title",
            SortKey::Artist => "artist",
            SortKey::Album => "album",
            SortKey::Released => "release date",
            SortKey::Duration => "duration",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// Most recently liked first: home is a mixed list, and liked date is the one
/// order that puts its tracks, albums and artists into a single timeline.
impl Default for Sort {
    fn default() -> Self {
        Sort { key: SortKey::Liked, descending: true }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SortValue {
    Text(String),
    Number(i64),
}

fn text(value: &str) -> Option<SortValue> {
    Some(SortValue::Text(value.to_lowercase()))
}

impl Sort {
    /// `items` in this order. "Default" is the order they came in, which
    /// descending just reverses. Anything without a value for the key goes
    /// last whichever way round, and ties keep the order they came in.
    pub fn apply<T>(self, items: Vec<T>, value: impl Fn(&T, SortKey) -> Option<SortValue>) -> Vec<T> {
        if self.key == SortKey::Default {
            let mut items = items;
            if self.descending {
                items.reverse();
            }
            return items;
        }
        let mut keyed: Vec<(Option<SortValue>, T)> =
            items.into_iter().map(|item| (value(&item, self.key), item)).collect();
        keyed.sort_by(|(a, _), (b, _)| match (a, b) {
            (Some(a), Some(b)) if self.descending => b.cmp(a),
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        keyed.into_iter().map(|(_, item)| item).collect()
    }
}

/// Whether every term turns up in a track's title, what a tracks search asks
/// for. Artist and album are left out: every track by a band, or on an
/// album, whose name happens to match is not a search for a track.
pub(crate) fn names_track(terms: &[String], title: &str) -> bool {
    terms.iter().all(|term| title.contains(term))
}

/// Which screen the main area is showing. Every page is one of these, and
/// `Library::history` is the way back through them.
#[derive(Clone, PartialEq, Debug)]
pub enum View {
    Search { query: String, scope: Scope },
    /// The home view: what the account has liked. Mixed, like a search,
    /// and narrowed by the same chips.
    Favourites { scope: Scope },
    Playlists,
    Playlist { id: i64, name: String },
    /// The full object, not just its id and title, carried over from
    /// wherever this was reached, so the page can show the cover and
    /// details while the tracklist is still loading.
    Album(RemoteAlbum),
    /// As `Album`: the page fetches the portrait and biography on top.
    Artist(RemoteArtist),
    /// One track, everything known about it, laid out to be screenshotted.
    Track(RemoteTrack),
    Settings,
    /// What the liked artists have released lately, newest first.
    Releases,
}

/// Where a search looks: the analysed space, or the provider's whole
/// catalogue. One or the other, never both at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Space,
    Catalogue,
}

impl View {
    pub(crate) fn label(&self) -> String {
        match self {
            View::Search { .. } => "search".into(),
            View::Favourites { .. } => "favourites".into(),
            View::Playlists => "your playlists".into(),
            View::Playlist { name, .. } => name.clone(),
            // The page's own header names it; the bar says what kind of
            // page it is.
            View::Album(_) => "album".into(),
            View::Artist(_) => "artist".into(),
            View::Track(_) => "track".into(),
            View::Settings => "settings".into(),
            View::Releases => "new releases".into(),
        }
    }

    /// Whether the source row (tracks/albums/artists/playlists) should light
    /// one of its chips up for this view, drilling into a specific album or
    /// artist leaves none of them lit, the same as before this replaced a
    /// separate "library" tab.
    fn is_playlists(&self) -> bool {
        matches!(self, View::Playlists | View::Playlist { .. })
    }

    /// The narrowing the tracks/albums/artists chips light up for. Only the
    /// two mixed views have one; anywhere else, a chip goes home.
    fn filter(&self) -> Option<Scope> {
        match self {
            View::Search { scope, .. } | View::Favourites { scope } => Some(*scope),
            _ => None,
        }
    }

    /// Which grid-or-list choice applies here. `None` for pages without a
    /// toggle.
    fn kind(&self) -> Option<ViewKind> {
        Some(match self {
            View::Search { scope, .. } | View::Favourites { scope } => match scope {
                Scope::Everything => ViewKind::Mixed,
                Scope::Tracks => ViewKind::Tracks,
                Scope::Albums => ViewKind::Albums,
                Scope::Artists => ViewKind::Artists,
            },
            View::Playlists => ViewKind::Playlists,
            View::Playlist { .. } => ViewKind::Playlist,
            View::Releases => ViewKind::Albums,
            View::Artist(_) => ViewKind::Artist,
            View::Album(_) | View::Track(_) | View::Settings => return None,
        })
    }
}

/// How the two track sections, the space and Qobuz, render. Grid by
/// default: a cover to recognise, the way albums and artists already worked.
/// List is where the detail that does not fit a tile lives (duration, the
/// "in your space" dot), for when that is what you are after.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TracksView {
    Grid,
    List,
}

/// The kinds of page that remember grid or list separately. A mixed home
/// wants covers while a narrowed track list wants durations, so one switch
/// for all of them kept being flipped back and forth.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewKind {
    /// Home or a search with no chip lit.
    Mixed,
    Tracks,
    Albums,
    Artists,
    Playlists,
    Playlist,
    /// An artist's page, whose discography is the one list with a toggle.
    Artist,
}

/// Whether a mixed view keeps its tracks, albums and artists in one list, or
/// gives each kind its own section. Either way the sort orders what is in
/// each list, and only mixed views have the choice: a narrowed one holds a
/// single kind already.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grouping {
    #[default]
    Mixed,
    Split,
}

/// Whatever the current view loaded. One struct rather than a per-view enum:
/// search fills three of these at once, and an artist fills two.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Shelf {
    pub tracks: Vec<RemoteTrack>,
    pub albums: Vec<crate::catalogue::RemoteAlbum>,
    pub artists: Vec<crate::catalogue::RemoteArtist>,
    pub playlists: Vec<crate::catalogue::RemotePlaylist>,
    /// Shown under an artist: the same hop the crawl takes when it expands
    /// the frontier, so you can see where analysis would go next.
    pub similar: Vec<crate::catalogue::RemoteArtist>,
}

#[derive(Clone, Copy)]
pub struct Library {
    pub view: Signal<View>,
    /// `loaded`, in `sort` order. Everything that indexes into the shelf
    /// (the menus, "play all", a click on row N) reads this one, so the
    /// order on screen and the order acted on cannot drift apart.
    pub shelf: Signal<Shelf>,
    /// The shelf as it came, kept so a different sort, or "default" again,
    /// does not need another fetch.
    loaded: Signal<Shelf>,
    pub sort: Signal<Sort>,
    pub loading: Signal<bool>,
    /// Loading, but a refined search's previous results stay on screen
    /// meanwhile rather than "loading…" in their place. See `keeps_results`.
    pub refining: Signal<bool>,
    /// The search fetch in flight, cancelled when another is asked for: its
    /// answer would only be thrown away, see `epoch`.
    searching: CopyValue<Option<Task>>,
    pub error: Signal<Option<String>>,
    /// Views visited on the way here, for the back button, each with how
    /// far down it was scrolled when it was left.
    pub history: Signal<Vec<(View, f64)>>,
    /// Views stepped back out of, for the mouse's forward button. Emptied by
    /// going anywhere new, as a browser does.
    forward: Signal<Vec<(View, f64)>>,
    /// How far down the main area is scrolled, kept by its `onscroll`. Not a
    /// signal: it changes every frame of a scroll and nothing renders from
    /// it, it is only read on the way out of a page.
    pub scroll: CopyValue<f64>,
    /// Where the page just asked for should be scrolled to, applied by
    /// `MainScreen` once it has rendered. Sent from `go` or `back` directly,
    /// the scroll reached the webview before the new page did and was set on
    /// the old one: a target the old page was long enough for counted as
    /// reached, and the new page coming in then cut it short.
    restore: Signal<Option<f64>>,
    /// A view asked for but not yet fetched. See `show`.
    pending: Signal<Option<View>>,
    pub notice: Signal<Option<String>>,
    /// Grid or list, per kind of page, and saved. Within a page it is one
    /// switch for both track sections (the space's and Qobuz's), not one
    /// each, they are the same kind of decision made twice, and a search
    /// result shows both at once.
    layouts: Signal<std::collections::HashMap<ViewKind, TracksView>>,
    /// Mixed views in one list or a section per kind. Saved, like `layouts`.
    grouping: Signal<Grouping>,
    /// Narrows a search to what is already favourited. Off by default: most
    /// searches are for something new, not a re-check of what is already
    /// kept.
    pub liked_only: Signal<bool>,
    /// The one provider a search of the catalogue asks, by name, or all of
    /// them. Sticks across searches, like `source`.
    pub only: Signal<Option<String>>,
    /// Where a search looks. Sticks across searches, like `liked_only`.
    pub source: Signal<Source>,
    /// The three favourited-id sets hearts and the "liked" filter read.
    /// Taken from the home page's answer, and at startup from last time's,
    /// so a page opening does not ask for them again. `None` until either.
    liked: Signal<Option<Liked>>,
    /// Likes and unlikes shown ahead of `liked`: sent and not yet answered,
    /// or answered before `liked` had loaded. Oldest first.
    flips: Signal<Vec<Flip>>,
    /// Bumped by every `show`. A fetch carries the value it started with and
    /// discards its result if it no longer matches, because `pending` is a
    /// single slot but the tasks it spawns are not: two views asked for in
    /// quick succession race, and the slower one would otherwise land last
    /// and win. Latent while navigation was a click at a time; a search that
    /// fires as you type makes it routine.
    epoch: Signal<u64>,
    /// What the last few views loaded, so going back to one shows it at once
    /// instead of an empty page until the fetch lands, which is a flash on
    /// every back. Keyed on the source as well: the same search asked of the
    /// space and of Qobuz finds different things.
    kept: Signal<Vec<(View, Source, Shelf)>>,
    /// Recent searches, newest first, saved. See `search_history`.
    pub(crate) searches: Signal<Vec<String>>,
}

/// How many views `Library::kept` remembers. Enough to step back through a
/// few pages; a shelf can hold hundreds of rows, so not the whole history.
const KEPT: usize = 8;

/// This user's likes, by id, fetched once and cached rather than asked per
/// row: the server lists them, it has no "is this one liked" lookup.
#[derive(Clone, Default, PartialEq, Debug)]
pub(crate) struct Liked {
    pub(crate) tracks: std::collections::HashSet<i64>,
    pub(crate) albums: std::collections::HashSet<String>,
    pub(crate) artists: std::collections::HashSet<i64>,
}

impl Liked {
    /// What a home page holds, before a liked album or artist stands in for
    /// the tracks in it.
    fn of(shelf: &Shelf) -> Self {
        Self {
            tracks: shelf.tracks.iter().map(|track| track.id).collect(),
            albums: shelf.albums.iter().map(|album| album.id.clone()).collect(),
            artists: shelf.artists.iter().map(|artist| artist.id).collect(),
        }
    }

    /// Put one id in or out of its set. `kind` is "track", "album" or
    /// "artist".
    fn set(&mut self, kind: &str, id: &str, liked: bool) {
        fn put<T: std::hash::Hash + Eq>(set: &mut std::collections::HashSet<T>, id: T, liked: bool) {
            if liked {
                set.insert(id);
            } else {
                set.remove(&id);
            }
        }
        match kind {
            "track" => {
                if let Ok(id) = id.parse() {
                    put(&mut self.tracks, id, liked);
                }
            }
            "album" => put(&mut self.albums, id.to_string(), liked),
            "artist" => {
                if let Ok(id) = id.parse() {
                    put(&mut self.artists, id, liked);
                }
            }
            _ => {}
        }
    }

    /// Whether one id is in its set. `kind` and `id` as for `set`.
    fn has(&self, kind: &str, id: &str) -> bool {
        match kind {
            "track" => id.parse().is_ok_and(|id: i64| self.tracks.contains(&id)),
            "album" => self.albums.contains(id),
            "artist" => id.parse().is_ok_and(|id: i64| self.artists.contains(&id)),
            _ => false,
        }
    }
}

/// One like or unlike the hearts show before the loaded sets have it.
#[derive(Clone, PartialEq)]
struct Flip {
    ticket: u64,
    kind: &'static str,
    id: String,
    liked: bool,
    /// The server has it, from before the fetch numbered this was asked
    /// for. Kept until a fetch that late or later lands: an earlier one may
    /// have been read before the change.
    saved: Option<u64>,
}

/// Numbers the fetches of the liked sets, so a saved flip knows which
/// answers may not have it yet.
static LIKED_ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The number of a fetch of the liked sets about to be sent.
fn ask_liked() -> u64 {
    LIKED_ASKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The number the next fetch will get.
fn next_liked_ask() -> u64 {
    LIKED_ASKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The loaded sets, or none yet, with the flips over them.
fn with_flips(base: Option<&Liked>, flips: &[Flip]) -> Liked {
    let mut liked = base.cloned().unwrap_or_default();
    for flip in flips {
        liked.set(flip.kind, &flip.id, flip.liked);
    }
    liked
}

/// Whether one id shows as liked: its last flip if it has one, or else the
/// loaded sets. `with_flips` for a single id, without copying the sets.
fn holds(base: Option<&Liked>, flips: &[Flip], kind: &str, id: &str) -> bool {
    match flips.iter().rev().find(|flip| flip.kind == kind && flip.id == id) {
        Some(flip) => flip.liked,
        None => base.is_some_and(|liked| liked.has(kind, id)),
    }
}

/// Let go of the flips an answer to fetch `asked` already has: those saved
/// before it was sent. The rest still show over it.
fn drop_seen(flips: &mut Vec<Flip>, asked: u64) {
    flips.retain(|flip| !flip.saved.is_some_and(|saved| saved <= asked));
}

impl Library {
    pub fn new() -> Self {
        let prefs = Prefs::load();
        Self {
            view: Signal::new(View::Favourites { scope: Scope::Everything }),
            shelf: Signal::new(Shelf::default()),
            loaded: Signal::new(Shelf::default()),
            sort: Signal::new(Sort::default()),
            loading: Signal::new(true),
            refining: Signal::new(false),
            searching: CopyValue::new(None),
            error: Signal::new(None),
            history: Signal::new(Vec::new()),
            forward: Signal::new(Vec::new()),
            scroll: CopyValue::new(0.0),
            restore: Signal::new(None),
            pending: Signal::new(None),
            notice: Signal::new(None),
            layouts: Signal::new(prefs.layouts),
            grouping: Signal::new(prefs.grouping),
            liked_only: Signal::new(false),
            only: Signal::new(None),
            source: Signal::new(Source::Catalogue),
            liked: Signal::new(None),
            flips: Signal::new(Vec::new()),
            epoch: Signal::new(0),
            kept: Signal::new(Vec::new()),
            searches: Signal::new(prefs.searches),
        }
    }

    /// Grid or list for the page on screen. Grid until chosen otherwise.
    pub(crate) fn layout(&self) -> TracksView {
        self.view
            .read()
            .kind()
            .and_then(|kind| self.layouts.read().get(&kind).copied())
            .unwrap_or(TracksView::Grid)
    }

    /// Set it for this kind of page, here and in every later session.
    pub(crate) fn set_layout(&self, layout: TracksView) {
        let Some(kind) = self.view.peek().kind() else {
            return;
        };
        let mut layouts = self.layouts;
        layouts.write().insert(kind, layout);
        prefs::update(|prefs| {
            prefs.layouts.insert(kind, layout);
        });
    }

    pub(crate) fn grouping(&self) -> Grouping {
        *self.grouping.read()
    }

    pub(crate) fn set_grouping(&self, grouping: Grouping) {
        let mut signal = self.grouping;
        signal.set(grouping);
        prefs::update(|prefs| prefs.grouping = grouping);
    }

    /// Fetch the three favourited-id sets, once. Safe to call every time the
    /// toggle turns on, the `is_some` guard means only the first call after
    /// launch actually reaches the network, and then only when neither the
    /// disk nor the home page has filled them in yet.
    pub(crate) fn ensure_liked_loaded(self) {
        if self.liked.peek().is_some() {
            return;
        }
        let library = self;
        spawn(async move {
            let asked = ask_liked();
            let (tracks, albums, artists) = tokio::join!(
                backend().favourite_tracks(LIST_CAP),
                backend().favourite_albums(LIST_CAP),
                backend().favourite_artists(LIST_CAP),
            );
            // A failed list would read as nothing liked, over sets the home
            // page may have filled in meanwhile.
            let (Ok(tracks), Ok(albums), Ok(artists)) = (tracks, albums, artists) else {
                return;
            };
            let loaded = Liked {
                tracks: tracks.iter().map(|t| t.id).collect(),
                albums: albums.iter().map(|a| a.id.clone()).collect(),
                artists: artists.iter().map(|a| a.id).collect(),
            };
            library.take_liked(loaded, Some(asked));
        });
    }

    /// Take fresh liked sets, from fetch `asked`, or from the disk with no
    /// number, which only fills in for none.
    fn take_liked(mut self, loaded: Liked, asked: Option<u64>) {
        match asked {
            Some(asked) => {
                let mut flips = self.flips.peek().clone();
                drop_seen(&mut flips, asked);
                if flips != *self.flips.peek() {
                    self.flips.set(flips);
                }
            }
            None if self.liked.peek().is_some() => return,
            None => {}
        }
        if self.liked.peek().as_ref() != Some(&loaded) {
            self.liked.set(Some(loaded));
        }
    }

    /// Whether the "liked" filter is on, and what to check against. Reading
    /// both together is what lets `unwrap_or_default`'s empty sets do double
    /// duty as "nothing is confirmed liked yet" while the fetch is still in
    /// flight, rather than needing a separate loading state every caller has
    /// to handle.
    ///
    /// The sets only while the filter is on: off, nothing reads them, and
    /// copying them on every render was for nothing.
    fn liked_state(&self) -> (bool, Liked) {
        let only = *self.liked_only.read();
        (only, if only { self.liked() } else { Liked::default() })
    }

    /// The favourited-id cache alone, regardless of whether the "liked"
    /// search filter is itself switched on, for anything that wants to
    /// know "is this one liked" rather than "should the shelf be narrowed to
    /// liked things". The detail sheet's heart icon is the other reader;
    /// both go through `ensure_liked_loaded` first, so whichever asks first
    /// pays for the fetch and the other finds it already there.
    ///
    /// Likes and unlikes not yet saved show as if they were.
    pub(crate) fn liked(&self) -> Liked {
        with_flips(self.liked.read().as_ref(), &self.flips.read())
    }

    /// Whether one track, album or artist shows as liked, which is all a
    /// heart needs: read in place rather than copying every set as `liked`
    /// does. `kind` is "track", "album" or "artist".
    pub(crate) fn is_liked(&self, kind: &str, id: &str) -> bool {
        holds(self.liked.read().as_ref(), &self.flips.read(), kind, id)
    }

    /// Show a like or unlike at once, before the server has it. Undone by
    /// `settle` if it fails. Returns the ticket `settle` takes.
    pub(crate) fn flip(mut self, kind: &'static str, id: &str, liked: bool) -> u64 {
        static TICKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let ticket = TICKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.flips.write().push(Flip {
            ticket,
            kind,
            id: id.to_string(),
            liked,
            saved: None,
        });
        if !liked {
            self.drop_from_favourites(kind, id);
        }
        ticket
    }

    /// The server has answered a `flip`. Saved, it stays until a fetch sent
    /// after now lands; refused, it is let go of and the heart goes back.
    /// Either way the home page asks again, to show what it now holds.
    pub(crate) fn settle(mut self, ticket: u64, saved: bool) {
        {
            let mut flips = self.flips.write();
            let Some(at) = flips.iter().position(|flip| flip.ticket == ticket) else {
                return;
            };
            if saved {
                flips[at].saved = Some(next_liked_ask());
            } else {
                flips.remove(at);
            }
        }
        let view = self.view.peek().clone();
        if matches!(view, View::Favourites { .. }) && !*self.loading.peek() {
            self.show(view);
        }
    }

    /// Take an unliked item off the home page, and off the copies of it kept
    /// for going back, rather than leave it there until the next fetch.
    fn drop_from_favourites(&mut self, kind: &str, id: &str) {
        if matches!(*self.view.peek(), View::Favourites { .. }) {
            for mut shelf in [self.shelf, self.loaded] {
                let mut kept = shelf.peek().clone();
                if kept.drop_liked(kind, id) {
                    shelf.set(kept);
                }
            }
        }
        for (view, _, shelf) in self.kept.write().iter_mut() {
            if matches!(view, View::Favourites { .. }) {
                shelf.drop_liked(kind, id);
            }
        }
    }

    /// Navigate, remembering where we came from.
    pub(crate) fn go(mut self, target: View) {
        leaving("go", 0.0);
        let previous = self.view.peek().clone();
        if previous != target {
            self.history.write().push((previous, self.scroll.cloned()));
            self.forward.write().clear();
        }
        self.show(target);
        self.restore.set(Some(0.0));
    }

    /// Back where it was left, scroll and all.
    pub(crate) fn back(mut self) {
        let previous = self.history.write().pop();
        if let Some((view, scroll)) = previous {
            leaving("back", scroll);
            self.forward.write().push((self.view.peek().clone(), self.scroll.cloned()));
            self.show(view);
            self.restore.set(Some(scroll));
        }
    }

    /// False once going somewhere new has emptied the way forward.
    pub(crate) fn can_forward(&self) -> bool {
        !self.forward.peek().is_empty()
    }

    pub(crate) fn forward(mut self) {
        let next = self.forward.write().pop();
        if let Some((view, scroll)) = next {
            leaving("forward", scroll);
            self.history.write().push((self.view.peek().clone(), self.scroll.cloned()));
            self.show(view);
            self.restore.set(Some(scroll));
        }
    }

    /// Home: favourites, as a place to go rather than a step back, so it
    /// is pushed like any other page.
    pub(crate) fn home(self) {
        if !matches!(&*self.view.peek(), View::Favourites { .. }) {
            self.go(View::Favourites { scope: Scope::Everything });
        }
    }

    /// The search button: open the search screen on whatever is already
    /// typed, or leave it again if it is the screen showing.
    pub(crate) fn toggle_search(self, query: String) {
        if matches!(&*self.view.peek(), View::Search { .. }) {
            if self.history.peek().is_empty() {
                self.show(View::Favourites { scope: Scope::Everything });
            } else {
                self.back();
            }
        } else {
            self.go(View::Search { query, scope: Scope::Everything });
        }
    }

    /// The new releases button, a toggle like search and settings.
    pub(crate) fn toggle_releases(self) {
        if matches!(&*self.view.peek(), View::Releases) {
            if self.history.peek().is_empty() {
                self.show(home());
            } else {
                self.back();
            }
        } else {
            self.go(View::Releases);
        }
    }

    /// The settings button: open settings, or leave them again if they are
    /// the screen showing, the same way the search button does.
    pub(crate) fn toggle_settings(self) {
        if matches!(&*self.view.peek(), View::Settings) {
            if self.history.peek().is_empty() {
                self.show(View::Favourites { scope: Scope::Everything });
            } else {
                self.back();
            }
        } else {
            self.go(View::Settings);
        }
    }

    /// Refine the search in place. Only while the search screen is showing:
    /// the debounce behind this fires a beat after the last keystroke, and
    /// by then you may have tapped your way somewhere else. Replaces rather
    /// than pushes, or "back" would walk you through your own spelling.
    pub(crate) fn search_to(self, query: String) {
        let current = self.view.peek().clone();
        if let View::Search { scope, .. } = current {
            self.show(View::Search { query, scope });
        }
    }

    /// A tracks/albums/artists chip. In a mixed view it narrows to that
    /// kind, or widens back to everything if it was already the one lit;
    /// replacing rather than pushing, the same as refining a query does.
    /// Anywhere else it goes home, narrowed to that kind.
    fn filter(self, chip: Scope) {
        let current = self.view.peek().clone();
        let scope = if current.filter() == Some(chip) { Scope::Everything } else { chip };
        match current {
            View::Search { query, .. } => self.show(View::Search { query, scope }),
            View::Favourites { .. } => self.show(View::Favourites { scope }),
            _ => self.go(View::Favourites { scope: chip }),
        }
    }

    /// Emptying the box empties the results, and stays on the search screen:
    /// it has its own button to leave by now.
    pub(crate) fn clear_search(self) {
        self.search_to(String::new());
    }

    fn set_sort(mut self, sort: Sort) {
        self.sort.set(sort);
        let loaded = self.loaded.peek().clone();
        self.shelf.set(loaded.sorted(sort));
    }

    /// Whether the space's own tracks are part of this view. Only for a
    /// search of the space: the tracks chip is the liked tracks, and the space
    /// holds everything the crawl reached from them, liked albums' tracks
    /// included, which is not what that chip is asking for.
    fn shows_space(&self) -> bool {
        match &*self.view.read() {
            View::Search { scope, .. } => scope.tracks() && *self.source.read() == Source::Space,
            _ => false,
        }
    }

    /// An album's tracklist, if its page is open or was one of the last few
    /// seen, so playing it from a menu need not ask again.
    pub(crate) fn held_album_tracks(&self, id: &str) -> Option<Vec<RemoteTrack>> {
        let open = (!*self.loading.peek()).then(|| (self.view.peek().clone(), self.loaded.peek().clone()));
        let kept = self.kept.peek();
        let views = open
            .iter()
            .map(|(view, shelf)| (view, shelf))
            .chain(kept.iter().map(|(view, _, shelf)| (view, shelf)));
        album_tracks_in(views, id)
    }

    /// Whether a search is looking at the space rather than Qobuz.
    fn searching_space_only(&self) -> bool {
        matches!(&*self.view.read(), View::Search { .. }) && *self.source.read() == Source::Space
    }

    /// Ask for a view. The fetch happens in `LibraryPanel`, not here.
    ///
    /// Called from the row handlers, and clearing the shelf unmounts those
    /// rows, a task spawned in a dying scope is dropped with it, which left
    /// the panel showing "loading..." for good.
    ///
    /// A view seen recently comes back with what it had, and is fetched again
    /// underneath, so a like or an unlike made since still shows up.
    pub fn show(mut self, target: View) {
        // Leaving the search screen with something on it is when a search
        // counts as used: whatever it found was opened, or read and left.
        if let View::Search { query, .. } = &*self.view.peek() {
            if !matches!(target, View::Search { .. }) {
                self.remember_search(query);
            }
        }
        let source = *self.source.peek();
        if !*self.loading.peek() && self.error.peek().is_none() {
            let leaving = (self.view.peek().clone(), source, self.loaded.peek().clone());
            let mut kept = self.kept.write();
            kept.retain(|(view, from, _)| (view, from) != (&leaving.0, &leaving.1));
            kept.push(leaving);
            if kept.len() > KEPT {
                kept.remove(0);
            }
        }
        let known = self
            .kept
            .peek()
            .iter()
            .find(|(view, from, _)| *view == target && *from == source)
            .map(|(_, _, shelf)| shelf.clone())
            // Releases can take minutes to fetch cold, so last time's list is
            // shown in the meantime, not only at startup the way home's is.
            .or_else(|| (target == View::Releases).then(|| crate::cache::read(RELEASES_CACHE)).flatten());

        if crate::timings::enabled() {
            crate::timings::nav_asked(timing_name(&target), *self.epoch.peek() + 1, known.is_some());
        }
        let refining = known.is_none() && keeps_results(&self.view.peek(), &target);
        self.view.set(target.clone());
        self.error.set(None);
        self.notice.set(None);
        match known {
            Some(shelf) => {
                let sort = *self.sort.peek();
                self.loaded.set(shelf.clone());
                self.shelf.set(shelf.sorted(sort));
                self.loading.set(false);
            }
            None if refining => self.loading.set(true),
            None => {
                self.shelf.set(Shelf::default());
                self.loaded.set(Shelf::default());
                self.loading.set(true);
            }
        }
        if *self.refining.peek() != refining {
            self.refining.set(refining);
        }
        self.pending.set(Some(target));
        *self.epoch.write() += 1;
    }

    /// Fetch whatever `show` last asked for. Runs in `LibraryPanel`, which is
    /// mounted for the life of the window.
    fn drive(mut self) {
        let target = self.pending.read().clone();
        let Some(target) = target else { return };
        self.pending.set(None);
        let epoch = *self.epoch.peek();
        let space_only = *self.source.peek() == Source::Space;
        let only = self.only.peek().clone();
        // Already showing what `kept` had for this view: a failed refresh
        // leaves that up rather than swapping it for an error.
        let refreshing = !*self.loading.peek();
        if let Some(task) = self.searching.take() {
            task.cancel();
        }
        let search = matches!(target, View::Search { .. });

        let task = spawn(async move {
            let mut library = self;
            let cache = if target == home() {
                Some(HOME_CACHE)
            } else if target == View::Releases {
                Some(RELEASES_CACHE)
            } else {
                None
            };
            let favourites = match &target {
                View::Favourites { scope } => Some((*scope, ask_liked())),
                _ => None,
            };
            let mut loaded = load(target, space_only, only).await;
            if let (Some((scope, asked)), Ok(shelf)) = (favourites, &mut loaded) {
                // Every like, before the tiles stand in for some: what a page
                // opening would otherwise fetch again for its heart.
                if scope == Scope::Everything {
                    library.take_liked(Liked::of(shelf), Some(asked));
                }
                if let Some(name) = cache.filter(|_| *library.epoch.peek() == epoch) {
                    crate::cache::write(name, &*shelf);
                }
                shelf.stand_in(scope);
            }

            // Someone asked for a different view while this was in flight.
            // Writing now would put these rows under that view's heading, and
            // clearing `loading` would call it finished.
            if *library.epoch.peek() != epoch {
                return;
            }

            match loaded {
                Ok(shelf) => {
                    crate::timings::nav_landed(epoch);
                    // Home's went to the disk as it came, above.
                    if let Some(name) = cache.filter(|_| favourites.is_none()) {
                        crate::cache::write(name, &shelf);
                    }
                    let sort = *library.sort.peek();
                    library.loaded.set(shelf.clone());
                    library.shelf.set(shelf.sorted(sort));
                }
                Err(_) if refreshing => {}
                Err(err) => {
                    // A refined search kept the last answer up meanwhile;
                    // it is not this query's, so "play all" must not reach it.
                    if *library.refining.peek() {
                        library.loaded.set(Shelf::default());
                        library.shelf.set(Shelf::default());
                    }
                    library.error.set(Some(format!("{err:#}")));
                }
            }
            library.loading.set(false);
            if *library.refining.peek() {
                library.refining.set(false);
            }
        });
        if search {
            self.searching.set(Some(task));
        }
    }
}

/// Whether going from `from` to `to` keeps what is on screen until the new
/// answer lands: a search refined in place, by its text or its chips. The
/// list stays put and is replaced in one go, rather than emptied for
/// "loading…" and rebuilt on every keystroke. An emptied box is not a
/// search, and clears at once.
fn keeps_results(from: &View, to: &View) -> bool {
    matches!(from, View::Search { .. })
        && matches!(to, View::Search { query, .. } if !query.trim().is_empty())
}

impl Shelf {
    /// A liked album or artist is shown as its tile, not again as every
    /// liked track inside it. Only when mixed: narrowed to tracks, there is
    /// no tile to stand in for them.
    fn stand_in(&mut self, scope: Scope) {
        if scope != Scope::Everything {
            return;
        }
        let albums: std::collections::HashSet<&str> =
            self.albums.iter().map(|album| album.id.as_str()).collect();
        let artists: std::collections::HashSet<i64> =
            self.artists.iter().map(|artist| artist.id).collect();
        self.tracks.retain(|track| {
            !track.album_id.as_deref().is_some_and(|id| albums.contains(id))
                && !track.artist_id.is_some_and(|id| artists.contains(&id))
        });
    }

    /// Take out the track, album or artist `id`. Says whether it was there.
    fn drop_liked(&mut self, kind: &str, id: &str) -> bool {
        let before = self.tracks.len() + self.albums.len() + self.artists.len();
        match kind {
            "track" => self.tracks.retain(|track| track.id.to_string() != id),
            "album" => self.albums.retain(|album| album.id != id),
            "artist" => self.artists.retain(|artist| artist.id.to_string() != id),
            _ => {}
        }
        before != self.tracks.len() + self.albums.len() + self.artists.len()
    }
}

/// The tracklist of album `id` among loaded views. An empty one counts as
/// not held: a page that found nothing is worth asking again.
fn album_tracks_in<'a>(
    mut views: impl Iterator<Item = (&'a View, &'a Shelf)>,
    id: &str,
) -> Option<Vec<RemoteTrack>> {
    views
        .find(|(view, shelf)| matches!(view, View::Album(album) if album.id == id) && !shelf.tracks.is_empty())
        .map(|(_, shelf)| shelf.tracks.clone())
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
    }
}

/// What a view is called in a timing line: the kind of page, never what is on
/// it.
fn timing_name(view: &View) -> &'static str {
    match view {
        View::Search { .. } => "search",
        View::Favourites { .. } => "favourites",
        View::Playlists => "playlists",
        View::Playlist { .. } => "playlist",
        View::Album(_) => "album",
        View::Artist(_) => "artist",
        View::Track(_) => "track",
        View::Settings => "settings",
        View::Releases => "releases",
    }
}

/// Tells transitions.js a page is about to be replaced, how, and where the
/// new one will be scrolled to. Sent before the view changes, so it reaches
/// the webview ahead of the render and the old page is still there to copy
/// and measure.
fn leaving(kind: &str, scroll: f64) {
    document::eval(&format!("window.stellyLeaving && window.stellyLeaving('{kind}', {scroll});"));
}

/// A new page starts at its top, and one gone back to where it was left. The
/// main area is one scroller that outlives every page in it, so without this
/// an album opened from far down the favourites opened just as far down
/// itself. Not on `show`: refining a search as you type is the same page.
///
/// Tried again each frame for a while: a page not in `kept` is still empty
/// when this lands, too short to scroll that far until its fetch fills it.
/// Given up as soon as the wheel, a finger or a key moves it instead, or a
/// later page asks for a scroll of its own.
fn scroll_to(top: f64) {
    document::eval(&format!(
        r#"(() => {{
            const el = document.querySelector('.screen-body');
            if (!el) return;
            const top = {top};
            const until = performance.now() + 2000;
            const turn = (window.stellyScrollTurn || 0) + 1;
            window.stellyScrollTurn = turn;
            let stop = false;
            const quit = () => {{ stop = true; }};
            const events = ['wheel', 'touchstart', 'keydown'];
            for (const e of events) el.addEventListener(e, quit, {{ once: true, passive: true }});
            const step = () => {{
                if (window.stellyScrollTurn !== turn) stop = true;
                if (!stop) el.scrollTop = top;
                if (!stop && Math.abs(el.scrollTop - top) > 1 && performance.now() < until) {{
                    requestAnimationFrame(step);
                }} else {{
                    for (const e of events) el.removeEventListener(e, quit);
                }}
            }};
            step();
        }})();"#
    ));
}

/// First load, so the app shell does not have to reach into navigation.
/// Favourites, because that is also what the crawl seeds from.
///
/// What the favourites were last time goes up at once, from the disk, and is
/// fetched again underneath like any view `kept` remembers: on a phone the
/// fetch can take seconds, and an empty home page for all of them is the app
/// looking broken.
pub fn open_initial(library: Library) {
    let mut library = library;
    if let Some(mut shelf) = crate::cache::read::<Shelf>(HOME_CACHE) {
        // Held as it came, so the hearts are right before anything lands.
        library.take_liked(Liked::of(&shelf), None);
        shelf.stand_in(Scope::Everything);
        let source = *library.source.peek();
        library.kept.write().push((home(), source, shelf));
    }
    library.show(home());
}

fn home() -> View {
    View::Favourites { scope: Scope::Everything }
}

const HOME_CACHE: &str = "favourites.json";
const RELEASES_CACHE: &str = "releases.json";

/// How far back new releases reach.
const RELEASES_DAYS: u32 = 90;

async fn load(view: View, space_only: bool, only: Option<String>) -> anyhow::Result<Shelf> {
    let mut shelf = Shelf::default();

    match view {
        // Clicking the search tab before typing anything is not a query.
        View::Search { query, .. } if query.trim().is_empty() => {}
        View::Search { query, scope } if space_only => {
            crate::timings::span("search-sent", "search", "space");
            let (albums, artists) = space_groups(&query).await?;
            if scope.albums() {
                shelf.albums = albums;
            }
            if scope.artists() {
                shelf.artists = artists;
            }
        }
        View::Search { query, scope } => {
            crate::timings::span("search-sent", "search", "catalogue");
            let found = backend().search(&query, SEARCH_LIMIT, only.as_deref()).await?;
            if scope.tracks() {
                shelf.tracks = found.tracks;
            }
            // Only what the artist's or album's name brought in: those are
            // the artist and album tiles' to show. Qobuz matches more loosely
            // than a substring test (accents, for one), so a track this test
            // cannot place at all stays.
            if scope.tracks() {
                let terms: Vec<String> =
                    query.to_lowercase().split_whitespace().map(str::to_string).collect();
                shelf.tracks.retain(|track| {
                    let artist = track.artist.to_lowercase();
                    let title = track.title.to_lowercase();
                    let album = track.album.to_lowercase();
                    names_track(&terms, &title)
                        || !terms.iter().all(|term| {
                            artist.contains(term) || title.contains(term) || album.contains(term)
                        })
                });
            }
            if scope.albums() {
                shelf.albums = found.albums;
            }
            if scope.artists() {
                shelf.artists = found.artists;
            }
        }
        View::Favourites { scope } => {
            let (tracks, albums, artists) = tokio::join!(
                async {
                    if scope.tracks() {
                        backend().favourite_tracks(LIST_CAP).await
                    } else {
                        Ok(Vec::new())
                    }
                },
                async {
                    if scope.albums() {
                        backend().favourite_albums(LIST_CAP).await
                    } else {
                        Ok(Vec::new())
                    }
                },
                async {
                    if scope.artists() {
                        backend().favourite_artists(LIST_CAP).await
                    } else {
                        Ok(Vec::new())
                    }
                },
            );
            // Every liked track, even those `stand_in` takes off later.
            shelf.tracks = tracks?;
            shelf.albums = albums?;
            shelf.artists = artists?;
        }
        View::Releases => shelf.albums = backend().recent_releases(RELEASES_DAYS).await?,
        View::Playlists => shelf.playlists = backend().playlists(LIST_CAP).await?,
        View::Playlist { id, .. } => shelf.tracks = backend().playlist_tracks(id, LIST_CAP).await?,
        View::Album(album) => shelf.tracks = backend().album_tracks(&album.id).await?,
        View::Artist(artist) => {
            // Asked together: neither needs the other, and one after the
            // other the page waited out both round trips.
            let (albums, similar) = tokio::join!(
                backend().artist_albums(artist.id, LIST_CAP),
                backend().similar_artists(artist.id, 20),
            );
            shelf.albums = albums?;
            // Not fatal: an artist with no similar list should still show a
            // discography rather than an error.
            shelf.similar = similar.unwrap_or_default();
        }
        // Pages that carry what they show, or fetch it themselves.
        View::Track(_) | View::Settings => {}
    }

    Ok(shelf)
}

/// The albums and artists behind the space's tracks matching `query`, for a
/// space-only search. Matched the way the space list is, every word somewhere
/// in the artist, title or album, and put in the shelf so the tiles, their
/// menus and the liked filter all work as they do for a Qobuz search.
async fn space_groups(query: &str) -> anyhow::Result<(Vec<RemoteAlbum>, Vec<RemoteArtist>)> {
    if query.trim().is_empty() {
        return Ok(Default::default());
    }
    let found = backend()
        .space_search(query, crate::api::SpaceSort::Rank, false, 0)
        .await?;
    let albums = found
        .albums
        .into_iter()
        .map(|album| RemoteAlbum {
            image: album.image,
            id: album.id,
            title: album.title,
            artist: album.artist,
            artist_id: album.artist_id,
            ..Default::default()
        })
        .collect();
    let artists = found
        .artists
        .into_iter()
        .map(|artist| RemoteArtist { id: artist.id, name: artist.name, ..Default::default() })
        .collect();
    Ok((albums, artists))
}

// ------------------------------------------------------------ adding to space

/// Put an album or artist on the map, ahead of the rest of the backlog. The
/// notice says it was asked for; the pipeline output in settings says how far
/// it has got.
pub(crate) fn add_to_space(library: Library, target: Target, label: &str) {
    let mut library = library;
    let label = label.to_string();
    spawn(async move {
        library.notice.set(Some(match backend().pipeline_add(&target).await {
            Ok(()) => format!("adding {label} to the space…"),
            Err(err) => format!("{err:#}"),
        }));
    });
}

// --------------------------------------------------------------- components

/// The main area: a slim bar for getting around, then whichever screen the
/// view names. Left of the generate panel at every width, under it on a phone
/// when that slides in.
#[component]
pub fn MainScreen() -> Element {
    let library = use_context::<Library>();
    let map = use_context::<MapView>();
    let search = use_context::<Search>();
    let view = library.view.read().clone();
    let has_history = !library.history.read().is_empty();

    // Drives every navigation. Deliberately here, mounted for the life of the
    // window, rather than in `show`: see the comment there.
    use_effect(move || library.drive());

    // After every render the shelf or the loading flag asked for, which is
    // how a navigation's timing knows its page is on screen. With timings off
    // it reads neither, so it never runs again.
    use_effect(move || {
        if !crate::timings::enabled() {
            return;
        }
        let _ = library.shelf.read();
        crate::timings::nav_rendered(*library.loading.read());
    });

    // After the render the new page came in with, so the scroll lands on it
    // and not on the page it replaced. See `Library::restore`.
    use_effect(move || {
        let mut restore = library.restore;
        let wanted = *restore.read();
        if let Some(top) = wanted {
            restore.set(None);
            scroll_to(top);
        }
    });

    let home = matches!(view, View::Favourites { .. });
    let searching = matches!(view, View::Search { .. });
    let settings = matches!(view, View::Settings);
    let releases = matches!(view, View::Releases);

    rsx! {
        section { class: "screen",
            nav { class: "topbar",
                if has_history {
                    button {
                        class: "icon-btn",
                        title: "back",
                        onclick: move |_| {
                            map.close();
                            library.back();
                        },
                        {icons::back()}
                    }
                }
                h2 { class: "ellipsis", "{view.label()}" }
                span { class: "spacer" }
                button {
                    class: if home { "icon-btn active" } else { "icon-btn" },
                    title: "home",
                    onclick: move |_| {
                        map.close();
                        library.home();
                    },
                    {icons::home()}
                }
                button {
                    class: if releases { "icon-btn active" } else { "icon-btn" },
                    title: "new releases",
                    onclick: move |_| {
                        map.close();
                        library.toggle_releases();
                    },
                    {icons::releases()}
                }
                button {
                    class: if searching { "icon-btn active" } else { "icon-btn" },
                    title: if searching { "close search" } else { "search" },
                    onclick: move |_| {
                        map.close();
                        library.toggle_search(search.text.peek().trim().to_string());
                    },
                    {icons::search()}
                }
                button {
                    class: if settings { "icon-btn active" } else { "icon-btn" },
                    title: "settings",
                    onclick: move |_| {
                        map.close();
                        library.toggle_settings();
                    },
                    {icons::settings()}
                }
            }

            div {
                class: "screen-body",
                onscroll: move |event: Event<ScrollData>| {
                    let mut scroll = library.scroll;
                    scroll.set(event.scroll_top());
                },
                match view {
                    // Keyed on what they show: going from one album straight
                    // to another is otherwise a prop change on the same
                    // component, and whatever the first one fetched for
                    // itself would sit under the second one's title.
                    View::Album(album) => rsx! { AlbumScreen { key: "{album.id}", album } },
                    View::Artist(artist) => rsx! { ArtistScreen { key: "{artist.id}", artist } },
                    View::Track(track) => rsx! { TrackScreen { key: "{track.id}", track } },
                    View::Settings => rsx! { SettingsScreen {} },
                    _ => rsx! { BrowseScreen {} },
                }
            }
        }
    }
}

/// The search box, and where it looks.
///
/// Its own component, and deliberately propless, so Dioxus memoises it: it
/// then re-renders when the text changes and at no other time.
///
/// That is not tidiness, it is the fix for a real bug. A controlled input in
/// a webview is a race, the keystroke travels to Rust, a render patches
/// `value` back into the DOM, and whatever was typed in the meantime is
/// overwritten. Re-rendering whenever anything else did, including when a
/// search result landed, which is precisely when you are still typing, lost
/// characters.
#[component]
fn SearchBar() -> Element {
    let library = use_context::<Library>();
    let search = use_context::<Search>();
    let provider = use_context::<super::Provider>();
    let provider_title = provider.title();
    let each = provider.each();
    let only = library.only.read().clone();
    let mut query = search.text;
    let source = *library.source.read();

    // The magnifier opened this screen to type into, so the keyboard should
    // already be up.
    use_effect(|| {
        document::eval(
            "setTimeout(() => { const el = document.querySelector('.search-bar .search'); if (el) el.focus(); }, 0);",
        );
    });

    // A source, and for the catalogue the one provider to ask, if any.
    let pick = move |wanted: Source, provider: Option<String>| {
        let (mut source, mut only) = (library.source, library.only);
        let provider = if wanted == Source::Space { only.peek().clone() } else { provider };
        if *source.peek() != wanted || *only.peek() != provider {
            source.set(wanted);
            only.set(provider);
            // Re-run rather than hide a half: a hidden shelf would still be
            // what "play all" plays.
            let view = library.view.peek().clone();
            library.show(view);
        }
    };

    rsx! {
        div { class: "search-bar",
            div { class: "search-wrap",
                input {
                    class: "search",
                    placeholder: "artist, title or album, any order",
                    value: "{query}",
                    oninput: move |e| {
                        // A search is timed from its last keystroke.
                        crate::timings::start("search");
                        query.set(e.value());
                    },
                    // Enter skips the debounce. The local list has already
                    // filtered; this is impatience with the round trip.
                    onkeydown: move |event| {
                        if event.key() != Key::Enter {
                            return;
                        }
                        let mut search = search;
                        let text = search.text.peek().trim().to_string();
                        if text.is_empty() || text == *search.submitted.peek() {
                            return;
                        }
                        search.submitted.set(text.clone());
                        library.remember_search(&text);
                        library.search_to(text);
                    },
                }
                if !query().is_empty() {
                    button {
                        class: "clear-search",
                        title: "clear",
                        onclick: move |_| query.set(String::new()),
                        "×"
                    }
                }
            }
            // Where to look, as a phrase: one dropdown, so the box keeps
            // its width on a phone, and the closed one says it in words.
            select {
                class: "source-select",
                title: "where to search",
                onchange: move |event| {
                    let value = event.value();
                    match value.as_str() {
                        "space" => pick(Source::Space, None),
                        "all" => pick(Source::Catalogue, None),
                        name => pick(Source::Catalogue, Some(name.trim_start_matches("in:").to_string())),
                    }
                },
                option { value: "space", selected: source == Source::Space, "in your space" }
                if each.is_empty() {
                    option { value: "all", selected: source == Source::Catalogue, "in {provider_title}" }
                } else {
                    option {
                        value: "all",
                        selected: source == Source::Catalogue && only.is_none(),
                        "in all providers"
                    }
                    for one in each {
                        option {
                            key: "{one.name}",
                            value: "in:{one.name}",
                            selected: source == Source::Catalogue && only.as_deref() == Some(one.name.as_str()),
                            "in {one.title}"
                        }
                    }
                }
            }
        }
    }
}

/// Favourites, a search, playlists: one list of tracks, albums and artists,
/// narrowed by the chips above it.
#[component]
fn BrowseScreen() -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();

    let blocklist = use_context::<Blocklist>();
    let view = library.view.read().clone();
    let searching = matches!(view, View::Search { .. });
    // Home or a search with no chip lit: tracks, albums and artists in one
    // list, sorted together, rather than a section for each.
    let mixed = view.filter() == Some(Scope::Everything);
    // Only while searching the space, see `Library::shows_space`.
    let show_space = library.shows_space();
    let space_only = library.searching_space_only();
    let shelf = library.shelf.read().clone();

    // Every emptiness test below asks about what is *visible*: no heading over
    // nothing, and the bulk actions must not reach past the filter.
    let space = use_context::<SpaceMatches>();
    // The whole shelf, for the bulk actions: "play all" means the view's
    // tracks, including ones the space section happens to be showing.
    let shelf_tracks = shelf.visible_tracks(&blocklist);
    let (liked_only, liked) = library.liked_state();
    let tracks = catalogue_only(shelf_tracks.clone(), &space_ids(&library, &space));
    let tracks: Vec<(usize, RemoteTrack)> = if liked_only {
        tracks.into_iter().filter(|(_, t)| liked.tracks.contains(&t.id)).collect()
    } else {
        tracks
    };
    let albums = shelf.visible_albums(&blocklist);
    let albums: Vec<crate::catalogue::RemoteAlbum> = if liked_only {
        albums.into_iter().filter(|a| liked.albums.contains(&a.id)).collect()
    } else {
        albums
    };
    let artists = shelf.visible_artists(&blocklist, false);
    let artists: Vec<crate::catalogue::RemoteArtist> = if liked_only {
        artists.into_iter().filter(|a| liked.artists.contains(&a.id)).collect()
    } else {
        artists
    };
    let nothing_from_catalogue =
        tracks.is_empty() && albums.is_empty() && artists.is_empty() && shelf.playlists.is_empty();
    let nothing_visible = nothing_from_catalogue && (!show_space || space.0.read().1 == 0);
    let empty_search = matches!(&view, View::Search { query, .. } if query.trim().is_empty());

    rsx! {
        if searching {
            SearchBar {}
        }

        // What kind of thing you are looking at. Home and a search are both
        // mixed, liked or found tracks, albums and artists together, and the
        // three chips narrow either one, exclusively: a tap narrows, a second
        // tap on the lit one widens back. Playlists is a place of its own
        // rather than a narrowing, and not something a search returns.
        div { class: "toolbar",
            div { class: "filters",
                for (label, chip) in [
                    ("tracks", Scope::Tracks),
                    ("albums", Scope::Albums),
                    ("artists", Scope::Artists),
                ] {
                    button {
                        class: if view.filter() == Some(chip) { "chip active" } else { "chip" },
                        onclick: move |_| library.filter(chip),
                        "{label}"
                    }
                }
                if !searching {
                    button {
                        class: if view.is_playlists() { "chip active" } else { "chip" },
                        onclick: move |_| library.go(View::Playlists),
                        "playlists"
                    }
                }
                if searching {
                    button {
                        class: if liked_only { "chip active" } else { "chip" },
                        title: "only show what you've already liked",
                        onclick: move |_| {
                            let mut liked_only = library.liked_only;
                            let now = liked_only();
                            liked_only.set(!now);
                            if !now {
                                library.ensure_liked_loaded();
                            }
                        },
                        "liked"
                    }
                }
            }
            span { class: "spacer" }
            // The results below are the last query's until this one lands.
            if *library.refining.read() {
                span { class: "busy-dot", title: "searching" }
            }
            SortMenu {}
            if mixed {
                GroupingToggle {}
            }
            ViewToggle {}
        }

        if !shelf_tracks.is_empty() {
            div { class: "actions",
                button {
                    class: "chip",
                    onclick: move |_| {
                        let queue = library.shelf.peek().visible_tracks(&blocklist);
                        play_list(player, queue, 0);
                    },
                    "play all"
                }
                button {
                    class: "chip",
                    onclick: move |_| {
                        let queue = library.shelf.peek().visible_tracks(&blocklist);
                        play_next(player, queue);
                    },
                    "next"
                }
                button {
                    class: "chip",
                    onclick: move |_| {
                        let queue = library.shelf.peek().visible_tracks(&blocklist);
                        enqueue(player, queue);
                    },
                    "queue"
                }
            }
        }

        if let Some(message) = library.notice.read().clone() {
            p { class: "muted notice", "{message}" }
        }

        if *library.loading.read() && !*library.refining.read() {
            p { class: "muted", "loading…" }
        } else if let Some(message) = library.error.read().clone() {
            p { class: "muted error", "{message}" }
        } else {
            div { class: "shelf",
                // A space-only mixed search puts the space's tracks in the one
                // list with its albums and artists, see `MixedRows`.
                if show_space && !(mixed && space_only) {
                    SpaceRows {}
                }

                if mixed {
                    if !nothing_from_catalogue || space_only {
                        MixedRows {}
                    }
                } else {
                    if !artists.is_empty() {
                        ArtistRows { heading: "Artists".to_string(), similar: false }
                    }
                    if !albums.is_empty() {
                        AlbumRows {}
                    }
                    if !shelf.playlists.is_empty() {
                        PlaylistRows {}
                    }
                    if !tracks.is_empty() {
                        TrackRows {}
                    }
                }
                if empty_search {
                    SearchHistory {}
                } else if nothing_visible && matches!(view, View::Favourites { scope: Scope::Everything }) {
                    p { class: "muted", "No likes yet. Heart anything to keep it here." }
                } else if nothing_visible && view == View::Releases {
                    p { class: "muted",
                        "Nothing new from the artists you like in the last {RELEASES_DAYS} days. Heart more artists to follow them here."
                    }
                } else if nothing_visible {
                    p { class: "muted", "nothing here" }
                }
            }
        }
    }
}

/// Grid for covers, list for the details that do not fit a tile. One switch
/// for every screen that has both.
#[component]
pub(crate) fn ViewToggle() -> Element {
    let library = use_context::<Library>();
    let current = library.layout();

    rsx! {
        div { class: "view-toggle",
            button {
                class: if current == TracksView::Grid { "icon-btn active" } else { "icon-btn" },
                title: "grid",
                onclick: move |_| library.set_layout(TracksView::Grid),
                {icons::grid()}
            }
            button {
                class: if current == TracksView::List { "icon-btn active" } else { "icon-btn" },
                title: "list",
                onclick: move |_| library.set_layout(TracksView::List),
                {icons::list()}
            }
        }
    }
}

/// One list, or a section per kind. Beside grid and list, and only on a mixed
/// view, the one kind of page with more than one kind of thing in it.
#[component]
fn GroupingToggle() -> Element {
    let library = use_context::<Library>();
    let current = library.grouping();

    rsx! {
        div { class: "view-toggle",
            button {
                class: if current == Grouping::Mixed { "icon-btn active" } else { "icon-btn" },
                title: "all together",
                onclick: move |_| library.set_grouping(Grouping::Mixed),
                {icons::mixed()}
            }
            button {
                class: if current == Grouping::Split { "icon-btn active" } else { "icon-btn" },
                title: "by type",
                onclick: move |_| library.set_grouping(Grouping::Split),
                {icons::split()}
            }
        }
    }
}

/// The sort chip and the menu it opens: what to order by, a rule, then which
/// way round. Stays open across picks, since a sort is usually both.
#[component]
fn SortMenu() -> Element {
    let library = use_context::<Library>();
    let mut open = use_signal(|| None::<(f64, f64)>);
    let sort = *library.sort.read();

    // Kept on screen the way the context menu is: opened from near the right
    // edge, it would otherwise run off it.
    use_effect(move || {
        if open().is_some() {
            document::eval(
                "const el = document.querySelector('.sort-menu');
                 if (el) {
                   el.style.transform = 'none';
                   const box = el.getBoundingClientRect();
                   const dx = Math.min(0, window.innerWidth - 8 - box.right);
                   const dy = Math.min(0, window.innerHeight - 8 - box.bottom);
                   el.style.transform = `translate(${dx}px, ${dy}px)`;
                 }",
            );
        }
    });

    let arrow = if sort.descending { "↓" } else { "↑" };

    rsx! {
        button {
            class: if sort == Sort::default() { "chip" } else { "chip active" },
            title: "sort",
            onclick: move |event: Event<MouseData>| {
                let point = event.client_coordinates();
                open.set(Some((point.x, point.y)));
            },
            "{sort.key.label()} {arrow}"
        }
        if let Some((x, y)) = open() {
            div { class: "menu-backdrop", onclick: move |_| open.set(None),
                div {
                    class: "context-menu sort-menu",
                    style: "left: {x}px; top: {y}px;",
                    onclick: move |event: Event<MouseData>| event.stop_propagation(),
                    for key in SortKey::ALL {
                        button {
                            class: if sort.key == key { "menu-item checked" } else { "menu-item" },
                            onclick: move |_| library.set_sort(Sort { key, ..sort }),
                            "{key.label()}"
                        }
                    }
                    div { class: "menu-rule" }
                    for (label, descending) in [("ascending", false), ("descending", true)] {
                        button {
                            class: if sort.descending == descending { "menu-item checked" } else { "menu-item" },
                            onclick: move |_| library.set_sort(Sort { descending, ..sort }),
                            "{label}"
                        }
                    }
                }
            }
        }
    }
}

/// One entry of a mixed view's single list. Shelf entries carry their index
/// into the matching `visible_*` list, which is how their menus and "play
/// from here" address them, the same as in the per-kind sections.
#[derive(Clone, PartialEq)]
enum Item {
    Track(usize, RemoteTrack),
    Album(usize, RemoteAlbum),
    Artist(usize, RemoteArtist),
    Space(SpaceRow),
}

impl Item {
    /// What the sort compares, across kinds: "title" is an artist's name,
    /// "artist" an album's artist. A key a kind has nothing for (an artist's
    /// release date) puts it after everything that has one.
    fn sort_value(&self, key: SortKey) -> Option<SortValue> {
        match (self, key) {
            (Item::Track(_, track), SortKey::Liked) => track.liked_at.map(SortValue::Number),
            (Item::Album(_, album), SortKey::Liked) => album.liked_at.map(SortValue::Number),
            (Item::Artist(_, artist), SortKey::Liked) => artist.liked_at.map(SortValue::Number),
            (Item::Track(_, track), SortKey::Title) => text(&track.title),
            (Item::Track(_, track), SortKey::Artist) => text(&track.artist),
            (Item::Track(_, track), SortKey::Album) => text(&track.album),
            (Item::Track(_, track), SortKey::Released) => track.released.as_deref().and_then(text),
            (Item::Track(_, track), SortKey::Duration) => track.duration.map(SortValue::Number),
            (Item::Album(_, album), SortKey::Title | SortKey::Album) => text(&album.title),
            (Item::Album(_, album), SortKey::Artist) => text(&album.artist),
            (Item::Album(_, album), SortKey::Released) => album.released.as_deref().and_then(text),
            (Item::Artist(_, artist), SortKey::Title | SortKey::Artist) => text(&artist.name),
            (Item::Space(row), SortKey::Title) => text(&row.title),
            (Item::Space(row), SortKey::Artist) => text(&row.artist),
            (Item::Space(row), SortKey::Album) => text(&row.album),
            _ => None,
        }
    }

    /// Its id, told apart by kind: a track and an album may share a number.
    fn id(&self) -> String {
        match self {
            Item::Track(_, track) => format!("track-{}", track.id),
            Item::Album(_, album) => format!("album-{}", album.id),
            Item::Artist(_, artist) => format!("artist-{}", artist.id),
            Item::Space(row) => format!("space-{}", row.track_id),
        }
    }
}

/// A mixed view's list: tracks, albums and artists together, in one order.
/// "Default" is artists, then albums, then tracks, each as they came; any
/// other sort interleaves them, unless split by kind, see `Grouping`.
#[component]
fn MixedRows() -> Element {
    let only = use_context::<Library>().only.read().clone();
    // The provider the search asked, or all of them.
    let provider_title = use_context::<super::Provider>().title_of(only.as_deref());
    let library = use_context::<Library>();
    let player = use_context::<Player>();
    let local = use_context::<LocalIds>();
    let blocklist = use_context::<Blocklist>();
    let selection = use_context::<Selection>().0;
    let menu = use_context::<ContextMenu>().0;
    let reach = use_context::<SpaceReach>().0;
    let matches = use_context::<SpaceMatches>();

    let (liked_only, liked) = library.liked_state();
    let space_only = library.searching_space_only();
    let searching = matches!(&*library.view.read(), View::Search { .. });
    let grid = library.layout() == TracksView::Grid;
    let now_playing = player.current().map(|t| t.id);

    let mut items: Vec<Item> = Vec::new();
    {
        let shelf = library.shelf.read();
        items.extend(
            shelf
                .visible_artists(&blocklist, false)
                .into_iter()
                .enumerate()
                .filter(|(_, a)| !liked_only || liked.artists.contains(&a.id))
                .map(|(index, artist)| Item::Artist(index, artist)),
        );
        items.extend(
            shelf
                .visible_albums(&blocklist)
                .into_iter()
                .enumerate()
                .filter(|(_, a)| !liked_only || liked.albums.contains(&a.id))
                .map(|(index, album)| Item::Album(index, album)),
        );
        items.extend(
            catalogue_only(shelf.visible_tracks(&blocklist), &space_ids(&library, &matches))
                .into_iter()
                .filter(|(_, t)| !liked_only || liked.tracks.contains(&t.id))
                .map(|(index, track)| Item::Track(index, track)),
        );
    }
    if space_only {
        items.extend(
            matches
                .0
                .read()
                .0
                .iter()
                .filter(|row| !liked_only || liked.tracks.contains(&row.track_id))
                .cloned()
                .map(Item::Space),
        );
    }
    let sort = *library.sort.read();
    let items = sort.apply(items, |item, key| item.sort_value(key));
    // Keyed here rather than on the `li`: `row` is called from a loop, and
    // the loop's own nodes are what get matched up between renders.
    let keys = row_keys(items.iter().map(Item::id));
    let items: Vec<(String, Item)> = keys.into_iter().zip(items).collect();

    if items.is_empty() {
        return rsx! {};
    }

    let tile = if grid { "tile" } else { "row" };
    let split = library.grouping() == Grouping::Split;

    let row = move |item: Item| -> Element {
        let mut menu = menu;
        match item {
            Item::Track(index, track) => rsx! {
                li {
                    class: if now_playing == Some(track.id) { "{tile} playing" } else { "{tile}" },
                    // The highlight follows the track from here, see
                    // `use_playing_highlight`.
                    "data-track": "{track.id}",
                    onclick: {
                        let track = track.clone();
                        move |_| open_remote_track(library, selection, &local, track.clone())
                    },
                    "data-menu": MenuTarget::ShelfTrack(index).tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfTrack(index));
                    },
                    Cover {
                        url: track.image.clone(),
                        class: if grid { String::new() } else { "thumb".to_string() },
                    }
                    if grid {
                        {space_mark(local.0.read().contains(&track.id))}
                        span { class: "title", "{track.title}" }
                        {artist_link(library, track.artist_id, track.artist.clone(), "artist")}
                    } else {
                        {artist_link(library, track.artist_id, track.artist.clone(), "artist")}
                        span { class: "title", "{track.title}" }
                        if local.0.read().contains(&track.id) {
                            span { class: "in-space-dot", title: "analysed, on the map", "•" }
                        }
                        span { class: "muted", "{track.duration_label()}" }
                    }
                    {menu_button(menu, MenuTarget::ShelfTrack(index))}
                }
            },
            Item::Album(index, album) => rsx! {
                li {
                    class: "{tile}",
                    onclick: {
                        let album = album.clone();
                        move |_| library.go(View::Album(album.clone()))
                    },
                    "data-menu": MenuTarget::ShelfAlbum(index).tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfAlbum(index));
                    },
                    Cover {
                        url: album.image.clone(),
                        class: if grid { String::new() } else { "thumb".to_string() },
                    }
                    if grid {
                        span { class: "kind", "album" }
                        {space_mark(reach.read().0.contains(&album.id))}
                        span { class: "title", "{album.title}" }
                        {artist_link(library, album.artist_id, album.artist.clone(), "artist")}
                    } else {
                        {artist_link(library, album.artist_id, album.artist.clone(), "artist")}
                        span { class: "title", "{album.title}" }
                        span { class: "muted", "album" }
                    }
                    {menu_button(menu, MenuTarget::ShelfAlbum(index))}
                }
            },
            Item::Artist(index, artist) => rsx! {
                li {
                    class: "{tile}",
                    onclick: {
                        let artist = artist.clone();
                        move |_| library.go(View::Artist(artist.clone()))
                    },
                    "data-menu": MenuTarget::ShelfArtist { index, similar: false }.tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfArtist { index, similar: false });
                    },
                    Cover {
                        url: artist.image.clone(),
                        class: if grid { "round".to_string() } else { "thumb round".to_string() },
                    }
                    if grid {
                        {space_mark(reach.read().1.contains(&artist.id))}
                        span { class: "title", "{artist.name}" }
                        span { class: "artist", "artist" }
                    } else {
                        span { class: "artist", "{artist.name}" }
                        span { class: "title" }
                        span { class: "muted", "artist" }
                    }
                    {menu_button(menu, MenuTarget::ShelfArtist { index, similar: false })}
                }
            },
            Item::Space(row) => rsx! {
                li {
                    class: if selection.read().as_ref() == Some(&row.track_id) { "{tile} selected" } else { "{tile}" },
                    onclick: {
                        let id = row.track_id;
                        move |_| open_track(library, selection, id)
                    },
                    "data-menu": MenuTarget::SpaceTrack(row.track_id).tag(),
                    oncontextmenu: {
                        let id = row.track_id;
                        move |event: Event<MouseData>| {
                            event.prevent_default();
                            open_menu(&mut menu, &event, MenuTarget::SpaceTrack(id));
                        }
                    },
                    Cover {
                        url: row.image.clone(),
                        class: if grid { String::new() } else { "thumb".to_string() },
                    }
                    if grid {
                        span { class: "title", "{row.title}" }
                        {artist_link(library, row.artist_id, row.artist.clone(), "artist")}
                    } else {
                        {artist_link(library, row.artist_id, row.artist.clone(), "artist")}
                        span { class: "title", "{row.title}" }
                    }
                    {menu_button(menu, MenuTarget::SpaceTrack(row.track_id))}
                }
            },
        }
    };

    // Split, the sort still orders each section; it just no longer
    // interleaves them. Artists, albums, then tracks, as in a narrowed view.
    let sections: Vec<(&str, Vec<(String, Item)>)> = if split {
        let mut artists = Vec::new();
        let mut albums = Vec::new();
        let mut tracks = Vec::new();
        for item in items {
            match item.1 {
                Item::Artist(..) => artists.push(item),
                Item::Album(..) => albums.push(item),
                Item::Track(..) | Item::Space(_) => tracks.push(item),
            }
        }
        [("Artists", artists), ("Albums", albums), ("Tracks", tracks)]
            .into_iter()
            .filter(|(_, items)| !items.is_empty())
            .collect()
    } else {
        vec![("", items)]
    };

    rsx! {
        // Under the space's own section in a search, so the two lists say
        // where each came from. Home is all Qobuz, and a space-only search
        // is all space, so neither needs it.
        if searching && !space_only {
            h3 { class: "shelf-head source-head", "{provider_title}" }
        }
        for (heading, items) in sections {
            if !heading.is_empty() {
                h3 { class: "shelf-head", "{heading}" }
            }
            ul { class: if grid { "tiles" } else { "list" },
                for (key, item) in items {
                    Fragment { key: "{key}", {row(item)} }
                }
            }
        }
    }
}

/// Track ids already listed under "In your space", none when that section
/// is not showing, or the Qobuz list would drop them with nowhere to go.
fn space_ids(library: &Library, matches: &SpaceMatches) -> std::collections::HashSet<i64> {
    if !library.shows_space() {
        return Default::default();
    }
    matches.0.read().0.iter().map(|row| row.track_id).collect()
}

/// Shelf tracks that the space section is not already showing, each paired
/// with its index into `visible_tracks`.
///
/// The index is the row's address: `menu.rs` re-reads `visible_tracks` when an
/// item is chosen, so it must survive the filter. Enumerating before filtering
/// is what keeps it correct, renumbering after would point every menu at a
/// different track, which is the bug `visible_tracks` itself was introduced to
/// fix.
fn catalogue_only(
    tracks: Vec<RemoteTrack>,
    in_space: &std::collections::HashSet<i64>,
) -> Vec<(usize, RemoteTrack)> {
    tracks
        .into_iter()
        .enumerate()
        .filter(|(_, track)| !in_space.contains(&track.id))
        .collect()
}

/// Tracks the space already holds, as the first section of the one list.
///
/// Addressed by track id, not by position: these rows come from the space's
/// own scan rather than from `Shelf`, so they must not share the shelf's
/// index space. That separation is the point, one list to read, two
/// independently addressed sections underneath, so the menu cannot act on a
/// neighbour of the row you opened it on.
#[component]
fn SpaceRows() -> Element {
    let library = use_context::<Library>();
    let matches = use_context::<SpaceMatches>().0;
    let selection = use_context::<Selection>().0;
    let mut menu = use_context::<ContextMenu>().0;
    let search = use_context::<Search>();

    // Grows on request rather than rendering 720 rows nobody scrolled to.
    let mut shown = use_signal(|| FIRST_SHOWN);

    let (rows, total) = matches();
    // With Qobuz out of the search, "liked" has only the space left to
    // narrow. Counted over the rows the scan kept, not every match.
    let (liked_only, liked) = library.liked_state();
    let liked_only = liked_only && library.searching_space_only();
    let (rows, total) = if liked_only {
        let rows: Vec<SpaceRow> =
            rows.into_iter().filter(|row| liked.tracks.contains(&row.track_id)).collect();
        let total = rows.len();
        (rows, total)
    } else {
        (rows, total)
    };
    // Whether the box is empty, not what is in it: read directly, every
    // keystroke would draw all these rows again.
    let searching = use_memo(move || !search.text.read().trim().is_empty());
    let visible = shown().min(rows.len());
    let grid = library.layout() == TracksView::Grid;

    if total == 0 {
        return rsx! {
            if searching() {
                h3 { class: "shelf-head source-head", "In your space" }
                if liked_only {
                    p { class: "muted", "Nothing you've liked matches." }
                } else {
                    p { class: "muted", "Nothing matches. Every word has to appear somewhere." }
                }
            }
        };
    }

    rsx! {
        h3 { class: "shelf-head source-head",
            "In your space"
            span { class: "spacer" }
            span { class: "muted",
                if visible < total { "{visible} of {total}" } else { "{total}" }
            }
        }

        ul { class: if grid { "tiles" } else { "list" },
            for row in rows.iter().take(visible) {
                li {
                    key: "{row.track_id}",
                    class: {
                        let base = if grid { "tile" } else { "row" };
                        if selection.read().as_ref() == Some(&row.track_id) {
                            format!("{base} selected")
                        } else {
                            base.to_string()
                        }
                    },
                    onclick: {
                        let id = row.track_id;
                        move |_| open_track(library, selection, id)
                    },
                    "data-menu": MenuTarget::SpaceTrack(row.track_id).tag(),
                    oncontextmenu: {
                        let id = row.track_id;
                        move |event: Event<MouseData>| {
                            event.prevent_default();
                            open_menu(&mut menu, &event, MenuTarget::SpaceTrack(id));
                        }
                    },
                    Cover {
                        url: row.image.clone(),
                        class: if grid { String::new() } else { "thumb".to_string() },
                    }
                    // Tiles read top-down as title-then-artist everywhere
                    // else (albums, artists); rows read artist-then-title.
                    // Grid mode follows the tile convention it just joined
                    // rather than keeping the row order under a cover.
                    if grid {
                        span { class: "title", "{row.title}" }
                        {artist_link(library, row.artist_id, row.artist.clone(), "artist")}
                    } else {
                        {artist_link(library, row.artist_id, row.artist.clone(), "artist")}
                        span { class: "title", "{row.title}" }
                    }
                    {menu_button(menu, MenuTarget::SpaceTrack(row.track_id))}
                }
            }
        }

        if visible < total {
            button {
                class: "chip more-rows",
                onclick: move |_| {
                    let next = shown() * 3;
                    shown.set(next);
                },
                "show more"
            }
        }
    }
}

impl Shelf {
    fn sorted(self, sort: Sort) -> Shelf {
        let artist = |artist: &RemoteArtist, key: SortKey| match key {
            SortKey::Liked => artist.liked_at.map(SortValue::Number),
            SortKey::Title | SortKey::Artist => text(&artist.name),
            _ => None,
        };
        Shelf {
            tracks: sort.apply(self.tracks, |track, key| match key {
                SortKey::Default => None,
                SortKey::Liked => track.liked_at.map(SortValue::Number),
                SortKey::Title => text(&track.title),
                SortKey::Artist => text(&track.artist),
                SortKey::Album => text(&track.album),
                SortKey::Released => track.released.as_deref().and_then(text),
                SortKey::Duration => track.duration.map(SortValue::Number),
            }),
            albums: sort.apply(self.albums, |album, key| match key {
                SortKey::Liked => album.liked_at.map(SortValue::Number),
                SortKey::Title | SortKey::Album => text(&album.title),
                SortKey::Artist => text(&album.artist),
                SortKey::Released => album.released.as_deref().and_then(text),
                _ => None,
            }),
            artists: sort.apply(self.artists, artist),
            playlists: sort.apply(self.playlists, |playlist, key| match key {
                SortKey::Title => text(&playlist.name),
                _ => None,
            }),
            similar: sort.apply(self.similar, artist),
        }
    }

    /// The rows a blocklist leaves visible, in display order. Rendering and
    /// the click handlers must both go through these, filtering in one and
    /// indexing the unfiltered list in the other made row N open row N+1.
    pub fn visible_tracks(&self, blocked: &Blocklist) -> Vec<RemoteTrack> {
        self.tracks
            .iter()
            .filter(|track| !blocked.hides(track))
            .cloned()
            .collect()
    }

    pub fn visible_albums(&self, blocked: &Blocklist) -> Vec<crate::catalogue::RemoteAlbum> {
        self.albums
            .iter()
            .filter(|album| !album.artist_id.is_some_and(|id| blocked.contains(id)))
            .cloned()
            .collect()
    }

    pub fn visible_artists(
        &self,
        blocked: &Blocklist,
        similar: bool,
    ) -> Vec<crate::catalogue::RemoteArtist> {
        let source = if similar { &self.similar } else { &self.artists };
        source
            .iter()
            .filter(|artist| !blocked.contains(artist.id))
            .cloned()
            .collect()
    }
}

#[component]
fn TrackRows() -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();
    let local = use_context::<LocalIds>();
    let blocklist = use_context::<Blocklist>();
    let selection = use_context::<Selection>().0;
    let mut menu = use_context::<ContextMenu>().0;

    let matches = use_context::<SpaceMatches>();

    // Matched on the Qobuz track id: that is the same recording *and* the
    // same release, so suppressing it hides nothing you could not already
    // reach. A different release of the same song stays, because fetching
    // that one is a real thing to want.
    let tracks = library.shelf.read().visible_tracks(&blocklist);
    let tracks = catalogue_only(tracks, &space_ids(&library, &matches));
    let (liked_only, liked) = library.liked_state();
    let tracks: Vec<(usize, RemoteTrack)> = if liked_only {
        tracks.into_iter().filter(|(_, t)| liked.tracks.contains(&t.id)).collect()
    } else {
        tracks
    };
    let now_playing = player.current().map(|t| t.id);
    let grid = library.layout() == TracksView::Grid;
    let count = tracks.len();
    let keys = row_keys(tracks.iter().map(|(_, track)| track.id));

    if tracks.is_empty() {
        return rsx! {};
    }

    rsx! {
        h3 { class: "shelf-head",
            "Tracks"
            // Qobuz's own cap, not this shelf's, see `SEARCH_LIMIT` where
            // the request is made. Worth saying so a short list here does
            // not read as "the space has no more of these".
            if count >= SEARCH_LIMIT {
                span { class: "spacer" }
                span { class: "muted", "first {SEARCH_LIMIT}" }
            }
        }
        ul { class: if grid { "tiles" } else { "list" },
            for ((index, track), key) in tracks.into_iter().zip(keys) {
                li {
                    key: "{key}",
                    class: {
                        let base = if grid { "tile" } else { "row" };
                        if now_playing == Some(track.id) {
                            format!("{base} playing")
                        } else {
                            base.to_string()
                        }
                    },
                    "data-track": "{track.id}",
                    // Opens the track's page, as an album or artist tile opens
                    // theirs. Playing is the page's button, the menu's, or
                    // "play all" above.
                    onclick: {
                        let track = track.clone();
                        move |_| open_remote_track(library, selection, &local, track.clone())
                    },
                    "data-menu": MenuTarget::ShelfTrack(index).tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfTrack(index));
                    },
                    Cover {
                        url: track.image.clone(),
                        class: if grid { String::new() } else { "thumb".to_string() },
                    }
                    if grid {
                        {space_mark(local.0.read().contains(&track.id))}
                        // The detail row keeps the streamability tag, the
                        // in-space dot and the duration; a tile is the cover
                        // and just enough text to recognise it by.
                        span { class: "title", "{track.title}" }
                        {artist_link(library, track.artist_id, track.artist.clone(), "artist")}
                    } else {
                        {artist_link(library, track.artist_id, track.artist.clone(), "artist")}
                        span { class: "title", "{track.title}" }
                        if !track.streamable {
                            span { class: "muted tag", "-" }
                        }
                        if local.0.read().contains(&track.id) {
                            // A mark, not a button: that this track is a
                            // point on the map is something to know while
                            // scanning the list, and the menu is where you
                            // act on it.
                            span { class: "in-space-dot", title: "analysed, on the map", "•" }
                        }
                        span { class: "muted", "{track.duration_label()}" }
                    }
                    {menu_button(menu, MenuTarget::ShelfTrack(index))}
                }
            }
        }
    }
}

#[component]
fn AlbumRows() -> Element {
    let library = use_context::<Library>();
    let blocklist = use_context::<Blocklist>();
    let mut menu = use_context::<ContextMenu>().0;
    let reach = use_context::<SpaceReach>().0;
    let (liked_only, liked) = library.liked_state();
    // Enumerated before filtering, same as `catalogue_only`: `index` is this
    // row's address into `visible_albums`, and filtering after numbering
    // would point the menu at whatever slid into a dropped row's place.
    let albums: Vec<(usize, crate::catalogue::RemoteAlbum)> = library
        .shelf
        .read()
        .visible_albums(&blocklist)
        .into_iter()
        .enumerate()
        .filter(|(_, a)| !liked_only || liked.albums.contains(&a.id))
        .collect();
    let keys = row_keys(albums.iter().map(|(_, album)| &album.id));
    // How recent is the point of new releases, so they say when, and what
    // sort of release when it is not a whole album.
    let releases = *library.view.read() == View::Releases;

    rsx! {
        h3 { class: "shelf-head", "Albums" }
        ul { class: "tiles",
            for ((index, album), key) in albums.into_iter().zip(keys) {
                li {
                    key: "{key}",
                    class: "tile",
                    // Straight to the tracklist: the album page carries the
                    // same cover, badges and actions the sheet would have,
                    // so the sheet was only ever a detour on the way there.
                    onclick: {
                        let album = album.clone();
                        move |_| library.go(View::Album(album.clone()))
                    },
                    "data-menu": MenuTarget::ShelfAlbum(index).tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfAlbum(index));
                    },
                    Cover { url: album.image.clone() }
                    if releases {
                        if let Some(kind) = release_kind(&album) {
                            span { class: "kind", "{kind}" }
                        }
                    }
                    {space_mark(reach.read().0.contains(&album.id))}
                    span { class: "title", "{album.title}" }
                    {artist_link(library, album.artist_id, album.artist.clone(), "artist")}
                    if releases {
                        span { class: "date", {album.released.clone().unwrap_or_default()} }
                    }
                    {menu_button(menu, MenuTarget::ShelfAlbum(index))}
                }
            }
        }
    }
}

/// The badge on a new release, none for a plain album.
fn release_kind(album: &crate::catalogue::RemoteAlbum) -> Option<&'static str> {
    use crate::catalogue::ReleaseKind;
    match album.kind? {
        ReleaseKind::Album => None,
        ReleaseKind::Ep => Some("ep"),
        ReleaseKind::Single => Some("single"),
        ReleaseKind::Live => Some("live"),
        ReleaseKind::Compilation => Some("compilation"),
    }
}

#[component]
pub(crate) fn ArtistRows(heading: String, similar: bool) -> Element {
    let library = use_context::<Library>();
    let blocklist = use_context::<Blocklist>();
    let mut menu = use_context::<ContextMenu>().0;
    let reach = use_context::<SpaceReach>().0;
    let (liked_only, liked) = library.liked_state();
    // Never for "similar artists": that section's entire point is showing
    // who you have *not* already reached, so filtering it by what is already
    // liked would just empty it.
    let artists: Vec<(usize, crate::catalogue::RemoteArtist)> = library
        .shelf
        .read()
        .visible_artists(&blocklist, similar)
        .into_iter()
        .enumerate()
        .filter(|(_, a)| similar || !liked_only || liked.artists.contains(&a.id))
        .collect();
    let keys = row_keys(artists.iter().map(|(_, artist)| artist.id));

    rsx! {
        h3 { class: "shelf-head", "{heading}" }
        ul { class: "tiles",
            for ((index, artist), key) in artists.into_iter().zip(keys) {
                li {
                    key: "{key}",
                    class: "tile",
                    onclick: {
                        let artist = artist.clone();
                        move |_| library.go(View::Artist(artist.clone()))
                    },
                    "data-menu": MenuTarget::ShelfArtist { index, similar }.tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfArtist { index, similar });
                    },
                    Cover { url: artist.image.clone(), class: "round" }
                    {space_mark(reach.read().1.contains(&artist.id))}
                    span { class: "title", "{artist.name}" }
                    if let Some(count) = artist.albums_count {
                        span { class: "artist", "{count} albums" }
                    }
                    {menu_button(menu, MenuTarget::ShelfArtist { index, similar })}
                }
            }
        }
    }
}

#[component]
fn PlaylistRows() -> Element {
    let library = use_context::<Library>();
    let playlists = library.shelf.read().playlists.clone();
    let keys = row_keys(playlists.iter().map(|playlist| &playlist.id));

    rsx! {
        h3 { class: "shelf-head", "Playlists" }
        ul { class: "list",
            for ((index, playlist), key) in playlists.into_iter().enumerate().zip(keys) {
                li {
                    key: "{key}",
                    class: "row",
                    onclick: move |_| {
                        let target = {
                            let shelf = library.shelf.peek();
                            shelf.playlists.get(index).map(|p| View::Playlist {
                                id: p.id,
                                name: p.name.clone(),
                            })
                        };
                        if let Some(target) = target {
                            library.go(target);
                        }
                    },
                    span { class: "title", "{playlist.name}" }
                    if let Some(count) = playlist.tracks_count {
                        span { class: "muted", "{count} tracks" }
                    }
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn search(query: &str) -> View {
        View::Search { query: query.into(), scope: Scope::Everything }
    }

    #[test]
    fn a_refined_search_keeps_its_results() {
        assert!(keeps_results(&search("radio"), &search("radiohead")));
        let narrowed = View::Search { query: "radio".into(), scope: Scope::Albums };
        assert!(keeps_results(&search("radio"), &narrowed));
    }

    #[test]
    fn only_a_refined_search_keeps_its_results() {
        // An emptied box clears at once, and a search opened from elsewhere
        // has nothing of its own to keep.
        assert!(!keeps_results(&search("radio"), &search(" ")));
        assert!(!keeps_results(&home(), &search("radio")));
        assert!(!keeps_results(&search("radio"), &home()));
    }

    fn album(id: &str) -> View {
        View::Album(RemoteAlbum { id: id.into(), ..Default::default() })
    }

    fn tracks(ids: &[i64]) -> Shelf {
        Shelf {
            tracks: ids.iter().map(|&id| RemoteTrack { id, ..Default::default() }).collect(),
            ..Default::default()
        }
    }

    fn flip(ticket: u64, kind: &'static str, id: &str, liked: bool, saved: Option<u64>) -> Flip {
        Flip { ticket, kind, id: id.into(), liked, saved }
    }

    #[test]
    fn a_like_shows_before_the_sets_have_loaded() {
        let liked = with_flips(None, &[flip(0, "track", "7", true, None)]);
        assert!(liked.tracks.contains(&7));
    }

    #[test]
    fn the_last_flip_of_an_id_wins() {
        let mut base = Liked::default();
        base.albums.insert("a".into());
        let flips = [flip(0, "album", "a", false, None), flip(1, "album", "a", true, None)];
        assert!(with_flips(Some(&base), &flips).albums.contains("a"));
        assert!(!with_flips(Some(&base), &flips[..1]).albums.contains("a"));
    }

    #[test]
    fn one_id_reads_the_same_as_the_whole_sets() {
        let mut base = Liked::default();
        base.tracks.insert(7);
        base.albums.insert("a".into());
        let flips = [flip(0, "album", "a", false, None), flip(1, "artist", "3", true, None)];
        let whole = with_flips(Some(&base), &flips);
        for (kind, id) in [("track", "7"), ("track", "8"), ("album", "a"), ("artist", "3"), ("artist", "4")] {
            assert_eq!(holds(Some(&base), &flips, kind, id), whole.has(kind, id), "{kind} {id}");
        }
        assert!(holds(None, &flips, "artist", "3"));
        assert!(!holds(None, &[], "track", "7"));
    }

    #[test]
    fn an_answer_lets_go_only_of_flips_saved_before_it_was_asked() {
        let mut flips = vec![
            flip(0, "artist", "3", false, Some(4)),
            flip(1, "artist", "5", true, Some(6)),
            flip(2, "track", "9", true, None),
        ];
        drop_seen(&mut flips, 5);
        let left: Vec<u64> = flips.iter().map(|flip| flip.ticket).collect();
        assert_eq!(left, vec![1, 2]);
    }

    #[test]
    fn home_seeds_every_like_and_then_shows_tiles_for_albums() {
        let mut shelf = tracks(&[1, 2]);
        shelf.tracks[1].album_id = Some("a".into());
        shelf.albums.push(RemoteAlbum { id: "a".into(), ..Default::default() });
        let liked = Liked::of(&shelf);
        assert!(liked.tracks.contains(&2));
        assert!(liked.albums.contains("a"));
        shelf.stand_in(Scope::Tracks);
        assert_eq!(shelf.tracks.len(), 2);
        shelf.stand_in(Scope::Everything);
        assert_eq!(shelf.tracks.len(), 1);
    }

    #[test]
    fn an_unlike_takes_the_row_off_a_shelf() {
        let mut shelf = tracks(&[1, 2]);
        assert!(shelf.drop_liked("track", "2"));
        assert!(!shelf.drop_liked("track", "5"));
        assert_eq!(shelf.tracks.len(), 1);
    }

    fn ids(tracks: Option<Vec<RemoteTrack>>) -> Option<Vec<i64>> {
        tracks.map(|tracks| tracks.iter().map(|track| track.id).collect())
    }

    #[test]
    fn an_album_seen_lately_is_played_from_what_it_loaded() {
        let views = [(album("a"), tracks(&[1, 2])), (album("b"), tracks(&[3, 4, 5]))];
        let found = album_tracks_in(views.iter().map(|(view, shelf)| (view, shelf)), "b");
        assert_eq!(ids(found), Some(vec![3, 4, 5]));
    }

    #[test]
    fn an_album_not_held_or_held_empty_is_fetched() {
        let views = [
            (album("a"), Shelf::default()),
            (View::Releases, tracks(&[1])),
        ];
        assert_eq!(ids(album_tracks_in(views.iter().map(|(view, shelf)| (view, shelf)), "a")), None);
        assert_eq!(ids(album_tracks_in(views.iter().map(|(view, shelf)| (view, shelf)), "z")), None);
    }
}

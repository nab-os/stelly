//! Browsing and searching Qobuz, and the shelf of results.
//!
//! Navigation is explicit rather than reactive: every move sets the view and
//! spawns its own load. One `Shelf` holds whatever the view returned.

use super::icons;
use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, play_next, Player};
use super::prefs::{self, Prefs};
use super::screens::{AlbumScreen, ArtistScreen, TrackScreen};
use super::settings::SettingsScreen;
use super::{
    artist_link, open_remote_track, open_track, Blocklist, Cover, LocalIds, MapView,
    Search, Selection, SpaceMatches, SpaceReach, SpaceRow, space_mark, LIST_CAP, SEARCH_LIMIT,
};
use dioxus::prelude::*;
use crate::api::Target;
use crate::backend::backend;
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemoteTrack};

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
}

/// Where a search looks: the analysed space, or the whole Qobuz catalogue.
/// One or the other, never both at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Space,
    Qobuz,
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
    pub albums: Vec<crate::qobuz::RemoteAlbum>,
    pub artists: Vec<crate::qobuz::RemoteArtist>,
    pub playlists: Vec<crate::qobuz::RemotePlaylist>,
    /// Shown under an artist: the same hop the crawl takes when it expands
    /// the frontier, so you can see where analysis would go next.
    pub similar: Vec<crate::qobuz::RemoteArtist>,
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
    /// Where a search looks. Sticks across searches, like `liked_only`.
    pub source: Signal<Source>,
    /// The three favourited-id sets a search result is checked against, or
    /// `None` before the first time `liked_only` turns on, the fetch that
    /// fills this is not worth paying for a session that never asks.
    liked: Signal<Option<Liked>>,
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
}

/// How many views `Library::kept` remembers. Enough to step back through a
/// few pages; a shelf can hold hundreds of rows, so not the whole history.
const KEPT: usize = 8;

/// This user's likes, by id, fetched once and cached rather than asked per
/// row: the server lists them, it has no "is this one liked" lookup.
#[derive(Clone, Default)]
pub(crate) struct Liked {
    pub(crate) tracks: std::collections::HashSet<i64>,
    pub(crate) albums: std::collections::HashSet<String>,
    pub(crate) artists: std::collections::HashSet<i64>,
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
            source: Signal::new(Source::Qobuz),
            liked: Signal::new(None),
            epoch: Signal::new(0),
            kept: Signal::new(Vec::new()),
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
    /// launch actually reaches the network.
    pub(crate) fn ensure_liked_loaded(self) {
        if self.liked.peek().is_some() {
            return;
        }
        let mut liked = self.liked;
        spawn(async move {
            let (tracks, albums, artists) = tokio::join!(
                backend().favourite_tracks(LIST_CAP),
                backend().favourite_albums(LIST_CAP),
                backend().favourite_artists(LIST_CAP),
            );
            liked.set(Some(Liked {
                tracks: tracks.unwrap_or_default().iter().map(|t| t.id).collect(),
                albums: albums.unwrap_or_default().iter().map(|a| a.id.clone()).collect(),
                artists: artists.unwrap_or_default().iter().map(|a| a.id).collect(),
            }));
        });
    }

    /// Whether the "liked" filter is on, and what to check against. Reading
    /// both together is what lets `unwrap_or_default`'s empty sets do double
    /// duty as "nothing is confirmed liked yet" while the fetch is still in
    /// flight, rather than needing a separate loading state every caller has
    /// to handle.
    fn liked_state(&self) -> (bool, Liked) {
        (*self.liked_only.read(), self.liked())
    }

    /// The favourited-id cache alone, regardless of whether the "liked"
    /// search filter is itself switched on, for anything that wants to
    /// know "is this one liked" rather than "should the shelf be narrowed to
    /// liked things". The detail sheet's heart icon is the other reader;
    /// both go through `ensure_liked_loaded` first, so whichever asks first
    /// pays for the fetch and the other finds it already there.
    pub(crate) fn liked(&self) -> Liked {
        self.liked.read().clone().unwrap_or_default()
    }

    /// Flip one id's membership in the cached favourite sets, optimistically,
    /// after a successful favourite add/remove, so the heart in the
    /// detail sheet does not sit wrong until the next full reload. A no-op
    /// before the cache has ever been loaded: there is no set to flip a
    /// member of yet, and by the time one is loaded it will ask Qobuz fresh
    /// rather than replay this.
    pub(crate) fn set_liked(&self, kind: &str, id: &str, liked: bool) {
        let mut signal = self.liked;
        let mut current = signal.write();
        let Some(current) = current.as_mut() else { return };
        match kind {
            "track" => {
                if let Ok(track_id) = id.parse::<i64>() {
                    if liked {
                        current.tracks.insert(track_id);
                    } else {
                        current.tracks.remove(&track_id);
                    }
                }
            }
            "album" => {
                if liked {
                    current.albums.insert(id.to_string());
                } else {
                    current.albums.remove(id);
                }
            }
            "artist" => {
                if let Ok(artist_id) = id.parse::<i64>() {
                    if liked {
                        current.artists.insert(artist_id);
                    } else {
                        current.artists.remove(&artist_id);
                    }
                }
            }
            _ => {}
        }
    }

    /// After an import: ask for the likes again, and redraw them if they are
    /// what is on screen.
    pub(crate) fn likes_changed(self) {
        let mut liked = self.liked;
        liked.set(None);
        self.ensure_liked_loaded();
        let view = self.view.peek().clone();
        if matches!(view, View::Favourites { .. }) {
            self.show(view);
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
            .map(|(_, _, shelf)| shelf.clone());

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
            None => {
                self.shelf.set(Shelf::default());
                self.loaded.set(Shelf::default());
                self.loading.set(true);
            }
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
        // Already showing what `kept` had for this view: a failed refresh
        // leaves that up rather than swapping it for an error.
        let refreshing = !*self.loading.peek();

        spawn(async move {
            let mut library = self;
            let home = target == home();
            let loaded = load(target, space_only).await;

            // Someone asked for a different view while this was in flight.
            // Writing now would put these rows under that view's heading, and
            // clearing `loading` would call it finished.
            if *library.epoch.peek() != epoch {
                return;
            }

            match loaded {
                Ok(shelf) => {
                    if home {
                        crate::cache::write(HOME_CACHE, &shelf);
                    }
                    let sort = *library.sort.peek();
                    library.loaded.set(shelf.clone());
                    library.shelf.set(shelf.sorted(sort));
                }
                Err(_) if refreshing => {}
                Err(err) => library.error.set(Some(format!("{err:#}"))),
            }
            library.loading.set(false);
        });
    }
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
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
    if let Some(shelf) = crate::cache::read::<Shelf>(HOME_CACHE) {
        let source = *library.source.peek();
        library.kept.write().push((home(), source, shelf));
    }
    library.show(home());
}

fn home() -> View {
    View::Favourites { scope: Scope::Everything }
}

const HOME_CACHE: &str = "favourites.json";

async fn load(view: View, space_only: bool) -> anyhow::Result<Shelf> {
    let mut shelf = Shelf::default();

    match view {
        // Clicking the search tab before typing anything is not a query.
        View::Search { query, .. } if query.trim().is_empty() => {}
        View::Search { query, scope } if space_only => {
            let (albums, artists) = space_groups(&query).await?;
            if scope.albums() {
                shelf.albums = albums;
            }
            if scope.artists() {
                shelf.artists = artists;
            }
        }
        View::Search { query, scope } => {
            let found = backend().search(&query, SEARCH_LIMIT).await?;
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
            shelf.tracks = tracks?;
            shelf.albums = albums?;
            shelf.artists = artists?;

            // A liked album or artist is shown as its tile, not again as
            // every liked track inside it. Only when mixed: narrowed to
            // tracks, there is no tile to stand in for them.
            if scope == Scope::Everything {
                let albums: std::collections::HashSet<&str> =
                    shelf.albums.iter().map(|album| album.id.as_str()).collect();
                let artists: std::collections::HashSet<i64> =
                    shelf.artists.iter().map(|artist| artist.id).collect();
                shelf.tracks.retain(|track| {
                    !track.album_id.as_deref().is_some_and(|id| albums.contains(id))
                        && !track.artist_id.is_some_and(|id| artists.contains(&id))
                });
            }
        }
        View::Playlists => shelf.playlists = backend().playlists(LIST_CAP).await?,
        View::Playlist { id, .. } => shelf.tracks = backend().playlist_tracks(id, LIST_CAP).await?,
        View::Album(album) => shelf.tracks = backend().album_tracks(&album.id).await?,
        View::Artist(artist) => {
            shelf.albums = backend().artist_albums(artist.id, LIST_CAP).await?;
            // Not fatal: an artist with no similar list should still show a
            // discography rather than an error.
            shelf.similar = backend().similar_artists(artist.id, 20).await.unwrap_or_default();
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
            image: crate::qobuz::cover_url(&album.id),
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
    let mut query = search.text;
    let source = *library.source.read();

    // The magnifier opened this screen to type into, so the keyboard should
    // already be up.
    use_effect(|| {
        document::eval(
            "setTimeout(() => { const el = document.querySelector('.search-bar .search'); if (el) el.focus(); }, 0);",
        );
    });

    let pick = move |wanted: Source| {
        let mut source = library.source;
        if *source.peek() != wanted {
            source.set(wanted);
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
                    oninput: move |e| query.set(e.value()),
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
            div { class: "segmented",
                button {
                    class: if source == Source::Space { "active" } else { "" },
                    title: "only what's already in your space",
                    onclick: move |_| pick(Source::Space),
                    "space"
                }
                button {
                    class: if source == Source::Qobuz { "active" } else { "" },
                    title: "the whole Qobuz catalogue",
                    onclick: move |_| pick(Source::Qobuz),
                    "qobuz"
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
    let tracks = qobuz_only(shelf_tracks.clone(), &space_ids(&library, &space));
    let tracks: Vec<(usize, RemoteTrack)> = if liked_only {
        tracks.into_iter().filter(|(_, t)| liked.tracks.contains(&t.id)).collect()
    } else {
        tracks
    };
    let albums = shelf.visible_albums(&blocklist);
    let albums: Vec<crate::qobuz::RemoteAlbum> = if liked_only {
        albums.into_iter().filter(|a| liked.albums.contains(&a.id)).collect()
    } else {
        albums
    };
    let artists = shelf.visible_artists(&blocklist, false);
    let artists: Vec<crate::qobuz::RemoteArtist> = if liked_only {
        artists.into_iter().filter(|a| liked.artists.contains(&a.id)).collect()
    } else {
        artists
    };
    let nothing_from_qobuz =
        tracks.is_empty() && albums.is_empty() && artists.is_empty() && shelf.playlists.is_empty();
    let nothing_visible = nothing_from_qobuz && (!show_space || space.0.read().1 == 0);
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

        if *library.loading.read() {
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
                    if !nothing_from_qobuz || space_only {
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
                    p { class: "muted", "Type to search." }
                } else if nothing_visible && matches!(view, View::Favourites { scope: Scope::Everything }) {
                    p { class: "muted", "No likes yet. Heart anything, or import the Qobuz favourites from settings." }
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
}

/// A mixed view's list: tracks, albums and artists together, in one order.
/// "Default" is artists, then albums, then tracks, each as they came; any
/// other sort interleaves them, unless split by kind, see `Grouping`.
#[component]
fn MixedRows() -> Element {
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
            qobuz_only(shelf.visible_tracks(&blocklist), &space_ids(&library, &matches))
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
                    key: "track-{index}-{track.id}",
                    class: if now_playing == Some(track.id) { "{tile} playing" } else { "{tile}" },
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
                    key: "album-{index}-{album.id}",
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
                    key: "artist-{index}-{artist.id}",
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
                    key: "space-{row.track_id}",
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
                        url: crate::qobuz::cover_url(&row.album_id),
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
    let sections: Vec<(&str, Vec<Item>)> = if split {
        let mut artists = Vec::new();
        let mut albums = Vec::new();
        let mut tracks = Vec::new();
        for item in items {
            match item {
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
            h3 { class: "shelf-head source-head", "Qobuz" }
        }
        for (heading, items) in sections {
            if !heading.is_empty() {
                h3 { class: "shelf-head", "{heading}" }
            }
            ul { class: if grid { "tiles" } else { "list" },
                for item in items {
                    {row(item)}
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
fn qobuz_only(
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
    let searching = !search.text.read().trim().is_empty();
    let visible = shown().min(rows.len());
    let grid = library.layout() == TracksView::Grid;

    if total == 0 {
        return rsx! {
            if searching {
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
                    // The space stores no art, so this is derived from the
                    // album id. A guess that misses leaves the placeholder
                    // tint, which is why it is worth guessing at all.
                    Cover {
                        url: crate::qobuz::cover_url(&row.album_id),
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

    pub fn visible_albums(&self, blocked: &Blocklist) -> Vec<crate::qobuz::RemoteAlbum> {
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
    ) -> Vec<crate::qobuz::RemoteArtist> {
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
    let tracks = qobuz_only(tracks, &space_ids(&library, &matches));
    let (liked_only, liked) = library.liked_state();
    let tracks: Vec<(usize, RemoteTrack)> = if liked_only {
        tracks.into_iter().filter(|(_, t)| liked.tracks.contains(&t.id)).collect()
    } else {
        tracks
    };
    let now_playing = player.current().map(|t| t.id);
    let grid = library.layout() == TracksView::Grid;
    let count = tracks.len();

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
            for (index, track) in tracks {
                li {
                    key: "{index}-{track.id}",
                    class: {
                        let base = if grid { "tile" } else { "row" };
                        if now_playing == Some(track.id) {
                            format!("{base} playing")
                        } else {
                            base.to_string()
                        }
                    },
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
    // Enumerated before filtering, same as `qobuz_only`: `index` is this
    // row's address into `visible_albums`, and filtering after numbering
    // would point the menu at whatever slid into a dropped row's place.
    let albums: Vec<(usize, crate::qobuz::RemoteAlbum)> = library
        .shelf
        .read()
        .visible_albums(&blocklist)
        .into_iter()
        .enumerate()
        .filter(|(_, a)| !liked_only || liked.albums.contains(&a.id))
        .collect();

    rsx! {
        h3 { class: "shelf-head", "Albums" }
        ul { class: "tiles",
            for (index, album) in albums {
                li {
                    key: "{index}-{album.id}",
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
                    {space_mark(reach.read().0.contains(&album.id))}
                    span { class: "title", "{album.title}" }
                    {artist_link(library, album.artist_id, album.artist.clone(), "artist")}
                    {menu_button(menu, MenuTarget::ShelfAlbum(index))}
                }
            }
        }
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
    let artists: Vec<(usize, crate::qobuz::RemoteArtist)> = library
        .shelf
        .read()
        .visible_artists(&blocklist, similar)
        .into_iter()
        .enumerate()
        .filter(|(_, a)| similar || !liked_only || liked.artists.contains(&a.id))
        .collect();

    rsx! {
        h3 { class: "shelf-head", "{heading}" }
        ul { class: "tiles",
            for (index, artist) in artists {
                li {
                    key: "{index}-{artist.id}",
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

    rsx! {
        h3 { class: "shelf-head", "Playlists" }
        ul { class: "list",
            for (index, playlist) in playlists.into_iter().enumerate() {
                li {
                    key: "{index}-{playlist.id}",
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


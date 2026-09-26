//! Browsing and searching Qobuz, and the shelf of results.
//!
//! Navigation is explicit rather than reactive: every move sets the view and
//! spawns its own load. One `Shelf` holds whatever the view returned.

use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, play_next, Player};
use super::{
    artist_link, open_track, AlbumDetail, ArtistDetail, Blocklist, Cover, Detail, DetailSubject, LocalIds,
    Search, Selection, SpaceMatches, SpaceReach, SpaceRow, space_mark, LIST_CAP, SEARCH_LIMIT,
};
use dioxus::prelude::*;
use crate::backend::backend;
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemoteTrack};

/// How many space rows the list opens with. Enough to fill the column on any
/// screen; "show more" triples it.
const FIRST_SHOWN: usize = 80;

/// Which kinds of result a search shows. Taken from the chip that was lit
/// when typing started: filtering from "tracks" and getting albums and
/// artists back as well read as the filter being ignored.
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortKey {
    #[default]
    Default,
    Title,
    Artist,
    Album,
    Released,
    Duration,
}

impl SortKey {
    const ALL: [SortKey; 6] = [
        SortKey::Default,
        SortKey::Title,
        SortKey::Artist,
        SortKey::Album,
        SortKey::Released,
        SortKey::Duration,
    ];

    fn label(self) -> &'static str {
        match self {
            SortKey::Default => "default",
            SortKey::Title => "title",
            SortKey::Artist => "artist",
            SortKey::Album => "album",
            SortKey::Released => "release date",
            SortKey::Duration => "duration",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
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

    /// The space's own rows, which know no release date or duration.
    pub(crate) fn space_rows(self, rows: Vec<SpaceRow>) -> Vec<SpaceRow> {
        self.apply(rows, |row, key| match key {
            SortKey::Title => text(&row.title),
            SortKey::Artist => text(&row.artist),
            SortKey::Album => text(&row.album),
            _ => None,
        })
    }
}

/// Whether every term turns up in a track's title, what a tracks search asks
/// for. Artist and album are left out: every track by a band, or on an
/// album, whose name happens to match is not a search for a track.
pub(crate) fn names_track(terms: &[String], title: &str) -> bool {
    terms.iter().all(|term| title.contains(term))
}

#[derive(Clone, PartialEq, Debug)]
pub enum View {
    Search { query: String, scope: Scope },
    FavouriteTracks,
    FavouriteAlbums,
    FavouriteArtists,
    Playlists,
    Playlist { id: i64, name: String },
    /// The full object, not just its id and title, carried over from
    /// wherever this was reached (almost always the detail sheet, which
    /// already had it in hand), so the page landed on can show the same
    /// cover/badges/actions inline instead of just a bare tracklist under a
    /// heading. See `LibraryPanel`'s `AlbumDetail` call.
    Album(RemoteAlbum),
    /// As `Album`: the full object, so the discography page can show it
    /// inline the same way.
    Artist(RemoteArtist),
}

impl View {
    fn label(&self) -> String {
        match self {
            View::Search { query, .. } => format!("results for “{query}”"),
            View::FavouriteTracks => "favourite tracks".into(),
            View::FavouriteAlbums => "favourite albums".into(),
            View::FavouriteArtists => "favourite artists".into(),
            View::Playlists => "your playlists".into(),
            View::Playlist { name, .. } => name.clone(),
            View::Album(album) => album.title.clone(),
            View::Artist(artist) => artist.name.clone(),
        }
    }

    /// Whether the source row (tracks/albums/artists/playlists) should light
    /// one of its chips up for this view, drilling into a specific album or
    /// artist leaves none of them lit, the same as before this replaced a
    /// separate "library" tab.
    fn is_playlists(&self) -> bool {
        matches!(self, View::Playlists | View::Playlist { .. })
    }

    /// What a search started from here should be narrowed to.
    pub(crate) fn scope(&self) -> Scope {
        match self {
            View::Search { scope, .. } => *scope,
            View::FavouriteTracks => Scope::Tracks,
            View::FavouriteAlbums => Scope::Albums,
            View::FavouriteArtists => Scope::Artists,
            _ => Scope::Everything,
        }
    }
}

/// How the two track sections, the space and Qobuz, render. Grid by
/// default: a cover to recognise, the way albums and artists already worked.
/// List is where the detail that does not fit a tile lives (duration, the
/// "in your space" dot), for when that is what you are after.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TracksView {
    Grid,
    List,
}

/// Whatever the current view loaded. One struct rather than a per-view enum:
/// search fills three of these at once, and an artist fills two.
#[derive(Clone, Default, PartialEq)]
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
    /// Views visited on the way here, for the back button.
    pub history: Signal<Vec<View>>,
    /// A view asked for but not yet fetched. See `show`.
    pending: Signal<Option<View>>,
    pub notice: Signal<Option<String>>,
    /// One switch for both track sections (the space's and Qobuz's), not one
    /// each, they are the same kind of decision made twice, and a search
    /// result shows both at once.
    pub tracks_view: Signal<TracksView>,
    /// Narrows a search to what is already favourited. Off by default: most
    /// searches are for something new, not a re-check of what is already
    /// kept.
    pub liked_only: Signal<bool>,
    /// Keeps a search to the space: no round trip to Qobuz, only what has
    /// already been analysed. Sticks across searches, like `liked_only`.
    pub space_only: Signal<bool>,
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
}

/// The signed-in account's favourites, by id, fetched once and cached
/// rather than asked per row: Qobuz has no "is this one favourited" lookup,
/// only "list them all", so checking membership after one fetch is the whole
/// of what is affordable.
#[derive(Clone, Default)]
pub(crate) struct Liked {
    pub(crate) tracks: std::collections::HashSet<i64>,
    pub(crate) albums: std::collections::HashSet<String>,
    pub(crate) artists: std::collections::HashSet<i64>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            view: Signal::new(View::FavouriteTracks),
            shelf: Signal::new(Shelf::default()),
            loaded: Signal::new(Shelf::default()),
            sort: Signal::new(Sort::default()),
            loading: Signal::new(true),
            error: Signal::new(None),
            history: Signal::new(Vec::new()),
            pending: Signal::new(None),
            notice: Signal::new(None),
            tracks_view: Signal::new(TracksView::Grid),
            liked_only: Signal::new(false),
            space_only: Signal::new(false),
            liked: Signal::new(None),
            epoch: Signal::new(0),
        }
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
                tracks: tracks
                    .unwrap_or_default()
                    .iter()
                    .map(|t| t.id)
                    .collect(),
                albums: albums
                    .unwrap_or_default()
                    .iter()
                    .map(|a| a.id.clone())
                    .collect(),
                artists: artists
                    .unwrap_or_default()
                    .iter()
                    .map(|a| a.id)
                    .collect(),
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

    /// Navigate, remembering where we came from.
    pub(crate) fn go(mut self, target: View) {
        let previous = self.view.peek().clone();
        if previous != target {
            self.history.write().push(previous);
        }
        self.show(target);
    }

    fn back(mut self) {
        let previous = self.history.write().pop();
        if let Some(view) = previous {
            self.show(view);
        }
    }

    /// Navigate to a search result, refining in place.
    ///
    /// The first search from an album or artist pushes that view, so back
    /// returns to what you were reading. Every refinement after it replaces:
    /// a query that fires as you type would otherwise leave one history entry
    /// per character, and "back" would walk you through your own spelling.
    pub(crate) fn search_to(self, query: String) {
        let current = self.view.peek().clone();
        let scope = current.scope();
        if matches!(current, View::Search { .. }) {
            self.show(View::Search { query, scope });
        } else {
            self.go(View::Search { query, scope });
        }
    }

    /// Re-run the current search over a different kind of result. Replaces
    /// rather than pushes, the same as refining the query does.
    fn rescope(self, scope: Scope) {
        let current = self.view.peek().clone();
        if let View::Search { query, .. } = current {
            self.show(View::Search { query, scope });
        }
    }

    /// Emptying the box should put back whatever the search covered up,
    /// rather than leaving an empty shelf with no way out but the tabs.
    pub(crate) fn leave_search(self) {
        if !matches!(&*self.view.peek(), View::Search { .. }) {
            return;
        }
        if self.history.peek().is_empty() {
            self.show(View::FavouriteTracks);
        } else {
            self.back();
        }
    }

    fn set_sort(mut self, sort: Sort) {
        self.sort.set(sort);
        let loaded = self.loaded.peek().clone();
        self.shelf.set(loaded.sorted(sort));
    }

    /// Whether the "In your space" section is part of this view. Only for a
    /// search: the tracks chip is the liked tracks, and the space holds
    /// everything the crawl reached from them, liked albums' tracks included,
    /// which is not what that chip is asking for.
    fn shows_space(&self) -> bool {
        match &*self.view.read() {
            View::Search { scope, .. } => scope.tracks(),
            _ => false,
        }
    }

    /// Whether a search is skipping Qobuz and showing only the space.
    fn searching_space_only(&self) -> bool {
        matches!(&*self.view.read(), View::Search { .. }) && *self.space_only.read()
    }

    /// Ask for a view. The fetch happens in `LibraryPanel`, not here.
    ///
    /// Called from the row handlers, and clearing the shelf unmounts those
    /// rows, a task spawned in a dying scope is dropped with it, which left
    /// the panel showing "loading..." for good.
    pub fn show(mut self, target: View) {
        self.view.set(target.clone());
        self.shelf.set(Shelf::default());
        self.loaded.set(Shelf::default());
        self.error.set(None);
        self.notice.set(None);
        self.loading.set(true);
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
        let space_only = *self.space_only.peek();

        spawn(async move {
            let mut library = self;
            let loaded = load(target, space_only).await;

            // Someone asked for a different view while this was in flight.
            // Writing now would put these rows under that view's heading, and
            // clearing `loading` would call it finished.
            if *library.epoch.peek() != epoch {
                return;
            }

            match loaded {
                Ok(shelf) => {
                    let sort = *library.sort.peek();
                    library.loaded.set(shelf.clone());
                    library.shelf.set(shelf.sorted(sort));
                }
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

/// First load, so the app shell does not have to reach into navigation.
/// Favourites, because that is also what the crawl seeds from.
pub fn open_initial(library: Library) {
    library.show(View::FavouriteTracks);
}

async fn load(view: View, space_only: bool) -> anyhow::Result<Shelf> {
    let mut shelf = Shelf::default();

    match view {
        // Clicking the search tab before typing anything is not a query.
        View::Search { query, .. } if query.trim().is_empty() => {}
        View::Search { query, scope } if space_only => {
            let (albums, artists) = space_groups(&query);
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
            // Only what the artist's or album's name brought in. Qobuz
            // matches more loosely than a substring test (accents, for one),
            // so a track this test cannot place at all stays.
            if scope == Scope::Tracks {
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
        View::FavouriteTracks => shelf.tracks = backend().favourite_tracks(LIST_CAP).await?,
        View::FavouriteAlbums => shelf.albums = backend().favourite_albums(LIST_CAP).await?,
        View::FavouriteArtists => shelf.artists = backend().favourite_artists(LIST_CAP).await?,
        View::Playlists => shelf.playlists = backend().playlists(LIST_CAP).await?,
        View::Playlist { id, .. } => shelf.tracks = backend().playlist_tracks(id, LIST_CAP).await?,
        View::Album(album) => shelf.tracks = backend().album_tracks(&album.id).await?,
        View::Artist(artist) => {
            shelf.albums = backend().artist_albums(artist.id, LIST_CAP).await?;
            // Not fatal: an artist with no similar list should still show a
            // discography rather than an error.
            shelf.similar = backend().similar_artists(artist.id, 20).await.unwrap_or_default();
        }
    }

    Ok(shelf)
}

/// The albums and artists behind the space's tracks matching `query`, for a
/// space-only search. Matched the way the space list is, every word somewhere
/// in the artist, title or album, and put in the shelf so the tiles, their
/// menus and the liked filter all work as they do for a Qobuz search.
fn space_groups(query: &str) -> (Vec<RemoteAlbum>, Vec<RemoteArtist>) {
    let terms: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();
    let Some(first) = terms.first() else {
        return Default::default();
    };

    let mut albums: Vec<(u8, RemoteAlbum)> = Vec::new();
    let mut artists: Vec<(u8, RemoteArtist)> = Vec::new();
    let mut album_at = std::collections::HashMap::new();
    let mut artist_at = std::collections::HashMap::new();

    let guard = crate::engine().lock().unwrap();
    let catalog = &guard.navigator.catalog;
    for i in catalog.visible() {
        let track = catalog.get(i);
        let artist = track.artist.to_lowercase();
        let title = track.title.to_lowercase();
        let album = track.album.to_lowercase();
        if !terms
            .iter()
            .all(|term| artist.contains(term) || title.contains(term) || album.contains(term))
        {
            continue;
        }

        // Named after what was typed first, the same preference the space
        // list gives a track, but on the album's own title here.
        let rank = u8::from(!(artist.starts_with(first) || album.starts_with(first)));
        let artist_id = Some(track.artist_id).filter(|id| *id >= 0);

        if !track.album_id.is_empty() {
            let at = *album_at.entry(track.album_id.clone()).or_insert_with(|| {
                albums.push((
                    rank,
                    RemoteAlbum {
                        id: track.album_id.clone(),
                        title: track.album.clone(),
                        artist: track.artist.clone(),
                        artist_id,
                        image: crate::qobuz::cover_url(&track.album_id),
                        ..Default::default()
                    },
                ));
                albums.len() - 1
            });
            albums[at].0 = albums[at].0.min(rank);
        }

        if let Some(id) = artist_id {
            let rank = u8::from(!artist.starts_with(first));
            let at = *artist_at.entry(id).or_insert_with(|| {
                artists.push((
                    rank,
                    RemoteArtist { id, name: track.artist.clone(), ..Default::default() },
                ));
                artists.len() - 1
            });
            artists[at].0 = artists[at].0.min(rank);
        }
    }

    albums.sort_by_key(|entry| entry.0);
    artists.sort_by_key(|entry| entry.0);
    (
        albums.into_iter().map(|(_, album)| album).collect(),
        artists.into_iter().map(|(_, artist)| artist).collect(),
    )
}

// --------------------------------------------------------------- crawl hook

/// Fetch an artist or album into the catalogue, here and now. An album's
/// tracklist lands immediately and a discography is queued; only feature
/// extraction still belongs to the pipeline.
pub(crate) fn request_analysis(library: Library, kind: &str, id: &str, label: &str) {
    let mut library = library;
    let (kind, id, label) = (kind.to_string(), id.to_string(), label.to_string());

    library.notice.set(Some(format!("fetching {label}…")));

    spawn(async move {
        match fetch_into_catalog(&kind, &id).await {
            Ok(count) => {
                let what = if kind == "artist" { "albums queued" } else { "tracks" };
                library.notice.set(Some(format!(
                    "{label}: {count} {what}. Run analyse from the pipeline view to extract features."
                )));
            }
            Err(err) => library.notice.set(Some(format!("{err:#}"))),
        }
    });
}

async fn fetch_into_catalog(kind: &str, id: &str) -> anyhow::Result<usize> {
    match kind {
        "artist" => backend().fetch_artist(id.parse()?).await,
        _ => backend().fetch_album(id).await,
    }
}

// --------------------------------------------------------------- components

#[component]
pub fn LibraryPanel() -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();

    let blocklist = use_context::<Blocklist>();
    let view = library.view.read().clone();
    let searching = matches!(view, View::Search { .. });
    let scope = view.scope();
    // Only while searching, see `Library::shows_space`. The space has no
    // album or artist grouping of its own, it is a flat
    // set of analysed tracks, so it only has anything to say for the two
    // views that are about tracks. Showing it regardless of which chip was
    // picked was the gap that made the source-row filter look like it only
    // applied to the Qobuz half of the list.
    let show_space = library.shows_space();
    let space_only = library.searching_space_only();
    let shelf = library.shelf.read().clone();
    let tracks_view = *library.tracks_view.read();

    // Every emptiness test below asks about what is *visible*: no heading over
    // nothing, and the bulk actions must not reach past the filter.
    // Tracks the space section is already showing do not count towards the
    // Qobuz section being non-empty, or its heading would stand over nothing.
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
    // Similar artists are Qobuz's suggestion, not something to have already
    // liked, filtering them by the same toggle would just empty a section
    // whose entire point is showing you what you have not reached yet.
    let similar = shelf.visible_artists(&blocklist, true);
    let nothing_from_qobuz = tracks.is_empty()
        && albums.is_empty()
        && artists.is_empty()
        && similar.is_empty()
        && shelf.playlists.is_empty();
    // "Nothing here" belongs to the whole list, not to the Qobuz half of it:
    // with matches in the space above, the list is plainly not empty.
    let nothing_visible = nothing_from_qobuz && (!show_space || space.0.read().1 == 0);
    let has_history = !library.history.read().is_empty();

    // Drives every navigation. Deliberately here rather than in `show`: see
    // the comment there.
    use_effect(move || library.drive());

    rsx! {
        aside { class: "panel library",
            // An album or an artist's own page shows the same cover, badges
            // and actions the detail sheet does, ahead of its tracklist or
            // discography below, rather than the sheet's content being
            // dismissed on the way here and this page starting over with
            // nothing but a bare heading over a list. `inline: true` is the
            // one difference: it drops the sheet's own "View tracklist" /
            // "View discography" button, which here would just reload the
            // list already sitting right underneath it.
            if let View::Album(album) = &view {
                div { class: "library-detail",
                    AlbumDetail { album: album.clone(), inline: true }
                }
            } else if let View::Artist(artist) = &view {
                div { class: "library-detail",
                    ArtistDetail { artist: artist.clone(), inline: true }
                }
            } else {
                // Named for what you are looking at, not for where the rows
                // came from. "Qobuz" as a column heading was half of what
                // made this feel like two rival lists; it survives below as
                // a section badge, which is the honest scope for it.
                h2 { class: "ellipsis", "{view.label()}" }
            }

            // What kind of thing you are browsing. Search used to be a tab
            // here too, sharing a row with library/playlists; it is a header
            // icon now (see `app.rs`'s `SearchBox`), so this row only ever
            // has to say what four sources look like, not decide between
            // finding something and browsing it. While a search is showing
            // the first three narrow it instead, and a second tap on the lit
            // one widens it back to everything; playlists are not something
            // a search returns, so that chip goes.
            if searching {
                div { class: "actions",
                    for (label, chip) in [
                        ("tracks", Scope::Tracks),
                        ("albums", Scope::Albums),
                        ("artists", Scope::Artists),
                    ] {
                        button {
                            class: if scope == chip { "chip active" } else { "chip" },
                            onclick: move |_| {
                                library.rescope(if scope == chip { Scope::Everything } else { chip })
                            },
                            "{label}"
                        }
                    }
                }
            } else {
                div { class: "actions",
                    button {
                        class: if view == View::FavouriteTracks { "chip active" } else { "chip" },
                        onclick: move |_| library.go(View::FavouriteTracks),
                        "tracks"
                    }
                    button {
                        class: if view == View::FavouriteAlbums { "chip active" } else { "chip" },
                        onclick: move |_| library.go(View::FavouriteAlbums),
                        "albums"
                    }
                    button {
                        class: if view == View::FavouriteArtists { "chip active" } else { "chip" },
                        onclick: move |_| library.go(View::FavouriteArtists),
                        "artists"
                    }
                    button {
                        class: if view.is_playlists() { "chip active" } else { "chip" },
                        onclick: move |_| library.go(View::Playlists),
                        "playlists"
                    }
                }
            }

            div { class: "crumb",
                if has_history {
                    button { class: "chip", onclick: move |_| library.back(), "‹ back" }
                }
                span { class: "spacer" }
                if !shelf_tracks.is_empty() {
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
                // A search asks the whole catalogue; this narrows the Qobuz
                // half of the answer to what is already favourited, or the
                // space's own when "space" has left Qobuz out. Only
                // shown while searching, the tracks/albums/artists chips
                // above already are the liked list the rest of the time, so
                // a second "liked" filter on top of them would filter
                // favourites by whether they are favourited.
                if searching {
                    // Toggling re-runs the search rather than hiding the
                    // Qobuz half: a hidden shelf would still be what "play
                    // all" plays.
                    button {
                        class: if space_only { "chip active" } else { "chip" },
                        title: "only search what's already in your space",
                        onclick: move |_| {
                            let mut space_only = library.space_only;
                            let now = space_only();
                            space_only.set(!now);
                            let view = library.view.peek().clone();
                            library.show(view);
                        },
                        "space"
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
                SortMenu {}
                // Grid by default, list for when the duration and the
                // "in your space" dot are what you came for. One switch for
                // both track sections below, see `Library::tracks_view`.
                div { class: "view-toggle",
                    button {
                        class: if tracks_view == TracksView::Grid { "chip active" } else { "chip" },
                        title: "grid",
                        onclick: move |_| {
                            let mut tracks_view = library.tracks_view;
                            tracks_view.set(TracksView::Grid);
                        },
                        "▦"
                    }
                    button {
                        class: if tracks_view == TracksView::List { "chip active" } else { "chip" },
                        title: "list",
                        onclick: move |_| {
                            let mut tracks_view = library.tracks_view;
                            tracks_view.set(TracksView::List);
                        },
                        "☰"
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
                    // What you already have, first: it needs no round trip,
                    // it is what the space can actually navigate, and it is
                    // usually what you were looking for. Only for the views
                    // "tracks" actually means, though, it used to show
                    // regardless of whether Albums, Artists or Playlists was
                    // the thing picked above, ignoring that filter entirely
                    // and reading as a second list bolted onto the one the
                    // chips claimed to control.
                    if show_space {
                        SpaceRows {}
                    }

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
                    if !similar.is_empty() {
                        ArtistRows { heading: "Similar artists".to_string(), similar: true }
                    }
                    if nothing_visible {
                        p { class: "muted", "nothing here" }
                    }
                }
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
    let detail = use_context::<Detail>().0;
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
    let grid = *library.tracks_view.read() == TracksView::Grid;

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
                        move |_| open_track(selection, detail, id)
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
                        {artist_link(detail, row.artist_id, row.artist.clone(), "artist")}
                    } else {
                        {artist_link(detail, row.artist_id, row.artist.clone(), "artist")}
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
            SortKey::Title | SortKey::Artist => text(&artist.name),
            _ => None,
        };
        Shelf {
            tracks: sort.apply(self.tracks, |track, key| match key {
                SortKey::Default => None,
                SortKey::Title => text(&track.title),
                SortKey::Artist => text(&track.artist),
                SortKey::Album => text(&track.album),
                SortKey::Released => track.released.as_deref().and_then(text),
                SortKey::Duration => track.duration.map(SortValue::Number),
            }),
            albums: sort.apply(self.albums, |album, key| match key {
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
    let detail = use_context::<Detail>().0;
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
    let grid = *library.tracks_view.read() == TracksView::Grid;
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
                    // Single click plays: this is a player, and the queue is
                    // the list you clicked in.
                    onclick: move |_| {
                        // Queue what is on screen, not what was fetched: a
                        // hidden artist must not come back through the queue.
                        let queue = library.shelf.peek().visible_tracks(&blocklist);
                        play_list(player, queue, index);
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
                        {artist_link(detail, track.artist_id, track.artist.clone(), "artist")}
                    } else {
                        {artist_link(detail, track.artist_id, track.artist.clone(), "artist")}
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
    let detail = use_context::<Detail>().0;
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
                    {artist_link(detail, album.artist_id, album.artist.clone(), "artist")}
                    {menu_button(menu, MenuTarget::ShelfAlbum(index))}
                }
            }
        }
    }
}

#[component]
fn ArtistRows(heading: String, similar: bool) -> Element {
    let library = use_context::<Library>();
    let blocklist = use_context::<Blocklist>();
    let mut detail = use_context::<Detail>().0;
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
                        move |_| detail.set(Some(DetailSubject::Artist(artist.clone())))
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


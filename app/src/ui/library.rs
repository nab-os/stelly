//! Browsing and searching Qobuz, and the shelf of results.
//!
//! Navigation is explicit rather than reactive: every move sets the view and
//! spawns its own load. One `Shelf` holds whatever the view returned.

use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, play_next, Player};
use super::{Blocklist, Cover, LocalIds, Search, Selection, SpaceMatches, LIST_CAP, SEARCH_LIMIT};
use dioxus::prelude::*;
use crate::backend::backend;
use crate::qobuz::RemoteTrack;

/// How many space rows the list opens with. Enough to fill the column on any
/// screen; "show more" triples it.
const FIRST_SHOWN: usize = 80;

#[derive(Clone, PartialEq, Debug)]
pub enum View {
    Search(String),
    FavouriteTracks,
    FavouriteAlbums,
    FavouriteArtists,
    Playlists,
    Playlist { id: i64, name: String },
    Album { id: String, title: String },
    Artist { id: i64, name: String },
}

impl View {
    fn label(&self) -> String {
        match self {
            View::Search(query) => format!("results for “{query}”"),
            View::FavouriteTracks => "favourite tracks".into(),
            View::FavouriteAlbums => "favourite albums".into(),
            View::FavouriteArtists => "favourite artists".into(),
            View::Playlists => "your playlists".into(),
            View::Playlist { name, .. } => name.clone(),
            View::Album { title, .. } => title.clone(),
            View::Artist { name, .. } => name.clone(),
        }
    }

    /// The tab a view belongs to, so the right one stays lit while drilling in.
    fn tab(&self) -> Tab {
        match self {
            View::Search(_) => Tab::Search,
            View::Playlists | View::Playlist { .. } => Tab::Playlists,
            _ => Tab::Library,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Search,
    Library,
    Playlists,
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
    pub shelf: Signal<Shelf>,
    pub loading: Signal<bool>,
    pub error: Signal<Option<String>>,
    /// Views visited on the way here, for the back button.
    pub history: Signal<Vec<View>>,
    /// A view asked for but not yet fetched. See `show`.
    pending: Signal<Option<View>>,
    pub notice: Signal<Option<String>>,
    /// Bumped by every `show`. A fetch carries the value it started with and
    /// discards its result if it no longer matches, because `pending` is a
    /// single slot but the tasks it spawns are not: two views asked for in
    /// quick succession race, and the slower one would otherwise land last
    /// and win. Latent while navigation was a click at a time; a search that
    /// fires as you type makes it routine.
    epoch: Signal<u64>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            view: Signal::new(View::FavouriteTracks),
            shelf: Signal::new(Shelf::default()),
            loading: Signal::new(true),
            error: Signal::new(None),
            history: Signal::new(Vec::new()),
            pending: Signal::new(None),
            notice: Signal::new(None),
            epoch: Signal::new(0),
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
        if matches!(&*self.view.peek(), View::Search(_)) {
            self.show(View::Search(query));
        } else {
            self.go(View::Search(query));
        }
    }

    /// Emptying the box should put back whatever the search covered up,
    /// rather than leaving an empty shelf with no way out but the tabs.
    pub(crate) fn leave_search(self) {
        if !matches!(&*self.view.peek(), View::Search(_)) {
            return;
        }
        if self.history.peek().is_empty() {
            self.show(View::FavouriteTracks);
        } else {
            self.back();
        }
    }

    /// Ask for a view. The fetch happens in `LibraryPanel`, not here.
    ///
    /// Called from the row handlers, and clearing the shelf unmounts those
    /// rows, a task spawned in a dying scope is dropped with it, which left
    /// the panel showing "loading..." for good.
    pub fn show(mut self, target: View) {
        self.view.set(target.clone());
        self.shelf.set(Shelf::default());
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

        spawn(async move {
            let mut library = self;
            let loaded = load(target).await;

            // Someone asked for a different view while this was in flight.
            // Writing now would put these rows under that view's heading, and
            // clearing `loading` would call it finished.
            if *library.epoch.peek() != epoch {
                return;
            }

            match loaded {
                Ok(shelf) => library.shelf.set(shelf),
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

async fn load(view: View) -> anyhow::Result<Shelf> {
    let mut shelf = Shelf::default();

    match view {
        // Clicking the search tab before typing anything is not a query.
        View::Search(query) if query.trim().is_empty() => {}
        View::Search(query) => {
            let found = backend().search(&query, SEARCH_LIMIT).await?;
            shelf.tracks = found.tracks;
            shelf.albums = found.albums;
            shelf.artists = found.artists;
        }
        View::FavouriteTracks => shelf.tracks = backend().favourite_tracks(LIST_CAP).await?,
        View::FavouriteAlbums => shelf.albums = backend().favourite_albums(LIST_CAP).await?,
        View::FavouriteArtists => shelf.artists = backend().favourite_artists(LIST_CAP).await?,
        View::Playlists => shelf.playlists = backend().playlists(LIST_CAP).await?,
        View::Playlist { id, .. } => shelf.tracks = backend().playlist_tracks(id, LIST_CAP).await?,
        View::Album { id, .. } => shelf.tracks = backend().album_tracks(&id).await?,
        View::Artist { id, .. } => {
            shelf.albums = backend().artist_albums(id, LIST_CAP).await?;
            // Not fatal: an artist with no similar list should still show a
            // discography rather than an error.
            shelf.similar = backend().similar_artists(id, 20).await.unwrap_or_default();
        }
    }

    Ok(shelf)
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
    let search = use_context::<Search>();

    let blocklist = use_context::<Blocklist>();
    let view = library.view.read().clone();
    let tab = view.tab();
    let shelf = library.shelf.read().clone();

    // Every emptiness test below asks about what is *visible*: no heading over
    // nothing, and the bulk actions must not reach past the filter.
    // Tracks the space section is already showing do not count towards the
    // Qobuz section being non-empty, or its heading would stand over nothing.
    let space = use_context::<SpaceMatches>();
    // The whole shelf, for the bulk actions: "play all" means the view's
    // tracks, including ones the space section happens to be showing.
    let shelf_tracks = shelf.visible_tracks(&blocklist);
    let tracks = qobuz_only(shelf_tracks.clone(), &space_ids(&space));
    let albums = shelf.visible_albums(&blocklist);
    let artists = shelf.visible_artists(&blocklist, false);
    let similar = shelf.visible_artists(&blocklist, true);
    let nothing_from_qobuz = tracks.is_empty()
        && albums.is_empty()
        && artists.is_empty()
        && similar.is_empty()
        && shelf.playlists.is_empty();
    // "Nothing here" belongs to the whole list, not to the Qobuz half of it:
    // with matches in the space above, the list is plainly not empty.
    let nothing_visible = nothing_from_qobuz && space.0.read().1 == 0;
    let has_history = !library.history.read().is_empty();

    // Drives every navigation. Deliberately here rather than in `show`: see
    // the comment there.
    use_effect(move || library.drive());

    rsx! {
        aside { class: "panel library",
            // Named for what you are looking at, not for where the rows came
            // from. "Qobuz" as a column heading was half of what made this
            // feel like two rival lists; it survives below as a section badge,
            // which is the honest scope for it.
            h2 { class: "ellipsis", "{view.label()}" }

            div { class: "tabs",
                button {
                    class: if tab == Tab::Search { "tab active" } else { "tab" },
                    onclick: move |_| {
                        let text = search.text.peek().trim().to_string();
                        library.search_to(text);
                    },
                    "search"
                }
                button {
                    class: if tab == Tab::Library { "tab active" } else { "tab" },
                    onclick: move |_| library.go(View::FavouriteTracks),
                    "library"
                }
                button {
                    class: if tab == Tab::Playlists { "tab active" } else { "tab" },
                    onclick: move |_| library.go(View::Playlists),
                    "playlists"
                }
            }

            if tab == Tab::Library {
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
                    // usually what you were looking for.
                    SpaceRows {}

                    if !nothing_from_qobuz {
                        h3 { class: "shelf-head source-head",
                            "On Qobuz"
                            span { class: "spacer" }
                            if tracks.len() >= SEARCH_LIMIT {
                                span { class: "muted", "first {SEARCH_LIMIT}" }
                            }
                        }
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

            HiddenArtists {}
        }
    }
}

/// Track ids already listed under "In your space".
fn space_ids(matches: &SpaceMatches) -> std::collections::HashSet<i64> {
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
    let matches = use_context::<SpaceMatches>().0;
    let mut selection = use_context::<Selection>().0;
    let mut menu = use_context::<ContextMenu>().0;
    let search = use_context::<Search>();

    // Grows on request rather than rendering 720 rows nobody scrolled to.
    let mut shown = use_signal(|| FIRST_SHOWN);

    let (rows, total) = matches();
    let searching = !search.text.read().trim().is_empty();
    let visible = shown().min(rows.len());

    if total == 0 {
        return rsx! {
            if searching {
                h3 { class: "shelf-head source-head", "In your space" }
                p { class: "muted", "Nothing matches. Every word has to appear somewhere." }
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

        ul { class: "list",
            for row in rows.iter().take(visible) {
                li {
                    key: "{row.track_id}",
                    class: if selection.read().as_ref() == Some(&row.track_id) {
                        "row selected"
                    } else {
                        "row"
                    },
                    onclick: {
                        let id = row.track_id;
                        move |_| selection.set(Some(id))
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
                    Cover { url: crate::qobuz::cover_url(&row.album_id), class: "thumb" }
                    span { class: "artist", "{row.artist}" }
                    span { class: "title", "{row.title}" }
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

/// The block list, with a way out of it. Only rendered when something is
/// hidden, so it stays out of the way until it is relevant.
#[component]
fn HiddenArtists() -> Element {
    let blocklist = use_context::<Blocklist>();
    let hidden = blocklist.artists.read().clone();

    // Collapsed by default: this list grows without bound, 115 entries on a
    // well-used corpus, and at a fixed height took a third of the column.
    let mut open = use_signal(|| false);

    if hidden.is_empty() {
        return rsx! {};
    }

    rsx! {
        div { class: if open() { "hidden-artists open" } else { "hidden-artists" },
            h3 { class: "shelf-head",
                "Hidden ({hidden.len()})"
                span { class: "spacer" }
                button {
                    class: "chip",
                    onclick: move |_| { let next = !open(); open.set(next); },
                    if open() { "hide list" } else { "show" }
                }
            }
            if open() {
            ul { class: "list",
                for entry in hidden {
                    li { key: "{entry.artist_id}", class: "row",
                        span { class: "title", "{entry.name}" }
                        if let Some(reason) = entry.reason.clone() {
                            span { class: "muted", "{reason}" }
                        }
                        button {
                            class: "chip",
                            onclick: move |_| blocklist.unblock.call(entry.artist_id),
                            "unhide"
                        }
                    }
                }
            }
            }
        }
    }
}

impl Shelf {
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
    let mut menu = use_context::<ContextMenu>().0;

    let matches = use_context::<SpaceMatches>();

    // Matched on the Qobuz track id: that is the same recording *and* the
    // same release, so suppressing it hides nothing you could not already
    // reach. A different release of the same song stays, because fetching
    // that one is a real thing to want.
    let tracks = library.shelf.read().visible_tracks(&blocklist);
    let tracks = qobuz_only(tracks, &space_ids(&matches));
    let now_playing = player.current().map(|t| t.id);

    if tracks.is_empty() {
        return rsx! {};
    }

    rsx! {
        h3 { class: "shelf-head", "Tracks" }
        ul { class: "list",
            for (index, track) in tracks {
                li {
                    key: "{index}-{track.id}",
                    class: if now_playing == Some(track.id) { "row playing" } else { "row" },
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
                    Cover { url: track.image.clone(), class: "thumb" }
                    span { class: "artist", "{track.artist}" }
                    span { class: "title", "{track.title}" }
                    if !track.streamable {
                        span { class: "muted tag", "-" }
                    }
                    if local.0.read().contains(&track.id) {
                        // A mark, not a button: that this track is a point on
                        // the map is something to know while scanning the
                        // list, and the menu is where you act on it.
                        span { class: "in-space-dot", title: "analysed, on the map", "•" }
                    }
                    span { class: "muted", "{track.duration_label()}" }
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
    let albums = library.shelf.read().visible_albums(&blocklist);

    rsx! {
        h3 { class: "shelf-head", "Albums" }
        ul { class: "tiles",
            for (index, album) in albums.into_iter().enumerate() {
                li {
                    key: "{index}-{album.id}",
                    class: "tile",
                    onclick: move |_| {
                        let target = library
                            .shelf
                            .peek()
                            .visible_albums(&blocklist)
                            .get(index)
                            .map(|a| View::Album {
                                id: a.id.clone(),
                                title: a.title.clone(),
                            });
                        if let Some(target) = target {
                            library.go(target);
                        }
                    },
                    "data-menu": MenuTarget::ShelfAlbum(index).tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfAlbum(index));
                    },
                    Cover { url: album.image.clone() }
                    span { class: "title", "{album.title}" }
                    span { class: "artist", "{album.artist}" }
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
    let mut menu = use_context::<ContextMenu>().0;
    let artists = library.shelf.read().visible_artists(&blocklist, similar);

    rsx! {
        h3 { class: "shelf-head", "{heading}" }
        ul { class: "tiles",
            for (index, artist) in artists.into_iter().enumerate() {
                li {
                    key: "{index}-{artist.id}",
                    class: "tile",
                    onclick: move |_| {
                        let target = library
                            .shelf
                            .peek()
                            .visible_artists(&blocklist, similar)
                            .get(index)
                            .map(|a| View::Artist {
                                id: a.id,
                                name: a.name.clone(),
                            });
                        if let Some(target) = target {
                            library.go(target);
                        }
                    },
                    "data-menu": MenuTarget::ShelfArtist { index, similar }.tag(),
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::ShelfArtist { index, similar });
                    },
                    Cover { url: artist.image.clone(), class: "round" }
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


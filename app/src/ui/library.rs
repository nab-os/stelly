//! Browsing and searching Qobuz, and the shelf of results.
//!
//! Navigation is explicit rather than reactive: every move sets the view and
//! spawns its own load. One `Shelf` holds whatever the view returned.

use super::player::{enqueue, play_list, play_next, Player};
use super::{Blocklist, Cover, LocalIds, Selection, LIST_CAP, SEARCH_LIMIT};
use dioxus::prelude::*;
use crate::backend::backend;
use crate::qobuz::RemoteTrack;

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
    pub query: Signal<String>,
    /// Views visited on the way here, for the back button.
    pub history: Signal<Vec<View>>,
    /// A view asked for but not yet fetched. See `show`.
    pending: Signal<Option<View>>,
    pub notice: Signal<Option<String>>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            view: Signal::new(View::FavouriteTracks),
            shelf: Signal::new(Shelf::default()),
            loading: Signal::new(true),
            error: Signal::new(None),
            query: Signal::new(String::new()),
            history: Signal::new(Vec::new()),
            pending: Signal::new(None),
            notice: Signal::new(None),
        }
    }

    /// Navigate, remembering where we came from.
    fn go(mut self, target: View) {
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
    }

    /// Fetch whatever `show` last asked for. Runs in `LibraryPanel`, which is
    /// mounted for the life of the window.
    fn drive(mut self) {
        let target = self.pending.read().clone();
        let Some(target) = target else { return };
        self.pending.set(None);

        spawn(async move {
            let mut library = self;
            match load(target).await {
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
fn request_analysis(library: Library, kind: &str, id: &str, label: &str) {
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
    let mut library = use_context::<Library>();
    let player = use_context::<Player>();

    let blocklist = use_context::<Blocklist>();
    let view = library.view.read().clone();
    let tab = view.tab();
    let shelf = library.shelf.read().clone();

    // Every emptiness test below asks about what is *visible*: no heading over
    // nothing, and the bulk actions must not reach past the filter.
    let tracks = shelf.visible_tracks(&blocklist);
    let albums = shelf.visible_albums(&blocklist);
    let artists = shelf.visible_artists(&blocklist, false);
    let similar = shelf.visible_artists(&blocklist, true);
    let nothing_visible = tracks.is_empty()
        && albums.is_empty()
        && artists.is_empty()
        && similar.is_empty()
        && shelf.playlists.is_empty();
    let has_history = !library.history.read().is_empty();

    // Drives every navigation. Deliberately here rather than in `show`: see
    // the comment there.
    use_effect(move || library.drive());

    let submit = move || {
        let text = library.query.peek().trim().to_string();
        if !text.is_empty() {
            library.go(View::Search(text));
        }
    };

    rsx! {
        aside { class: "panel library",
            h2 { "Qobuz" }

            div { class: "tabs",
                button {
                    class: if tab == Tab::Search { "tab active" } else { "tab" },
                    onclick: move |_| {
                        let text = library.query.peek().trim().to_string();
                        library.go(View::Search(text));
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

            input {
                class: "search",
                placeholder: "search the Qobuz catalogue",
                value: "{library.query}",
                oninput: move |event| library.query.set(event.value()),
                onkeydown: move |event| {
                    if event.key() == Key::Enter {
                        submit();
                    }
                },
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
                span { class: "muted", "{view.label()}" }
                span { class: "spacer" }
                if !tracks.is_empty() {
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
    let mut selection = use_context::<Selection>();
    let local = use_context::<LocalIds>();
    let blocklist = use_context::<Blocklist>();

    let tracks = library.shelf.read().visible_tracks(&blocklist);
    let now_playing = player.current().map(|t| t.id);

    rsx! {
        h3 { class: "shelf-head", "Tracks" }
        ul { class: "list",
            for (index, track) in tracks.into_iter().enumerate() {
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
                    Cover { url: track.image.clone(), class: "thumb" }
                    span { class: "artist", "{track.artist}" }
                    span { class: "title", "{track.title}" }
                    if !track.streamable {
                        span { class: "muted tag", "-" }
                    }
                    if let Some(artist_id) = track.artist_id {
                        button {
                            class: "chip danger",
                            title: "hide this artist everywhere",
                            onclick: move |event| {
                                event.stop_propagation();
                                let name = library
                                    .shelf
                                    .peek()
                                    .tracks
                                    .iter()
                                    .find(|t| t.artist_id == Some(artist_id))
                                    .map(|t| t.artist.clone())
                                    .unwrap_or_default();

                                blocklist.block.call((artist_id, name));
                            },
                            "hide"
                        }
                    }
                    // Read back out of the shelf by index rather than captured:
                    // one owned `track` cannot move into two closures, and the
                    // visible list is the one the index refers to.
                    button {
                        class: "chip",
                        title: "play after the current track",
                        onclick: move |event| {
                            event.stop_propagation();
                            let found = library
                                .shelf
                                .peek()
                                .visible_tracks(&blocklist)
                                .get(index)
                                .cloned();
                            if let Some(track) = found {
                                play_next(player, vec![track]);
                            }
                        },
                        "next"
                    }
                    button {
                        class: "chip",
                        title: "add to the end of the queue",
                        onclick: move |event| {
                            event.stop_propagation();
                            let found = library
                                .shelf
                                .peek()
                                .visible_tracks(&blocklist)
                                .get(index)
                                .cloned();
                            if let Some(track) = found {
                                enqueue(player, vec![track]);
                            }
                        },
                        "last"
                    }
                    if local.0.read().contains(&track.id) {
                        // Analysed: this one exists as a point on the map.
                        button {
                            class: "chip",
                            onclick: move |event| {
                                event.stop_propagation();
                                selection.0.set(Some(track.id));
                            },
                            "in space"
                        }
                    }
                    span { class: "muted", "{track.duration_label()}" }
                }
            }
        }
    }
}

#[component]
fn AlbumRows() -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();

    let blocklist = use_context::<Blocklist>();
    let albums = library.shelf.read().visible_albums(&blocklist);

    rsx! {
        h3 { class: "shelf-head", "Albums" }
        ul { class: "list",
            for (index, album) in albums.into_iter().enumerate() {
                li {
                    key: "{index}-{album.id}",
                    class: "row",
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
                    Cover { url: album.image.clone(), class: "thumb" }
                    span { class: "artist", "{album.artist}" }
                    span { class: "title", "{album.title}" }
                    span { class: "muted", "{album.year()}" }
                    button {
                        class: "chip",
                        title: "play this album",
                        onclick: move |event| {
                            event.stop_propagation();
                            let id = library
                                .shelf
                                .peek()
                                .visible_albums(&blocklist)
                                .get(index)
                                .map(|a| a.id.clone());
                            let Some(id) = id else { return };
                            spawn(async move {
                                if let Ok(tracks) = backend().album_tracks(&id).await {
                                    play_list(player, tracks, 0);
                                }
                            });
                        },
                        "▶"
                    }
                    button {
                        class: "chip",
                        title: "fetch this tracklist into the catalogue",
                        onclick: move |event| {
                            event.stop_propagation();
                            let found = library
                                .shelf
                                .peek()
                                .visible_albums(&blocklist)
                                .get(index)
                                .map(|a| (a.id.clone(), a.title.clone()));
                            if let Some((id, title)) = found {
                                request_analysis(library, "album", &id, &title);
                            }
                        },
                        "fetch"
                    }
                }
            }
        }
    }
}

#[component]
fn ArtistRows(heading: String, similar: bool) -> Element {
    let library = use_context::<Library>();

    let blocklist = use_context::<Blocklist>();
    let artists = library.shelf.read().visible_artists(&blocklist, similar);

    rsx! {
        h3 { class: "shelf-head", "{heading}" }
        ul { class: "list",
            for (index, artist) in artists.into_iter().enumerate() {
                li {
                    key: "{index}-{artist.id}",
                    class: "row",
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
                    Cover { url: artist.image.clone(), class: "thumb round" }
                    span { class: "title", "{artist.name}" }
                    if let Some(count) = artist.albums_count {
                        span { class: "muted", "{count} albums" }
                    }
                    button {
                        class: "chip",
                        title: "fetch this discography into the catalogue",
                        onclick: move |event| {
                            event.stop_propagation();
                            let found = library
                                .shelf
                                .peek()
                                .visible_artists(&blocklist, similar)
                                .get(index)
                                .map(|a| (a.id, a.name.clone()));
                            if let Some((id, name)) = found {
                                request_analysis(library, "artist", &id.to_string(), &name);
                            }
                        },
                        "fetch"
                    }
                    button {
                        class: "chip danger",
                        title: "hide this artist everywhere",
                        onclick: move |event| {
                            event.stop_propagation();
                            blocklist.block.call((artist.id, artist.name.clone()));
                        },
                        "hide"
                    }
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


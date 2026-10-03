//! The detail pages: one track, one album, one artist, each a screen of the
//! main area rather than a sheet over it.
//!
//! What you can do with a thing sits at the top, under its cover: play it,
//! like it, add it to the space, or, for a track the space holds, name it as
//! either end of a path. The generate panel beside the main area is where
//! that path, or any other walk, then shows up.

use super::generate::{Generator, PathEnd};
use super::icons;
use super::library::{ArtistRows, Library, TracksView, View, ViewToggle};
use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, Player};
use super::{
    album_link, artist_link, open_remote_track, space_mark, Blocklist, Cover, LocalIds, MapView,
    Pipeline, Selection, SpaceReach,
};
use crate::api::{PipelineStatus, Progress, Stage, Target, MIN_TRACK_SECONDS};
use crate::backend::backend;
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemoteTrack};
use dioxus::prelude::*;

/// Add to or remove from the account's Qobuz favourites. `kind` is "track",
/// "album" or "artist". The heart flipping is the confirmation; only a
/// failure is worth a line of text.
fn toggle_like(
    kind: &'static str,
    id: String,
    currently_liked: bool,
    library: Library,
    mut status: Signal<Option<String>>,
) {
    let want = !currently_liked;
    let me = try_consume_context::<super::Me>().and_then(|me| me.0.peek().as_ref().map(|device| device.user.clone()));
    spawn(async move {
        let result = if want {
            backend().favorite_add(kind, &id).await
        } else {
            backend().favorite_remove(kind, &id).await
        };
        match result {
            Ok(()) => library.set_liked(kind, &id, want, me.filter(|name| !name.is_empty())),
            Err(err) => status.set(Some(format!("{err:#}"))),
        }
    });
}

/// How much of a page's album or discography the loaded space holds.
#[derive(Clone, Copy, PartialEq)]
enum Held {
    None,
    Some,
    All,
}

impl Held {
    fn of(held: usize, total: usize) -> Self {
        match held {
            0 => Held::None,
            n if n >= total => Held::All,
            _ => Held::Some,
        }
    }
}

/// How far `target` is on its way to the map, and what is happening to it.
/// Its own analyse is most of the bar; the rebuild after is shared with
/// whatever else is being added.
fn on_its_way(status: &PipelineStatus, target: &Target) -> (f64, &'static str) {
    let step = status.progress.as_ref();
    let fraction = step.map_or(0.0, Progress::fraction);
    let doing = |name: &str| step.is_some_and(|p| p.step == name);

    if status.waiting.contains(target) {
        (0.0, "waiting its turn")
    } else if status.target.as_ref() == Some(target) {
        if doing("analyse") {
            (0.1 + 0.6 * fraction, "analysing")
        } else if doing("catalogue") {
            (0.1 * fraction, "cataloguing")
        } else {
            (0.0, "starting")
        }
    } else if status.running == Some(Stage::BuildSpace) {
        (0.7 + 0.1 * fraction, "building the space")
    } else if status.running == Some(Stage::Layout) {
        (0.8 + 0.2 * fraction, "laying out the map")
    } else {
        (0.7, "waiting for the rest to be analysed")
    }
}

/// "In your space" when it already is, "Adding…" while the server works on it,
/// otherwise the button that asks it to: catalogue, analyse, then rebuild the
/// space and the map, ahead of the rest of the backlog.
fn add_or_badge(
    held: Held,
    target: Target,
    title: &'static str,
    mut status: Signal<Option<String>>,
) -> Element {
    let mut pipeline = consume_context::<Pipeline>();
    if held == Held::All {
        return rsx! { span { class: "badge in-space", "in your space" } };
    }
    if pipeline.status.read().adding.contains(&target) {
        let (fraction, doing) = on_its_way(&pipeline.status.read(), &target);
        let percent = (fraction * 100.0).round();
        return rsx! {
            button {
                class: "adding",
                style: "--progress: {percent}%",
                title: "{doing}; the pipeline output in settings has the detail",
                disabled: true,
                "Adding… {percent}%"
            }
        };
    }
    rsx! {
        button {
            title: "{title}",
            onclick: move |_| {
                let target = target.clone();
                status.set(None);
                spawn(async move {
                    match backend().pipeline_add(&target).await {
                        // Ahead of the next poll, so the button does not
                        // offer the same thing twice.
                        Ok(()) => pipeline.status.write().adding.push(target),
                        Err(err) => status.set(Some(format!("{err:#}"))),
                    }
                });
            },
            if held == Held::Some { "Add the rest" } else { "Add to space" }
        }
    }
}

/// The two path ends, green for where it starts and red for where it goes.
/// Disabled for a track the space does not hold: it has no coordinates to
/// walk from or to.
#[component]
pub(crate) fn PathButtons(track_id: i64, enabled: bool, compact: bool) -> Element {
    let generator = use_context::<Generator>();
    let from = *generator.from.read();
    let to = *generator.to.read();
    let why = if enabled { "" } else { ", only for tracks in your space" };

    rsx! {
        button {
            class: if from == Some(track_id) { "path-end a set" } else { "path-end a" },
            disabled: !enabled,
            title: "start a path here{why}",
            onclick: move |event: Event<MouseData>| {
                event.stop_propagation();
                generator.set_path_end(PathEnd::A, track_id);
            },
            if compact { "A" } else { "Path A" }
        }
        button {
            class: if to == Some(track_id) { "path-end b set" } else { "path-end b" },
            disabled: !enabled,
            title: "end a path here{why}",
            onclick: move |event: Event<MouseData>| {
                event.stop_propagation();
                generator.set_path_end(PathEnd::B, track_id);
            },
            if compact { "B" } else { "Path B" }
        }
    }
}

/// One line of the facts list, skipped when there is nothing to say.
fn fact(label: &'static str, value: Option<String>) -> Element {
    match value.filter(|v| !v.trim().is_empty()) {
        Some(value) => rsx! {
            div { class: "fact",
                dt { "{label}" }
                dd { "{value}" }
            }
        },
        None => rsx! {},
    }
}

fn clock(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

// -------------------------------------------------------------------- track

/// Everything known about one track, on one screen, so it can be shared as a
/// single screenshot: cover and names up top, then every fact the catalogue
/// and the space have about it.
#[component]
pub fn TrackScreen(track: RemoteTrack) -> Element {
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let local = use_context::<LocalIds>();
    let blocklist = use_context::<Blocklist>();
    let map = use_context::<MapView>();
    let mut selection = use_context::<Selection>().0;
    let mut panel_open = use_context::<Generator>().panel_open;
    let status = use_signal(|| None::<String>);

    library.ensure_liked_loaded();
    let liked = library.liked().tracks.contains(&track.id);
    let liked_by = library.liked().by("track", &track.id.to_string());
    let in_space = local.0.read().contains(&track.id);

    // The space's own row: genre, tempo, where the crawl found it, where it
    // sits on the map. None of it travels on a Qobuz track.
    let meta = if in_space { super::space_row(track.id) } else { None };

    // Looking at a track the space holds makes it the one in hand, for the
    // generate panel and the map, however the page was reached.
    let seed = track.id;
    use_effect(move || {
        if in_space {
            selection.set(Some(seed));
        }
    });

    let quality = if track.hires { "hi-res available" } else { "CD quality" };
    let credits: Vec<String> = track
        .performers
        .as_deref()
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();

    rsx! {
        div { class: "page",
            div { class: "hero",
                Cover { url: track.image.clone(), class: "hero-art eager" }
                div { class: "hero-meta",
                    h1 { class: "hero-title", "{track.title}" }
                    {artist_link(library, track.artist_id, track.artist.clone(), "hero-line")}
                    {album_link(library, &track, "hero-line muted")}

                    div { class: "hero-actions",
                        button {
                            class: "primary",
                            disabled: !track.streamable,
                            onclick: {
                                let track = track.clone();
                                move |_| play_list(player, vec![track.clone()], 0)
                            },
                            {icons::play()}
                            "Play"
                        }
                        button {
                            disabled: !track.streamable,
                            onclick: {
                                let track = track.clone();
                                move |_| enqueue(player, vec![track.clone()])
                            },
                            "Queue"
                        }
                        button {
                            class: if liked { "icon-btn liked" } else { "icon-btn" },
                            title: if liked { "unlike" } else { "like" },
                            onclick: {
                                let id = track.id.to_string();
                                move |_| toggle_like("track", id.clone(), liked, library, status)
                            },
                            {icons::heart(liked)}
                        }
                    }
                    div { class: "hero-actions",
                        {add_or_badge(
                            if in_space { Held::All } else { Held::None },
                            Target::Track(track.id),
                            "analyse this track and put it on the map",
                            status,
                        )}
                        PathButtons { track_id: track.id, enabled: in_space, compact: false }
                        if in_space {
                            button {
                                title: "show where it sits",
                                onclick: move |_| map.browse(),
                                {icons::constellation()}
                                "Map"
                            }
                            // Phone only: beside the main area the panel is
                            // already in view.
                            button {
                                class: "hero-generate",
                                title: "make a playlist from this track",
                                onclick: move |_| panel_open.set(true),
                                {icons::spark()}
                                "Generate"
                            }
                        }
                    }
                    if let Some(message) = status() {
                        p { class: "muted", "{message}" }
                    }
                }
            }

            div { class: "rule" }

            dl { class: "facts",
                {fact("Title", Some(track.title.clone()))}
                {fact("Artist", Some(track.artist.clone()))}
                {fact("Album", Some(track.album.clone()))}
                {fact("Duration", track.duration.filter(|d| *d > 0).map(clock))}
                {fact("Released", track.released.clone())}
                {fact("Genre", meta.as_ref().map(|m| m.genre.clone()))}
                {fact("Tempo", meta.as_ref().and_then(|m| m.bpm).map(|bpm| format!("{bpm:.0} bpm")))}
                {fact("ISRC", track.isrc.clone().or_else(|| meta.as_ref().and_then(|m| m.isrc.clone())))}
                {fact("Quality", Some(quality.to_string()))}
                {fact("Streamable", Some(if track.streamable { "yes" } else { "not here" }.to_string()))}
                {fact("Liked by", liked_by)}
                {fact("In your space", Some(if in_space { "yes" } else { "no" }.to_string()))}
                {fact("Crawl distance", meta.as_ref().map(|m| format!("{} hops from a like", m.seed_distance)))}
                {fact("On the map", meta.as_ref().and_then(|m| Some(format!("{:.2}, {:.2}", m.x?, m.y?))))}
                {fact("Qobuz track", Some(track.id.to_string()))}
                {fact("Qobuz album", track.album_id.clone())}
            }

            if !credits.is_empty() {
                h3 { class: "shelf-head", "Credits" }
                ul { class: "credits",
                    for (i, credit) in credits.into_iter().enumerate() {
                        li { key: "{i}", "{credit}" }
                    }
                }
            }

            if let Some(artist_id) = track.artist_id.filter(|_| *blocklist.editable.read()) {
                div { class: "page-foot",
                    button {
                        class: "danger",
                        onclick: {
                            let name = track.artist.clone();
                            move |_| {
                                blocklist.block.call((artist_id, name.clone()));
                                library.back();
                            }
                        },
                        "Hide {track.artist}"
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------------- album

/// An album: cover and details, then its tracklist, each row playable and
/// ready to be either end of a path.
#[component]
pub fn AlbumScreen(album: RemoteAlbum) -> Element {
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let local = use_context::<LocalIds>();
    let reach = use_context::<SpaceReach>().0;
    let blocklist = use_context::<Blocklist>();
    let selection = use_context::<Selection>().0;
    let mut menu = use_context::<ContextMenu>().0;
    let status = use_signal(|| None::<String>);

    library.ensure_liked_loaded();
    let liked = library.liked().albums.contains(&album.id);
    let liked_by = library.liked().by("album", &album.id);

    // `LibraryPanel`'s drive fetched these for this view; the menu's
    // `ShelfTrack` indices address the same list.
    let tracks = library.shelf.read().visible_tracks(&blocklist);
    let no_tracks = tracks.is_empty();
    let loading = *library.loading.read();
    let now_playing = player.current().map(|t| t.id);
    let local_ids = local.0.read().clone();
    // Counted over the tracklist rather than asked of the album: one track
    // added on its own puts the album in the space, but not the rest of it.
    // Interludes too short to analyse never will be, so they do not count.
    let analysable: Vec<i64> = tracks
        .iter()
        .filter(|t| t.duration.is_none_or(|d| d >= MIN_TRACK_SECONDS))
        .map(|t| t.id)
        .collect();
    let held = if loading || analysable.is_empty() {
        if reach.read().0.contains(&album.id) { Held::All } else { Held::None }
    } else {
        Held::of(analysable.iter().filter(|id| local_ids.contains(id)).count(), analysable.len())
    };

    // An album reached from a track only knows what the track did; the
    // tracklist fills in the rest once it lands.
    let released = album
        .released
        .clone()
        .or_else(|| tracks.iter().find_map(|t| t.released.clone()));
    let total: i64 = tracks.iter().filter_map(|t| t.duration).sum();
    let count = album.tracks_count.unwrap_or(tracks.len() as i64);
    let details: Vec<String> = [
        released,
        album.genre.clone(),
        album.label.clone(),
        (count > 0).then(|| format!("{count} tracks")),
        (total > 0).then(|| clock(total)),
        liked_by.map(|name| format!("liked by {name}")),
    ]
    .into_iter()
    .flatten()
    .collect();

    rsx! {
        div { class: "page",
            div { class: "hero",
                Cover { url: album.image.clone(), class: "hero-art eager" }
                div { class: "hero-meta",
                    h1 { class: "hero-title", "{album.title}" }
                    {artist_link(library, album.artist_id, album.artist.clone(), "hero-line")}
                    if !details.is_empty() {
                        span { class: "hero-line muted", {details.join(" · ")} }
                    }
                    div { class: "hero-actions",
                        button {
                            class: "primary",
                            disabled: no_tracks,
                            onclick: move |_| {
                                let queue = library.shelf.peek().visible_tracks(&blocklist);
                                play_list(player, queue, 0);
                            },
                            {icons::play()}
                            "Play"
                        }
                        button {
                            disabled: no_tracks,
                            onclick: move |_| {
                                let queue = library.shelf.peek().visible_tracks(&blocklist);
                                enqueue(player, queue);
                            },
                            "Queue"
                        }
                        button {
                            class: if liked { "icon-btn liked" } else { "icon-btn" },
                            title: if liked { "unlike" } else { "like" },
                            onclick: {
                                let id = album.id.clone();
                                move |_| toggle_like("album", id.clone(), liked, library, status)
                            },
                            {icons::heart(liked)}
                        }
                        {add_or_badge(
                            held,
                            Target::Album(album.id.clone()),
                            "analyse this album and put it on the map",
                            status,
                        )}
                    }
                    if let Some(message) = status() {
                        p { class: "muted", "{message}" }
                    }
                }
            }

            div { class: "rule" }

            if loading {
                p { class: "muted", "loading…" }
            } else if let Some(message) = library.error.read().clone() {
                p { class: "muted error", "{message}" }
            } else {
                ol { class: "list album-tracks",
                    for (index, track) in tracks.into_iter().enumerate() {
                        li {
                            key: "{index}-{track.id}",
                            class: if now_playing == Some(track.id) { "row playing" } else { "row" },
                            onclick: {
                                let track = track.clone();
                                move |_| open_remote_track(library, selection, &local, track.clone())
                            },
                            "data-menu": MenuTarget::ShelfTrack(index).tag(),
                            oncontextmenu: move |event: Event<MouseData>| {
                                event.prevent_default();
                                open_menu(&mut menu, &event, MenuTarget::ShelfTrack(index));
                            },
                            span { class: "title",
                                "{track.title}"
                                if track.artist != album.artist {
                                    span { class: "muted", " · {track.artist}" }
                                }
                            }
                            if local_ids.contains(&track.id) {
                                span { class: "in-space-dot", title: "in your space", "•" }
                            }
                            span { class: "muted", "{track.duration_label()}" }
                            div { class: "row-actions",
                                button {
                                    class: "icon-btn",
                                    title: "play from here",
                                    disabled: !track.streamable,
                                    onclick: move |event: Event<MouseData>| {
                                        event.stop_propagation();
                                        let queue = library.shelf.peek().visible_tracks(&blocklist);
                                        play_list(player, queue, index);
                                    },
                                    {icons::play()}
                                }
                                PathButtons {
                                    track_id: track.id,
                                    enabled: local_ids.contains(&track.id),
                                    compact: true,
                                }
                            }
                            {menu_button(menu, MenuTarget::ShelfTrack(index))}
                        }
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------- artist

/// An artist: portrait, a biography folded to a few lines, then the
/// discography as a grid or a list, then who Qobuz thinks is close.
#[component]
pub fn ArtistScreen(artist: RemoteArtist) -> Element {
    let library = use_context::<Library>();
    let reach = use_context::<SpaceReach>().0;
    let blocklist = use_context::<Blocklist>();
    let mut menu = use_context::<ContextMenu>().0;
    let status = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| false);

    library.ensure_liked_loaded();
    let followed = library.liked().artists.contains(&artist.id);
    let followed_by = library.liked().by("artist", &artist.id.to_string());

    // The portrait and biography, which nothing that links here carries. A
    // server from before that route answers 404; the page just goes without.
    let mut info: Signal<Option<RemoteArtist>> = use_signal(|| None);
    {
        let id = artist.id;
        use_future(move || async move {
            if let Ok(found) = backend().artist(id).await {
                info.set(Some(found));
            }
        });
    }
    let fetched = info.read().clone();
    let image = fetched.as_ref().and_then(|a| a.image.clone()).or_else(|| artist.image.clone());
    let biography = fetched.as_ref().and_then(|a| a.biography.clone());
    let albums_count = fetched.as_ref().and_then(|a| a.albums_count).or(artist.albums_count);

    let shelf = library.shelf.read().clone();
    let albums = shelf.visible_albums(&blocklist);
    let has_similar = !shelf.visible_artists(&blocklist, true).is_empty();
    let loading = *library.loading.read();
    let grid = library.layout() == TracksView::Grid;
    // Over the discography the page lists, which is what adding them takes.
    let held = {
        let reach = reach.read();
        if loading || shelf.albums.is_empty() {
            if reach.1.contains(&artist.id) { Held::Some } else { Held::None }
        } else {
            let count = shelf.albums.iter().filter(|album| reach.0.contains(&album.id)).count();
            Held::of(count, shelf.albums.len())
        }
    };

    rsx! {
        div { class: "page",
            div { class: "hero",
                Cover { url: image, class: "hero-art round eager" }
                div { class: "hero-meta",
                    h1 { class: "hero-title", "{artist.name}" }
                    if let Some(count) = albums_count {
                        span { class: "hero-line muted", "{count} albums" }
                    }
                    if let Some(name) = followed_by {
                        span { class: "hero-line muted", "followed by {name}" }
                    }
                    div { class: "hero-actions",
                        button {
                            class: if followed { "icon-btn liked" } else { "icon-btn" },
                            title: if followed { "unfollow" } else { "follow" },
                            onclick: {
                                let id = artist.id.to_string();
                                move |_| toggle_like("artist", id.clone(), followed, library, status)
                            },
                            {icons::heart(followed)}
                        }
                        {add_or_badge(
                            held,
                            Target::Artist(artist.id),
                            "analyse every album of theirs and put them on the map",
                            status,
                        )}
                        if *blocklist.editable.read() {
                            button {
                                class: "danger",
                                onclick: {
                                    let (id, name) = (artist.id, artist.name.clone());
                                    move |_| {
                                        blocklist.block.call((id, name.clone()));
                                        library.back();
                                    }
                                },
                                "Hide"
                            }
                        }
                    }
                    if let Some(message) = status() {
                        p { class: "muted", "{message}" }
                    }
                }
            }

            // Folded by default: a biography can run to pages, and the
            // discography under it is what most visits are for.
            if let Some(biography) = biography {
                div { class: if expanded() { "bio open" } else { "bio" },
                    for (i, paragraph) in biography.lines().enumerate() {
                        p { key: "{i}", "{paragraph}" }
                    }
                }
                button {
                    class: "link bio-toggle",
                    onclick: move |_| {
                        let now = expanded();
                        expanded.set(!now);
                    },
                    if expanded() { "less" } else { "more" }
                }
            }

            div { class: "rule" }

            div { class: "shelf-head",
                "Albums"
                span { class: "spacer" }
                ViewToggle {}
            }

            if loading {
                p { class: "muted", "loading…" }
            } else if let Some(message) = library.error.read().clone() {
                p { class: "muted error", "{message}" }
            } else if albums.is_empty() {
                p { class: "muted", "nothing here" }
            } else {
                ul { class: if grid { "tiles" } else { "list" },
                    for (index, album) in albums.into_iter().enumerate() {
                        li {
                            key: "{index}-{album.id}",
                            class: if grid { "tile" } else { "row" },
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
                                {space_mark(reach.read().0.contains(&album.id))}
                                span { class: "title", "{album.title}" }
                                span { class: "artist", "{album.year()}" }
                            } else {
                                span { class: "title", "{album.title}" }
                                if reach.read().0.contains(&album.id) {
                                    span { class: "in-space-dot", title: "in your space", "•" }
                                }
                                if let Some(count) = album.tracks_count {
                                    span { class: "muted", "{count} tracks" }
                                }
                                span { class: "muted", "{album.year()}" }
                            }
                            {menu_button(menu, MenuTarget::ShelfAlbum(index))}
                        }
                    }
                }
            }

            if has_similar {
                ArtistRows { heading: "Similar artists".to_string(), similar: true }
            }
        }
    }
}

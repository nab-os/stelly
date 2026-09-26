//! The detail sheet: a track, an album or an artist, and everything you can
//! do with it.
//!
//! Used to be two panels in a column beside the browse list: `DetailPane` for
//! the track itself, `GeneratePanel` for what the space could make from it,
//! plus the weight sliders sitting under both. On a phone that column was a
//! second screen behind a tab switch, so the single most common thing this
//! app is for, find a track, do something with it, cost a navigation on
//! top of a tap.
//!
//! One sheet now, and it is no longer track-only: an album or an artist opens
//! it too, by the same `Detail` context, dispatched on `DetailSubject`.
//! Opening and closing go through `Detail`, not `Selection`, see
//! `Selection`'s doc comment in `ui/mod.rs` for why the two used to be the
//! same flag and why that broke "close the sheet, keep the point highlighted
//! on the map". Below the docking breakpoint the sheet overlays the list as a
//! bottom sheet; above it, `.sheet` docks in the flex row beside `.library`
//! instead, see the `@media` block in `style.css` this shares with the old
//! `.side` column's width.

use super::generate::{as_remote, describe, Generator, Mode, PathEnd};
use super::library::{request_analysis, Library, View};
use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, play_next, Player};
use super::{
    artist_link, open_artist, open_track, Blocklist, Cover, Detail, DetailSubject, LocalIds,
    MapView, Selection, SpaceReach, space_mark, Weights, LIST_CAP,
};
use crate::backend::backend;
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemoteTrack};
use dioxus::prelude::*;
use std::collections::HashMap;

/// A space track's bpm, the one field `RemoteTrack` does not carry, Qobuz's
/// API has no such thing, so it only ever exists once a track has been
/// through the pipeline.
fn space_bpm(track_id: i64) -> Option<f32> {
    let guard = crate::engine().lock().unwrap();
    let row = *guard.navigator.index_of.get(&track_id)?;
    guard.navigator.catalog.get(row).bpm
}

async fn export(track_ids: Vec<i64>) -> anyhow::Result<i64> {
    let name = format!("two_khz ({} tracks)", track_ids.len());
    backend().export_playlist(&name, &track_ids).await
}

/// Fetch one album's tracklist and hand it to a player verb. Every playback
/// action from an album needs this, and it is the same fetch three times.
fn with_album_tracks(album_id: String, player: Player, then: fn(Player, Vec<RemoteTrack>)) {
    spawn(async move {
        if let Ok(tracks) = backend().album_tracks(&album_id).await {
            then(player, tracks);
        }
    });
}

/// Add to the account's Qobuz favourites. `kind` is "track", "album" or
/// "artist"; `progress`/`done` are the status line before and after ("liking
/// this track…" / "liked", "following…" / "followed", for an artist).
/// Reports success or failure rather than reflecting back whether the thing
/// was already liked, that would need the same favourited-id cache the
/// "liked" search filter lazily loads (`Library::liked_state`), which most
/// sessions opening a detail sheet without ever searching will not have
/// fetched.
fn toggle_like(
    kind: &'static str,
    id: String,
    currently_liked: bool,
    library: Library,
    mut status: Signal<Option<String>>,
) {
    let want = !currently_liked;
    spawn(async move {
        let result = if want {
            backend().favorite_add(kind, &id).await
        } else {
            backend().favorite_remove(kind, &id).await
        };
        match result {
            // The filled/empty heart is the confirmation; a status line on
            // top of it would just be saying the same thing twice.
            Ok(()) => library.set_liked(kind, &id, want),
            Err(err) => status.set(Some(format!("{err:#}"))),
        }
    });
}

#[component]
pub fn DetailSheet() -> Element {
    let detail = use_context::<Detail>().0;

    let Some(subject) = detail() else {
        return rsx! {};
    };

    let mut close_signal = detail;
    let close = move |_| close_signal.set(None);

    rsx! {
        // Mobile only, see `.sheet-backdrop` in style.css. On a docked
        // sheet this is invisible and unclickable, so it never swallows a
        // click meant for the list beside it.
        div { class: "sheet-backdrop", onclick: close }

        section { class: "panel sheet",
            // A handle to drag closed, and something for a mouse to grab
            // even though there is no drag-to-dismiss wired up yet, it
            // still reads as "this is a sheet" rather than "this is a panel".
            div { class: "sheet-drag" }

            div { class: "sheet-head",
                span { class: "spacer" }
                button { class: "chip sheet-close", onclick: close, "×" }
            }

            match subject {
                // Keyed on the subject's own id: switching from one album's
                // sheet straight to another's is a prop change on the *same*
                // component instance as far as Dioxus is concerned, which
                // would leave the previous album's fetched tracklist on
                // screen under the new heading until the new fetch lands.
                // The key forces a fresh mount instead.
                DetailSubject::Track(track) => rsx! {
                    TrackDetail { key: "{track.id}", track }
                },
                DetailSubject::Album(album) => rsx! {
                    AlbumDetail { key: "{album.id}", album, inline: false }
                },
                DetailSubject::Artist(artist) => rsx! {
                    ArtistDetail { key: "{artist.id}", artist, inline: false }
                },
            }
        }
    }
}

#[component]
fn TrackDetail(track: RemoteTrack) -> Element {
    let player = use_context::<Player>();
    let generator = use_context::<Generator>();
    let map = use_context::<MapView>();
    let mut detail = use_context::<Detail>().0;
    let local = use_context::<LocalIds>();
    let blocklist = use_context::<Blocklist>();
    let library = use_context::<Library>();
    let status = use_signal(|| None::<String>);

    // Whichever sheet asks first pays for the fetch; see `Library::liked`.
    library.ensure_liked_loaded();
    let liked = library.liked().tracks.contains(&track.id);

    let in_space = local.0.read().contains(&track.id);
    let bpm = if in_space { space_bpm(track.id) } else { None };

    // Generate, Path A/B and the map all read `Selection`, not the sheet.
    // `open_track` sets both, but not every way into a track's sheet goes
    // through it, an album's embedded tracklist sets the sheet alone, and
    // Generate then seeded from whatever was selected *before*. Syncing here
    // covers every path at once. Runs once per track: the sheet keys this
    // component on the track id, so a different track is a fresh mount.
    let mut selection = use_context::<Selection>().0;
    let seed = track.id;
    use_effect(move || {
        if in_space {
            selection.set(Some(seed));
        }
    });
    let from = *generator.from.read();
    let to = *generator.to.read();

    rsx! {
        div { class: "detail",
            Cover { url: track.image.clone(), class: "detail-art eager" }

            div { class: "detail-meta",
                span { class: "detail-title ellipsis", "{track.title}" }
                if let Some(artist_id) = track.artist_id {
                    button {
                        class: "link ellipsis",
                        onclick: {
                            let name = track.artist.clone();
                            move |_| open_artist(detail, artist_id, name.clone())
                        },
                        "{track.artist}"
                    }
                } else {
                    span { class: "muted ellipsis", "{track.artist}" }
                }
                if !track.album.is_empty() {
                    span { class: "muted ellipsis", "{track.album}" }
                }
                div { class: "detail-badges",
                    if in_space {
                        span { class: "chip active", "in your space" }
                    }
                    if let Some(bpm) = bpm {
                        span { class: "chip", "{bpm:.0} bpm" }
                    }
                    if let Some(released) = &track.released {
                        span { class: "chip", "{released}" }
                    }
                    if !track.streamable {
                        span { class: "chip", "not streamable here" }
                    }
                }
                // Qobuz's own credit string, split and printed rather than
                // reparsed, see `RemoteTrack::performers`'s doc comment on
                // why its exact grammar is not something to lean on.
                if let Some(performers) = &track.performers {
                    div { class: "detail-credits",
                        for (i, credit) in performers.split(';').map(str::trim).filter(|c| !c.is_empty()).enumerate() {
                            span { key: "{i}", class: "muted", "{credit}" }
                        }
                    }
                }
            }
        }

        div { class: "detail-actions",
            button {
                class: "primary",
                disabled: !track.streamable,
                onclick: {
                    let track = track.clone();
                    move |_| play_list(player, vec![track.clone()], 0)
                },
                "Play"
            }
            button {
                disabled: !track.streamable,
                onclick: {
                    let track = track.clone();
                    move |_| play_next(player, vec![track.clone()])
                },
                "Play next"
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
                class: if liked { "liked" } else { "" },
                title: if liked { "unlike" } else { "like" },
                onclick: {
                    let id = track.id.to_string();
                    move |_| toggle_like("track", id.clone(), liked, library, status)
                },
                if liked { "♥ Liked" } else { "♡ Like" }
            }
            // Closes on the way there, keeping `Selection` (so the map still
            // highlights this track), unlike every other close in this
            // file, which is a plain deselect. See `Selection`'s doc comment.
            if in_space {
                button {
                    onclick: move |_| {
                        map.browse();
                        detail.set(None);
                    },
                    "On the map"
                }
            }
        }

        // Right under the heart that produced it. It used to sit at the very
        // foot of the sheet, below Generate and the weight sliders, where a
        // failed like read as the button simply doing nothing.
        if let Some(message) = status() {
            p { class: "error", "{message}" }
        }

        // The verbs that used to fire the instant anything was selected.
        // Named, and asked for. Only offered once the space actually has
        // coordinates to walk from.
        if in_space {
            div { class: "detail-actions",
                button {
                    onclick: move |_| generator.request(Mode::Neighbours, track.id),
                    "Find neighbours"
                }
                button {
                    onclick: move |_| generator.request(Mode::Radio, track.id),
                    "Start radio"
                }
                button {
                    disabled: from == Some(track.id),
                    onclick: move |_| generator.set_path_end(PathEnd::A, track.id),
                    if to.is_some() && from != Some(track.id) { "Path A, go" } else { "Path A" }
                }
                button {
                    disabled: to == Some(track.id),
                    onclick: move |_| generator.set_path_end(PathEnd::B, track.id),
                    if from.is_some() && to != Some(track.id) { "Path B, go" } else { "Path B" }
                }
                button {
                    // Deliberately does not run: drift still needs a phrase,
                    // and inventing one would be inventing the intent. This
                    // was reachable from the row menu and nowhere in the
                    // sheet itself, despite drift having its own tab below.
                    onclick: move |_| generator.aim_drift(),
                    "Drift from here"
                }
            }

            div { class: "sheet-rule" }

            GenerateSection {}

            WeightsDisclosure {}
        }

        if let Some((artist_id, name)) = track.artist_id.map(|id| (id, track.artist.clone())) {
            div { class: "detail-actions",
                button {
                    class: "danger",
                    onclick: move |_| {
                        blocklist.block.call((artist_id, name.clone()));
                        detail.set(None);
                    },
                    "Hide {name}"
                }
            }
        }
    }
}

/// `inline` is true when this is rendering atop the album's own page
/// (`LibraryPanel`, once `View::Album` is showing) rather than in the popup
/// sheet, the one difference being that fetching and showing the tracklist
/// again here would just duplicate the one `LibraryPanel` already shows
/// right below it.
#[component]
pub(crate) fn AlbumDetail(album: RemoteAlbum, inline: bool) -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();
    let blocklist = use_context::<Blocklist>();
    let mut detail = use_context::<Detail>().0;
    let local = use_context::<LocalIds>();
    let mut status = use_signal(|| None::<String>);

    library.ensure_liked_loaded();
    let liked = library.liked().albums.contains(&album.id);

    // The tracklist, fetched once on mount rather than behind a click,
    // asking to see an album and getting a button that asks again was the
    // extra step this replaced. `None` while in flight; the caller keys this
    // component on the album id (see `DetailSheet`), so a different album
    // is a fresh mount and a fresh fetch rather than stale rows lingering
    // under a new heading.
    let mut tracks: Signal<Option<Vec<RemoteTrack>>> = use_signal(|| None);
    // Unconditional, Dioxus panics if the number of hooks a component
    // calls changes between renders, so the `inline` check has to live
    // inside the future rather than around the `use_future` call itself.
    {
        let id = album.id.clone();
        use_future(move || {
            let id = id.clone();
            async move {
                if !inline {
                    tracks.set(backend().album_tracks(&id).await.ok());
                }
            }
        });
    }

    rsx! {
        div { class: "detail",
            Cover { url: album.image.clone(), class: "detail-art eager" }
            div { class: "detail-meta",
                span { class: "detail-title ellipsis", "{album.title}" }
                if let Some(artist_id) = album.artist_id {
                    button {
                        class: "link ellipsis",
                        onclick: {
                            let name = album.artist.clone();
                            move |_| open_artist(detail, artist_id, name.clone())
                        },
                        "{album.artist}"
                    }
                } else {
                    span { class: "muted ellipsis", "{album.artist}" }
                }
                if let Some(released) = &album.released {
                    span { class: "muted ellipsis", "{released}" }
                }
                div { class: "detail-badges",
                    if let Some(count) = album.tracks_count {
                        span { class: "chip", "{count} tracks" }
                    }
                    if let Some(genre) = &album.genre {
                        span { class: "chip", "{genre}" }
                    }
                    if let Some(label) = &album.label {
                        span { class: "chip", "{label}" }
                    }
                }
            }
        }

        div { class: "detail-actions",
            button {
                class: "primary",
                onclick: {
                    let id = album.id.clone();
                    move |_| with_album_tracks(id.clone(), player, |p, t| play_list(p, t, 0))
                },
                "Play"
            }
            button {
                onclick: {
                    let id = album.id.clone();
                    move |_| with_album_tracks(id.clone(), player, play_next)
                },
                "Play next"
            }
            button {
                onclick: {
                    let id = album.id.clone();
                    move |_| with_album_tracks(id.clone(), player, enqueue)
                },
                "Queue"
            }
            button {
                class: if liked { "liked" } else { "" },
                title: if liked { "unlike" } else { "like" },
                onclick: {
                    let id = album.id.clone();
                    move |_| toggle_like("album", id.clone(), liked, library, status)
                },
                if liked { "♥ Liked" } else { "♡ Like" }
            }
        }

        div { class: "detail-actions",
            button {
                title: "fetch this tracklist into the catalogue",
                onclick: {
                    let (id, title) = (album.id.clone(), album.title.clone());
                    move |_| {
                        request_analysis(library, "album", &id, &title);
                        status.set(Some("fetching…".into()));
                    }
                },
                "Fetch into catalogue"
            }
            if let Some(artist_id) = album.artist_id {
                button {
                    class: "danger",
                    onclick: {
                        let name = album.artist.clone();
                        move |_| {
                            blocklist.block.call((artist_id, name.clone()));
                            detail.set(None);
                        }
                    },
                    "Hide {album.artist}"
                }
            }
        }

        if let Some(message) = status() {
            p { class: "muted", "{message}" }
        }

        // The tracklist itself, in the popup sheet only, `LibraryPanel`
        // already draws this album's tracks below when this is the inline
        // header on the album's own page.
        if !inline {
            div { class: "shelf-head",
                span { class: "muted", "Tracklist" }
                span { class: "spacer" }
                // The grid/list toggle, per-row menu (hide, queue, play
                // next…) and the "in your space" mark all live on the full
                // page, not on this quick look, this is the way there for
                // when a plain list of titles is not enough.
                button {
                    class: "link",
                    onclick: {
                        let album = album.clone();
                        move |_| {
                            library.go(View::Album(album.clone()));
                            detail.set(None);
                        }
                    },
                    "open as a page →"
                }
            }
            match tracks() {
                None => rsx! { p { class: "muted", "loading…" } },
                Some(tracks) if tracks.is_empty() => rsx! {
                    p { class: "muted", "nothing here" }
                },
                Some(tracks) => rsx! {
                    ol { class: "list",
                        for (index, track) in tracks.into_iter().enumerate() {
                            li {
                                key: "{index}-{track.id}",
                                class: "row",
                                onclick: {
                                    let track = track.clone();
                                    move |_| detail.set(Some(DetailSubject::Track(track.clone())))
                                },
                                {artist_link(detail, track.artist_id, track.artist.clone(), "artist")}
                                span { class: "title", "{track.title}" }
                                if local.0.read().contains(&track.id) {
                                    span { class: "in-space-dot", title: "analysed, on the map", "•" }
                                }
                                span { class: "muted", "{track.duration_label()}" }
                            }
                        }
                    }
                },
            }
        }
    }
}

/// `inline` is true atop the artist's own page, same as `AlbumDetail`.
#[component]
pub(crate) fn ArtistDetail(artist: RemoteArtist, inline: bool) -> Element {
    let library = use_context::<Library>();
    let blocklist = use_context::<Blocklist>();
    let mut detail = use_context::<Detail>().0;
    let reach = use_context::<SpaceReach>().0;
    let mut status = use_signal(|| None::<String>);

    library.ensure_liked_loaded();
    let followed = library.liked().artists.contains(&artist.id);

    // As `AlbumDetail`'s tracklist: fetched once on mount rather than
    // behind a "View discography" click. See its comment on why this is
    // unconditional and the id-keying that keeps it from going stale.
    let mut albums: Signal<Option<Vec<RemoteAlbum>>> = use_signal(|| None);
    {
        let id = artist.id;
        use_future(move || async move {
            if !inline {
                albums.set(backend().artist_albums(id, LIST_CAP).await.ok());
            }
        });
    }

    rsx! {
        div { class: "detail",
            Cover { url: artist.image.clone(), class: "detail-art eager round" }
            div { class: "detail-meta",
                span { class: "detail-title ellipsis", "{artist.name}" }
                if let Some(count) = artist.albums_count {
                    span { class: "muted ellipsis", "{count} albums" }
                }
            }
        }

        div { class: "detail-actions",
            button {
                title: "fetch this discography into the catalogue",
                onclick: {
                    let (id, name) = (artist.id, artist.name.clone());
                    move |_| {
                        request_analysis(library, "artist", &id.to_string(), &name);
                        status.set(Some("fetching…".into()));
                    }
                },
                "Fetch discography"
            }
            button {
                class: if followed { "liked" } else { "" },
                title: if followed { "unfollow" } else { "follow" },
                onclick: {
                    let id = artist.id.to_string();
                    move |_| toggle_like("artist", id.clone(), followed, library, status)
                },
                if followed { "♥ Following" } else { "♡ Follow" }
            }
        }

        div { class: "detail-actions",
            button {
                class: "danger",
                onclick: {
                    let (id, name) = (artist.id, artist.name.clone());
                    move |_| {
                        blocklist.block.call((id, name.clone()));
                        detail.set(None);
                    }
                },
                "Hide everywhere"
            }
        }

        if let Some(message) = status() {
            p { class: "muted", "{message}" }
        }

        // The discography itself, in the popup sheet only, same reasoning
        // as `AlbumDetail`'s tracklist.
        if !inline {
            div { class: "shelf-head",
                span { class: "muted", "Discography" }
                span { class: "spacer" }
                button {
                    class: "link",
                    onclick: {
                        let artist = artist.clone();
                        move |_| {
                            library.go(View::Artist(artist.clone()));
                            detail.set(None);
                        }
                    },
                    "open as a page →"
                }
            }
            match albums() {
                None => rsx! { p { class: "muted", "loading…" } },
                Some(albums) if albums.is_empty() => rsx! {
                    p { class: "muted", "nothing here" }
                },
                Some(albums) => rsx! {
                    ul { class: "tiles",
                        for (index, album) in albums.into_iter().enumerate() {
                            li {
                                key: "{index}-{album.id}",
                                class: "tile",
                                onclick: {
                                    let album = album.clone();
                                    move |_| detail.set(Some(DetailSubject::Album(album.clone())))
                                },
                                Cover { url: album.image.clone() }
                                {space_mark(reach.read().0.contains(&album.id))}
                                span { class: "title", "{album.title}" }
                                span { class: "artist", "{album.artist}" }
                            }
                        }
                    }
                },
            }
        }
    }
}

/// The four modes and their result, unindented from the track above: this is
/// "what to make from it", not a separate feature. Only ever shown inside a
/// `TrackDetail` for a space track, see there.
#[component]
fn GenerateSection() -> Element {
    let generator = use_context::<Generator>();
    let player = use_context::<Player>();
    let selection = use_context::<Selection>();
    let detail = use_context::<Detail>().0;
    let selected = selection.0;
    let mut menu = use_context::<ContextMenu>().0;
    let map = use_context::<MapView>();

    let mode = *generator.mode.read();
    let result = generator.result.read().clone();
    let empty = result.is_empty();
    let has_selection = selected().is_some();

    let label_for = |id: Option<i64>| -> String {
        let Some(id) = id else { return "-".into() };
        let guard = crate::engine().lock().unwrap();
        guard
            .navigator
            .catalog
            .tracks
            .iter()
            .find(|t| t.track_id == id)
            .map(|t| format!("{} - {}", t.artist, t.title))
            .unwrap_or_else(|| id.to_string())
    };

    // Asked once: whether the backend has a text tower at all. Locally that
    // is an exported ONNX file, remotely it is whether the server has one.
    use_future(move || async move {
        let mut tower = generator.tower;
        if let Ok(available) = backend().can_steer().await {
            tower.set(available);
        }
    });

    rsx! {
        h2 { "Explore from here" }

        div { class: "tabs",
            for option in Mode::ALL {
                button {
                    key: "{option.label()}",
                    class: if mode == option { "tab active" } else { "tab" },
                    // Switching tabs changes which inputs are shown, and
                    // nothing else. It used to clear the result, which the
                    // auto-run effect then immediately undid; now the
                    // result stays, labelled with what actually made it.
                    onclick: move |_| {
                        let mut mode = generator.mode;
                        mode.set(option);
                    },
                    "{option.label()}"
                }
            }
        }
        p { class: "muted", "{mode.blurb()}" }

        // ------------------------------------------------ mode inputs
        if mode == Mode::Path {
            div { class: "field",
                span { class: "muted", "A" }
                span { class: "ellipsis", {label_for(*generator.from.read())} }
                button {
                    class: "chip",
                    disabled: !has_selection,
                    onclick: move |_| { let mut from = generator.from; from.set(selected()); },
                    "set"
                }
            }
            div { class: "field",
                span { class: "muted", "B" }
                span { class: "ellipsis", {label_for(*generator.to.read())} }
                button {
                    class: "chip",
                    disabled: !has_selection,
                    onclick: move |_| { let mut to = generator.to; to.set(selected()); },
                    "set"
                }
            }
            div { class: "field",
                span { class: "muted", "shape" }
                button {
                    class: if *generator.even.read() { "chip" } else { "chip active" },
                    onclick: move |_| { let mut even = generator.even; even.set(false); },
                    "shortest"
                }
                button {
                    class: if *generator.even.read() { "chip active" } else { "chip" },
                    onclick: move |_| { let mut even = generator.even; even.set(true); },
                    "evenly paced"
                }
            }
        }

        if mode == Mode::Drift && !generator.can_steer() {
            p { class: "muted error",
                "Drift needs the CLAP text tower on the server. Fetch it there with: "
                code { "two-khz-server models" }
            }
        }

        if mode == Mode::Drift {
            input {
                class: "search",
                placeholder: "darker and slower",
                value: "{generator.phrase}",
                oninput: move |event| { let mut phrase = generator.phrase; phrase.set(event.value()); },
            }
        }

        if mode == Mode::Radio {
            div { class: "slider",
                label { "artist pull" }
                input {
                    r#type: "range",
                    min: "0",
                    max: "0.3",
                    step: "0.01",
                    value: "{generator.artist_penalty}",
                    oninput: move |event| {
                        if let Ok(value) = event.value().parse::<f32>() {
                            let mut penalty = generator.artist_penalty;
                            penalty.set(value);
                        }
                    },
                }
                span { class: "muted", {format!("{:.2}", generator.artist_penalty)} }
            }
        }

        if mode != Mode::Path || *generator.even.read() {
            div { class: "slider",
                label { "tracks" }
                input {
                    r#type: "range",
                    min: "5",
                    max: "60",
                    step: "1",
                    value: "{generator.count}",
                    oninput: move |event| {
                        if let Ok(value) = event.value().parse::<usize>() {
                            let mut count = generator.count;
                            count.set(value);
                        }
                    },
                }
                span { class: "muted", "{generator.count}" }
            }
        }

        // ---------------------------------------------------- actions
        div { class: "actions",
            button {
                class: "primary",
                disabled: !generator.ready(selected()) || *generator.busy.read(),
                onclick: move |_| generator.run(selected()),
                if *generator.busy.read() { "working…" } else { "generate" }
            }
            button {
                disabled: empty,
                onclick: move |_| {
                    let queue: Vec<RemoteTrack> = generator
                        .result
                        .peek()
                        .iter()
                        .map(|step| as_remote(&step.track))
                        .collect();
                    play_list(player, queue, 0);
                },
                "play"
            }
            button {
                disabled: empty,
                onclick: move |_| {
                    let ids: Vec<i64> = generator
                        .result
                        .peek()
                        .iter()
                        .map(|step| step.track.track_id)
                        .collect();
                    let mut status = generator.status;
                    status.set(Some("exporting…".into()));
                    spawn(async move {
                        match export(ids).await {
                            Ok(id) => status.set(Some(format!("exported as playlist {id}"))),
                            Err(err) => status.set(Some(format!("{err:#}"))),
                        }
                    });
                },
                "export"
            }
            button {
                disabled: empty,
                onclick: move |_| generator.clear(),
                "clear"
            }
        }

        if let Some(message) = generator.status.read().clone() {
            p { class: "muted ellipsis", "{message}" }
        }

        // ----------------------------------------------------- result
        if let Some(recipe) = generator.produced_by.read().clone() {
            div { class: "shelf-head result-head",
                span { class: "ellipsis", "{describe(&recipe, label_for)}" }
                span { class: "spacer" }
                // The one action that dims the map: here the route is the
                // point, so everything not on it drops back.
                button {
                    class: "chip nav-btn",
                    title: "trace this on the map",
                    onclick: move |_| map.show_route(),
                    "on the map"
                }
                if *generator.stale.read() {
                    button {
                        class: "chip notice",
                        title: "the weights moved since these were chosen",
                        onclick: {
                            let recipe = recipe.clone();
                            move |_| generator.rerun(&recipe)
                        },
                        "regenerate"
                    }
                }
            }
        }
        ol { class: "list",
            for step in result {
                li {
                    key: "{step.track.track_id}",
                    class: if selected() == Some(step.track.track_id) { "row selected" } else { "row" },
                    onclick: {
                        let id = step.track.track_id;
                        move |_| open_track(selected, detail, id)
                    },
                    "data-menu": MenuTarget::SpaceTrack(step.track.track_id).tag(),
                    oncontextmenu: {
                        let id = step.track.track_id;
                        move |event: Event<MouseData>| {
                            event.prevent_default();
                            open_menu(&mut menu, &event, MenuTarget::SpaceTrack(id));
                        }
                    },
                    {artist_link(
                        detail,
                        Some(step.track.artist_id).filter(|id| *id >= 0),
                        step.track.artist.clone(),
                        "artist",
                    )}
                    span { class: "title", "{step.track.title}" }
                    span { class: "muted",
                        {step.similarity.map(|s| format!("{s:.3}")).unwrap_or_default()}
                    }
                    {menu_button(menu, MenuTarget::SpaceTrack(step.track.track_id))}
                }
            }
        }
    }
}

/// The weight sliders, collapsed by default. They used to sit permanently
/// under the generate panel, in a column visible whether or not you were
/// using it; most sessions never touch them, so on a phone that was a
/// scroll's worth of real estate spent on a control that is not the reason
/// anyone opened the sheet.
#[component]
fn WeightsDisclosure() -> Element {
    let generator = use_context::<Generator>();
    let mut weights = use_context::<Weights>().0;
    let mut expanded = use_signal(|| false);

    let block_names: Vec<String> = engine_block_names();

    let weights_changed = {
        let defaults = crate::engine().lock().unwrap().space.default_weights();
        let current = weights();
        defaults.iter().any(|(name, default)| {
            current
                .get(name)
                .is_some_and(|value| (value - default).abs() > 1e-6)
        })
    };

    rsx! {
        section { class: "weights-disclosure",
            button {
                class: "weights-toggle",
                onclick: move |_| { let open = expanded(); expanded.set(!open); },
                span { "Tune the space" }
                if weights_changed {
                    span { class: "chip active", "changed" }
                }
                span { class: "spacer" }
                span { class: "muted", if expanded() { "▲" } else { "▼" } }
            }

            if expanded() {
                p { class: "muted",
                    "Reshapes distances immediately. Map positions are fixed until the layout step is re-run."
                }
                div { class: "actions",
                    span { class: "spacer" }
                    button {
                        class: "chip",
                        title: "back to the weights the space was built with",
                        disabled: !weights_changed,
                        onclick: move |_| {
                            let defaults = crate::engine().lock().unwrap().space.default_weights();
                            weights.set(defaults.clone());
                            let _ = crate::engine().lock().unwrap().set_weights(&defaults);
                            generator.invalidate();
                        },
                        "reset"
                    }
                }
                for name in block_names {
                    div { class: "slider", key: "{name}",
                        label { "{name}" }
                        input {
                            r#type: "range",
                            min: "0",
                            max: "3",
                            step: "0.1",
                            value: "{weights().get(&name).copied().unwrap_or(1.0)}",
                            oninput: {
                                let name = name.clone();
                                move |e: FormEvent| {
                                    if let Ok(v) = e.value().parse::<f32>() {
                                        let mut w: HashMap<String, f32> = weights();
                                        w.insert(name.clone(), v);
                                        weights.set(w.clone());
                                        let _ = crate::engine().lock().unwrap().set_weights(&w);
                                        generator.invalidate();
                                    }
                                }
                            },
                        }
                        span { class: "muted",
                            {format!("{:.1}", weights().get(&name).copied().unwrap_or(1.0))}
                        }
                    }
                }
            }
        }
    }
}

fn engine_block_names() -> Vec<String> {
    crate::engine()
        .lock()
        .unwrap()
        .space
        .manifest
        .blocks
        .iter()
        .map(|b| b.name.clone())
        .collect()
}

/// A persistent reminder that a path is half-built, docked above the player
/// so it survives being scrolled away from, backgrounded, or simply
/// forgotten about, which is the default condition of using a phone.
/// `Generator::set_path_end` already updates `from`/`to`; this only reads
/// them from outside the sheet, so picking A, closing the sheet, and finding
/// B later does not lose the first choice.
#[component]
pub fn PathPill() -> Element {
    let generator = use_context::<Generator>();
    let selection = use_context::<Selection>().0;
    let detail = use_context::<Detail>().0;

    // Only worth showing mid-pick: both ends known means it has already run
    // and is showing in the sheet; showing means selecting again to look at
    // it, not a dangling pill.
    let (Some(from), None) = (*generator.from.read(), *generator.to.read()) else {
        return rsx! {};
    };

    let guard = crate::engine().lock().unwrap();
    let label = guard
        .navigator
        .catalog
        .tracks
        .iter()
        .find(|t| t.track_id == from)
        .map(|t| format!("{} - {}", t.artist, t.title))
        .unwrap_or_else(|| from.to_string());
    drop(guard);

    rsx! {
        div { class: "path-pill",
            span { class: "ellipsis",
                "Path from "
                strong { "{label}" }
                ", pick an end"
            }
            button {
                class: "chip",
                onclick: move |_| open_track(selection, detail, from),
                "reopen"
            }
            button {
                class: "chip",
                title: "cancel this path",
                onclick: move |_| {
                    let mut from = generator.from;
                    from.set(None);
                },
                "×"
            }
        }
    }
}

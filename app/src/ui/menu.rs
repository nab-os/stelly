//! Row actions, as one context menu instead of a chip per verb.
//!
//! A single menu lives at the shell and every row opens it, rather than each
//! row owning a popup: one thing to dismiss, one place the actions are
//! written, and a row that stays three spans and a button however many verbs
//! it grows.
//!
//! Rows are named by index into the list they are drawn from, not by a
//! captured copy, the same reason the old chips read back out of the shelf.
//! An index is only meaningful against the list it came from, so the menu
//! re-reads that list when the item is chosen, and does nothing if the row has
//! since gone.

use super::library::{request_analysis, Library, View};
use super::player::{clear_queue, enqueue, move_by, play_at, play_next, play_list, remove_at, Player};
use super::{Blocklist, LocalIds, Selection};
use crate::backend::backend;
use crate::engine;
use crate::qobuz::RemoteTrack;
use dioxus::prelude::*;

/// What the menu was opened on.
#[derive(Clone, PartialEq, Debug)]
pub enum MenuTarget {
    /// A track in the Qobuz shelf, by index into the *visible* tracks.
    ShelfTrack(usize),
    ShelfAlbum(usize),
    ShelfArtist { index: usize, similar: bool },
    /// A track in the play queue, by queue position.
    QueueEntry(usize),
    /// A row of a generated sequence, which exists in the space but was never
    /// browsed, so it is named by track id rather than by position.
    SpaceTrack(i64),
}

#[derive(Clone, PartialEq, Debug)]
pub struct MenuState {
    /// Viewport coordinates of the click that opened it.
    pub x: f64,
    pub y: f64,
    pub target: MenuTarget,
}

/// The open menu, if any. Provided by the shell.
#[derive(Clone, Copy)]
pub struct ContextMenu(pub Signal<Option<MenuState>>);

/// Open the menu at the pointer. Shared by the `⋯` button and the row's own
/// right-click, which differ only in needing the native menu suppressed.
pub fn open_menu(menu: &mut Signal<Option<MenuState>>, event: &Event<MouseData>, target: MenuTarget) {
    let point = event.client_coordinates();
    menu.set(Some(MenuState {
        x: point.x,
        y: point.y,
        target,
    }));
}

/// The one button a row keeps. Not a component: it takes no hooks, and a
/// component per row would mean a props struct per row as well.
pub fn menu_button(mut menu: Signal<Option<MenuState>>, target: MenuTarget) -> Element {
    rsx! {
        button {
            class: "chip more",
            title: "actions",
            onclick: move |event: Event<MouseData>| {
                event.stop_propagation();
                open_menu(&mut menu, &event, target.clone());
            },
            "⋯"
        }
    }
}

/// Look a space track up as something the player can take.
fn space_track(track_id: i64) -> Option<RemoteTrack> {
    let guard = engine().lock().unwrap();
    let row = *guard.navigator.index_of.get(&track_id)?;
    Some(super::generate::as_remote(guard.navigator.catalog.get(row)))
}

#[component]
pub fn ContextMenuView() -> Element {
    // Only the menu's own state: each item set reads the contexts it needs,
    // so adding a target does not widen this.
    let mut menu = use_context::<ContextMenu>().0;

    // Nudge the menu back inside the window once it has a size. Opened near
    // the right or bottom edge it would otherwise hang off; this is what a
    // native menu does, and if the eval is lost the menu is merely clipped.
    use_effect(move || {
        if menu.read().is_some() {
            document::eval(
                "const el = document.querySelector('.context-menu');
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

    let Some(state) = menu.read().clone() else {
        return rsx! {};
    };

    let close = move |_: Event<MouseData>| menu.set(None);

    rsx! {
        div {
            class: "menu-backdrop",
            onclick: close,
            // Right-clicking away from a menu should dismiss it, not stack a
            // second one underneath.
            oncontextmenu: move |event: Event<MouseData>| {
                event.prevent_default();
                menu.set(None);
            },

            div {
                class: "context-menu",
                style: "left: {state.x}px; top: {state.y}px;",
                onclick: move |event: Event<MouseData>| event.stop_propagation(),

                match state.target.clone() {
                    MenuTarget::ShelfTrack(index) => rsx! { ShelfTrackItems { index } },
                    MenuTarget::ShelfAlbum(index) => rsx! { ShelfAlbumItems { index } },
                    MenuTarget::ShelfArtist { index, similar } => rsx! {
                        ShelfArtistItems { index, similar }
                    },
                    MenuTarget::QueueEntry(index) => rsx! { QueueItems { index } },
                    MenuTarget::SpaceTrack(track_id) => rsx! { SpaceTrackItems { track_id } },
                }
            }
        }
    }
}

// ------------------------------------------------------------------- items
//
// One component per target so each can read the list it indexes into at the
// moment the item is chosen, rather than at the moment the menu was opened.

#[component]
fn ShelfTrackItems(index: usize) -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();
    let blocklist = use_context::<Blocklist>();
    let local = use_context::<LocalIds>();
    let selection = use_context::<Selection>().0;
    let menu = use_context::<ContextMenu>().0;

    let track = library
        .shelf
        .read()
        .visible_tracks(&blocklist)
        .get(index)
        .cloned();
    let Some(track) = track else {
        return rsx! {};
    };

    let mut menu = menu;
    let mut selection = selection;
    let in_space = local.0.read().contains(&track.id);
    let artist = track.artist_id.map(|id| (id, track.artist.clone()));

    rsx! {
        div { class: "menu-head",
            span { class: "title", "{track.title}" }
            span { class: "muted", "{track.artist}" }
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                let queue = library.shelf.peek().visible_tracks(&blocklist);
                play_list(player, queue, index);
                menu.set(None);
            },
            "Play now"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                if let Some(track) = library.shelf.peek().visible_tracks(&blocklist).get(index) {
                    play_next(player, vec![track.clone()]);
                }
                menu.set(None);
            },
            "Play next"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                if let Some(track) = library.shelf.peek().visible_tracks(&blocklist).get(index) {
                    enqueue(player, vec![track.clone()]);
                }
                menu.set(None);
            },
            "Add to queue"
        }

        if in_space {
            div { class: "menu-rule" }
            button {
                class: "menu-item",
                onclick: move |_| {
                    selection.set(Some(track.id));
                    menu.set(None);
                },
                "Show on the map"
            }
        }

        if let Some((artist_id, name)) = artist {
            div { class: "menu-rule" }
            button {
                class: "menu-item danger",
                onclick: move |_| {
                    blocklist.block.call((artist_id, name.clone()));
                    menu.set(None);
                },
                "Hide {name}"
            }
        }
    }
}

#[component]
fn ShelfAlbumItems(index: usize) -> Element {
    let library = use_context::<Library>();
    let player = use_context::<Player>();
    let blocklist = use_context::<Blocklist>();
    let menu = use_context::<ContextMenu>().0;

    let album = library
        .shelf
        .read()
        .visible_albums(&blocklist)
        .get(index)
        .cloned();
    let Some(album) = album else {
        return rsx! {};
    };

    let mut menu = menu;
    let artist = album.artist_id.map(|id| (id, album.artist.clone()));

    // Every playback item needs the tracklist, which an album row does not
    // carry; one helper rather than the same fetch written three times.
    //
    // It captures nothing but `Copy` values, the id is re-read from the
    // shelf rather than held, which is what lets the same closure be used
    // from three separate `move` handlers.
    let with_tracks = move |then: fn(Player, Vec<RemoteTrack>)| {
        let id = library
            .shelf
            .peek()
            .visible_albums(&blocklist)
            .get(index)
            .map(|album| album.id.clone());
        let Some(id) = id else { return };

        spawn(async move {
            if let Ok(tracks) = backend().album_tracks(&id).await {
                then(player, tracks);
            }
        });
    };

    rsx! {
        div { class: "menu-head",
            span { class: "title", "{album.title}" }
            span { class: "muted", "{album.artist}" }
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                let target = library
                    .shelf
                    .peek()
                    .visible_albums(&blocklist)
                    .get(index)
                    .map(|album| View::Album {
                        id: album.id.clone(),
                        title: album.title.clone(),
                    });
                if let Some(target) = target {
                    library.go(target);
                }
                menu.set(None);
            },
            "Open"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                with_tracks(|player, tracks| play_list(player, tracks, 0));
                menu.set(None);
            },
            "Play now"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                with_tracks(play_next);
                menu.set(None);
            },
            "Play next"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                with_tracks(enqueue);
                menu.set(None);
            },
            "Add to queue"
        }

        div { class: "menu-rule" }
        button {
            class: "menu-item",
            title: "fetch this tracklist into the catalogue",
            onclick: move |_| {
                let found = library
                    .shelf
                    .peek()
                    .visible_albums(&blocklist)
                    .get(index)
                    .map(|album| (album.id.clone(), album.title.clone()));
                if let Some((id, title)) = found {
                    request_analysis(library, "album", &id, &title);
                }
                menu.set(None);
            },
            "Fetch into catalogue"
        }

        if let Some((artist_id, name)) = artist {
            button {
                class: "menu-item danger",
                onclick: move |_| {
                    blocklist.block.call((artist_id, name.clone()));
                    menu.set(None);
                },
                "Hide {name}"
            }
        }
    }
}

#[component]
fn ShelfArtistItems(index: usize, similar: bool) -> Element {
    let library = use_context::<Library>();
    let blocklist = use_context::<Blocklist>();
    let menu = use_context::<ContextMenu>().0;

    let artist = library
        .shelf
        .read()
        .visible_artists(&blocklist, similar)
        .get(index)
        .cloned();
    let Some(artist) = artist else {
        return rsx! {};
    };

    let mut menu = menu;

    // As in the album menu: only `Copy` captures, so every handler can use it.
    let artist_at = move || {
        library
            .shelf
            .peek()
            .visible_artists(&blocklist, similar)
            .get(index)
            .map(|artist| (artist.id, artist.name.clone()))
    };

    rsx! {
        div { class: "menu-head",
            span { class: "title", "{artist.name}" }
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                if let Some((id, name)) = artist_at() {
                    library.go(View::Artist { id, name });
                }
                menu.set(None);
            },
            "Open"
        }
        button {
            class: "menu-item",
            title: "fetch this discography into the catalogue",
            onclick: move |_| {
                if let Some((id, name)) = artist_at() {
                    request_analysis(library, "artist", &id.to_string(), &name);
                }
                menu.set(None);
            },
            "Fetch discography"
        }

        div { class: "menu-rule" }
        button {
            class: "menu-item danger",
            onclick: move |_| {
                if let Some(entry) = artist_at() {
                    blocklist.block.call(entry);
                }
                menu.set(None);
            },
            "Hide everywhere"
        }
    }
}

#[component]
fn QueueItems(index: usize) -> Element {
    let player = use_context::<Player>();
    let menu = use_context::<ContextMenu>().0;

    let track = player.queue.read().get(index).cloned();
    let Some(track) = track else {
        return rsx! {};
    };

    let mut menu = menu;
    let total = player.queue.read().len();
    let playing = *player.index.read() == index;

    rsx! {
        div { class: "menu-head",
            span { class: "title", "{track.title}" }
            span { class: "muted", "{track.artist}" }
        }
        if !playing {
            button {
                class: "menu-item",
                onclick: move |_| {
                    spawn(async move { play_at(player, index).await });
                    menu.set(None);
                },
                "Play now"
            }
        }
        button {
            class: "menu-item",
            disabled: index == 0,
            onclick: move |_| {
                move_by(player, index, -1);
                menu.set(None);
            },
            "Move up"
        }
        button {
            class: "menu-item",
            disabled: index + 1 >= total,
            onclick: move |_| {
                move_by(player, index, 1);
                menu.set(None);
            },
            "Move down"
        }

        div { class: "menu-rule" }
        button {
            class: "menu-item danger",
            onclick: move |_| {
                remove_at(player, index);
                menu.set(None);
            },
            "Remove from queue"
        }
        button {
            class: "menu-item danger",
            onclick: move |_| {
                clear_queue(player);
                menu.set(None);
            },
            "Clear the queue"
        }
    }
}

#[component]
fn SpaceTrackItems(track_id: i64) -> Element {
    let player = use_context::<Player>();
    let selection = use_context::<Selection>().0;
    let menu = use_context::<ContextMenu>().0;

    let Some(track) = space_track(track_id) else {
        return rsx! {};
    };

    let mut menu = menu;
    let mut selection = selection;

    rsx! {
        div { class: "menu-head",
            span { class: "title", "{track.title}" }
            span { class: "muted", "{track.artist}" }
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                selection.set(Some(track_id));
                menu.set(None);
            },
            "Show on the map"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                if let Some(track) = space_track(track_id) {
                    play_next(player, vec![track]);
                }
                menu.set(None);
            },
            "Play next"
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                if let Some(track) = space_track(track_id) {
                    enqueue(player, vec![track]);
                }
                menu.set(None);
            },
            "Add to queue"
        }
    }
}

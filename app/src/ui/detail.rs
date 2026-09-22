//! The selected track, at a size worth looking at.
//!
//! Selecting used to do two things at once: highlight a row, and silently
//! re-run the generator. Now that it only selects, it needed somewhere to
//! land, otherwise "select" meant watching a row change colour.
//!
//! So this is the other half of making generation explicit: the selection
//! becomes a thing you can see and act on, and every verb that used to fire
//! on its own is a button here with a name on it.

use super::generate::{as_remote, Generator, Mode, PathEnd};
use super::player::{enqueue, play_list, play_next, Player};
use super::{Cover, MapView, Selection};
use crate::qobuz::cover_url;
use dioxus::prelude::*;

/// What the pane needs to draw, lifted out from under the engine lock.
#[derive(Clone, PartialEq)]
struct Detail {
    title: String,
    artist: String,
    album: String,
    album_id: String,
    bpm: Option<f32>,
}

fn detail(track_id: i64) -> Option<Detail> {
    let guard = crate::engine().lock().unwrap();
    let row = *guard.navigator.index_of.get(&track_id)?;
    let meta = guard.navigator.catalog.get(row);
    Some(Detail {
        title: meta.title.clone(),
        artist: meta.artist.clone(),
        album: meta.album.clone(),
        album_id: meta.album_id.clone(),
        bpm: meta.bpm,
    })
}

#[component]
pub fn DetailPane() -> Element {
    let selection = use_context::<Selection>().0;
    let player = use_context::<Player>();
    let generator = use_context::<Generator>();
    let map = use_context::<MapView>();

    let Some(track_id) = selection() else {
        // An empty state rather than nothing: the column keeps its width, so
        // selecting a track does not shove the list sideways.
        return rsx! {
            section { class: "panel detail detail-empty",
                p { class: "muted",
                    "Select a track to see it here, and to walk the space from it."
                }
            }
        };
    };

    let Some(track) = detail(track_id) else {
        // Selected, then the space was rebuilt without it.
        return rsx! {
            section { class: "panel detail detail-empty",
                p { class: "muted", "That track is no longer in the space." }
            }
        };
    };

    let from = *generator.from.read();
    let to = *generator.to.read();

    rsx! {
        section { class: "panel detail",
            Cover {
                url: cover_url(&track.album_id),
                class: "detail-art eager",
            }

            div { class: "detail-meta",
                span { class: "detail-title ellipsis", "{track.title}" }
                span { class: "muted ellipsis", "{track.artist}" }
                if !track.album.is_empty() {
                    span { class: "muted ellipsis", "{track.album}" }
                }
                div { class: "detail-badges",
                    span { class: "chip active", "in your space" }
                    if let Some(bpm) = track.bpm {
                        span { class: "chip", "{bpm:.0} bpm" }
                    }
                }
            }

            div { class: "detail-actions",
                button {
                    class: "primary",
                    onclick: move |_| play_list(player, playable(track_id), 0),
                    "Play"
                }
                button {
                    onclick: move |_| play_next(player, playable(track_id)),
                    "Play next"
                }
                button {
                    onclick: move |_| enqueue(player, playable(track_id)),
                    "Queue"
                }
                button {
                    onclick: move |_| map.browse(),
                    "On the map"
                }
            }

            // The verbs that used to fire the instant anything was selected.
            // Named, and asked for.
            div { class: "detail-actions",
                button {
                    onclick: move |_| generator.request(Mode::Neighbours, track_id),
                    "Find neighbours"
                }
                button {
                    onclick: move |_| generator.request(Mode::Radio, track_id),
                    "Start radio"
                }
                button {
                    disabled: from == Some(track_id),
                    onclick: move |_| generator.set_path_end(PathEnd::A, track_id),
                    if to.is_some() && from != Some(track_id) { "Path A, go" } else { "Path A" }
                }
                button {
                    disabled: to == Some(track_id),
                    onclick: move |_| generator.set_path_end(PathEnd::B, track_id),
                    if from.is_some() && to != Some(track_id) { "Path B, go" } else { "Path B" }
                }
            }
        }
    }
}

/// The track as the player takes it, read fresh at the moment of the click:
/// the space can be rebuilt between rendering this pane and pressing a button
/// on it, and an empty queue is a better answer than a stale one.
fn playable(track_id: i64) -> Vec<crate::qobuz::RemoteTrack> {
    let guard = crate::engine().lock().unwrap();
    match guard.navigator.index_of.get(&track_id) {
        Some(&row) => vec![as_remote(guard.navigator.catalog.get(row))],
        None => Vec::new(),
    }
}

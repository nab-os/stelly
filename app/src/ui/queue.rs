//! The play queue, as a drawer above the player bar.
//!
//! Reordering is by up/down rather than drag: a drag needs pointer capture and
//! an autoscroll to be usable, and neither survives contact with a touch
//! screen. Two buttons work identically everywhere.

use super::player::{move_by, play_at, remove_at, Player};
use super::Cover;
use dioxus::prelude::*;

#[component]
pub fn QueueView() -> Element {
    let player = use_context::<Player>();
    let mut queue_open = player.queue_open;

    let queue = player.queue.read().clone();
    let current = *player.index.read();
    let total = queue.len();

    rsx! {
        if queue_open() {
            section { class: "panel queue-drawer",
                h2 {
                    "Queue"
                    span { class: "muted", "{total} tracks" }
                    span { class: "spacer" }
                    button {
                        class: "chip",
                        onclick: move |_| queue_open.set(false),
                        "close"
                    }
                }

                if total == 0 {
                    p { class: "muted", "Nothing queued." }
                } else {
                    ol { class: "list queue-list",
                        for (index, track) in queue.into_iter().enumerate() {
                            li {
                                key: "{index}-{track.id}",
                                class: if index == current { "row playing" } else { "row" },
                                onclick: move |_| {
                                    spawn(async move { play_at(player, index).await });
                                },
                                Cover { url: track.image.clone(), class: "thumb" }
                                span { class: "artist", "{track.artist}" }
                                span { class: "title", "{track.title}" }
                                button {
                                    class: "chip",
                                    title: "move up",
                                    disabled: index == 0,
                                    onclick: move |event| {
                                        event.stop_propagation();
                                        move_by(player, index, -1);
                                    },
                                    "↑"
                                }
                                button {
                                    class: "chip",
                                    title: "move down",
                                    disabled: index + 1 == total,
                                    onclick: move |event| {
                                        event.stop_propagation();
                                        move_by(player, index, 1);
                                    },
                                    "↓"
                                }
                                button {
                                    class: "chip danger",
                                    title: "remove from the queue",
                                    onclick: move |event| {
                                        event.stop_propagation();
                                        remove_at(player, index);
                                    },
                                    "×"
                                }
                                span { class: "muted", "{track.duration_label()}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

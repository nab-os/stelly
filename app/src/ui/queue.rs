//! The play queue, as a drawer above the player bar.
//!
//! Reordering is by up/down rather than drag: a drag needs pointer capture and
//! an autoscroll to be usable, and neither survives contact with a touch
//! screen. Two buttons work identically everywhere.

use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{clear_queue, move_to, play_at, Player};
use super::Cover;
use crate::engine;
use crate::qobuz::RemoteTrack;
use dioxus::prelude::*;

/// Reorder what follows the current track so each step through the space is as
/// short as possible, the most gradual way through what is already queued.
///
/// What has played and what is playing stay put: this reshapes the future of
/// the queue, not its past. Tracks the space has never analysed keep their
/// order among themselves and follow the sorted ones, since there is no
/// distance by which to place them.
fn sort_queue(mut player: Player) {
    let queue = player.queue.peek().clone();
    if queue.is_empty() {
        return;
    }

    let index = (*player.index.peek()).min(queue.len() - 1);
    let upcoming = &queue[index + 1..];
    if upcoming.len() < 2 {
        return;
    }

    let ids: Vec<i64> = upcoming.iter().map(|track| track.id).collect();
    let ordered = engine()
        .lock()
        .unwrap()
        .navigator
        .shortest_path_order(&ids, Some(queue[index].id));

    if ordered.is_empty() {
        player.status.set(Some(
            "none of the queued tracks have been analysed, so there is no distance to sort by"
                .into(),
        ));
        return;
    }

    // Consume by id rather than index: `ordered` drops what the space does not
    // hold. `position` takes one entry at a time, so this stays correct even
    // though the queue's own dedupe means there should be nothing to repeat.
    let mut remaining: Vec<RemoteTrack> = upcoming.to_vec();
    let mut sorted: Vec<RemoteTrack> = Vec::with_capacity(remaining.len());
    for id in ordered {
        if let Some(at) = remaining.iter().position(|track| track.id == id) {
            sorted.push(remaining.remove(at));
        }
    }
    let unplaced = remaining.len();
    sorted.extend(remaining);

    let mut next = queue[..=index].to_vec();
    next.extend(sorted);
    player.queue.set(next);

    player.status.set(Some(match unplaced {
        0 => "queue sorted by distance".into(),
        n => format!("queue sorted; {n} not in the space, left at the end"),
    }));
}

#[component]
pub fn QueueView() -> Element {
    let player = use_context::<Player>();
    let mut queue_open = player.queue_open;
    let mut menu = use_context::<ContextMenu>().0;

    // Long-lived: this component stays mounted whether or not the drawer is
    // open, so the channel outlives any one drag. The script delegates from
    // the document, so it does not care that the list comes and goes.
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../../assets/queue-drag.js"));
        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            let at = |key| message.get(key).and_then(|v| v.as_u64()).map(|v| v as usize);
            if let (Some(from), Some(to)) = (at("from"), at("to")) {
                move_to(player, from, to);
            }
        }
    });

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
                        title: "reorder what follows by the shortest route through the space",
                        disabled: total < current + 3,
                        onclick: move |_| sort_queue(player),
                        "sort by distance"
                    }
                    button {
                        class: "chip danger",
                        title: "empty the queue and stop",
                        disabled: total == 0,
                        onclick: move |_| clear_queue(player),
                        "clear"
                    }
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
                                "data-menu": MenuTarget::QueueEntry(index).tag(),
                                oncontextmenu: move |event: Event<MouseData>| {
                                    event.prevent_default();
                                    open_menu(&mut menu, &event, MenuTarget::QueueEntry(index));
                                },
                                Cover { url: track.image.clone(), class: "thumb" }
                                span { class: "artist", "{track.artist}" }
                                span { class: "title", "{track.title}" }
                                span { class: "muted", "{track.duration_label()}" }
                                // The drag handle. Only this has
                                // `touch-action: none`, so a drag anywhere
                                // else on the row still scrolls the list.
                                // Precise moves stay in the menu.
                                span {
                                    class: "queue-grip",
                                    title: "drag to reorder",
                                    onclick: move |event| event.stop_propagation(),
                                    "⠿"
                                }
                                {menu_button(menu, MenuTarget::QueueEntry(index))}
                            }
                        }
                    }
                }
            }
        }
    }
}

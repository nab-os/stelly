//! The play queue: the right-hand column beside the main area, or on a phone
//! a drawer over it.
//!
//! Opened and closed by the player's queue button; on a phone it also has a
//! handle to drag it back down by. Rows reorder by dragging (after a long
//! press, on a touch screen), and leave by a swipe or the red cross that
//! shows on hover. The gestures live in `queue-drag.js`.
//!
//! What generating produced is a section of it, under a header that says
//! what made it and how much of it is left, and rerolls, saves or stops it.

use super::generate::{describe, Generator, Recipe};
use super::menu::{open_menu, ContextMenu, MenuTarget};
use super::player::{clear_queue, move_to, play_at, remove_at, set_upcoming, Player};
use super::side::{export_result, title_for};
use super::{album_link, artist_link, icons, Cover, Library, MapView};
use crate::api::OrderRequest;
use crate::backend::backend;
use crate::catalogue::RemoteTrack;
use dioxus::core::Task;
use dioxus::prelude::*;

/// Reorder what follows the current track so each step through the space is as
/// short as possible, the most gradual way through what is already queued.
///
/// What has played and what is playing stay put: this reshapes the future of
/// the queue, not its past. Tracks the space has never analysed keep their
/// order among themselves and follow the sorted ones, since there is no
/// distance by which to place them.
fn sort_queue(player: Player) {
    let weights = try_consume_context::<super::Weights>()
        .map(|weights| weights.0.peek().clone())
        .unwrap_or_default();
    spawn(async move {
        let mut player = player;
        if let Err(err) = sort_upcoming(player, weights).await {
            player.status.set(Some(format!("could not sort the queue: {err:#}")));
        }
    });
}

async fn sort_upcoming(mut player: Player, weights: std::collections::HashMap<String, f32>) -> anyhow::Result<()> {
    let queue = player.queue.peek().clone();
    if queue.is_empty() {
        return Ok(());
    }

    let index = (*player.index.peek()).min(queue.len() - 1);
    let upcoming = &queue[index + 1..];
    if upcoming.len() < 2 {
        return Ok(());
    }

    let ids: Vec<i64> = upcoming.iter().map(|track| track.id).collect();
    let request = OrderRequest {
        ids,
        from: Some(queue[index].id),
        weights,
    };
    let ordered = backend().order(&request).await?;

    if ordered.is_empty() {
        player.status.set(Some(
            "none of the queued tracks have been analysed, so there is no distance to sort by"
                .into(),
        ));
        return Ok(());
    }

    // The queue moved while the server was thinking: sorting the old one
    // would undo whatever was just done to it.
    if *player.queue.peek() != queue || (*player.index.peek()).min(queue.len() - 1) != index {
        return Ok(());
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

    set_upcoming(player, sorted);

    player.status.set(Some(match unplaced {
        0 => "queue sorted by distance".into(),
        n => format!("queue sorted; {n} not in the space, left at the end"),
    }));
    Ok(())
}

/// How long the drawer takes to slide shut, `.queue-drawer`'s transition on
/// a phone.
const SLIDE: std::time::Duration = std::time::Duration::from_millis(200);

#[component]
pub fn QueueView() -> Element {
    let player = use_context::<Player>();
    let mut queue_open = player.queue_open;

    // Long-lived: this component stays mounted whether or not the drawer is
    // open, so the channel outlives any one gesture. The script delegates
    // from the document and the drawer, so it does not care that the rows
    // come and go, nor that they are only up while the drawer is open.
    use_future(move || async move {
        let mut handle = document::eval(include_str!("../../assets/queue-drag.js"));
        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            let at = |key| message.get(key).and_then(|v| v.as_u64()).map(|v| v as usize);
            match message.get("type").and_then(|v| v.as_str()) {
                Some("move") => {
                    if let (Some(from), Some(to)) = (at("from"), at("to")) {
                        move_to(player, from, to);
                    }
                }
                Some("remove") => {
                    if let Some(index) = at("index") {
                        remove_at(player, index);
                    }
                }
                Some("close") => queue_open.set(false),
                _ => {}
            }
        }
    });

    // The rows are up only while the drawer is open, and while it slides
    // shut. Closed, it is hidden, and every row of a 500-track "play all"
    // was being drawn there for nobody. Taken down rather than kept once
    // opened: the queue is mostly replaced from the library, with the drawer
    // shut, and opening it again draws them in the time the first opening
    // did.
    let mut rows_up = use_signal(|| false);
    let mut closing = use_signal(|| None::<Task>);
    use_effect(move || {
        let open = queue_open();
        if let Some(task) = closing.write().take() {
            task.cancel();
        }
        if open {
            if !*rows_up.peek() {
                rows_up.set(true);
            }
        } else if *rows_up.peek() {
            closing.set(Some(spawn(async move {
                tokio::time::sleep(SLIDE).await;
                rows_up.set(false);
            })));
        }
    });
    let shown = queue_open() || rows_up();

    let total = player.queue.read().len();
    let current = *player.index.read();

    rsx! {
        // Always mounted, so opening and closing can slide rather than pop.
        section { class: if queue_open() { "panel queue-drawer open" } else { "panel queue-drawer" },
            // Phone only: dragged down, or tapped, it closes the drawer.
            div { class: "drawer-handle", title: "close" }

            div { class: "queue-head",
                h2 { "Queue" }
                if total > 0 {
                    span { class: "muted queue-where", "{(current + 1).min(total)} of {total}" }
                }
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
            }

            if total == 0 {
                p { class: "muted",
                    "Nothing queued. Open a track from your space and start a radio from it, or describe a mood in the search box."
                }
            } else if shown {
                QueueRows {}
            }
        }
    }
}

/// The queue's rows. Apart from the drawer, and propless, so the drawer
/// following the track to keep its head right does not draw them all again:
/// what is playing is read here without subscribing, and its highlight is
/// moved like any list's, see `use_playing_highlight`.
#[component]
fn QueueRows() -> Element {
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let generator = use_context::<Generator>();
    let mut menu = use_context::<ContextMenu>().0;

    let queue = player.queue.read().clone();
    let now_playing = player.current().map(|track| track.id);
    let keys = super::row_keys(queue.iter().map(|track| track.id));

    // The generated section: its header goes over the first of its rows
    // still in the queue, and each of them is marked as its own. Not
    // necessarily one run, since rows can be dragged out of it.
    let section: std::collections::HashSet<i64> = generator
        .result
        .read()
        .iter()
        .map(|step| step.track.track_id)
        .collect();
    let head_at = queue.iter().position(|track| section.contains(&track.id));

    rsx! {
        // The header is not an `li`: queue-drag.js counts only those as rows.
        // Drawn after the row before the section, the row being what each
        // turn of the loop is keyed by.
        ol { class: "queue-list",
            if head_at == Some(0) {
                SectionHead {}
            }
            for ((index, track), key) in queue.into_iter().enumerate().zip(keys) {
                li {
                    key: "{key}",
                    class: match (now_playing == Some(track.id), section.contains(&track.id)) {
                        (true, true) => "queue-row playing generated",
                        (true, false) => "queue-row playing",
                        (false, true) => "queue-row generated",
                        (false, false) => "queue-row",
                    },
                    "data-track": "{track.id}",
                    onclick: move |_| play_at(player, index),
                    // Right-click only: a long press here starts a
                    // drag, see queue-drag.js.
                    oncontextmenu: move |event: Event<MouseData>| {
                        event.prevent_default();
                        open_menu(&mut menu, &event, MenuTarget::QueueEntry(index));
                    },
                    Cover { url: track.image.clone(), class: "thumb" }
                    div { class: "queue-text",
                        span { class: "title", "{track.title}" }
                        div { class: "queue-sub",
                            {artist_link(library, track.artist_id, track.artist.clone(), "muted")}
                            if !track.album.is_empty() {
                                span { class: "muted", "·" }
                                {album_link(library, &track, "muted")}
                            }
                        }
                    }
                    span { class: "muted duration", "{track.duration_label()}" }
                    button {
                        class: "queue-remove",
                        title: "remove from the queue",
                        onclick: move |event: Event<MouseData>| {
                            event.stop_propagation();
                            remove_at(player, index);
                        },
                        {icons::close()}
                    }
                }
                if head_at == Some(index + 1) {
                    SectionHead {}
                }
            }
        }
    }
}

/// The generated section's header: what made it, how many of its tracks are
/// still to come, and what can be done with it.
#[component]
fn SectionHead() -> Element {
    let player = use_context::<Player>();
    let generator = use_context::<Generator>();
    let provider = use_context::<super::Provider>();
    let map = use_context::<MapView>();

    let Some(recipe) = generator.produced_by.read().clone() else {
        return rsx! { div { class: "section-head" } };
    };
    let section = generator.ids();
    let index = *player.index.read();
    let left = player
        .queue
        .read()
        .iter()
        .skip(index + 1)
        .filter(|track| section.contains(&track.id))
        .count();
    let busy = *generator.busy.read();
    let (icon, heading) = heading(&recipe);

    rsx! {
        div { class: "section-head",
            span { class: "section-icon", {icon} }
            div { class: "section-text",
                span { class: "title ellipsis", title: "{describe(&recipe, label)}", "{heading}" }
                span { class: "muted",
                    if busy {
                        "working…"
                    } else if *generator.stale.read() {
                        "{left} left · the space changed, reroll for a fresh one"
                    } else {
                        "{left} left"
                    }
                }
            }
            div { class: "section-actions",
                button {
                    class: "link",
                    title: "make it again, in place of what is left of it",
                    disabled: busy,
                    onclick: move |_| generator.reroll(),
                    "reroll"
                }
                button {
                    class: "link",
                    title: "trace it on the map",
                    onclick: move |_| map.show_route(),
                    "map"
                }
                if provider.exports() {
                    button {
                        class: "link",
                        title: "save it as a playlist",
                        onclick: move |_| export_result(generator),
                        "save"
                    }
                }
                button {
                    class: "link",
                    title: "take what is left of it out of the queue",
                    onclick: move |_| generator.stop(),
                    "stop"
                }
            }
        }
        if let Some(message) = generator.status.read().clone() {
            p { class: if *generator.failed.read() { "section-status error" } else { "section-status muted" }, "{message}" }
        }
    }
}

/// The mode, and what it started from, as short as a header has room for.
fn heading(recipe: &Recipe) -> (Element, String) {
    use super::generate::Mode;
    let (mode, from) = match recipe {
        Recipe::Neighbours { seed } => (Mode::Neighbours, title_for(*seed)),
        Recipe::Radio { seed } => (Mode::Radio, title_for(*seed)),
        Recipe::Path { a, b, .. } => (Mode::Path, format!("{} → {}", title_for(*a), title_for(*b))),
        Recipe::Drift { seed, phrase } => (Mode::Drift, format!("{} → “{phrase}”", title_for(*seed))),
        Recipe::Mood { phrase } => (Mode::Mood, format!("“{phrase}”")),
    };
    (mode.icon(), format!("{} · {from}", mode.title()))
}

fn label(id: Option<i64>) -> String {
    super::side::label_for(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rows taken down before the drawer is out of sight would vanish
    // mid-slide; long after, and a closed drawer keeps them for nothing.
    // Only the phone's queue slides; the docked column just goes.
    #[test]
    fn the_rows_wait_for_the_drawer_to_slide_shut() {
        let css = include_str!("../../assets/style.css");
        let phone = css.find("@media (max-width: 900px)").expect("the phone's block");
        let at = phone + css[phone..].find(".queue-drawer {").expect("the drawer's rule");
        let rule = &css[at..at + css[at..].find('}').unwrap()];
        let slide = format!("transform {}ms", SLIDE.as_millis());
        assert!(rule.contains(&slide), "{rule}");
    }
}

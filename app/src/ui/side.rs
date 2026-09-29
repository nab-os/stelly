//! The generate panel: the space's tool, always one glance away.
//!
//! Docked to the right of the main area on a wide screen, whatever the main
//! area shows. On a phone it slides in from the right over the main area and
//! a floating arrow takes it away again; the player stays visible under both.
//!
//! It works on the selection, the track in hand, which anything in the main
//! area or on the map can set: open a track in your space and it is the seed
//! here. A path's two ends are set from anywhere a track is (the album page's
//! green A and red B, a track's page, a row's menu) and shown here until both
//! are known and the path is walked.

use super::generate::{as_remote, describe, Generator, Mode};
use super::icons;
use super::library::Library;
use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, Player};
use super::{artist_link, open_track, space_track, Cover, MapView, Selection, Weights};
use crate::backend::backend;
use crate::qobuz::RemoteTrack;
use dioxus::prelude::*;
use std::collections::HashMap;

async fn export(track_ids: Vec<i64>) -> anyhow::Result<i64> {
    let name = format!("Stelly ({} tracks)", track_ids.len());
    backend().export_playlist(&name, &track_ids).await
}

/// "artist - title" for a space track, or its id when the space no longer
/// holds it.
fn label_for(id: Option<i64>) -> String {
    let Some(id) = id else { return "-".into() };
    let guard = crate::engine().lock().unwrap();
    guard
        .navigator
        .index_of
        .get(&id)
        .map(|&row| {
            let track = guard.navigator.catalog.get(row);
            format!("{} - {}", track.artist, track.title)
        })
        .unwrap_or_else(|| id.to_string())
}

#[component]
pub fn GeneratePanel() -> Element {
    let generator = use_context::<Generator>();
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let selected = use_context::<Selection>().0;
    let mut menu = use_context::<ContextMenu>().0;
    let map = use_context::<MapView>();
    let mut panel_open = generator.panel_open;

    let mode = *generator.mode.read();
    let result = generator.result.read().clone();
    let empty = result.is_empty();
    let seed = selected().and_then(space_track);
    let has_selection = seed.is_some();
    let from = *generator.from.read();
    let to = *generator.to.read();

    // Asked once: whether the server has a text tower at all, which drift
    // needs to turn a phrase into somewhere to go.
    use_future(move || async move {
        let mut tower = generator.tower;
        if let Ok(available) = backend().can_steer().await {
            tower.set(available);
        }
    });

    // On a phone the panel covers the main area, so anything here that
    // navigates the main area has to get out of the way first.
    let mut close = move || panel_open.set(false);

    rsx! {
        aside { class: if panel_open() { "panel side-panel open" } else { "panel side-panel" },
            // Phone only, see `.side-back` in style.css.
            button {
                class: "side-back",
                title: "back",
                onclick: move |_| close(),
                {icons::back()}
            }

            h2 { "Generate" }

            // What the walk starts from. Path mode names its own two ends
            // below instead.
            if mode != Mode::Path {
                match seed.clone() {
                    Some(track) => rsx! {
                        div {
                            class: "seed",
                            onclick: move |_| {
                                close();
                                open_track(library, selected, track.id);
                            },
                            Cover { url: track.image.clone(), class: "thumb eager" }
                            div { class: "seed-text",
                                span { class: "title", "{track.title}" }
                                span { class: "muted ellipsis", "{track.artist}" }
                            }
                        }
                    },
                    None => rsx! {
                        p { class: "muted seed-empty",
                            "Open a track from your space, or pick one on the map, to start from it."
                        }
                    },
                }
            }

            div { class: "tabs",
                for option in Mode::ALL {
                    button {
                        key: "{option.label()}",
                        class: if mode == option { "tab active" } else { "tab" },
                        // Switching tabs changes which inputs are shown, and
                        // nothing else: the result stays, labelled with what
                        // actually made it.
                        onclick: move |_| {
                            let mut mode = generator.mode;
                            mode.set(option);
                        },
                        "{option.label()}"
                    }
                }
            }
            p { class: "muted", "{mode.blurb()}" }

            if mode == Mode::Path {
                PathEndRow { label: "A", class: "a", id: from, from_selection: selected() }
                PathEndRow { label: "B", class: "b", id: to, from_selection: selected() }
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
                    code { "stelly-server models" }
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

            div { class: "actions",
                button {
                    class: "primary",
                    disabled: !has_selection && mode != Mode::Path
                        || !generator.ready(selected())
                        || *generator.busy.read(),
                    onclick: move |_| generator.run(selected()),
                    if *generator.busy.read() { "working…" } else { "Generate" }
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
                    "Play"
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
                        enqueue(player, queue);
                    },
                    "Queue"
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
                    "Export"
                }
                button {
                    disabled: empty,
                    onclick: move |_| generator.clear(),
                    "Clear"
                }
            }

            if let Some(message) = generator.status.read().clone() {
                p { class: "muted ellipsis", "{message}" }
            }

            if let Some(recipe) = generator.produced_by.read().clone() {
                div { class: "shelf-head result-head",
                    span { class: "ellipsis", "{describe(&recipe, label_for)}" }
                    span { class: "spacer" }
                    // The one action that dims the map: here the route is
                    // the point, so everything not on it drops back.
                    button {
                        class: "chip",
                        title: "trace this on the map",
                        onclick: move |_| {
                            close();
                            map.show_route();
                        },
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
            ol { class: "list result",
                for step in result {
                    li {
                        key: "{step.track.track_id}",
                        class: if selected() == Some(step.track.track_id) { "row selected" } else { "row" },
                        onclick: {
                            let id = step.track.track_id;
                            move |_| {
                                close();
                                open_track(library, selected, id);
                            }
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
                            library,
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

            WeightsDisclosure {}
        }
    }
}

/// One end of a path: which track it is, a way to make it the selection, and
/// a way to clear it.
#[component]
fn PathEndRow(label: &'static str, class: &'static str, id: Option<i64>, from_selection: Option<i64>) -> Element {
    let generator = use_context::<Generator>();
    let mut end = if label == "A" { generator.from } else { generator.to };

    rsx! {
        div { class: "field path-field",
            span { class: "path-end {class} set", "{label}" }
            span { class: if id.is_some() { "ellipsis" } else { "ellipsis muted" },
                if id.is_some() { {label_for(id)} } else { "not set" }
            }
            button {
                class: "chip",
                title: "use the track in hand",
                disabled: from_selection.is_none() || from_selection == id,
                onclick: move |_| end.set(from_selection),
                "set"
            }
            if id.is_some() {
                button {
                    class: "chip",
                    title: "clear",
                    onclick: move |_| end.set(None),
                    "×"
                }
            }
        }
    }
}

/// The weight sliders, collapsed by default: most sessions never touch them.
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

/// A path half-built, on a phone, where the panel that shows it is usually
/// slid away. Above the player so it survives being scrolled past or the app
/// being backgrounded; tapping it brings the panel back. Hidden on a wide
/// screen, where the panel itself is always showing.
#[component]
pub fn PathPill() -> Element {
    let generator = use_context::<Generator>();
    let mut panel_open = generator.panel_open;

    let (Some(from), None) = (*generator.from.read(), *generator.to.read()) else {
        return rsx! {};
    };

    rsx! {
        div { class: "path-pill",
            span { class: "path-end a set", "A" }
            span {
                class: "ellipsis",
                onclick: move |_| panel_open.set(true),
                strong { {label_for(Some(from))} }
                ", pick an end"
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

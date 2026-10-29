//! What is left of the generate panel once generating moved to the track
//! page and its results into the queue: a half-built path, said above the
//! player, and the knobs that shape every result, kept in settings.

use super::generate::{failure, Generator};
use super::{space_row_or_glimpse, Space, Weights};
use crate::backend::backend;
use dioxus::prelude::*;
use std::collections::HashMap;

/// Save tracks as a playlist with the provider, named for their count.
pub(crate) async fn export(track_ids: Vec<i64>) -> anyhow::Result<i64> {
    let name = format!("Stelly ({} tracks)", track_ids.len());
    backend().export_playlist(&name, &track_ids).await
}

/// Save a generated group, reporting how it went under its header.
pub(crate) fn export_group(generator: Generator, group: u64, ids: Vec<i64>) {
    let mut about = generator.about;
    let mut status = generator.status;
    let mut failed = generator.failed;
    about.set(Some(group));
    status.set(Some("saving…".into()));
    failed.set(false);
    spawn(async move {
        match export(ids).await {
            Ok(id) => status.set(Some(format!("saved as playlist {id}"))),
            Err(err) => {
                status.set(Some(failure(&err)));
                failed.set(true);
            }
        }
    });
}

/// "artist - title" for a space track, or its id when the space no longer
/// holds it.
pub(crate) fn label_for(id: Option<i64>) -> String {
    let Some(id) = id else { return "-".into() };
    space_row_or_glimpse(id)
        .map(|track| format!("{} - {}", track.artist, track.title))
        .unwrap_or_else(|| id.to_string())
}

/// The title alone, for a header with little room.
pub(crate) fn title_for(id: i64) -> String {
    space_row_or_glimpse(id).map(|track| track.title).unwrap_or_else(|| id.to_string())
}

/// Keep the weights for this account, so its other devices, and this one
/// next time, start from them. Only what differs from the defaults goes.
fn save_tuning(
    weights: HashMap<String, f32>,
    defaults: HashMap<String, f32>,
    mut failure: Signal<Option<String>>,
) {
    let tuning = super::tuning_of(&weights, &defaults);
    spawn(async move {
        match backend().set_tuning(&tuning).await {
            Ok(()) => failure.set(None),
            Err(err) => failure.set(Some(format!("Not saved for your other devices: {}", super::generate::failure(&err)))),
        }
    });
}

/// How results are shaped, in settings: how many tracks, how hard radio
/// pushes away from artists it has used, a path's pace, and the weights the
/// distances are measured with, collapsed since most sessions never touch
/// them. The weights are the account's, kept on the server.
#[component]
pub fn GenerateRow() -> Element {
    let generator = use_context::<Generator>();
    let mut weights = use_context::<Weights>().0;
    let mut expanded = use_signal(|| false);
    let save_failed = use_signal(|| None::<String>);

    let blocks = use_context::<Space>().info.read().blocks.clone();
    let defaults: HashMap<String, f32> =
        blocks.iter().map(|block| (block.name.clone(), block.default_weight)).collect();
    let block_names: Vec<String> = blocks.into_iter().map(|block| block.name).collect();

    let weights_changed = {
        let current = weights();
        defaults.iter().any(|(name, default)| {
            current
                .get(name)
                .is_some_and(|value| (value - default).abs() > 1e-6)
        })
    };
    let even = *generator.even.read();

    rsx! {
        section { class: "panel setting generate-setting",
            h2 { "Generate" }
            p { class: "muted", "How a radio, path, drift or mood comes out, from the next one on." }

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
            div { class: "slider",
                label { title: "how strongly radio avoids artists it has already played", "artist pull" }
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
            div { class: "field",
                span { class: "muted", "path" }
                button {
                    class: if even { "chip" } else { "chip active" },
                    onclick: move |_| { let mut even = generator.even; even.set(false); },
                    "shortest"
                }
                button {
                    class: if even { "chip active" } else { "chip" },
                    title: "the track count, spaced evenly from start to end",
                    onclick: move |_| { let mut even = generator.even; even.set(true); },
                    "evenly paced"
                }
            }

            div { class: "weights-disclosure",
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
                        "Reshapes distances from the next result on, on all your devices. Map positions are fixed until the layout step is re-run."
                    }
                    div { class: "actions",
                        span { class: "spacer" }
                        button {
                            class: "chip",
                            title: "back to the weights the space was built with",
                            disabled: !weights_changed,
                            onclick: {
                                let defaults = defaults.clone();
                                move |_| {
                                    weights.set(defaults.clone());
                                    generator.invalidate();
                                    save_tuning(defaults.clone(), defaults.clone(), save_failed);
                                }
                            },
                            "reset"
                        }
                    }
                    if let Some(message) = save_failed() {
                        p { class: "error", "{message}" }
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
                                            weights.set(w);
                                            generator.invalidate();
                                        }
                                    }
                                },
                                // Once let go, not on every step of the drag.
                                onchange: {
                                    let defaults = defaults.clone();
                                    move |_| save_tuning(weights.peek().clone(), defaults.clone(), save_failed)
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
}

/// A path half-built: its start is set and its end is whichever track's
/// page "Path to here" is pressed on next. Above the player, so it follows
/// you from page to page while you look for that track.
#[component]
pub fn PathPill() -> Element {
    let generator = use_context::<Generator>();

    let (Some(from), None) = (*generator.from.read(), *generator.to.read()) else {
        return rsx! {};
    };

    rsx! {
        div { class: "path-pill",
            span { class: "path-end a set", "A" }
            span { class: "ellipsis",
                "Path from "
                strong { {label_for(Some(from))} }
                ". Open a track and pick “Path to here”."
            }
            button {
                class: "chip",
                title: "cancel this path",
                onclick: move |_| generator.cancel_path(),
                "×"
            }
        }
    }
}

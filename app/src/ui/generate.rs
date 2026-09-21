//! Making a sequence of tracks out of the space.
//!
//! One panel with four modes, because they differ only in how the sequence is
//! chosen:
//!   - **neighbours**: the k most similar tracks.
//!   - **radio**: a greedy walk, pushed away from artists already used.
//!   - **path**: A to B, shortest route or evenly paced.
//!   - **drift**: away from a track, towards a phrase.

use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{play_list, Player};
use super::Selection;
use dioxus::prelude::*;
use crate::backend::backend;
use crate::paths::{Constraints, Step};
use crate::qobuz::RemoteTrack;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Neighbours,
    Radio,
    Path,
    Drift,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Neighbours, Mode::Radio, Mode::Path, Mode::Drift];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Neighbours => "neighbours",
            Mode::Radio => "radio",
            Mode::Path => "path",
            Mode::Drift => "drift",
        }
    }

    /// Whether the selection is this mode's only input, and so whether it can
    /// follow it instead of waiting for the generate button. Same split as
    /// `Generator::ready`, derived from the same question so the two cannot
    /// disagree.
    fn follows_selection(self) -> bool {
        matches!(self, Mode::Neighbours | Mode::Radio)
    }

    fn blurb(self) -> &'static str {
        match self {
            Mode::Neighbours => "The most similar tracks to the selection.",
            Mode::Radio => "Keep jumping to the nearest track not yet played.",
            Mode::Path => "Travel from A to B through the space.",
            Mode::Drift => "Walk away from the selection, towards a phrase.",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Generator {
    pub mode: Signal<Mode>,
    pub result: Signal<Vec<Step>>,
    /// How many tracks to produce, for the modes that take a count.
    pub count: Signal<usize>,
    /// Similarity subtracted per prior use of an artist, in radio mode.
    pub artist_penalty: Signal<f32>,
    pub phrase: Signal<String>,
    pub from: Signal<Option<i64>>,
    pub to: Signal<Option<i64>>,
    /// Path shape: evenly paced interpolation rather than the shortest route.
    pub even: Signal<bool>,
    pub status: Signal<Option<String>>,
    /// Whether the backend can turn a phrase into an embedding. Half of what
    /// drift needs; the engine answers the other half.
    pub tower: Signal<bool>,
}

impl Generator {
    pub fn new() -> Self {
        Self {
            mode: Signal::new(Mode::Neighbours),
            result: Signal::new(Vec::new()),
            count: Signal::new(20),
            // 0.10 measured out as 19.4 distinct artists per 20 tracks
            // against 14.8 unpenalised, for 0.786 similarity against 0.796.
            artist_penalty: Signal::new(0.10),
            phrase: Signal::new(String::new()),
            from: Signal::new(None),
            to: Signal::new(None),
            even: Signal::new(false),
            status: Signal::new(None),
            tower: Signal::new(false),
        }
    }

    /// Whether drift is offerable at all.
    fn can_steer(&self) -> bool {
        *self.tower.read() && crate::engine().lock().unwrap().has_audio_embeddings()
    }

    /// Whether the current mode has what it needs.
    fn ready(&self, selected: Option<i64>) -> bool {
        match *self.mode.peek() {
            Mode::Neighbours | Mode::Radio => selected.is_some(),
            Mode::Path => self.from.peek().is_some() && self.to.peek().is_some(),
            Mode::Drift => selected.is_some() && !self.phrase.peek().trim().is_empty(),
        }
    }

    /// Produce a sequence. Spawned rather than inline because drift has to
    /// await: turning a phrase into a CLAP embedding may be a round trip. The
    /// walk itself is always local.
    pub fn run(self, selected: Option<i64>) {
        let mut generator = self;
        generator.status.set(None);

        let count = *self.count.peek();
        let mode = *self.mode.peek();
        let penalty = *self.artist_penalty.peek();
        let even = *self.even.peek();
        let phrase = self.phrase.peek().clone();
        let (from, to) = (*self.from.peek(), *self.to.peek());

        spawn(async move {
            let produced = match mode {
                Mode::Neighbours => selected.map(|id| {
                    crate::engine()
                        .lock()
                        .unwrap()
                        .navigator
                        .neighbours(id, count, false)
                }),
                Mode::Radio => selected.map(|id| {
                    crate::engine().lock().unwrap().navigator.radio_nearest(
                        id,
                        count,
                        penalty,
                        &Constraints::default(),
                    )
                }),
                Mode::Path => match (from, to) {
                    (Some(a), Some(b)) => {
                        let mut guard = crate::engine().lock().unwrap();
                        Some(if even {
                            guard.navigator.interpolate(a, b, count, &Constraints::default())
                        } else {
                            guard.navigator.graph_path(a, b, 16)
                        })
                    }
                    _ => None,
                },
                Mode::Drift => match selected {
                    Some(id) => {
                        // Embed first, then walk. The await is outside the
                        // lock: holding the engine across a round trip would
                        // freeze every slider.
                        match backend().embed(&phrase).await {
                            Ok(embedding) => {
                                let guard = &mut *crate::engine().lock().unwrap();
                                Some(guard.navigator.drift_to_text(
                                    id,
                                    &embedding,
                                    count,
                                    5,
                                    &Constraints::default(),
                                ))
                            }
                            Err(err) => {
                                generator.status.set(Some(format!("{err:#}")));
                                None
                            }
                        }
                    }
                    None => None,
                },
            };

            let Some(produced) = produced else { return };
            if produced.is_empty() {
                generator
                    .status
                    .set(Some("nothing found, try a different start".into()));
            }
            generator.result.set(produced);
        });
    }

    pub fn clear(mut self) {
        self.result.set(Vec::new());
        self.status.set(None);
    }
}

impl Default for Generator {
    fn default() -> Self {
        Self::new()
    }
}

/// Present a space track to the player, which speaks in Qobuz terms.
pub(crate) fn as_remote(meta: &crate::db::TrackMeta) -> RemoteTrack {
    RemoteTrack {
        id: meta.track_id,
        title: meta.title.clone(),
        artist: meta.artist.clone(),
        artist_id: Some(meta.artist_id).filter(|id| *id >= 0),
        album: meta.album.clone(),
        album_id: Some(meta.album_id.clone()).filter(|id| !id.is_empty()),
        duration: None,
        streamable: true,
        hires: false,
        // The space stores no art, so the cover is derived from the album id.
        image: crate::qobuz::cover_url(&meta.album_id),
        isrc: meta.isrc.clone(),
    }
}

async fn export(track_ids: Vec<i64>) -> anyhow::Result<i64> {
    let name = format!("two_khz ({} tracks)", track_ids.len());
    backend().export_playlist(&name, &track_ids).await
}

#[component]
pub fn GeneratePanel() -> Element {
    let generator = use_context::<Generator>();
    let player = use_context::<Player>();
    let selection = use_context::<Selection>();
    let mut selected = selection.0;
    let mut menu = use_context::<ContextMenu>().0;

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

    // Modes whose only input is the selection follow it; the others need a
    // second endpoint or a phrase, so they stay deliberate. Clicking a result
    // row re-runs from that track.
    use_effect(move || {
        let current = selected();
        if current.is_some() && generator.mode.read().follows_selection() {
            generator.run(current);
        }
    });

    rsx! {
        section { class: "panel generate",
            h2 { "Generate" }

            div { class: "tabs",
                for option in Mode::ALL {
                    button {
                        key: "{option.label()}",
                        class: if mode == option { "tab active" } else { "tab" },
                        onclick: move |_| {
                            let mut mode = generator.mode;
                            mode.set(option);
                            generator.clear();
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
                    disabled: !generator.ready(selected()),
                    onclick: move |_| generator.run(selected()),
                    "generate"
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
            ol { class: "list",
                for step in result {
                    li {
                        key: "{step.track.track_id}",
                        class: if selected() == Some(step.track.track_id) { "row selected" } else { "row" },
                        onclick: {
                            let id = step.track.track_id;
                            move |_| selected.set(Some(id))
                        },
                        oncontextmenu: {
                            let id = step.track.track_id;
                            move |event: Event<MouseData>| {
                                event.prevent_default();
                                open_menu(&mut menu, &event, MenuTarget::SpaceTrack(id));
                            }
                        },
                        span { class: "artist", "{step.track.artist}" }
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
}

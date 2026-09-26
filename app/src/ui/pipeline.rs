//! Driving the whole pipeline, on the server.
//!
//! Four stages, crawl, analyse, build space, layout, all run in the
//! server's process. This view asks it to start one and watches the status and
//! log it publishes, so a stage started here outlives this window, which is
//! the point. **Stop** is the only way one ends early.

use super::crawler::Crawler;
use super::POLL;
use dioxus::prelude::*;
use crate::api::{Corpus, Device, PipelineStatus, Scope, Stage};
use crate::backend::backend;

/// How many status polls pass between corpus recounts. Six SQL aggregates
/// over the whole catalogue would be the most expensive idle thing the app
/// does at 200ms.
const CORPUS_EVERY: u32 = 10;

#[derive(Clone, Copy)]
pub struct Pipeline {
    pub status: Signal<PipelineStatus>,
    pub log: Signal<Vec<String>>,
    pub corpus: Signal<Corpus>,
    /// Bumped when the space underneath has been rebuilt, so the shell knows
    /// to reload the engine and redraw the map.
    pub generation: Signal<u64>,
    /// (in_space, on_map), written by the shell, only it can see the engine.
    pub space_counts: Signal<(i64, i64)>,
    /// Whatever the backend last refused to do.
    pub error: Signal<Option<String>>,
    pub devices: Signal<Vec<Device>>,
    /// A token, shown exactly once, right after pairing.
    pub granted: Signal<Option<String>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            status: Signal::new(PipelineStatus::default()),
            log: Signal::new(Vec::new()),
            corpus: Signal::new(Corpus::default()),
            generation: Signal::new(0),
            space_counts: Signal::new((0, 0)),
            error: Signal::new(None),
            devices: Signal::new(Vec::new()),
            granted: Signal::new(None),
        }
    }

    pub fn running(&self) -> Option<Stage> {
        self.status.read().running
    }

    pub fn clear_log(mut self) {
        self.log.set(Vec::new());
        spawn(async move {
            let _ = backend().pipeline_clear_log().await;
        });
    }

    /// Re-read the counts that tell you what still needs running. `in_space`
    /// and `on_map` come from the loaded engine: the question is what this app
    /// is drawing, not what is on disk.
    pub fn refresh(self) {
        spawn(async move {
            let mut pipeline = self;
            if let Ok(mut corpus) = backend().corpus().await {
                let (in_space, on_map) = *pipeline.space_counts.peek();
                corpus.in_space = in_space;
                corpus.on_map = on_map;
                pipeline.corpus.set(corpus);
            }
        });
    }

    pub fn start(mut self, stage: Stage, crawler: Crawler) {
        if self.running().is_some() {
            return;
        }
        if stage == Stage::Crawl {
            crawler.start();
            return;
        }
        self.error.set(None);
        spawn(async move {
            let mut pipeline = self;
            if let Err(err) = backend().pipeline_start(stage).await {
                pipeline.error.set(Some(format!("{err:#}")));
            }
        });
    }

    /// Queue analyse, then build-space, then layout.
    pub fn start_full_run(mut self) {
        if self.running().is_some() {
            return;
        }
        self.error.set(None);
        spawn(async move {
            let mut pipeline = self;
            if let Err(err) = backend().pipeline_start_full().await {
                pipeline.error.set(Some(format!("{err:#}")));
            }
        });
    }

    pub fn stop(mut self, crawler: Crawler) {
        self.error.set(None);
        crawler.request_stop();
        spawn(async move {
            let mut pipeline = self;
            if let Err(err) = backend().pipeline_stop().await {
                pipeline.error.set(Some(format!("{err:#}")));
            }
        });
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Keep the status, the log and the counts fresh. Mounted once. One future
/// rather than three, so the log cursor and the status cannot disagree about
/// whether a stage is running.
pub fn use_pipeline_watch(pipeline: Pipeline) {
    use_future(move || async move {
        let mut pipeline = pipeline;
        let mut ticks: u32 = 0;

        loop {
            if let Ok(status) = backend().pipeline_status().await {
                let generation = status.generation;
                pipeline.status.set(status);
                if generation != *pipeline.generation.peek() {
                    pipeline.generation.set(generation);
                }
            }

            // A mirror, not an accumulation, the backend's buffer is
            // already bounded and authoritative, so a reconnecting log shows
            // the tail once.
            if let Ok(lines) = backend().pipeline_log().await {
                if *pipeline.log.peek() != lines {
                    pipeline.log.set(lines);
                }
            }

            if ticks % CORPUS_EVERY == 0 {
                pipeline.refresh();
            }
            ticks = ticks.wrapping_add(1);

            tokio::time::sleep(POLL).await;
        }
    });
}

// ------------------------------------------------------------------- view

#[component]
pub fn PipelineView() -> Element {
    let pipeline = use_context::<Pipeline>();
    let crawler = use_context::<Crawler>();

    let running = pipeline.running();
    let crawling = crawler.running();
    let busy = running.is_some() || crawling;
    let corpus = pipeline.corpus.read().clone();
    let log = pipeline.log.read().clone();
    let queued = pipeline.status.read().queued.clone();

    // Each hint names the stage that would fix it, and is derived from the
    // same question that stage asks.
    let needs_build = corpus.buildable != corpus.in_space;
    let needs_layout = corpus.in_space > corpus.on_map;

    rsx! {
        div { class: "pipeline",
            section { class: "panel",
                h2 { "Corpus" }
                div { class: "counts",
                    div { span { class: "count", "{corpus.tracks}" } span { class: "muted", "tracks" } }
                    div { span { class: "count", "{corpus.analysed}" } span { class: "muted", "analysed" } }
                    div { span { class: "count", "{corpus.to_analyse}" } span { class: "muted", "to analyse" } }
                    div { span { class: "count", "{corpus.on_map}" } span { class: "muted", "on the map" } }
                    div { span { class: "count", "{corpus.pending}" } span { class: "muted", "queued to crawl" } }
                    div { span { class: "count", "{corpus.failed}" } span { class: "muted", "failed" } }
                }
                if needs_build {
                    p { class: "muted notice",
                        "The space holds {corpus.in_space} tracks but {corpus.buildable} are ready, run build space."
                    }
                }
                if needs_layout {
                    p { class: "muted notice",
                        {format!("{} tracks in the space have no coordinates, run layout.",
                                 corpus.in_space - corpus.on_map)}
                    }
                }
                if !needs_build && !needs_layout && corpus.to_analyse == 0 {
                    p { class: "muted", "Up to date. Crawl for more, or analyse after a crawl." }
                }
            }

            section { class: "panel",
                h2 { "Stages" }
                p { class: "muted",
                    "Running on the server. A stage started here keeps going if you close this window, use stop to end it."
                }
                div { class: "actions",
                    button {
                        class: "primary",
                        disabled: busy,
                        onclick: move |_| pipeline.start_full_run(),
                        "run analyse → space → layout"
                    }
                    if busy {
                        button {
                            class: "danger",
                            onclick: move |_| pipeline.stop(crawler),
                            "stop"
                        }
                    }
                }

                if let Some(message) = pipeline.error.read().clone() {
                    p { class: "muted error", "{message}" }
                }

                ul { class: "stages",
                    for stage in Stage::ALL {
                        li {
                            key: "{stage.label()}",
                            class: if running == Some(stage) || (stage == Stage::Crawl && crawling) {
                                "stage active"
                            } else {
                                "stage"
                            },
                            div { class: "stage-name", "{stage.label()}" }
                            div { class: "stage-blurb muted", "{stage.blurb()}" }
                            div { class: "stage-run",
                                if queued.contains(&stage) {
                                    span { class: "muted", "queued" }
                                } else if stage == Stage::Crawl && crawling {
                                    button {
                                        class: "danger",
                                        onclick: move |_| crawler.request_stop(),
                                        "stop"
                                    }
                                } else {
                                    button {
                                        disabled: busy,
                                        onclick: move |_| pipeline.start(stage, crawler),
                                        "run"
                                    }
                                }
                            }
                        }
                    }
                }

                if crawling {
                    p { class: "muted ellipsis",
                        "crawling: "
                        {crawler.last().unwrap_or_default()}
                    }
                }
            }

            section { class: "panel log-panel",
                h2 {
                    "Output"
                    span { class: "spacer" }
                    button { class: "chip", onclick: move |_| pipeline.clear_log(), "clear" }
                }
                pre { class: "log",
                    if log.is_empty() {
                        span { class: "muted", "Nothing run yet. Output from the pipeline appears here." }
                    }
                    for (index, line) in log.into_iter().enumerate() {
                        div { key: "{index}", "{line}" }
                    }
                }
            }

            Devices {}
        }
    }
}

/// Paired devices, and a way to add or revoke one. Needs the `pipeline`
/// scope itself, a `play` phone cannot mint itself a
/// promotion.
#[component]
fn Devices() -> Element {
    let pipeline = use_context::<Pipeline>();
    let mut name = use_signal(String::new);
    let mut scope = use_signal(|| Scope::Play);

    let devices = pipeline.devices.read().clone();

    use_future(move || async move {
        let mut pipeline = pipeline;
        if let Ok(found) = backend().devices().await {
            pipeline.devices.set(found);
        }
    });

    let refresh = move || {
        spawn(async move {
            let mut pipeline = pipeline;
            if let Ok(found) = backend().devices().await {
                pipeline.devices.set(found);
            }
        });
    };

    rsx! {
        section { class: "panel",
            h2 { "Devices" }
            p { class: "muted",
                "Each device gets its own token, so one phone can be revoked without re-pairing the rest. The token is shown once."
            }

            div { class: "field",
                input {
                    class: "search",
                    placeholder: "device name, e.g. phone",
                    value: "{name}",
                    oninput: move |event| name.set(event.value()),
                }
                button {
                    class: if *scope.read() == Scope::Play { "chip active" } else { "chip" },
                    onclick: move |_| scope.set(Scope::Play),
                    "play"
                }
                button {
                    class: if *scope.read() == Scope::Pipeline { "chip active" } else { "chip" },
                    onclick: move |_| scope.set(Scope::Pipeline),
                    "pipeline"
                }
                button {
                    class: "primary",
                    disabled: name.read().trim().is_empty(),
                    onclick: move |_| {
                        let wanted = name.peek().trim().to_string();
                        let chosen = *scope.peek();
                        spawn(async move {
                            let mut pipeline = pipeline;
                            match backend().pair_device(&wanted, chosen).await {
                                Ok(grant) => {
                                    pipeline.granted.set(Some(grant.token));
                                    name.set(String::new());
                                    if let Ok(found) = backend().devices().await {
                                        pipeline.devices.set(found);
                                    }
                                }
                                Err(err) => pipeline.error.set(Some(format!("{err:#}"))),
                            }
                        });
                    },
                    "pair"
                }
            }

            if let Some(token) = pipeline.granted.read().clone() {
                div { class: "notice",
                    p { class: "muted", "Set this on the device, then close this. It is not shown again." }
                    pre { class: "log", "TWO_KHZ_TOKEN={token}" }
                    button {
                        class: "chip",
                        onclick: move |_| { let mut pipeline = pipeline; pipeline.granted.set(None); },
                        "done"
                    }
                }
            }

            ul { class: "list",
                for device in devices {
                    li { key: "{device.id}", class: "row",
                        span { class: "title", "{device.name}" }
                        span { class: "muted tag", "{device.scope.as_str()}" }
                        span { class: "muted",
                            {device.last_seen.clone().unwrap_or_else(|| "never seen".into())}
                        }
                        button {
                            class: "chip danger",
                            onclick: move |_| {
                                let id = device.id;
                                spawn(async move {
                                    let mut pipeline = pipeline;
                                    if let Err(err) = backend().revoke_device(id).await {
                                        pipeline.error.set(Some(format!("{err:#}")));
                                    }
                                });
                                refresh();
                            },
                            "revoke"
                        }
                    }
                }
            }
        }
    }
}

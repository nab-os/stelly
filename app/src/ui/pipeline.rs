//! Driving the whole pipeline, on the server.
//!
//! Four stages, crawl, analyse, build space, layout, all run in the
//! server's process. This view asks it to start one and watches the status and
//! log it publishes, so a stage started here outlives this window, which is
//! the point. **Stop** is the only way one ends early.

use super::crawler::Crawler;
use super::{icons, pace, Me, Space};
use dioxus::prelude::*;
use crate::api::{Corpus, MixQueue, PipelineStatus, Progress, Scope, Stage};
use crate::backend::backend;

/// How many status polls pass between corpus recounts. Six SQL aggregates
/// over the whole catalogue would be the most expensive idle thing the app
/// does at 200ms. Only the pipeline view shows them, so only while it is open.
const CORPUS_EVERY: u32 = 10;

#[derive(Clone, Copy)]
pub struct Pipeline {
    pub status: Signal<PipelineStatus>,
    pub log: Signal<Vec<String>>,
    pub corpus: Signal<Corpus>,
    /// Bumped when the space underneath has been rebuilt, so the shell knows
    /// to ask for it again and redraw the map.
    pub generation: Signal<u64>,
    /// (in_space, on_map), written by the shell from what the server says.
    pub space_counts: Signal<(i64, i64)>,
    /// Whatever the backend last refused to do.
    pub error: Signal<Option<String>>,
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
    /// and `on_map` come from the loaded space: the question is what the map
    /// draws, not what is on disk.
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
        pace::kick();
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
        pace::kick();
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
        pace::kick();
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

/// Keep the status and the counts fresh. Mounted once. The log is
/// `PipelineLog`'s own, followed only while it is shown.
///
/// Also the app's pulse: the space is only asked for when it moves, so this
/// is what notices a server going away mid-session, and coming back, for the
/// notice over the main area. Only a server that does not answer counts; one
/// refusing a play device the pipeline is still there. Between jobs it asks
/// every `pace::IDLE`, enough for that.
pub fn use_pipeline_watch(pipeline: Pipeline, space: Space) {
    use_future(move || async move {
        let mut pipeline = pipeline;
        let mut offline = space.offline;
        let mut ticks: u32 = 0;

        loop {
            match backend().pipeline_status().await {
                Ok(status) => {
                    if offline.peek().is_some() {
                        offline.set(None);
                    }
                    let generation = status.generation;
                    // Only a change is written: every write redraws what
                    // reads it, and most polls bring the same answer.
                    if *pipeline.status.peek() != status {
                        pipeline.status.set(status);
                    }
                    if generation != *pipeline.generation.peek() {
                        pipeline.generation.set(generation);
                    }
                }
                Err(err) if crate::backend::is_unreachable(&err) => {
                    if offline.peek().is_none() {
                        offline.set(Some(format!("{err:#}")));
                    }
                }
                Err(_) => {}
            }

            // Counted from the view opening, so it opens on fresh counts.
            if pace::watched() {
                if ticks % CORPUS_EVERY == 0 {
                    pipeline.refresh();
                }
                ticks = ticks.wrapping_add(1);
            } else {
                ticks = 0;
            }

            let busy = {
                let status = pipeline.status.peek();
                status.running.is_some() || !status.adding.is_empty()
            };
            pace::wait(busy, Some(pace::IDLE)).await;
        }
    });
}

// ------------------------------------------------------------------- view

/// Where one step stands, from the same counts the step itself would ask.
#[derive(Clone, Copy, PartialEq)]
enum StepState {
    Running,
    Queued,
    /// Has work it could do now.
    Needed,
    /// Nothing left for it, as far as the counts can tell.
    Done,
    /// The crawl: never done, only ever stopped.
    Idle,
}

impl StepState {
    fn class(self) -> &'static str {
        match self {
            StepState::Running => "running",
            StepState::Queued => "queued",
            StepState::Needed => "needed",
            StepState::Done => "done",
            StepState::Idle => "idle",
        }
    }
}

fn stage_icon(stage: Stage) -> Element {
    match stage {
        Stage::Crawl => icons::crawl(),
        Stage::Analyse => icons::waveform(),
        Stage::Dedupe => icons::twins(),
        Stage::BuildSpace => icons::cube(),
        Stage::Layout => icons::constellation(),
    }
}

fn capitalised(label: &str) -> String {
    let mut chars = label.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The stages as the steps they are, each with the count it moves and
/// whether it has anything to do, so what to run next reads off the page.
#[component]
pub fn PipelineControls() -> Element {
    let pipeline = use_context::<Pipeline>();
    let crawler = use_context::<Crawler>();
    let me = use_context::<Me>().0;
    // A server older than users answers no `me`, and let any device run it.
    let admin = me.read().as_ref().is_none_or(|device| device.scope == Scope::Admin);
    pace::use_watch();

    // The counts are the last the server sent, zeros if it never answered:
    // nothing to judge a step done or due by.
    let offline = use_context::<Space>().offline.read().is_some();

    let running = pipeline.running();
    let crawling = crawler.running();
    let busy = running.is_some() || crawling;
    let corpus = pipeline.corpus.read().clone();
    let queued = pipeline.status.read().queued.clone();
    let adding = pipeline.status.read().adding.len();
    let progress = pipeline.status.read().progress.clone();

    // Each derived from the same question the stage asks.
    let needs = |stage: Stage| match stage {
        Stage::Crawl => false,
        Stage::Analyse => corpus.to_analyse > 0,
        Stage::Dedupe => corpus.to_dedupe > 0,
        Stage::BuildSpace => corpus.buildable != corpus.in_space,
        Stage::Layout => corpus.in_space > corpus.on_map,
    };
    let state = |stage: Stage| {
        if running == Some(stage) || (stage == Stage::Crawl && crawling) {
            StepState::Running
        } else if queued.contains(&stage) {
            StepState::Queued
        } else if stage == Stage::Crawl || offline {
            StepState::Idle
        } else if needs(stage) {
            StepState::Needed
        } else {
            StepState::Done
        }
    };
    let up_to_date = !busy && !offline && Stage::ALL.iter().all(|stage| !needs(*stage));

    let figure = |stage: Stage| -> (String, Option<String>) {
        match stage {
            Stage::Crawl => (
                format!("{} tracks found", corpus.tracks),
                (corpus.pending > 0).then(|| format!("{} still to visit", corpus.pending)),
            ),
            Stage::Analyse => (
                format!("{} analysed", corpus.analysed),
                match (corpus.to_analyse, corpus.failed) {
                    (0, 0) => None,
                    (0, failed) => Some(format!("{failed} failed")),
                    (left, 0) => Some(format!("{left} to go")),
                    (left, failed) => Some(format!("{left} to go, {failed} failed")),
                },
            ),
            Stage::Dedupe => (
                format!("{} compared", corpus.analysed - corpus.to_dedupe),
                needs(stage).then(|| format!("{} to compare", corpus.to_dedupe)),
            ),
            Stage::BuildSpace => (
                format!("{} in the space", corpus.in_space),
                needs(stage).then(|| format!("{} ready", corpus.buildable)),
            ),
            Stage::Layout => (
                format!("{} on the map", corpus.on_map),
                needs(stage).then(|| format!("{} without a place", corpus.in_space - corpus.on_map)),
            ),
        }
    };

    let headline = match running {
        Some(stage) => format!("{} running", capitalised(stage.label())),
        None if crawling => "Crawling".into(),
        None if offline => "Server unreachable".into(),
        None if up_to_date => "Up to date".into(),
        None => "Work waiting".into(),
    };
    let headline_class = if offline {
        "pipeline-state down"
    } else if busy {
        "pipeline-state running"
    } else if up_to_date {
        "pipeline-state done"
    } else {
        "pipeline-state needed"
    };

    rsx! {
        section { class: "panel setting pipeline",
            h2 {
                "Pipeline"
                span { class: "{headline_class}", "{headline}" }
                span { class: "spacer" }
                if admin {
                    if busy {
                        button {
                            class: "danger",
                            onclick: move |_| pipeline.stop(crawler),
                            {icons::stop()}
                            "Stop"
                        }
                    } else {
                        button {
                            class: "primary",
                            title: "analyse what is new, then build the space and lay out the map",
                            disabled: up_to_date || offline,
                            onclick: move |_| pipeline.start_full_run(),
                            "Update the space"
                        }
                    }
                }
            }
            p { class: "muted",
                if admin {
                    "Stages run on the server and keep going if you close the app. Update the space runs analyse, build space and layout in turn."
                } else {
                    "Stages run on the server. Only an admin device can start or stop them."
                }
            }

            if let Some(message) = pipeline.error.read().clone() {
                p { class: "setup-error", "{message}" }
            }

            ol { class: "steps",
                for stage in Stage::ALL {
                    {
                        let now = state(stage);
                        let (main, aside) = figure(stage);
                        rsx! {
                            li { key: "{stage.label()}", class: "step {now.class()}",
                                span { class: "step-icon",
                                    if now == StepState::Done { {icons::check()} } else { {stage_icon(stage)} }
                                }
                                div { class: "step-body",
                                    div { class: "step-head",
                                        span { class: "step-name", {capitalised(stage.label())} }
                                        match now {
                                            StepState::Running => rsx! { span { class: "step-badge running", "running" } },
                                            StepState::Queued => rsx! { span { class: "step-badge", "queued" } },
                                            StepState::Needed => rsx! { span { class: "step-badge needed", "to run" } },
                                            _ => rsx! {},
                                        }
                                    }
                                    span { class: "step-figure",
                                        "{main}"
                                        if let Some(aside) = aside {
                                            span { class: "muted", " · {aside}" }
                                        }
                                    }
                                    span { class: "muted step-blurb", "{stage.blurb()}" }
                                    if running == Some(stage) {
                                        if let Some(progress) = progress.clone() {
                                            {progress_row(&progress)}
                                        }
                                    }
                                    if stage == Stage::Crawl && crawling {
                                        if let Some(last) = crawler.last() {
                                            span { class: "muted ellipsis step-last", "{last}" }
                                        }
                                    }
                                }
                                if admin {
                                    div { class: "step-run",
                                        if stage == Stage::Crawl && crawling {
                                            button {
                                                class: "chip danger",
                                                onclick: move |_| crawler.request_stop(),
                                                "stop"
                                            }
                                        } else if now != StepState::Queued && now != StepState::Running {
                                            button {
                                                class: if now == StepState::Needed { "chip active" } else { "chip" },
                                                title: "run {stage.label()} on its own",
                                                disabled: busy || offline,
                                                onclick: move |_| pipeline.start(stage, crawler),
                                                {icons::play()}
                                                "run"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if adding > 0 {
                p { class: "pipeline-note",
                    "{adding} asked for by name, going into the space ahead of the backlog."
                }
            }
        }
    }
}

/// The mix queue: whole tracks analysed for mixing, what waits and why, and
/// a box to queue a selection the way stelly-mix's `server-analyse` does.
/// Asked for every couple of seconds while it is on screen and moving.
#[component]
pub fn MixQueuePanel() -> Element {
    let me = use_context::<Me>().0;
    let admin = me.read().as_ref().is_none_or(|device| device.scope == Scope::Admin);
    let mut queue = use_signal(|| None::<Result<MixQueue, String>>);
    let mut selection = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut asked = use_signal(|| 0u64);

    use_future(move || async move {
        loop {
            let fetched = backend().mix_queue().await.map_err(|err| format!("{err:#}"));
            let moving = matches!(&fetched, Ok(q) if q.selection_running || !q.analysing.is_empty());
            if *queue.peek() != Some(fetched.clone()) {
                queue.set(Some(fetched));
            }
            let wait = if moving || asked.peek().clone() > 0 { 2 } else { 10 };
            asked.set(0);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }
    });

    let mut submit = move || {
        let text = selection.peek().trim().to_string();
        if text.is_empty() {
            return;
        }
        error.set(None);
        spawn(async move {
            match backend().mix_select(&text).await {
                Ok(()) => {
                    selection.set(String::new());
                    asked.set(1);
                }
                Err(err) => error.set(Some(format!("{err:#}"))),
            }
        });
    };

    let Some(Ok(q)) = queue.read().clone() else {
        return rsx! {
            section { class: "panel setting pipeline",
                h2 { "Mix analysis" }
                match queue.read().clone() {
                    Some(Err(err)) => rsx! { p { class: "muted", "{err}" } },
                    _ => rsx! { p { class: "muted", "…" } },
                }
            }
        };
    };
    let waiting: usize = q.waiting.iter().map(|(_, n)| n).sum();
    let (headline, headline_class) = match (q.analysing.len(), waiting) {
        (0, 0) => ("Up to date".to_string(), "pipeline-state done"),
        (0, n) => (format!("{n} queued"), "pipeline-state needed"),
        (_, n) => (format!("analysing, {n} queued"), "pipeline-state running"),
    };

    rsx! {
        section { class: "panel setting pipeline mix-queue",
            h2 {
                "Mix analysis"
                span { class: "{headline_class}", "{headline}" }
                span { class: "spacer" }
                if admin && q.selection_running {
                    button {
                        class: "danger",
                        title: "call the selection off; liked and played tracks stay queued",
                        onclick: move |_| {
                            spawn(async move {
                                if let Err(err) = backend().mix_stop().await {
                                    error.set(Some(format!("{err:#}")));
                                }
                                asked.set(1);
                            });
                        },
                        {icons::stop()}
                        "Stop selection"
                    }
                }
            }
            p { class: "muted",
                "Whole tracks, for mixing: beats, key, phrases. What is liked or asked for goes first, then selections, then what was played through, one at a time. "
                "{q.analysed} analysed"
                if q.failed > 0 { ", {q.failed} could not be" }
                "."
            }
            if let Some(message) = error.read().clone() {
                p { class: "setup-error", "{message}" }
            }
            if !q.waiting.is_empty() {
                div { class: "moods",
                    for (reason, n) in q.waiting.iter() {
                        span { key: "{reason}", class: "chip", "{n} {queued_label(reason)}" }
                    }
                }
            }
            if !q.analysing.is_empty() || !q.next.is_empty() {
                ol { class: "list mix-next",
                    for track in q.analysing.iter() {
                        li { key: "now-{track.track_id}", class: "row playing",
                            span { class: "title", "{track.title}" span { class: "muted", " · {track.artist}" } }
                            span { class: "chip", "now" }
                        }
                    }
                    for track in q.next.iter() {
                        li { key: "next-{track.track_id}", class: "row",
                            span { class: "title", "{track.title}" span { class: "muted", " · {track.artist}" } }
                            span { class: "muted", "{queued_label(&track.reason)}" }
                        }
                    }
                }
            }
            if let Some(selection) = q.selection.clone() {
                p { class: "muted ellipsis",
                    if q.selection_running { "{selection}: " } else { "Last selection, {selection}: " }
                    {q.last.clone().unwrap_or_default()}
                }
            }
            if admin {
                div { class: "pair-actions mix-select",
                    input {
                        class: "search",
                        placeholder: "album/<id>, artist-name/Lun?n=30, mood/dark rolling techno?n=200, favourites",
                        value: "{selection}",
                        oninput: move |event| {
                            selection.set(event.value());
                            error.set(None);
                        },
                        onkeydown: move |event: KeyboardEvent| {
                            if event.key() == Key::Enter {
                                submit();
                            }
                        },
                    }
                    button {
                        class: "primary",
                        disabled: q.selection_running,
                        title: "analyse these whole tracks for mixing, behind what is liked",
                        onclick: move |_| submit(),
                        "Queue"
                    }
                }
            }
        }
    }
}

/// Why a track is in the mix queue, as a person would say it.
pub(super) fn queued_label(reason: &str) -> &'static str {
    match reason {
        "asked" => "asked for",
        "liked" => "liked",
        "selection" => "in a selection",
        _ => "played through",
    }
}

/// A bar under the running stage, with the count and, once there is a rate to
/// go on, how long the rest should take.
fn progress_row(progress: &Progress) -> Element {
    let percent = progress.fraction() * 100.0;
    let fill = progress.fraction();
    let mut detail = Vec::new();
    if progress.step == "catalogue" {
        detail.push("cataloguing".to_string());
    }
    // Albums and tracks are worth counting; build-space and layout count
    // internal units, so they get a percentage.
    if matches!(progress.step.as_str(), "catalogue" | "analyse") {
        if progress.total > 1 {
            detail.push(format!("{}/{}", progress.done, progress.total));
        }
    } else {
        detail.push(format!("{percent:.0}%"));
    }
    if let Some(left) = progress.remaining() {
        detail.push(format!("{} left", span_of(left)));
    }
    rsx! {
        div { class: "stage-progress",
            // Scaled rather than sized, so each step it moves is composited
            // rather than laid out.
            div { class: "bar", div { style: "transform: scaleX({fill:.4})" } }
            span { class: "muted", {detail.join(" · ")} }
        }
    }
}

fn span_of(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{seconds:.0}s")
    } else if seconds < 90.0 * 60.0 {
        format!("{:.0} min", seconds / 60.0)
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

/// What the pipeline has been printing, as the last settings row. Follows
/// the newest line unless scrolled up to read an older one.
#[component]
pub fn PipelineLog() -> Element {
    let mut pipeline = use_context::<Pipeline>();
    let me = use_context::<Me>().0;
    let admin = me.read().as_ref().is_none_or(|device| device.scope == Scope::Admin);

    // Followed only while shown. The server replays what it retains on each
    // connect, so reopening still shows the tail. The backend is held, not
    // asked again, so a pairing changed meanwhile drops the right socket.
    let remote = use_hook(|| {
        let remote = backend();
        remote.follow_pipeline_log();
        remote
    });
    use_drop(move || remote.unfollow_pipeline_log());

    // A mirror, not an accumulation: the backend's buffer is already bounded
    // and authoritative. Asleep until a line comes in.
    use_future(move || async move {
        loop {
            // Taken before the lines, so one pushed in between still wakes.
            let cursor = remote.pipeline_log_cursor();
            let lines = remote.pipeline_log();
            if *pipeline.log.peek() != lines {
                pipeline.log.set(lines);
            }
            remote.pipeline_log_past(cursor).await;
        }
    });

    let log = pipeline.log.read().clone();
    let count = log.len();

    use_effect(|| {
        document::eval(
            "const log = document.getElementById('pipeline-log');
             if (log && !log.dataset.follow) {
               log.dataset.follow = '1';
               let stuck = true;
               log.addEventListener('scroll', () => {
                 stuck = log.scrollHeight - log.scrollTop - log.clientHeight < 40;
               });
               new MutationObserver(() => { if (stuck) log.scrollTop = log.scrollHeight; })
                 .observe(log, { childList: true, subtree: true });
               log.scrollTop = log.scrollHeight;
             }",
        );
    });

    rsx! {
        section { class: "panel setting log-panel",
            h2 {
                "Pipeline output"
                if count > 0 {
                    span { class: "muted", "{count} lines" }
                }
                span { class: "spacer" }
                if admin {
                    button {
                        class: "chip",
                        disabled: count == 0,
                        onclick: move |_| pipeline.clear_log(),
                        "clear"
                    }
                }
            }
            pre { id: "pipeline-log", class: "log",
                if log.is_empty() {
                    span { class: "muted", "Nothing run yet. What the stages print appears here." }
                }
                for (index, line) in log.into_iter().enumerate() {
                    div { key: "{index}", class: line_class(&line), "{line}" }
                }
            }
        }
    }
}

/// Failures and warnings stand out of a few thousand lines of progress.
fn line_class(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("failed") || lower.contains("panic") {
        "log-line error"
    } else if lower.contains("warn") {
        "log-line warn"
    } else {
        "log-line"
    }
}

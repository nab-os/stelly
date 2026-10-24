//! The detail pages: one track, one album, one artist, each a screen of the
//! main area rather than a sheet over it.
//!
//! What you can do with a thing sits at the top, under its cover: play it,
//! like it, add it to the space, or, for a track the space holds, name it as
//! either end of a path. The generate panel beside the main area is where
//! that path, or any other walk, then shows up.

use super::generate::{Generator, PathEnd};
use super::icons;
use super::library::{ArtistRows, Library, TracksView, View, ViewToggle};
use super::menu::{menu_button, open_menu, ContextMenu, MenuTarget};
use super::player::{enqueue, play_list, Player};
use super::window::{split_window, use_rows, MoreRows};
use super::{
    album_link, artist_link, open_remote_track, row_keys, space_mark, Blocklist, Cover, LocalIds, MapView,
    Pipeline, Selection, SpaceReach,
};
use crate::api::{MixBrief, MixState, MixSummary, PipelineStatus, Progress, Stage, Target, MIN_TRACK_SECONDS};
use crate::backend::backend;
use crate::catalogue::{Credit, ReleaseKind, RemoteAlbum, RemoteArtist, RemoteTrack};
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use std::collections::HashMap;

/// Add to or remove from this user's likes, which the server keeps and Qobuz
/// never hears of. `kind` is "track", "album" or "artist". The heart flips at
/// once, the server taking its time to look the item up; only a failure is
/// worth a line of text, and puts the heart back.
pub(super) fn toggle_like(
    kind: &'static str,
    id: String,
    currently_liked: bool,
    library: Library,
    mut status: Signal<Option<String>>,
) {
    let want = !currently_liked;
    let ticket = library.flip(kind, &id, want);
    // Not tied to the page: leaving it must not drop the request, nor leave
    // the heart flipped for good.
    spawn_forever(async move {
        let result = if want {
            backend().favorite_add(kind, &id).await
        } else {
            backend().favorite_remove(kind, &id).await
        };
        library.settle(ticket, result.is_ok());
        if let Err(err) = result {
            let undone = if want { "like" } else { "unlike" };
            status.set(Some(format!("could not {undone} it: {err:#}")));
        }
    });
}

/// How much of a page's album or discography the loaded space holds.
#[derive(Clone, Copy, PartialEq)]
enum Held {
    None,
    Some,
    All,
}

impl Held {
    fn of(held: usize, total: usize) -> Self {
        match held {
            0 => Held::None,
            n if n >= total => Held::All,
            _ => Held::Some,
        }
    }
}

/// How far `target` is on its way to the map, and what is happening to it.
/// Its own analyse is most of the bar; the rebuild after is shared with
/// whatever else is being added.
fn on_its_way(status: &PipelineStatus, target: &Target) -> (f64, &'static str) {
    let step = status.progress.as_ref();
    let fraction = step.map_or(0.0, Progress::fraction);
    let doing = |name: &str| step.is_some_and(|p| p.step == name);

    if status.waiting.contains(target) {
        (0.0, "waiting its turn")
    } else if status.target.as_ref() == Some(target) {
        if doing("analyse") {
            (0.1 + 0.6 * fraction, "analysing")
        } else if doing("catalogue") {
            (0.1 * fraction, "cataloguing")
        } else {
            (0.0, "starting")
        }
    } else if status.running == Some(Stage::BuildSpace) {
        (0.7 + 0.1 * fraction, "building the space")
    } else if status.running == Some(Stage::Layout) {
        (0.8 + 0.2 * fraction, "laying out the map")
    } else {
        (0.7, "waiting for the rest to be analysed")
    }
}

/// "In your space" when it already is, "Adding…" while the server works on it,
/// otherwise the button that asks it to: catalogue, analyse, then rebuild the
/// space and the map, ahead of the rest of the backlog.
///
/// Its own component, so the status changing under a running job redraws
/// this button and not the tracklist or discography around it.
#[component]
fn AddOrBadge(
    held: Held,
    target: ReadSignal<Target>,
    title: &'static str,
    status: Signal<Option<String>>,
) -> Element {
    let mut pipeline = use_context::<Pipeline>();
    // Rounded to what the button shows, so a job creeping on does not
    // redraw it with the same number.
    let adding = use_memo(move || {
        let pipeline_status = pipeline.status.read();
        let target = target.read();
        pipeline_status.adding.contains(&target).then(|| {
            let (fraction, doing) = on_its_way(&pipeline_status, &target);
            ((fraction * 100.0).round() as u32, doing)
        })
    });

    if held == Held::All {
        return rsx! { span { class: "badge in-space", "in your space" } };
    }
    if let Some((percent, doing)) = adding() {
        return rsx! {
            button {
                class: "adding",
                style: "--progress: {percent}%",
                title: "{doing}; the pipeline output in settings has the detail",
                disabled: true,
                "Adding… {percent}%"
            }
        };
    }
    rsx! {
        button {
            title: "{title}",
            onclick: move |_| {
                let target = target();
                status.set(None);
                super::pace::kick();
                spawn(async move {
                    match backend().pipeline_add(&target).await {
                        // Ahead of the next poll, so the button does not
                        // offer the same thing twice.
                        Ok(()) => pipeline.status.write().adding.push(target),
                        Err(err) => status.set(Some(format!("{err:#}"))),
                    }
                });
            },
            if held == Held::Some { "Add the rest" } else { "Add to space" }
        }
    }
}

/// The two path ends, green for where it starts and red for where it goes.
/// Disabled for a track the space does not hold: it has no coordinates to
/// walk from or to.
#[component]
pub(crate) fn PathButtons(track: RemoteTrack, enabled: bool, compact: bool) -> Element {
    let generator = use_context::<Generator>();
    let track_id = track.id;
    if enabled {
        // So the panel can name the end even with the server out of reach.
        super::glimpse(&track);
    }
    let from = *generator.from.read();
    let to = *generator.to.read();
    let why = if enabled { "" } else { ", only for tracks in your space" };

    rsx! {
        button {
            class: if from == Some(track_id) { "path-end a set" } else { "path-end a" },
            disabled: !enabled,
            title: "start a path here{why}",
            onclick: move |event: Event<MouseData>| {
                event.stop_propagation();
                generator.set_path_end(PathEnd::A, track_id);
            },
            if compact { "A" } else { "Path A" }
        }
        button {
            class: if to == Some(track_id) { "path-end b set" } else { "path-end b" },
            disabled: !enabled,
            title: "end a path here{why}",
            onclick: move |event: Event<MouseData>| {
                event.stop_propagation();
                generator.set_path_end(PathEnd::B, track_id);
            },
            if compact { "B" } else { "Path B" }
        }
    }
}

/// One line of the facts list, skipped when there is nothing to say.
fn fact(label: &str, value: Option<String>) -> Element {
    match value.filter(|v| !v.trim().is_empty()) {
        Some(value) => rsx! {
            div { class: "fact",
                dt { "{label}" }
                dd { "{value}" }
            }
        },
        None => rsx! {},
    }
}

/// Seconds to a tenth, "1:04.9": where a downbeat or a section falls.
fn clock_tenths(seconds: f64) -> String {
    let tenths = (seconds.max(0.0) * 10.0).round() as i64;
    format!("{}:{:02}.{}", tenths / 600, tenths / 10 % 60, tenths % 10)
}

fn clock(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

// -------------------------------------------------------------------- track

/// Everything known about one track, on one screen, so it can be shared as a
/// single screenshot: cover and names up top, then every fact the catalogue
/// and the space have about it.
#[component]
pub fn TrackScreen(track: RemoteTrack) -> Element {
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let local = use_context::<LocalIds>();
    let blocklist = use_context::<Blocklist>();
    let map = use_context::<MapView>();
    let mut selection = use_context::<Selection>().0;
    let generator = use_context::<Generator>();
    let mut panel_open = generator.panel_open;
    let status = use_signal(|| None::<String>);

    library.ensure_liked_loaded();
    let liked = library.is_liked("track", &track.id.to_string());
    let in_space = local.0.read().contains(&track.id);

    // The space's own row: genre, tempo, where the crawl found it, where it
    // sits on the map. None of it travels on a Qobuz track.
    let meta = if in_space { super::space_row(track.id) } else { None };
    // The whole track analysed for mixing, once it has been: its tempo and
    // key over the space's, which are of a 90s excerpt.
    let mix = use_mix_state(track.id);
    let mixed = match &*mix.read() {
        Some(Ok(MixState::Analysed { summary })) => Some(summary.clone()),
        _ => None,
    };

    // Looking at a track the space holds makes it the one in hand, for the
    // generate panel and the map, however the page was reached.
    let seed = track.id;
    if in_space {
        super::glimpse(&track);
    }
    use_effect(move || {
        if in_space {
            selection.set(Some(seed));
        }
    });

    let quality = if track.hires { "hi-res available" } else { "CD quality" };
    let credits: Vec<String> = track
        .performers
        .as_deref()
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();

    rsx! {
        div { class: "page",
            div { class: "hero",
                Cover { url: track.image.clone(), class: "hero-art eager" }
                div { class: "hero-meta",
                    h1 { class: "hero-title", "{track.title}" }
                    {artist_link(library, track.artist_id, track.artist.clone(), "hero-line")}
                    {album_link(library, &track, "hero-line muted")}

                    div { class: "hero-actions",
                        button {
                            class: "primary",
                            disabled: !track.streamable,
                            onclick: {
                                let track = track.clone();
                                move |_| play_list(player, vec![track.clone()], 0)
                            },
                            {icons::play()}
                            "Play"
                        }
                        button {
                            disabled: !track.streamable,
                            onclick: {
                                let track = track.clone();
                                move |_| enqueue(player, vec![track.clone()])
                            },
                            "Queue"
                        }
                        button {
                            class: if liked { "icon-btn liked" } else { "icon-btn" },
                            title: if liked { "unlike" } else { "like" },
                            onclick: {
                                let id = track.id.to_string();
                                move |_| toggle_like("track", id.clone(), liked, library, status)
                            },
                            {icons::heart(liked)}
                        }
                    }
                    div { class: "hero-actions",
                        AddOrBadge {
                            held: if in_space { Held::All } else { Held::None },
                            target: Target::Track(track.id),
                            title: "analyse this track and put it on the map",
                            status,
                        }
                        PathButtons { track: track.clone(), enabled: in_space, compact: false }
                        if in_space {
                            button {
                                title: "show where it sits",
                                onclick: move |_| map.browse(),
                                {icons::constellation()}
                                "Map"
                            }
                            // Phone only: beside the main area the panel is
                            // already in view.
                            button {
                                class: "hero-generate",
                                title: "make a playlist from this track",
                                onclick: move |_| panel_open.set(true),
                                {icons::spark()}
                                "Generate"
                            }
                        }
                    }
                    if let Some(message) = status() {
                        p { class: "muted", "{message}" }
                    }
                }
            }

            div { class: "rule" }

            dl { class: "facts",
                {fact("Title", Some(track.title.clone()))}
                {fact("Artist", Some(track.artist.clone()))}
                {fact("Album", Some(track.album.clone()))}
                {fact("Duration", track.duration.filter(|d| *d > 0).map(clock))}
                {fact("Released", track.released.clone())}
                {fact("Genre", meta.as_ref().map(|m| m.genre.clone()))}
                {fact("Tempo", mixed.as_ref().and_then(|m| m.bpm).or_else(|| meta.as_ref().and_then(|m| m.bpm).map(f64::from)).map(|bpm| format!("{bpm:.0} bpm")))}
                {fact("Key", mixed.as_ref().and_then(|m| Some(format!("{} · {}", m.key.as_ref()?, m.camelot.as_ref()?))))}
                {fact("ISRC", track.isrc.clone().or_else(|| meta.as_ref().and_then(|m| m.isrc.clone())))}
                {fact("Quality", Some(quality.to_string()))}
                {fact("Streamable", Some(if track.streamable { "yes" } else { "not here" }.to_string()))}
                {fact("In your space", Some(if in_space { "yes" } else { "no" }.to_string()))}
                {fact("Crawl distance", meta.as_ref().map(|m| format!("{} hops from a like", m.seed_distance)))}
                {fact("On the map", meta.as_ref().and_then(|m| Some(format!("{:.2}, {:.2}", m.x?, m.y?))))}
                {fact("Track id", Some(track.id.to_string()))}
                {fact("Album id", track.album_id.clone())}
            }

            ForMixing { track_id: track.id, mix }

            if in_space && generator.can_steer() {
                Described { key: "{track.id}", track_id: track.id }
            }

            if !credits.is_empty() {
                h3 { class: "shelf-head", "Credits" }
                ul { class: "credits",
                    for (i, credit) in credits.into_iter().enumerate() {
                        li { key: "{i}", "{credit}" }
                    }
                }
            }

            if let Some(artist_id) = track.artist_id.filter(|_| *blocklist.editable.read()) {
                div { class: "page-foot",
                    button {
                        class: "danger",
                        onclick: {
                            let name = track.artist.clone();
                            move |_| {
                                blocklist.block.call((artist_id, name.clone()));
                                library.back();
                            }
                        },
                        "Hide {track.artist}"
                    }
                }
            }
        }
    }
}

/// Where a track stands for mixing, asked again every few seconds while it
/// waits its turn or is being analysed. The screen is keyed by the track, so
/// another track starts afresh.
fn use_mix_state(track_id: i64) -> Signal<Option<Result<MixState, String>>> {
    let mut state = use_signal(|| None);
    use_future(move || async move {
        loop {
            let fetched = backend().mix_state(track_id).await.map_err(|err| format!("{err:#}"));
            let waiting = matches!(fetched, Ok(MixState::Queued { .. } | MixState::Analysing));
            state.set(Some(fetched));
            if !waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
    state
}

/// The whole track as stelly-mix hears it: tempo and grid, key, loudness,
/// bars, phrases and sections. A track not analysed yet says where it stands,
/// with a button to have it done now.
#[component]
fn ForMixing(track_id: i64, mix: Signal<Option<Result<MixState, String>>>) -> Element {
    let analyse_now = move |_| {
        mix.set(Some(Ok(MixState::Analysing)));
        spawn(async move {
            let done = match backend().mix_analyse(track_id).await {
                Ok(()) => backend().mix_state(track_id).await,
                Err(err) => Err(err),
            };
            mix.set(Some(done.map_err(|err| format!("{err:#}"))));
        });
    };
    rsx! {
        h3 { class: "shelf-head", "For mixing" }
        match &*mix.read() {
            None => rsx! { p { class: "muted", "…" } },
            Some(Err(err)) => rsx! { p { class: "muted", "{err}" } },
            Some(Ok(MixState::Analysed { summary })) => mix_facts(summary),
            Some(Ok(MixState::Analysing)) => rsx! { p { class: "muted", "analysing the whole track…" } },
            Some(Ok(MixState::Queued { reason, ahead })) => rsx! {
                div { class: "mix-wait",
                    p { class: "muted",
                        "queued, {queued_because(reason)}"
                        if *ahead > 0 { ", {ahead} before it" }
                    }
                    button { onclick: analyse_now, "Analyse now" }
                }
            },
            Some(Ok(MixState::Failed { reason })) => rsx! {
                div { class: "mix-wait",
                    p { class: "muted", "could not be analysed: {reason}" }
                    button { onclick: analyse_now, "Try again" }
                }
            },
            Some(Ok(MixState::Unasked)) => rsx! {
                div { class: "mix-wait",
                    p { class: "muted", "not analysed yet: liking it or playing it through queues it" }
                    button { onclick: analyse_now, "Analyse now" }
                }
            },
        }
    }
}

/// "128 · 5A", or whichever of the two there is.
fn brief_label(brief: &MixBrief) -> String {
    let bpm = brief.bpm.map(|bpm| format!("{bpm:.0}"));
    [bpm, brief.camelot.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

fn queued_because(reason: &str) -> &'static str {
    match reason {
        "asked" => "asked for",
        "liked" => "liked",
        "selection" => "in a selection",
        _ => "played through",
    }
}

fn mix_facts(summary: &MixSummary) -> Element {
    let tempo = summary.bpm.map(|bpm| match (summary.steady, summary.on_grid) {
        (true, _) => format!("{bpm:.1} bpm, on a grid"),
        (false, Some(on)) => format!("{bpm:.1} bpm, drifting ({:.0}% of beats on the grid)", on * 100.0),
        (false, None) => format!("{bpm:.1} bpm, drifting"),
    });
    let key = summary.key.as_ref().map(|key| match (&summary.camelot, summary.key_strength) {
        (Some(camelot), Some(strength)) => format!("{camelot} · {key}, strength {strength:.2}"),
        (Some(camelot), None) => format!("{camelot} · {key}"),
        _ => key.clone(),
    });
    let downbeat = summary.first_downbeat.map(|at| match summary.downbeat_confidence < 0.2 {
        true => format!("{}, unsure", clock_tenths(at)),
        false => clock_tenths(at),
    });
    let shape = (summary.bars > 0).then(|| format!("{} bars, {} phrases of eight", summary.bars, summary.phrases));
    let sections = summary.sections.iter().map(|&at| clock_tenths(at)).collect::<Vec<_>>().join(", ");
    rsx! {
        dl { class: "facts",
            {fact("Tempo", tempo)}
            {fact("Key", key)}
            {fact("Loudness", summary.loudness.map(|lufs| format!("{lufs:.1} LUFS")))}
            {fact("First downbeat", downbeat)}
            {fact("Structure", shape)}
            {fact("Sections", Some(sections))}
        }
    }
}

/// The words a track sounds like, as CLAP hears it: its audio scored against
/// a vocabulary, next to how the rest of the space scores. Experimental: the
/// words are only as good as CLAP's sense of them.
#[component]
fn Described(track_id: i64) -> Element {
    let mut words = use_signal(|| None::<Result<Vec<crate::api::Word>, String>>);
    use_future(move || async move {
        words.set(Some(backend().describe(track_id).await.map_err(|err| format!("{err:#}"))));
    });

    rsx! {
        h3 { class: "shelf-head",
            "In words"
            span { class: "chip", title: "zero-shot, from the CLAP model: a guess, not a tag", "experimental" }
        }
        match &*words.read() {
            None => rsx! { p { class: "muted", "listening…" } },
            Some(Err(err)) => rsx! { p { class: "muted", "{err}" } },
            Some(Ok(found)) if found.is_empty() => rsx! {
                p { class: "muted", "nothing to go on: this track has no CLAP embedding" }
            },
            Some(Ok(found)) => rsx! {
                div { class: "moods",
                    for word in found.iter() {
                        span {
                            key: "{word.word}",
                            class: "chip",
                            title: "{word.score:+.1} spreads above the average track",
                            "{word.word}"
                        }
                    }
                }
            },
        }
    }
}

// -------------------------------------------------------------------- album

/// An album: cover and details, then its tracklist, each row playable and
/// ready to be either end of a path.
#[component]
pub fn AlbumScreen(album: RemoteAlbum) -> Element {
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let local = use_context::<LocalIds>();
    let reach = use_context::<SpaceReach>().0;
    let blocklist = use_context::<Blocklist>();
    let selection = use_context::<Selection>().0;
    let mut menu = use_context::<ContextMenu>().0;
    let status = use_signal(|| None::<String>);
    // A box set runs to hundreds of tracks.
    let window = use_rows();

    library.ensure_liked_loaded();
    let liked = library.is_liked("album", &album.id);

    // The tempo and key of the tracks analysed for mixing, asked once the
    // tracklist is in. A server from before them answers nothing.
    let briefs = use_resource(move || async move {
        let ids: Vec<i64> = library.shelf.read().tracks.iter().map(|t| t.id).collect();
        if ids.is_empty() {
            return HashMap::new();
        }
        let briefs = backend().mix_briefs(ids).await.unwrap_or_default();
        briefs.into_iter().map(|brief| (brief.track_id, brief)).collect::<HashMap<i64, MixBrief>>()
    });
    let briefs = briefs.read().clone().unwrap_or_default();

    // `LibraryPanel`'s drive fetched these for this view; the menu's
    // `ShelfTrack` indices address the same list. Read where they are, as
    // every render would otherwise copy the whole tracklist.
    let shelf = library.shelf.read().clone();
    let tracks: Vec<&RemoteTrack> = shelf.visible_tracks_iter(&blocklist).collect();
    let no_tracks = tracks.is_empty();
    let visible = window.shown().min(tracks.len());
    let more = visible < tracks.len();
    let keys = row_keys(tracks.iter().take(visible).map(|track| track.id));
    let loading = *library.loading.read();
    let now_playing = player.current().map(|t| t.id);
    let local_ids = local.0.read().clone();
    // Counted over the tracklist rather than asked of the album: one track
    // added on its own puts the album in the space, but not the rest of it.
    // Interludes too short to analyse never will be, so they do not count.
    let analysable: Vec<i64> = tracks
        .iter()
        .filter(|t| t.duration.is_none_or(|d| d >= MIN_TRACK_SECONDS))
        .map(|t| t.id)
        .collect();
    let held = if loading || analysable.is_empty() {
        if reach.read().0.contains(&album.id) { Held::All } else { Held::None }
    } else {
        Held::of(analysable.iter().filter(|id| local_ids.contains(id)).count(), analysable.len())
    };

    // An album reached from a track only knows what the track did; the
    // tracklist fills in the rest once it lands.
    let released = album
        .released
        .clone()
        .or_else(|| tracks.iter().find_map(|t| t.released.clone()));
    let total: i64 = tracks.iter().filter_map(|t| t.duration).sum();
    let count = album.tracks_count.unwrap_or(tracks.len() as i64);
    let details: Vec<String> = [
        released,
        album.genre.clone(),
        album.label.clone(),
        (count > 0).then(|| format!("{count} tracks")),
        (total > 0).then(|| clock(total)),
    ]
    .into_iter()
    .flatten()
    .collect();

    rsx! {
        div { class: "page",
            div { class: "hero",
                Cover { url: album.image.clone(), class: "hero-art eager" }
                div { class: "hero-meta",
                    h1 { class: "hero-title", "{album.title}" }
                    {artist_link(library, album.artist_id, album.artist.clone(), "hero-line")}
                    if !details.is_empty() {
                        span { class: "hero-line muted", {details.join(" · ")} }
                    }
                    div { class: "hero-actions",
                        button {
                            class: "primary",
                            disabled: no_tracks,
                            onclick: move |_| {
                                let queue = library.shelf.peek().visible_tracks(&blocklist);
                                play_list(player, queue, 0);
                            },
                            {icons::play()}
                            "Play"
                        }
                        button {
                            disabled: no_tracks,
                            onclick: move |_| {
                                let queue = library.shelf.peek().visible_tracks(&blocklist);
                                enqueue(player, queue);
                            },
                            "Queue"
                        }
                        button {
                            class: if liked { "icon-btn liked" } else { "icon-btn" },
                            title: if liked { "unlike" } else { "like" },
                            onclick: {
                                let id = album.id.clone();
                                move |_| toggle_like("album", id.clone(), liked, library, status)
                            },
                            {icons::heart(liked)}
                        }
                        AddOrBadge {
                            held,
                            target: Target::Album(album.id.clone()),
                            title: "analyse this album and put it on the map",
                            status,
                        }
                    }
                    if let Some(message) = status() {
                        p { class: "muted", "{message}" }
                    }
                }
            }

            div { class: "rule" }

            if loading {
                p { class: "muted", "loading…" }
            } else if let Some(message) = library.error.read().clone() {
                p { class: "muted error", "{message}" }
            } else {
                ol { class: "list album-tracks",
                    for ((index, track), key) in tracks.iter().copied().enumerate().take(visible).zip(keys) {
                        li {
                            key: "{key}",
                            class: if now_playing == Some(track.id) { "row playing" } else { "row" },
                            "data-track": "{track.id}",
                            onclick: {
                                let track = track.clone();
                                move |_| open_remote_track(library, selection, &local, track.clone())
                            },
                            "data-menu": MenuTarget::ShelfTrack(index).tag(),
                            oncontextmenu: move |event: Event<MouseData>| {
                                event.prevent_default();
                                open_menu(&mut menu, &event, MenuTarget::ShelfTrack(index));
                            },
                            span { class: "title",
                                "{track.title}"
                                if track.artist != album.artist {
                                    span { class: "muted", " · {track.artist}" }
                                }
                            }
                            if local_ids.contains(&track.id) {
                                span { class: "in-space-dot", title: "in your space", "•" }
                            }
                            // Empty for a track not analysed, so the column stays put.
                            span {
                                class: "muted mix-brief",
                                title: "tempo and key of the whole track",
                                {briefs.get(&track.id).map(brief_label).unwrap_or_default()}
                            }
                            span { class: "muted", "{track.duration_label()}" }
                            div { class: "row-actions",
                                button {
                                    class: "icon-btn",
                                    title: "play from here",
                                    disabled: !track.streamable,
                                    onclick: move |event: Event<MouseData>| {
                                        event.stop_propagation();
                                        let queue = library.shelf.peek().visible_tracks(&blocklist);
                                        play_list(player, queue, index);
                                    },
                                    {icons::play()}
                                }
                                PathButtons {
                                    track: track.clone(),
                                    enabled: local_ids.contains(&track.id),
                                    compact: true,
                                }
                            }
                            {menu_button(menu, MenuTarget::ShelfTrack(index))}
                        }
                    }
                }
                if more {
                    MoreRows { key: "{visible}", rows: window }
                }
            }
        }
    }
}

// ------------------------------------------------------------------- artist

/// The headings an artist's discography is split under, in page order.
const SECTIONS: [&str; 7] =
    ["Albums", "EPs", "Singles", "Live", "Compilations", "Featured on", "Appears on"];

/// Which of `SECTIONS` an album goes under. Their own releases by format,
/// anyone else's by how they are on it. An album of theirs whose format the
/// server did not say, an older server's, counts as an album.
fn section(album: &RemoteAlbum, artist_id: i64) -> usize {
    match album.credit_on(artist_id) {
        Credit::Featured => 5,
        Credit::Appears => 6,
        Credit::Main => match album.kind {
            Some(ReleaseKind::Album) | None => 0,
            Some(ReleaseKind::Ep) => 1,
            Some(ReleaseKind::Single) => 2,
            Some(ReleaseKind::Live) => 3,
            Some(ReleaseKind::Compilation) => 4,
        },
    }
}

/// A heading, its albums with their index into `visible_albums`, and their
/// row keys.
type Section = (&'static str, Vec<(usize, RemoteAlbum)>, Vec<String>);

/// The discography sorted under `SECTIONS`, empty ones dropped. Each album
/// keeps its index into `visible_albums`, which is how its menu finds it.
fn sections(albums: Vec<RemoteAlbum>, artist_id: i64) -> Vec<Section> {
    let mut grouped: [Vec<(usize, RemoteAlbum)>; 7] = Default::default();
    for (index, album) in albums.into_iter().enumerate() {
        grouped[section(&album, artist_id)].push((index, album));
    }
    SECTIONS
        .into_iter()
        .zip(grouped)
        .filter(|(_, albums)| !albums.is_empty())
        .map(|(heading, albums)| {
            let keys = row_keys(albums.iter().map(|(_, album)| &album.id));
            (heading, albums, keys)
        })
        .collect()
}

/// An artist: portrait, a biography folded to a few lines, then the
/// discography as a grid or a list, split into albums, EPs, singles and the
/// rest, then who Qobuz thinks is close.
#[component]
pub fn ArtistScreen(artist: RemoteArtist) -> Element {
    let library = use_context::<Library>();
    let reach = use_context::<SpaceReach>().0;
    let blocklist = use_context::<Blocklist>();
    let mut menu = use_context::<ContextMenu>().0;
    let status = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| false);
    let window = use_rows();

    library.ensure_liked_loaded();
    let followed = library.is_liked("artist", &artist.id.to_string());

    // Sorted and keyed once per shelf, not on every render: the follow
    // button, the bio folding out and each step of the window render again.
    let id = artist.id;
    let grouped = use_memo(move || sections(library.shelf.read().visible_albums(&blocklist), id));
    let grouped = grouped.read();
    let shelf = library.shelf.read().clone();
    // The portrait and biography, fetched with the discography and kept
    // with it. Until then, whatever the link here carried.
    let about = shelf.about.as_ref().filter(|about| about.id == artist.id);
    let image = about.and_then(|a| a.image.clone()).or_else(|| artist.image.clone());
    let biography = about.and_then(|a| a.biography.clone());
    let albums_count = about.and_then(|a| a.albums_count).or(artist.albums_count);
    // One window over the whole discography, in page order: a section
    // further down starts only once those above it are drawn in full.
    let lengths: Vec<usize> = grouped.iter().map(|(_, albums, _)| albums.len()).collect();
    let count: usize = lengths.iter().sum();
    let keys = grouped.iter().flat_map(|(_, _, keys)| keys.iter().map(String::as_str));
    let drawn = split_window(&lengths, window.shown_keeping(keys));
    let visible: usize = drawn.iter().sum();
    let sections: Vec<(&str, &[(usize, RemoteAlbum)], &[String])> = grouped
        .iter()
        .zip(drawn)
        .filter(|(_, drawn)| *drawn > 0)
        .map(|((heading, albums, keys), drawn)| (*heading, &albums[..drawn], &keys[..drawn]))
        .collect();
    let has_similar = !shelf.visible_artists(&blocklist, true).is_empty();
    let loading = *library.loading.read();
    let grid = library.layout() == TracksView::Grid;
    // Over their own releases, which is what adding them takes; the albums
    // they are only featured or credited on are other artists' to add.
    let held = {
        let reach = reach.read();
        let own: Vec<&RemoteAlbum> = shelf
            .albums
            .iter()
            .filter(|album| album.credit_on(artist.id) == Credit::Main)
            .collect();
        if loading || own.is_empty() {
            if reach.1.contains(&artist.id) { Held::Some } else { Held::None }
        } else {
            let count = own.iter().filter(|album| reach.0.contains(&album.id)).count();
            Held::of(count, own.len())
        }
    };

    rsx! {
        div { class: "page",
            div { class: "hero",
                Cover { url: image, class: "hero-art round eager" }
                div { class: "hero-meta",
                    h1 { class: "hero-title", "{artist.name}" }
                    if let Some(count) = albums_count {
                        span { class: "hero-line muted", "{count} albums" }
                    }
                    div { class: "hero-actions",
                        button {
                            class: if followed { "icon-btn liked" } else { "icon-btn" },
                            title: if followed { "unfollow" } else { "follow" },
                            onclick: {
                                let id = artist.id.to_string();
                                move |_| toggle_like("artist", id.clone(), followed, library, status)
                            },
                            {icons::heart(followed)}
                        }
                        AddOrBadge {
                            held,
                            target: Target::Artist(artist.id),
                            title: "analyse every album of theirs and put them on the map",
                            status,
                        }
                        if *blocklist.editable.read() {
                            button {
                                class: "danger",
                                onclick: {
                                    let (id, name) = (artist.id, artist.name.clone());
                                    move |_| {
                                        blocklist.block.call((id, name.clone()));
                                        library.back();
                                    }
                                },
                                "Hide"
                            }
                        }
                    }
                    if let Some(message) = status() {
                        p { class: "muted", "{message}" }
                    }
                }
            }

            // Folded by default: a biography can run to pages, and the
            // discography under it is what most visits are for.
            if let Some(biography) = biography {
                div { class: if expanded() { "bio open" } else { "bio" },
                    for (i, paragraph) in biography.lines().enumerate() {
                        p { key: "{i}", "{paragraph}" }
                    }
                }
                button {
                    class: "link bio-toggle",
                    onclick: move |_| {
                        let now = expanded();
                        expanded.set(!now);
                    },
                    if expanded() { "less" } else { "more" }
                }
            }

            div { class: "rule" }

            // The view toggle rides on the first heading, whichever that is.
            if loading || library.error.read().is_some() || sections.is_empty() {
                div { class: "shelf-head",
                    "Albums"
                    span { class: "spacer" }
                    ViewToggle {}
                }
            }

            if loading {
                p { class: "muted", "loading…" }
            } else if let Some(message) = library.error.read().clone() {
                p { class: "muted error", "{message}" }
            } else if sections.is_empty() {
                p { class: "muted", "nothing here" }
            } else {
                for (position, (heading, albums, keys)) in sections.into_iter().enumerate() {
                    div { class: "shelf-head", key: "{heading}",
                        "{heading}"
                        if position == 0 {
                            span { class: "spacer" }
                            ViewToggle {}
                        }
                    }
                    ul { class: if grid { "tiles" } else { "list" },
                        for ((index, album), key) in albums.iter().map(|(index, album)| (*index, album)).zip(keys) {
                            li {
                                key: "{key}",
                                // What a row looked at is kept in place by
                                // when the whole discography comes in.
                                "data-key": "{key}",
                                class: if grid { "tile" } else { "row" },
                                onclick: {
                                    let album = album.clone();
                                    move |_| library.go(View::Album(album.clone()))
                                },
                                "data-menu": MenuTarget::ShelfAlbum(index).tag(),
                                oncontextmenu: move |event: Event<MouseData>| {
                                    event.prevent_default();
                                    open_menu(&mut menu, &event, MenuTarget::ShelfAlbum(index));
                                },
                                Cover {
                                    url: album.image.clone(),
                                    class: if grid { String::new() } else { "thumb".to_string() },
                                }
                                if grid {
                                    {space_mark(reach.read().0.contains(&album.id))}
                                    span { class: "title", "{album.title}" }
                                    span { class: "artist", "{album.year()}" }
                                } else {
                                    span { class: "title", "{album.title}" }
                                    if reach.read().0.contains(&album.id) {
                                        span { class: "in-space-dot", title: "in your space", "•" }
                                    }
                                    if let Some(count) = album.tracks_count {
                                        span { class: "muted", "{count} tracks" }
                                    }
                                    span { class: "muted", "{album.year()}" }
                                }
                                {menu_button(menu, MenuTarget::ShelfAlbum(index))}
                            }
                        }
                    }
                }
                if visible < count {
                    MoreRows { key: "{visible}", rows: window }
                }
            }

            if has_similar {
                ArtistRows { heading: "Similar artists".to_string(), similar: true }
            }
        }
    }
}

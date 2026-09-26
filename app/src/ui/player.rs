//! The play queue and the transport.
//!
//! The `<audio>` element owns playback; see `assets/player.js`. Rust hands it
//! a URL and gets track-boundary events back.

use super::{artist_link, open_artist, Cover, Detail, MapView};
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use crate::backend::backend;
use crate::qobuz::{
    RemoteTrack, TrackIdentity, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES, FORMAT_MP3_320,
};
use std::collections::HashSet;

#[derive(Clone, Copy)]
pub struct Player {
    pub queue: Signal<Vec<RemoteTrack>>,
    pub index: Signal<usize>,
    pub playing: Signal<bool>,
    /// (position, duration) in seconds, as reported by the audio element.
    pub position: Signal<(f64, f64)>,
    pub quality: Signal<u32>,
    pub status: Signal<Option<String>>,
    /// 0.0 to 1.0, independent of the device's own volume. Rides on the
    /// `<audio>` element, which survives a src swap.
    pub volume: Signal<f64>,
    pub muted: Signal<bool>,
    /// Whether the queue drawer is showing. Lives here rather than in the
    /// shell so the player bar's toggle and the drawer share one switch.
    pub queue_open: Signal<bool>,
    /// Whether the full "now playing" screen is showing, over everything,
    /// opened from the mini-bar's art or title rather than its transport
    /// buttons, which still want a bare click to do their own thing.
    pub full_open: Signal<bool>,
}

impl Player {
    pub fn current(&self) -> Option<RemoteTrack> {
        let index = *self.index.peek();
        self.queue.peek().get(index).cloned()
    }
}

/// Push the level at the audio element. Muting sends 0 rather than setting
/// `audio.muted`, so unmuting restores the slider without a second copy.
pub fn apply_volume(player: Player) {
    let level = if *player.muted.peek() {
        0.0
    } else {
        *player.volume.peek()
    };
    document::eval(&format!(
        "window.twoKhzVolume && window.twoKhzVolume({level});"
    ));
}

pub fn quality_label(format_id: u32) -> &'static str {
    match format_id {
        FORMAT_MP3_320 => "MP3 320",
        FORMAT_FLAC_CD => "FLAC 16/44",
        FORMAT_FLAC_HIRES => "FLAC hi-res",
        _ => "unknown",
    }
}

async fn stream_url(track_id: i64, format_id: u32) -> anyhow::Result<String> {
    backend().file_url(track_id, format_id).await
}

/// Whether a queued track belongs to a hidden artist. Asked of the loaded
/// space, not the database: a remote client has none, and the engine's copy is
/// the one kept in step.
fn is_hidden(artist_id: i64) -> bool {
    crate::engine()
        .lock()
        .unwrap()
        .navigator
        .catalog
        .blocked_artists
        .contains(&artist_id)
}

/// Fire a transport command at the audio element. Guarded because the first
/// click can land before player.js has finished installing its handles.
fn transport(command: &str) {
    document::eval(&format!("window.{command} && window.{command}();"));
}

/// Drop tracks already spoken for, keeping the first of each.
///
/// A queue never holds the same recording twice, so every path into it comes
/// through here. `seen` carries whatever the queue already holds, so a list
/// being appended is filtered both against the queue and against itself.
fn unduplicated(tracks: Vec<RemoteTrack>, seen: &mut HashSet<TrackIdentity>) -> Vec<RemoteTrack> {
    tracks
        .into_iter()
        .filter(|track| seen.insert(track.identity()))
        .collect()
}

/// The identities a queue currently holds.
fn identities(tracks: &[RemoteTrack]) -> HashSet<TrackIdentity> {
    tracks.iter().map(|track| track.identity()).collect()
}

/// Replace the queue and start at `index`.
///
/// `index` refers to the caller's list, which may lose entries on the way in;
/// the track it pointed at is looked up again afterwards so that clicking the
/// third row still plays the third row, not whatever slid into its place.
pub fn play_list(mut player: Player, queue: Vec<RemoteTrack>, index: usize) {
    let wanted = queue.get(index).map(|track| track.identity());
    let queue = unduplicated(queue, &mut HashSet::new());

    let start = wanted
        .and_then(|id| queue.iter().position(|track| track.identity() == id))
        .unwrap_or(0);

    player.queue.set(queue);
    spawn_forever(async move { play_at(player, start).await });
}

/// Append to the queue, starting playback if nothing is going.
pub fn enqueue(mut player: Player, tracks: Vec<RemoteTrack>) {
    let was_empty = player.queue.peek().is_empty();
    let start = player.queue.peek().len();

    let tracks = unduplicated(tracks, &mut identities(&player.queue.peek()));
    if tracks.is_empty() {
        return;
    }

    player.queue.write().extend(tracks);
    if was_empty {
        spawn_forever(async move { play_at(player, start).await });
    }
}

/// Insert directly after whatever is playing, so these come next and the rest
/// of the queue still follows. With nothing playing this is `enqueue`.
pub fn play_next(mut player: Player, tracks: Vec<RemoteTrack>) {
    let tracks = unduplicated(tracks, &mut identities(&player.queue.peek()));
    if tracks.is_empty() {
        return;
    }

    // Both reads finish before the write guard is taken; holding it across a
    // `peek` of the same signal would deadlock.
    let length = player.queue.peek().len();
    let at = if length == 0 {
        0
    } else {
        (*player.index.peek() + 1).min(length)
    };

    {
        let mut queue = player.queue.write();
        for (offset, track) in tracks.into_iter().enumerate() {
            queue.insert(at + offset, track);
        }
    }

    if length == 0 {
        spawn_forever(async move { play_at(player, 0).await });
    }
}

/// Empty the queue and stop. Here rather than in the drawer that offers it,
/// because `transport` is private to this module.
pub fn clear_queue(mut player: Player) {
    player.queue.set(Vec::new());
    player.index.set(0);
    player.position.set((0.0, 0.0));
    player.playing.set(false);
    transport("twoKhzStop");
}

/// Drop one entry, keeping `index` on whatever is playing.
///
/// Removing the playing track hands over to whatever slides into its place.
/// When nothing does, it was last, playback stops and `index` stays on the
/// track before it, which is where reaching the end of a queue leaves it too.
pub fn remove_at(mut player: Player, at: usize) {
    let length = player.queue.peek().len();
    if at >= length {
        return;
    }

    let current = *player.index.peek();
    player.queue.write().remove(at);

    if at < current {
        // Everything below it shifted up by one, the playing track included.
        player.index.set(current - 1);
    } else if at == current {
        if at < length - 1 {
            spawn_forever(async move { play_at(player, at).await });
        } else {
            player.index.set(at.saturating_sub(1));
            player.position.set((0.0, 0.0));
            player.playing.set(false);
            transport("twoKhzStop");
        }
    }
}

/// Where the entry at `index` ends up once the one at `from` is taken out and
/// put back at `to`.
///
/// Pure, and separate from `move_to`, because this is the whole of what a
/// reorder can get wrong: the queue is only a `Vec`, but `index` has to keep
/// pointing at the track that is actually playing.
fn index_after_move(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        // The moved entry itself.
        to
    } else if from < index && to >= index {
        // Removed from above it and put back at or below it: it rises one.
        index - 1
    } else if from > index && to <= index {
        // Removed from below it and put back at or above it: it sinks one.
        index + 1
    } else {
        // The move happened entirely on one side of it.
        index
    }
}

/// Move one entry to another position, following it with `index` if it was the
/// one playing. This is what a drag reports; see `queue-drag.js`.
pub fn move_to(mut player: Player, from: usize, to: usize) {
    let length = player.queue.peek().len();
    if from >= length || to >= length || from == to {
        return;
    }

    let current = *player.index.peek();
    {
        let mut queue = player.queue.write();
        let track = queue.remove(from);
        queue.insert(to, track);
    }
    player.index.set(index_after_move(current, from, to));
}

/// Shift one entry by `delta` places, following it with `index` if it was the
/// one playing. Out-of-range moves are ignored, so the ends simply do nothing.
pub fn move_by(mut player: Player, at: usize, delta: i64) {
    let length = player.queue.peek().len();
    let target = at as i64 + delta;
    if at >= length || target < 0 || target as usize >= length {
        return;
    }
    let target = target as usize;

    player.queue.write().swap(at, target);

    let current = *player.index.peek();
    if current == at {
        player.index.set(target);
    } else if current == target {
        player.index.set(at);
    }
}

pub async fn play_at(mut player: Player, index: usize) {
    // Scoped so the read guard is gone before the first await.
    let track = {
        let queue = player.queue.peek();
        queue.get(index).cloned()
    };
    let Some(track) = track else { return };

    player.index.set(index);
    player.position.set((0.0, track.duration.unwrap_or(0) as f64));

    if !track.streamable {
        player
            .status
            .set(Some(format!("“{}” is not streamable here", track.title)));
        return;
    }

    // A queue built before a block still holds the hidden artist's tracks.
    if track.artist_id.is_some_and(is_hidden) {
        player.status.set(Some(format!("skipped {}", track.artist)));
        step(player, 1);
        return;
    }

    player.status.set(Some(format!("loading {}…", track.title)));
    apply_volume(player);
    let format_id = *player.quality.peek();

    match stream_url(track.id, format_id).await {
        Ok(url) => {
            player.status.set(None);
            // The signed URL is short-lived and this is the app's own webview,
            // so there is nothing to proxy it away from.
            document::eval(&format!(
                "(window.twoKhzPlayUrl || function (u) {{ \
                   var p = document.getElementById('player'); \
                   if (p) {{ p.src = u; p.play(); }} \
                 }})({});",
                serde_json::to_string(&url).unwrap_or_else(|_| "''".into())
            ));
            // What the OS shows as "now playing", see `twoKhzSetMetadata`'s
            // doc comment in player.js for why this is the same fix as the
            // hardware/mouse previous-next buttons.
            document::eval(&format!(
                "window.twoKhzSetMetadata && window.twoKhzSetMetadata({}, {}, {}, {});",
                serde_json::to_string(&track.title).unwrap_or_else(|_| "''".into()),
                serde_json::to_string(&track.artist).unwrap_or_else(|_| "''".into()),
                serde_json::to_string(&track.album).unwrap_or_else(|_| "''".into()),
                serde_json::to_string(&track.image).unwrap_or_else(|_| "null".into()),
            ));
        }
        Err(err) => player.status.set(Some(format!("{err:#}"))),
    }
}

/// Step through the queue. Stops at the ends rather than wrapping, so a
/// finished queue stays finished.
pub fn step(player: Player, delta: isize) {
    let length = player.queue.peek().len() as isize;
    if length == 0 {
        return;
    }
    let next = *player.index.peek() as isize + delta;
    if next < 0 || next >= length {
        return;
    }
    spawn_forever(async move { play_at(player, next as usize).await });
}

fn toggle(player: Player) {
    if *player.playing.peek() {
        transport("twoKhzPause");
    } else if player.position.peek().1 > 0.0 || *player.index.peek() > 0 {
        transport("twoKhzResume");
    } else if !player.queue.peek().is_empty() {
        // Queued but never started.
        let start = *player.index.peek();
        spawn_forever(async move { play_at(player, start).await });
    }
}

fn clock(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0:00".into();
    }
    let total = seconds as i64;
    format!("{}:{:02}", total / 60, total % 60)
}


#[component]
pub fn PlayerBar() -> Element {
    let mut player = use_context::<Player>();

    let current = player.current();
    let (position, duration) = *player.position.read();
    let queue_length = player.queue.read().len();
    let index = *player.index.read();
    let playing = *player.playing.read();
    let quality = *player.quality.read();
    let volume = *player.volume.read();
    let muted = *player.muted.read();
    let mut queue_open = player.queue_open;
    let mut full_open = player.full_open;
    let map = use_context::<MapView>();
    let detail = use_context::<Detail>().0;

    // Only worth a screen of its own with something on it. `.now` stays
    // clickable-looking either way, the disabled state is silent rather
    // than a dead cursor, since "nothing playing" already says why.
    let has_current = current.is_some();

    // The audio element's own duration is authoritative once it has loaded;
    // Qobuz's metadata fills the gap before that.
    let total = if duration > 0.0 {
        duration
    } else {
        current.as_ref().and_then(|t| t.duration).unwrap_or(0) as f64
    };
    let progress = if total > 0.0 {
        (position / total * 1000.0).clamp(0.0, 1000.0)
    } else {
        0.0
    };

    rsx! {
        footer { class: "player",
            // Hidden: the transport below drives it, and the native controls
            // would duplicate every button.
            audio { id: "player" }

            // Cover and title share the "now" grid area, side by side,
            // next to the thing it names, and shorter for it: two stacked
            // lines fit in less width than one long one did.
            div { class: "now",
                // Eager: the player bar is always on screen, and there is
                // exactly one of these. Waiting for an intersection would
                // only delay it. The art is the opener for the full "now
                // playing" screen, the transport buttons stay bare clicks
                // that do their own thing, which is why this is not on the
                // whole `footer`.
                button {
                    class: "now-art-button",
                    disabled: !has_current,
                    title: "now playing",
                    onclick: move |_| full_open.set(true),
                    Cover {
                        url: current.as_ref().and_then(|track| track.image.clone()),
                        class: "now-art eager",
                    }
                }
                if let Some(track) = current.clone() {
                    div {
                        class: "now-title",
                        onclick: move |_| full_open.set(true),
                        span { class: "title", "{track.title}" }
                        {artist_link(detail, track.artist_id, track.artist.clone(), "muted")}
                    }
                } else {
                    div { class: "now-title muted", "nothing playing" }
                }
            }

            div { class: "transport",
                button {
                    disabled: index == 0 || queue_length == 0,
                    onclick: move |_| step(player, -1),
                    "⏮"
                }
                button {
                    class: "play",
                    disabled: queue_length == 0,
                    onclick: move |_| toggle(player),
                    if playing { "⏸" } else { "▶" }
                }
                button {
                    disabled: index + 1 >= queue_length,
                    onclick: move |_| step(player, 1),
                    "⏭"
                }
            }

            div { class: "seek",
                span { class: "muted", "{clock(position)}" }
                input {
                    r#type: "range",
                    min: "0",
                    max: "1000",
                    value: "{progress}",
                    disabled: total <= 0.0,
                    oninput: move |event| {
                        if let Ok(value) = event.value().parse::<f64>() {
                            document::eval(&format!(
                                "window.twoKhzSeek && window.twoKhzSeek({});",
                                value / 1000.0
                            ));
                        }
                    },
                }
                span { class: "muted", "{clock(total)}" }
            }

            div { class: "player-meta",
                div { class: "volume",
                    button {
                        class: "chip",
                        title: if muted { "unmute" } else { "mute" },
                        onclick: move |_| {
                            let next = !*player.muted.peek();
                            player.muted.set(next);
                            apply_volume(player);
                        },
                        if muted || volume <= 0.0 { "🔇" } else { "🔊" }
                    }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        step: "1",
                        value: "{(volume * 100.0).round() as i64}",
                        oninput: move |event| {
                            if let Ok(percent) = event.value().parse::<f64>() {
                                player.volume.set((percent / 100.0).clamp(0.0, 1.0));
                                // Dragging the slider is an unambiguous request
                                // to hear something.
                                if percent > 0.0 {
                                    player.muted.set(false);
                                }
                                apply_volume(player);
                            }
                        },
                    }
                }
                select {
                    onchange: move |event| {
                        if let Ok(value) = event.value().parse::<u32>() {
                            player.quality.set(value);
                        }
                    },
                    // `selected` per option rather than `value` on the select:
                    // the latter does not mark an option chosen in the webview.
                    for format_id in [FORMAT_MP3_320, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES] {
                        option {
                            key: "{format_id}",
                            value: "{format_id}",
                            selected: quality == format_id,
                            "{quality_label(format_id)}"
                        }
                    }
                }
                // The map's entry point in the primary chrome: not the header,
                // which competes with search and settings for room a session
                // touches every time, and not a place that stands empty for
                // 82% of a corpus that is not on it yet, reached from
                // wherever playback already is instead.
                button {
                    class: if *map.map_open.read() { "chip nav-btn active" } else { "chip nav-btn" },
                    title: "browse the space as a map",
                    onclick: move |_| map.toggle(),
                    "map"
                }
                // Last on the line, and the way into the drawer that holds
                // everything else the queue can do, clearing included.
                button {
                    class: if queue_open() { "chip active" } else { "chip" },
                    title: "show the tracklist",
                    disabled: queue_length == 0,
                    onclick: move |_| {
                        let next = !queue_open();
                        queue_open.set(next);
                    },
                    if queue_length > 0 {
                        "{index + 1}/{queue_length}"
                    } else {
                        "queue"
                    }
                }
            }

            if let Some(message) = player.status.read().clone() {
                div { class: "player-status muted", "{message}" }
            }
        }
    }
}

/// The mini-bar blown up to a screen of its own, opened from its art or
/// title, at every width, rather than only existing as a phone concession.
/// Desktop gets the same benefit a "now playing" window always has: art big
/// enough to look at, rather than a 48px thumbnail.
///
/// Queue and map both close this on the way to opening themselves, rather
/// than stacking overlay on overlay, the queue drawer and the map already
/// have their own places to be, and this is not trying to hold either of
/// them itself.
#[component]
pub fn FullPlayer() -> Element {
    let mut player = use_context::<Player>();
    let map = use_context::<MapView>();
    let detail = use_context::<Detail>().0;

    let mut full_open = player.full_open;
    if !full_open() {
        return rsx! {};
    }

    let Some(current) = player.current() else {
        // Closed itself: nothing plays while this was open, which the queue
        // running out is the one way to reach.
        full_open.set(false);
        return rsx! {};
    };

    let (position, duration) = *player.position.read();
    let queue_length = player.queue.read().len();
    let index = *player.index.read();
    let playing = *player.playing.read();
    let quality = *player.quality.read();
    let volume = *player.volume.read();
    let muted = *player.muted.read();

    let total = if duration > 0.0 {
        duration
    } else {
        current.duration.unwrap_or(0) as f64
    };
    let progress = if total > 0.0 {
        (position / total * 1000.0).clamp(0.0, 1000.0)
    } else {
        0.0
    };

    let close = move |_| full_open.set(false);

    rsx! {
        div { class: "full-player-backdrop", onclick: close }
        section { class: "panel full-player",
            div { class: "sheet-head",
                span { class: "spacer" }
                button { class: "chip sheet-close", onclick: close, "×" }
            }

            Cover { url: current.image.clone(), class: "full-art eager" }

            div { class: "full-meta",
                span { class: "detail-title ellipsis", "{current.title}" }
                // Not `artist_link`: this sits above the sheet it would open,
                // so it has to close itself on the way, or the artist's sheet
                // opens underneath and the click looks like it did nothing.
                if let Some(artist_id) = current.artist_id {
                    span {
                        class: "muted ellipsis clickable",
                        title: "open {current.artist}",
                        onclick: {
                            let name = current.artist.clone();
                            move |_| {
                                full_open.set(false);
                                open_artist(detail, artist_id, name.clone());
                            }
                        },
                        "{current.artist}"
                    }
                } else {
                    span { class: "muted ellipsis", "{current.artist}" }
                }
            }

            div { class: "seek full-seek",
                span { class: "muted", "{clock(position)}" }
                input {
                    r#type: "range",
                    min: "0",
                    max: "1000",
                    value: "{progress}",
                    disabled: total <= 0.0,
                    oninput: move |event| {
                        if let Ok(value) = event.value().parse::<f64>() {
                            document::eval(&format!(
                                "window.twoKhzSeek && window.twoKhzSeek({});",
                                value / 1000.0
                            ));
                        }
                    },
                }
                span { class: "muted", "{clock(total)}" }
            }

            div { class: "transport full-transport",
                button {
                    disabled: index == 0 || queue_length == 0,
                    onclick: move |_| step(player, -1),
                    "⏮"
                }
                button {
                    class: "play",
                    disabled: queue_length == 0,
                    onclick: move |_| toggle(player),
                    if playing { "⏸" } else { "▶" }
                }
                button {
                    disabled: index + 1 >= queue_length,
                    onclick: move |_| step(player, 1),
                    "⏭"
                }
            }

            div { class: "player-meta",
                div { class: "volume",
                    button {
                        class: "chip",
                        title: if muted { "unmute" } else { "mute" },
                        onclick: move |_| {
                            let next = !*player.muted.peek();
                            player.muted.set(next);
                            apply_volume(player);
                        },
                        if muted || volume <= 0.0 { "🔇" } else { "🔊" }
                    }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        step: "1",
                        value: "{(volume * 100.0).round() as i64}",
                        oninput: move |event| {
                            if let Ok(percent) = event.value().parse::<f64>() {
                                player.volume.set((percent / 100.0).clamp(0.0, 1.0));
                                if percent > 0.0 {
                                    player.muted.set(false);
                                }
                                apply_volume(player);
                            }
                        },
                    }
                }
                select {
                    onchange: move |event| {
                        if let Ok(value) = event.value().parse::<u32>() {
                            player.quality.set(value);
                        }
                    },
                    for format_id in [FORMAT_MP3_320, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES] {
                        option {
                            key: "{format_id}",
                            value: "{format_id}",
                            selected: quality == format_id,
                            "{quality_label(format_id)}"
                        }
                    }
                }
            }

            div { class: "actions",
                button {
                    class: if *player.queue_open.read() { "chip active" } else { "chip" },
                    disabled: queue_length == 0,
                    onclick: move |_| {
                        full_open.set(false);
                        let mut queue_open = player.queue_open;
                        queue_open.set(true);
                    },
                    if queue_length > 0 { "queue {index + 1}/{queue_length}" } else { "queue" }
                }
                button {
                    class: "chip nav-btn",
                    onclick: move |_| {
                        full_open.set(false);
                        map.browse();
                    },
                    "on the map"
                }
            }

            if let Some(message) = player.status.read().clone() {
                p { class: "muted ellipsis", "{message}" }
            }
        }
    }
}

/// Long-lived transport channel: installs player.js and folds its events back
/// into the player signals. Auto-advance lives here.
pub fn use_transport(player: Player) {
    use_future(move || async move {
        let mut player = player;
        let mut handle = document::eval(include_str!("../../assets/player.js"));

        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            match message.get("type").and_then(|v| v.as_str()) {
                Some("time") => {
                    let position = message
                        .get("position")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let duration = message
                        .get("duration")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    player.position.set((position, duration));
                }
                Some("playing") => {
                    let is_playing = message
                        .get("playing")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    player.playing.set(is_playing);
                }
                Some("ended") => {
                    let next = *player.index.peek() + 1;
                    if next < player.queue.peek().len() {
                        play_at(player, next).await;
                    } else {
                        player.playing.set(false);
                    }
                }
                Some("failed") => {
                    // Only report it if we were not the ones who cleared the src.
                    if !player.queue.peek().is_empty() {
                        player
                            .status
                            .set(Some("playback failed, the stream URL may have expired".into()));
                    }
                }
                // A mouse's side buttons and a keyboard's media keys both
                // arrive as `previoustrack`/`nexttrack` MediaSession actions,
                // not as clicks on the transport bar, see `player.js`.
                // They mean "change track", which only the queue here
                // knows how to do, so this is the one message type that
                // does not just mirror the `<audio>` element's own state.
                Some("transport") => match message.get("action").and_then(|v| v.as_str()) {
                    Some("previous") => step(player, -1),
                    Some("next") => step(player, 1),
                    _ => {}
                },
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::index_after_move;

    /// The oracle: actually reorder a list and look up where the marked entry
    /// went. `index_after_move` has to agree with this for every move, and
    /// checking against a real `Vec` beats restating the arithmetic.
    fn reorder_and_find(length: usize, index: usize, from: usize, to: usize) -> usize {
        let mut queue: Vec<usize> = (0..length).collect();
        let moved = queue.remove(from);
        queue.insert(to, moved);
        queue.iter().position(|&entry| entry == index).unwrap()
    }

    #[test]
    fn agrees_with_an_actual_reorder_for_every_move() {
        const LENGTH: usize = 7;
        for index in 0..LENGTH {
            for from in 0..LENGTH {
                for to in 0..LENGTH {
                    assert_eq!(
                        index_after_move(index, from, to),
                        reorder_and_find(LENGTH, index, from, to),
                        "index {index} after moving {from} -> {to}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_dragged_entry_lands_where_it_was_dropped() {
        assert_eq!(index_after_move(3, 3, 0), 0);
        assert_eq!(index_after_move(3, 3, 6), 6);
    }

    #[test]
    fn a_move_on_one_side_leaves_the_playing_track_alone() {
        // Both ends below it.
        assert_eq!(index_after_move(5, 1, 3), 5);
        // Both ends above it.
        assert_eq!(index_after_move(5, 7, 9), 5);
    }

    #[test]
    fn dragging_past_the_playing_track_shifts_it_by_one() {
        // From above it to below it: it rises.
        assert_eq!(index_after_move(5, 2, 8), 4);
        // From below it to above it: it sinks.
        assert_eq!(index_after_move(5, 8, 2), 6);
    }
}

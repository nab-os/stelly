//! The play queue and the transport.
//!
//! The `<audio>` element owns playback; see `assets/player.js`. Rust hands it
//! a URL and gets track-boundary events back.

use super::Cover;
use dioxus::prelude::*;
use crate::backend::backend;
use crate::qobuz::{RemoteTrack, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES, FORMAT_MP3_320};

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

/// Replace the queue and start at `index`.
pub fn play_list(mut player: Player, queue: Vec<RemoteTrack>, index: usize) {
    player.queue.set(queue);
    spawn(async move { play_at(player, index).await });
}

/// Append to the queue, starting playback if nothing is going.
pub fn enqueue(mut player: Player, tracks: Vec<RemoteTrack>) {
    let was_empty = player.queue.peek().is_empty();
    let start = player.queue.peek().len();
    player.queue.write().extend(tracks);
    if was_empty {
        spawn(async move { play_at(player, start).await });
    }
}

/// Insert directly after whatever is playing, so these come next and the rest
/// of the queue still follows. With nothing playing this is `enqueue`.
pub fn play_next(mut player: Player, tracks: Vec<RemoteTrack>) {
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
        spawn(async move { play_at(player, 0).await });
    }
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
            spawn(async move { play_at(player, at).await });
        } else {
            player.index.set(at.saturating_sub(1));
            player.position.set((0.0, 0.0));
            player.playing.set(false);
            transport("twoKhzStop");
        }
    }
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
    spawn(async move { play_at(player, next as usize).await });
}

fn toggle(player: Player) {
    if *player.playing.peek() {
        transport("twoKhzPause");
    } else if player.position.peek().1 > 0.0 || *player.index.peek() > 0 {
        transport("twoKhzResume");
    } else if !player.queue.peek().is_empty() {
        // Queued but never started.
        let start = *player.index.peek();
        spawn(async move { play_at(player, start).await });
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

            Cover {
                url: current.as_ref().and_then(|track| track.image.clone()),
                class: "now-art",
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

            div { class: "now",
                if let Some(track) = current.clone() {
                    div { class: "now-title",
                        span { class: "title", "{track.title}" }
                        span { class: "muted", " - {track.artist}" }
                    }
                } else {
                    div { class: "now-title muted", "nothing playing" }
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
            }

            div { class: "player-meta",
                if queue_length > 0 {
                    button {
                        class: if queue_open() { "chip active" } else { "chip" },
                        title: "show the queue",
                        onclick: move |_| {
                            let next = !queue_open();
                            queue_open.set(next);
                        },
                        "{index + 1}/{queue_length}"
                    }
                }

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
                button {
                    disabled: queue_length == 0,
                    onclick: move |_| {
                        player.queue.set(Vec::new());
                        player.index.set(0);
                        player.position.set((0.0, 0.0));
                        transport("twoKhzStop");
                    },
                    "clear"
                }
            }

            if let Some(message) = player.status.read().clone() {
                div { class: "player-status muted", "{message}" }
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
                _ => {}
            }
        }
    });
}

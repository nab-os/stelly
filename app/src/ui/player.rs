//! The play queue and the transport.
//!
//! The queue lives in the session every paired device shares, see
//! `crate::session`. Every button here turns into an `Op`, applied to this
//! device's copy at once and sent on to the server, which pushes the result
//! to every device. Only the device picked as the output makes a sound: its
//! `<audio>` element (see `assets/player.js`) follows the session, and reports
//! back what it is actually doing.

use super::{album_link, artist_link, icons, open_remote_track, Cover, Library, LocalIds, Selection};
use crate::backend::{backend, SessionFeed};
use crate::qobuz::{RemoteTrack, FORMAT_FLAC_CD, FORMAT_FLAC_HIRES, FORMAT_MP3_320};
use crate::session::{Clocked, Command, Op, Output, Session, Update};
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Player {
    pub queue: Signal<Vec<RemoteTrack>>,
    pub index: Signal<usize>,
    pub playing: Signal<bool>,
    /// (position, duration) in seconds. From the audio element on the output,
    /// carried forward from the session's last word everywhere else, where
    /// the duration is left at 0 for the track's own to fill in.
    pub position: Signal<(f64, f64)>,
    pub quality: Signal<u32>,
    pub status: Signal<Option<String>>,
    /// 0.0 to 1.0, independent of the device's own volume. Rides on the
    /// `<audio>` element, which survives a src swap. Per device, not shared:
    /// it is this device's speakers.
    pub volume: Signal<f64>,
    pub muted: Signal<bool>,
    /// Whether the queue drawer is showing. Lives here rather than in the
    /// shell so the player bar's toggle and the drawer share one switch.
    pub queue_open: Signal<bool>,
    /// The devices following the session, for the output picker.
    pub devices: Signal<Vec<Output>>,
    /// The device the sound comes out of.
    pub output: Signal<Option<i64>>,
    /// This device, as the server knows it. `None` until the session is
    /// joined, and for good against a server that has none.
    pub me: Signal<Option<i64>>,
}

impl Player {
    pub fn current(&self) -> Option<RemoteTrack> {
        let index = *self.index.peek();
        self.queue.peek().get(index).cloned()
    }
}

// ------------------------------------------------------------- the mirror

/// Who this device is before it has joined a session, or when the server has
/// none. Device ids start at 1, so it cannot be mistaken for one.
const SOLO: i64 = 0;

/// How often the output tells the others where it is, so a position carried
/// forward on another screen does not drift far from the one being heard.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// This device's copy of the session, and what its audio element is doing.
///
/// A global rather than a signal: the audio channel, the session stream and
/// the buttons all write it, and none of it should re-render anything until
/// `show` has decided what changed.
struct Mirror {
    clocked: Clocked,
    me: i64,
    /// Whether changes go to a server. Off until the session is joined, and
    /// for good against a server that has none.
    shared: bool,
    /// Commands sent and not yet answered. While there are any, this copy
    /// may be ahead of what the server has pushed.
    pending: usize,
    /// The cue and track the audio element was last pointed at. `None` when
    /// it holds nothing.
    loaded: Option<(u64, i64)>,
    /// Set from handing the element a new track until it has started, or
    /// given up. The element's own events in between are about the swap.
    loading: Option<u64>,
    tickets: u64,
    /// What the element last said about itself.
    sounding: bool,
    heard: f64,
    reported: Option<Instant>,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            clocked: Clocked::default(),
            me: SOLO,
            shared: false,
            pending: 0,
            loaded: None,
            loading: None,
            tickets: 0,
            sounding: false,
            heard: 0.0,
            reported: None,
        }
    }
}

impl Mirror {
    fn session(&self) -> &Session {
        &self.clocked.session
    }

    fn is_output(&self) -> bool {
        self.session().output == Some(self.me)
    }
}

static MIRROR: LazyLock<Mutex<Mirror>> = LazyLock::new(Default::default);

/// Where commands wait to be sent, one at a time, so they reach the server in
/// the order they were made.
static OUTBOX: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Command>>> = Mutex::new(None);

fn mirror() -> MutexGuard<'static, Mirror> {
    MIRROR.lock().unwrap_or_else(|e| e.into_inner())
}

/// Make a change: to this copy now, to the shared one as soon as it arrives.
fn commit(player: Player, op: Op) {
    let command = {
        let mut mirror = mirror();
        let queue_version = mirror.session().queue_version;
        let me = mirror.me;
        // A copy cannot be stale against itself.
        let _ = mirror.clocked.apply(op.clone(), queue_version, me);
        if mirror.shared {
            mirror.pending += 1;
            Some(Command { queue_version, op })
        } else {
            None
        }
    };

    if let Some(command) = command {
        let sent = OUTBOX
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|outbox| outbox.send(command).is_ok());
        if !sent {
            let mut mirror = mirror();
            mirror.pending = mirror.pending.saturating_sub(1);
        }
    }
    show(player);
}

/// Take what the server pushed.
fn receive(player: Player, update: Update) {
    let mut resync = false;
    {
        let mut mirror = mirror();
        mirror.me = update.you;
        mirror.shared = true;

        let mut session = update.session;
        // With commands still in flight this copy is ahead, and an older
        // push would only undo them for a moment.
        if mirror.pending > 0 && session.version < mirror.session().version {
            return;
        }
        if update.same_queue {
            // The stream thinks this copy has a queue it does not: an update
            // that carried it was skipped above, or this copy has moved on
            // on its own. Either way only a full fetch can say which.
            if session.queue_version != mirror.session().queue_version {
                resync = true;
            }
            session.queue = mirror.session().queue.clone();
        }
        mirror.clocked = Clocked::new(session);
    }
    show(player);
    if resync {
        spawn_forever(catch_up(player));
    }
}

/// Fetch the whole session, after a refusal or a push that could not be
/// placed.
async fn catch_up(player: Player) {
    match backend().session().await {
        Ok(session) => {
            let you = mirror().me;
            receive(
                player,
                Update {
                    you,
                    same_queue: false,
                    session,
                },
            );
        }
        Err(err) => {
            let mut status = player.status;
            status.set(Some(format!("{err:#}")));
        }
    }
}

fn set_if_changed<T: PartialEq + 'static>(mut signal: Signal<T>, value: T) {
    if *signal.peek() != value {
        signal.set(value);
    }
}

/// Bring the signals, and the audio element, into line with the mirror.
fn show(mut player: Player) {
    let (session, position, me, shared) = {
        let mirror = mirror();
        (
            mirror.session().clone(),
            mirror.clocked.position(),
            mirror.me,
            mirror.shared,
        )
    };

    set_if_changed(player.queue, session.queue.clone());
    set_if_changed(player.index, session.index);
    set_if_changed(player.playing, session.playing);
    set_if_changed(player.output, session.output);
    set_if_changed(player.devices, session.devices.clone());
    set_if_changed(player.me, shared.then_some(me));

    if session.output != Some(me) {
        player.position.set((position, 0.0));
    }
    follow(player);
}

// ------------------------------------------------------------- the output

/// Point the audio element at whatever the session says, when this device is
/// the output, and silence it when it is not.
fn follow(player: Player) {
    let mut mirror = mirror();
    let current = mirror.session().current().cloned();

    let Some(track) = current.filter(|_| mirror.is_output()) else {
        if mirror.loaded.take().is_some() {
            mirror.loading = None;
            mirror.sounding = false;
            drop(mirror);
            transport("stellyStop");
            announce();
        }
        return;
    };

    let cue = mirror.session().cue;
    let index = mirror.session().index;
    match mirror.loaded {
        Some((seen, id)) if id == track.id => {
            if seen != cue {
                mirror.loaded = Some((cue, id));
                // Still loading, it starts from the latest position anyway.
                if mirror.loading.is_none() {
                    let position = mirror.clocked.position();
                    document::eval(&format!(
                        "window.stellySeekTo && window.stellySeekTo({position});"
                    ));
                }
            }
        }
        _ => {
            mirror.loaded = Some((cue, track.id));
            mirror.tickets += 1;
            let ticket = mirror.tickets;
            mirror.loading = Some(ticket);
            drop(mirror);
            spawn_forever(load(player, track, index, ticket));
            return;
        }
    }

    if mirror.loading.is_none() {
        let playing = mirror.session().playing;
        if playing && !mirror.sounding {
            transport("stellyResume");
        } else if !playing && mirror.sounding {
            transport("stellyPause");
        }
    }
}

/// Hand the audio element a track, starting from wherever the session is by
/// the time its URL is signed.
async fn load(mut player: Player, track: RemoteTrack, index: usize, ticket: u64) {
    let give_up = |mut player: Player, message: String| {
        let mut mirror = mirror();
        if mirror.loading == Some(ticket) {
            mirror.loading = None;
        }
        drop(mirror);
        player.status.set(Some(message));
    };

    if !track.streamable {
        give_up(player, format!("“{}” is not streamable here", track.title));
        return;
    }

    // A queue built before a block still holds the hidden artist's tracks.
    if track.artist_id.is_some_and(is_hidden) {
        give_up(player, format!("skipped {}", track.artist));
        commit(player, Op::Ended { index });
        return;
    }

    player.status.set(Some(format!("loading {}…", track.title)));
    player.position.set((0.0, track.duration.unwrap_or(0) as f64));
    apply_volume(player);
    let format_id = *player.quality.peek();

    let url = match stream_url(track.id, format_id).await {
        Ok(url) => url,
        Err(err) => {
            give_up(player, format!("{err:#}"));
            return;
        }
    };

    // The session may have moved on while the URL was being signed.
    let (start, autoplay) = {
        let mirror = mirror();
        if mirror.loading != Some(ticket) {
            return;
        }
        (mirror.clocked.position(), mirror.session().playing)
    };

    player.status.set(None);
    // The signed URL is short-lived and this is the app's own webview, so
    // there is nothing to proxy it away from.
    document::eval(&format!(
        "window.stellyPlayUrl && window.stellyPlayUrl({}, {start}, {autoplay});",
        serde_json::to_string(&url).unwrap_or_else(|_| "''".into()),
    ));
    // What the OS shows as "now playing", see `stellySetMetadata`'s doc
    // comment in player.js for why this is the same fix as the
    // hardware/mouse previous-next buttons.
    document::eval(&format!(
        "window.stellySetMetadata && window.stellySetMetadata({}, {}, {}, {});",
        serde_json::to_string(&track.title).unwrap_or_else(|_| "''".into()),
        serde_json::to_string(&track.artist).unwrap_or_else(|_| "''".into()),
        serde_json::to_string(&track.album).unwrap_or_else(|_| "''".into()),
        serde_json::to_string(&track.image).unwrap_or_else(|_| "null".into()),
    ));
}

/// The element has started, or has settled on not starting. Either way the
/// swap is over and its events mean what they say again.
fn loaded(player: Player) {
    {
        let mut mirror = mirror();
        if mirror.loading.is_none() {
            return;
        }
        mirror.loading = None;
    }
    follow(player);
}

/// The element's own play and pause, which on the output are the truth.
fn sounding(player: Player, playing: bool, position: f64) {
    let report = {
        let mut mirror = mirror();
        mirror.sounding = playing;
        mirror.heard = position;
        if !mirror.is_output() || (mirror.loading.is_some() && !playing) {
            return;
        }
        mirror.loading = None;
        mirror.session().playing != playing
    };
    if report {
        // Paused from outside the app: a media key, a headset unplugged.
        report_position(player);
    }
}

fn report_position(player: Player) {
    let op = {
        let mut mirror = mirror();
        if !mirror.is_output() {
            return;
        }
        mirror.reported = Some(Instant::now());
        Op::Report {
            index: mirror.session().index,
            playing: mirror.sounding,
            position: mirror.heard,
        }
    };
    commit(player, op);
}

/// Tell the system what the element holds, for the notification and the lock
/// screen. Only Android needs telling, see `now_playing`.
fn announce() {
    #[cfg(target_os = "android")]
    {
        let mirror = mirror();
        if mirror.loading.is_some() {
            return;
        }
        let track = mirror
            .session()
            .current()
            .filter(|track| mirror.loaded.is_some_and(|(_, id)| id == track.id))
            .cloned();
        let (playing, heard) = (mirror.sounding, mirror.heard);
        drop(mirror);
        super::now_playing::show(track.as_ref(), playing, heard);
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
        "window.stellyVolume && window.stellyVolume({level});"
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

// ---------------------------------------------------------- the gestures

/// Replace the queue and start at `index`, which refers to the caller's
/// list; see `Op::Load`.
pub fn play_list(player: Player, queue: Vec<RemoteTrack>, index: usize) {
    commit(player, Op::Load { tracks: queue, index });
}

/// Append to the queue, starting playback if nothing is going.
pub fn enqueue(player: Player, tracks: Vec<RemoteTrack>) {
    commit(player, Op::Append { tracks });
}

/// Insert directly after whatever is playing, so these come next and the rest
/// of the queue still follows. With nothing playing this is `enqueue`.
pub fn play_next(player: Player, tracks: Vec<RemoteTrack>) {
    commit(player, Op::PlayNext { tracks });
}

/// Replace what follows the current track, leaving it and what came before.
pub fn set_upcoming(player: Player, tracks: Vec<RemoteTrack>) {
    commit(player, Op::Upcoming { tracks });
}

/// Empty the queue and stop.
pub fn clear_queue(player: Player) {
    commit(player, Op::Clear);
}

pub fn remove_at(player: Player, at: usize) {
    commit(player, Op::Remove { at });
}

/// Move one entry to another position. This is what a drag reports; see
/// `queue-drag.js`.
pub fn move_to(player: Player, from: usize, to: usize) {
    commit(player, Op::Move { from, to });
}

/// Shift one entry by `delta` places. Out-of-range moves are ignored, so the
/// ends simply do nothing.
pub fn move_by(player: Player, at: usize, delta: i64) {
    commit(player, Op::Shift { at, delta });
}

pub fn play_at(player: Player, index: usize) {
    commit(player, Op::Jump { index });
}

/// Step through the queue. Stops at the ends rather than wrapping, so a
/// finished queue stays finished.
pub fn step(player: Player, delta: i64) {
    commit(player, Op::Step { delta });
}

fn toggle(player: Player) {
    let op = if mirror().session().playing {
        Op::Pause
    } else {
        Op::Play
    };
    commit(player, op);
}

fn seek(player: Player, position: f64) {
    commit(player, Op::Seek { position });
}

/// Send the sound to another device, or nowhere.
fn send_output(player: Player, device_id: Option<i64>) {
    commit(player, Op::Output { device_id });
}

fn clock(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0:00".into();
    }
    let total = seconds as i64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// The bar along the bottom of the window, the full width of it, under the
/// main area and the generate panel alike.
///
/// A thin seek line runs along its top edge. Below it, left to right: what is
/// playing, whose names each open their own page; the transport, centred on
/// the bar rather than on whatever space the sides leave; the output picker,
/// the volume, which a scroll wheel turns, and the queue.
#[component]
pub fn PlayerBar() -> Element {
    let mut player = use_context::<Player>();
    let library = use_context::<Library>();
    let selection = use_context::<Selection>().0;
    let local = use_context::<LocalIds>();

    let current = player.current();
    let (position, duration) = *player.position.read();
    let queue_length = player.queue.read().len();
    let index = *player.index.read();
    let playing = *player.playing.read();
    let volume = *player.volume.read();
    let muted = *player.muted.read();
    let mut queue_open = player.queue_open;

    // The audio element's own duration is authoritative once it has loaded;
    // Qobuz's metadata fills the gap before that, and on every device that is
    // not the output.
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
    let filled = progress / 10.0;

    let mut set_volume = move |level: f64| {
        let level = level.clamp(0.0, 1.0);
        player.volume.set(level);
        // Turning it up is an unambiguous request to hear something.
        if level > 0.0 {
            player.muted.set(false);
        }
        apply_volume(player);
    };

    rsx! {
        footer { class: "player",
            // Hidden: the transport below drives it, and the native controls
            // would duplicate every button.
            audio { id: "player" }

            input {
                class: "seek-line",
                r#type: "range",
                min: "0",
                max: "1000",
                value: "{progress}",
                style: "--filled: {filled}%",
                disabled: total <= 0.0,
                title: "{clock(position)} / {clock(total)}",
                oninput: move |event| {
                    if let Ok(value) = event.value().parse::<f64>() {
                        seek(player, value / 1000.0 * total);
                    }
                },
            }

            div { class: "now",
                if let Some(track) = current.clone() {
                    // Eager: always on screen, and exactly one of it.
                    button {
                        class: "now-art-button",
                        title: "open {track.title}",
                        onclick: {
                            let track = track.clone();
                            move |_| open_remote_track(library, selection, &local, track.clone())
                        },
                        Cover { url: track.image.clone(), class: "now-art eager" }
                    }
                    div { class: "now-text",
                        span {
                            class: "title clickable",
                            title: "open {track.title}",
                            onclick: {
                                let track = track.clone();
                                move |_| open_remote_track(library, selection, &local, track.clone())
                            },
                            "{track.title}"
                        }
                        div { class: "now-sub",
                            {artist_link(library, track.artist_id, track.artist.clone(), "muted")}
                            if !track.album.is_empty() {
                                span { class: "muted", "·" }
                                {album_link(library, &track, "muted")}
                            }
                        }
                    }
                } else {
                    Cover { class: "now-art" }
                    div { class: "now-text muted", "nothing playing" }
                }
            }

            div { class: "transport",
                button {
                    class: "icon-btn",
                    title: "previous",
                    disabled: index == 0 || queue_length == 0,
                    onclick: move |_| step(player, -1),
                    {icons::previous()}
                }
                button {
                    class: "icon-btn play",
                    title: if playing { "pause" } else { "play" },
                    disabled: queue_length == 0,
                    onclick: move |_| toggle(player),
                    if playing { {icons::pause()} } else { {icons::play()} }
                }
                button {
                    class: "icon-btn",
                    title: "next",
                    disabled: index + 1 >= queue_length,
                    onclick: move |_| step(player, 1),
                    {icons::next()}
                }
                span { class: "time muted", "{clock(position)} / {clock(total)}" }
            }

            div { class: "player-meta",
                OutputPicker {}
                div {
                    class: "volume",
                    title: "scroll to change the volume",
                    onwheel: move |event: WheelEvent| {
                        event.prevent_default();
                        let dy = event.delta().strip_units().y;
                        if dy == 0.0 {
                            return;
                        }
                        let step = if dy < 0.0 { 0.05 } else { -0.05 };
                        // Read into a local first: a guard held as a call
                        // argument lives through the call, and `set_volume`
                        // writes the same signal.
                        let level = *player.volume.peek() + step;
                        set_volume(level);
                    },
                    button {
                        class: "icon-btn",
                        title: if muted { "unmute" } else { "mute" },
                        onclick: move |_| {
                            let next = !*player.muted.peek();
                            player.muted.set(next);
                            apply_volume(player);
                        },
                        {icons::volume(muted || volume <= 0.0)}
                    }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        step: "1",
                        value: "{(volume * 100.0).round() as i64}",
                        style: "--filled: {volume * 100.0}%",
                        oninput: move |event| {
                            if let Ok(percent) = event.value().parse::<f64>() {
                                set_volume(percent / 100.0);
                            }
                        },
                    }
                }
                button {
                    class: if queue_open() { "icon-btn queue-btn active" } else { "icon-btn queue-btn" },
                    title: "queue",
                    onclick: move |_| {
                        let next = !queue_open();
                        queue_open.set(next);
                    },
                    {icons::queue()}
                    if queue_length > 0 {
                        span { class: "queue-count", "{index + 1}/{queue_length}" }
                    }
                }
            }

            if let Some(message) = player.status.read().clone() {
                div { class: "player-status", "{message}" }
            }
        }
    }
}

/// Which device the sound comes out of, picked from the ones connected right
/// now, each under the name it was paired with. Names the output on the
/// button itself whenever it is not this device, so a silent screen says why.
///
/// Absent against a server without a session, where this device is the only
/// output there is.
#[component]
fn OutputPicker() -> Element {
    let player = use_context::<Player>();
    let mut open = use_signal(|| false);

    let Some(me) = *player.me.read() else {
        return rsx! {};
    };
    let devices = player.devices.read().clone();
    let output = *player.output.read();

    let elsewhere = output
        .filter(|&id| id != me)
        .and_then(|id| devices.iter().find(|device| device.device_id == id))
        .map(|device| device.name.clone());

    rsx! {
        div { class: "output-picker",
            button {
                class: if elsewhere.is_some() { "icon-btn output-btn active" } else { "icon-btn output-btn" },
                title: match &elsewhere {
                    Some(name) => format!("playing on {name}"),
                    None => "play on…".to_string(),
                },
                onclick: move |_| open.set(!open()),
                {icons::speaker()}
                if let Some(name) = elsewhere.clone() {
                    span { class: "output-name", "{name}" }
                }
            }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "context-menu output-menu",
                    div { class: "menu-head",
                        span { class: "title", "Play on" }
                    }
                    for device in devices {
                        button {
                            key: "{device.device_id}",
                            class: if output == Some(device.device_id) { "menu-item checked" } else { "menu-item" },
                            onclick: move |_| {
                                send_output(player, Some(device.device_id));
                                open.set(false);
                            },
                            "{device.name}"
                            if device.device_id == me {
                                span { class: "muted", " · this device" }
                            }
                        }
                    }
                    // Stops the sound everywhere; the queue and the position
                    // stay for whichever device plays next.
                    if output.is_some() {
                        div { class: "menu-rule" }
                        button {
                            class: "menu-item danger",
                            onclick: move |_| {
                                send_output(player, None);
                                open.set(false);
                            },
                            "disconnect"
                        }
                    }
                }
            }
        }
    }
}

/// Long-lived transport channel: installs player.js and folds its events back
/// into the session. Only the output's element ever holds a track, so on any
/// other device these events are the element being told to stop.
pub fn use_transport(player: Player) {
    use_future(move || async move {
        let mut player = player;
        let mut handle = document::eval(include_str!("../../assets/player.js"));

        while let Ok(message) = handle.recv::<serde_json::Value>().await {
            let number = |key| message.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
            match message.get("type").and_then(|v| v.as_str()) {
                Some("time") => {
                    let (position, duration) = (number("position"), number("duration"));
                    let due = {
                        let mut mirror = mirror();
                        if !mirror.is_output() {
                            continue;
                        }
                        mirror.heard = position;
                        mirror.sounding
                            && mirror
                                .reported
                                .is_none_or(|at| at.elapsed() >= REPORT_EVERY)
                    };
                    player.position.set((position, duration));
                    if due {
                        report_position(player);
                    }
                    announce();
                }
                Some("playing") => {
                    let playing = message
                        .get("playing")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    sounding(player, playing, number("position"));
                    announce();
                }
                Some("loaded") => {
                    loaded(player);
                    announce();
                }
                Some("ended") => {
                    let index = {
                        let mut mirror = mirror();
                        if !mirror.is_output() {
                            continue;
                        }
                        mirror.sounding = false;
                        mirror.session().index
                    };
                    commit(player, Op::Ended { index });
                }
                Some("failed") => {
                    // Only report it if we were not the ones who cleared the src.
                    let ours = {
                        let mut mirror = mirror();
                        mirror.loading = None;
                        mirror.is_output() && mirror.loaded.is_some()
                    };
                    if ours {
                        player
                            .status
                            .set(Some("playback failed, the stream URL may have expired".into()));
                    }
                }
                // A mouse's side buttons and a keyboard's media keys both
                // arrive as `previoustrack`/`nexttrack` MediaSession actions,
                // not as clicks on the transport bar, see `player.js`.
                // They mean "change track", which only the queue knows how
                // to do, so this is the one message type that does not just
                // mirror the `<audio>` element's own state.
                Some("transport") => match message.get("action").and_then(|v| v.as_str()) {
                    Some("previous") => step(player, -1),
                    Some("next") => step(player, 1),
                    _ => {}
                },
                _ => {}
            }
        }
    });

    // The notification's buttons, which go through the session like the
    // bar's own.
    #[cfg(target_os = "android")]
    use_future(move || async move {
        use super::now_playing::{buttons, Button};
        let mut buttons = buttons().await;
        while let Some(button) = buttons.recv().await {
            match button {
                Button::Play => commit(player, Op::Play),
                Button::Pause => commit(player, Op::Pause),
                Button::Next => step(player, 1),
                Button::Previous => step(player, -1),
                Button::Seek(position) => seek(player, position),
            }
        }
    });
}

/// Join the shared session: follow what the server pushes, send what this
/// device changes, and keep the position moving on screens that are not
/// making the sound.
pub fn use_session(player: Player) {
    use_future(move || async move {
        // A fresh start, and a fresh outbox, for each pairing.
        *mirror() = Mirror::default();
        let (outbox, mut commands) = tokio::sync::mpsc::unbounded_channel::<Command>();
        *OUTBOX.lock().unwrap_or_else(|e| e.into_inner()) = Some(outbox);

        spawn(async move {
            let mut status = player.status;
            while let Some(command) = commands.recv().await {
                let sent = backend().session_command(&command).await;
                let mut mirror = mirror();
                mirror.pending = mirror.pending.saturating_sub(1);
                drop(mirror);
                match sent {
                    Ok(true) => {}
                    Ok(false) => catch_up(player).await,
                    Err(err) => {
                        status.set(Some(format!("{err:#}")));
                        catch_up(player).await;
                    }
                }
            }
        });

        let mut feed = backend().follow_session();
        while let Some(event) = feed.recv().await {
            match event {
                SessionFeed::Update(update) => receive(player, update),
                SessionFeed::Lost => {}
                SessionFeed::Unsupported => break,
            }
        }
    });

    use_future(move || async move {
        let mut position = player.position;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let shown = {
                let mirror = mirror();
                (!mirror.is_output() && mirror.session().playing).then(|| mirror.clocked.position())
            };
            if let Some(shown) = shown {
                position.set((shown, 0.0));
            }
        }
    });
}

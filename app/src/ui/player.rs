//! The play queue and the transport.
//!
//! The queue lives in the session this user's devices share, see
//! `crate::session`. Every button here turns into an `Op`, applied to this
//! device's copy at once and sent on to the server, which pushes the result
//! to each of them. Only the device picked as the output makes a sound: its
//! `<audio>` element (see `assets/player.js`) follows the session, and reports
//! back what it is actually doing.

use super::screens::toggle_like;
use super::{album_link, artist_link, icons, open_remote_track, Cover, Library, LocalIds, Selection};
use crate::backend::{backend, Sent, SessionFeed};
use crate::catalogue::{Quality, RemoteTrack};
use crate::session::{Clocked, Command, Group, GroupKind, Op, Output, Session, Update};
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Player {
    pub queue: Signal<Vec<RemoteTrack>>,
    /// The albums and generated lists the queue holds, each shown, moved and
    /// dropped as one; see `Session::groups`.
    pub groups: Signal<Vec<Group>>,
    pub index: Signal<usize>,
    pub playing: Signal<bool>,
    /// (position, duration) in seconds. From the audio element on the output,
    /// carried forward from the session's last word everywhere else, where
    /// the duration is left at 0 for the track's own to fill in.
    pub position: Signal<(f64, f64)>,
    pub quality: Signal<Quality>,
    pub status: Signal<Option<String>>,
    /// 0.0 to 1.0, the output's, which is what the slider shows and turns.
    /// This device's own while it is the output, or nothing is: the
    /// `<audio>` element's, or on Android the system's media volume, which is
    /// what its keys turn. `None` while another device is the output and has
    /// not said, or never will against an older server.
    pub volume: Signal<Option<f64>>,
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
    /// What is playing, peeked: a list reading it is not drawn again on
    /// every change of track. Its rows' highlight is moved instead, see
    /// `use_playing_highlight`.
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
    /// The queue as the server last sent it, and its `queue_version`: what an
    /// update that leaves the queue out, or sends a splice of it, is read
    /// against. This copy's own may have moved on since.
    confirmed: Option<(u64, Vec<RemoteTrack>)>,
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
    /// This device's own volume, as its system last said. `None` until then.
    local: Option<f64>,
    /// The server could not read a volume op, being older than it, so there
    /// is no sending another.
    volume_refused: bool,
    /// The volume slider is held. This device's level follows it, but is
    /// only told to the others, or overruled by them, once it is let go.
    turning: bool,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            clocked: Clocked::default(),
            me: SOLO,
            shared: false,
            pending: 0,
            confirmed: None,
            loaded: None,
            loading: None,
            tickets: 0,
            sounding: false,
            heard: 0.0,
            reported: None,
            local: None,
            volume_refused: false,
            turning: false,
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

    /// Whether the sound is another device's, whose volume is the session's.
    fn is_remote(&self) -> bool {
        self.session().output.is_some_and(|output| output != self.me)
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

        let mut session = update.session;
        // The queue first, even from a push skipped below: the next one may
        // be a splice of it.
        let confirmed = |version: u64| match &mirror.confirmed {
            Some((at, queue)) if *at == version => Some(queue.clone()),
            _ => None,
        };
        let queue = match &update.splice {
            Some(splice) => confirmed(splice.base).and_then(|mut queue| splice.apply(&mut queue).then_some(queue)),
            None if update.same_queue => confirmed(session.queue_version),
            None => Some(std::mem::take(&mut session.queue)),
        };
        match queue {
            Some(queue) => {
                mirror.confirmed = Some((session.queue_version, queue.clone()));
                session.queue = queue;
            }
            None => {
                // The stream thinks this copy has a queue it does not: an
                // update that carried it went unread, or came while a full
                // fetch was out. Only another full fetch can say which.
                resync = true;
                mirror.confirmed = None;
                session.queue = mirror.session().queue.clone();
            }
        }

        // With commands still in flight this copy is ahead, and an older
        // push would only undo them for a moment.
        if mirror.pending > 0 && session.version < mirror.session().version {
            return;
        }
        let first = !mirror.shared;
        mirror.shared = true;
        if first || moved(mirror.session(), &session) {
            crate::cache::write_soon(SESSION_CACHE, CachedSession::of(&session));
        }
        mirror.clocked = Clocked::new(session);
    }
    show(player);
    if resync {
        spawn_forever(catch_up(player));
    }
}

/// The queue and the track as the session last had them, for the next start
/// to show while the stream is joined. Shown only, never played.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedSession {
    queue: Vec<RemoteTrack>,
    /// Absent from a cache written before there were groups.
    #[serde(default)]
    groups: Vec<Group>,
    index: usize,
    position: f64,
}

impl CachedSession {
    fn of(session: &Session) -> Self {
        CachedSession {
            queue: session.queue.clone(),
            groups: session.groups.clone(),
            index: session.index,
            position: session.position,
        }
    }
}

const SESSION_CACHE: &str = "session.json";

/// Whether the session moved to another queue, grouping or track, which is
/// all the cache keeps: a position report every few seconds is not worth a
/// write.
fn moved(before: &Session, after: &Session) -> bool {
    before.queue_version != after.queue_version || before.index != after.index || before.groups != after.groups
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
                    splice: None,
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
    let (session, position, me, shared, volume) = {
        let mirror = mirror();
        let volume = if mirror.is_remote() {
            mirror.session().volume.filter(|_| !mirror.volume_refused)
        } else {
            mirror.local
        };
        (
            mirror.session().clone(),
            mirror.clocked.position(),
            mirror.me,
            mirror.shared,
            volume,
        )
    };

    set_if_changed(player.queue, session.queue.clone());
    set_if_changed(player.groups, session.groups.clone());
    set_if_changed(player.index, session.index);
    set_if_changed(player.playing, session.playing);
    set_if_changed(player.output, session.output);
    set_if_changed(player.devices, session.devices.clone());
    set_if_changed(player.me, shared.then_some(me));
    set_if_changed(player.volume, volume);

    if session.output != Some(me) {
        player.position.set((position, 0.0));
    }
    follow(player);
    follow_volume(player);
    #[cfg(target_os = "android")]
    announce();
}

// ------------------------------------------------------------- the output

/// Point the audio element at whatever the session says, when this device is
/// the output, and silence it when it is not.
fn follow(player: Player) {
    let mut mirror = mirror();
    let current = mirror.session().current().cloned();
    let output = mirror.is_output();

    let Some(track) = current.clone().filter(|_| output) else {
        let stopped = mirror.loaded.take().is_some();
        if stopped {
            mirror.loading = None;
            mirror.sounding = false;
        }
        drop(mirror);
        if stopped {
            transport("stellyStop");
            announce();
        }
        if let Some(track) = current {
            presign(player, &track);
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
            crate::timings::start("play");
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
            crate::timings::begin("play");
            transport("stellyResume");
        } else if !playing && mirror.sounding {
            transport("stellyPause");
        }
        // Once this one has started, so the two requests do not race.
        let next = upcoming(mirror.session()).cloned();
        drop(mirror);
        presign_next(player, &track, next.as_ref());
    }
}

/// The track that plays when the current one ends, `Op::Ended`'s pick.
fn upcoming(session: &Session) -> Option<&RemoteTrack> {
    session.queue.get(session.index + 1)
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
        crate::timings::end("play-failed", "play", "");
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
    let quality = *player.quality.peek();

    let presigned = crate::timings::enabled() && signed().fresh((track.id, quality)).is_some();
    let url = match stream_url(track.id, quality).await {
        Ok(url) => {
            crate::timings::span("play-url", "play", if presigned { "presigned" } else { "signed" });
            url
        }
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
    if !autoplay {
        // Cued paused, as at startup: the next press of play is its own span.
        crate::timings::end("play-cued", "play", "");
    }

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
///
/// While another device is the output, what it is playing instead, so that
/// the volume keys reach it.
fn announce() {
    #[cfg(target_os = "android")]
    {
        let mirror = mirror();
        if mirror.is_remote() {
            let track = mirror.session().current().cloned();
            let playing = mirror.session().playing;
            let position = mirror.clocked.position();
            // Until the output has said, the keys stay on this device's own.
            let volume = mirror.session().volume.filter(|_| !mirror.volume_refused);
            drop(mirror);
            super::now_playing::show(track.as_ref(), playing, position, volume);
            return;
        }
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
        super::now_playing::show(track.as_ref(), playing, heard, None);
    }
}

// ------------------------------------------------------------- the volume

/// Levels this close are the same one, so a level set and reported back does
/// not go round again.
const SAME_LEVEL: f64 = 0.005;

fn same_level(a: f64, b: f64) -> bool {
    (a - b).abs() < SAME_LEVEL
}

/// Turn this device's own volume. It comes back as `heard_volume` once the
/// system has it.
fn set_local(level: f64) {
    #[cfg(target_os = "android")]
    super::now_playing::set_volume(level);
    #[cfg(not(target_os = "android"))]
    document::eval(&format!(
        "window.stellyVolume && window.stellyVolume({level});"
    ));
}

/// This device's own volume moved: from here, from its keys, from the
/// system's mixer. As the output it tells the others.
fn heard_volume(player: Player, level: f64) {
    let report = {
        let mut mirror = mirror();
        mirror.local = Some(level);
        if mirror.turning {
            return;
        }
        mirror.is_output()
            && !mirror.volume_refused
            && !mirror.session().volume.is_some_and(|wanted| same_level(wanted, level))
    };
    if report {
        commit(player, Op::Volume { level });
    } else {
        show(player);
    }
}

/// As the output, take up a level another device asked for, or say what this
/// one's is on taking over.
fn follow_volume(player: Player) {
    let op = {
        let mut mirror = mirror();
        if !mirror.is_output() || mirror.volume_refused || mirror.turning {
            return;
        }
        let Some(local) = mirror.local else {
            return;
        };
        match mirror.session().volume {
            None if mirror.shared => Some(Op::Volume { level: local }),
            Some(wanted) if !same_level(wanted, local) => {
                // Taken as done, so the next push does not set it again
                // before the system has answered.
                mirror.local = Some(wanted);
                drop(mirror);
                set_local(wanted);
                set_if_changed(player.volume, Some(wanted));
                None
            }
            _ => None,
        }
    };
    if let Some(op) = op {
        commit(player, op);
    }
}

/// Turn the output's volume, from the slider, the wheel or the keys.
pub fn set_volume(player: Player, level: f64) {
    let level = level.clamp(0.0, 1.0);
    let remote = {
        let mirror = mirror();
        if mirror.is_remote() && mirror.volume_refused {
            return;
        }
        mirror.is_remote()
    };
    if remote {
        commit(player, Op::Volume { level });
    } else {
        set_local(level);
        // Android's volume comes in steps, so it waits for the one the system
        // landed on.
        #[cfg(not(target_os = "android"))]
        heard_volume(player, level);
    }
}

/// The volume slider moved under a finger or the mouse. Heard at once when
/// the sound is this device's, so the level can be found by ear; another
/// device's is only asked for on release, see `let_go_volume`.
fn turn_volume(level: f64) {
    let local = {
        let mut mirror = mirror();
        if mirror.is_remote() {
            false
        } else {
            mirror.turning = true;
            true
        }
    };
    if local {
        set_local(level.clamp(0.0, 1.0));
    }
}

/// The volume slider let go at `level`: the one change the others hear of.
fn let_go_volume(player: Player, level: f64) {
    mirror().turning = false;
    set_volume(player, level);
}

pub fn quality_label(quality: Quality) -> &'static str {
    match quality {
        Quality::Lossy => "Lossy",
        Quality::Lossless => "CD quality",
        Quality::Hires => "Hi-res",
    }
}

/// Stream URLs signed earlier, by (track, quality). A provider's may expire,
/// Qobuz's after an hour, so they are kept for less, and one handed to the
/// audio element still has time to run.
#[derive(Default)]
struct Signed {
    urls: HashMap<(i64, Quality), (String, Instant)>,
    signing: HashSet<(i64, Quality)>,
    /// What the output signed as the next track. Let go of once the queue
    /// says something else comes next.
    ahead: Option<(i64, Quality)>,
}

const SIGNED_FOR: Duration = Duration::from_secs(40 * 60);

static SIGNED: LazyLock<Mutex<Signed>> = LazyLock::new(Default::default);

fn signed() -> MutexGuard<'static, Signed> {
    SIGNED.lock().unwrap_or_else(|e| e.into_inner())
}

impl Signed {
    fn fresh(&mut self, key: (i64, Quality)) -> Option<String> {
        self.urls.retain(|_, (_, at)| at.elapsed() < SIGNED_FOR);
        self.urls.get(&key).map(|(url, _)| url.clone())
    }

    fn keep(&mut self, key: (i64, Quality), url: String) {
        self.urls.retain(|_, (_, at)| at.elapsed() < SIGNED_FOR);
        self.urls.insert(key, (url, Instant::now()));
    }

    /// Mark `next` as the track coming up, dropping the one that was unless
    /// it is `current`, which is playing from it. Says whether `next` is to
    /// be signed now, and marks it as signing if so. Only once per change: a
    /// request that failed is not sent again on every push, the load will
    /// ask for itself.
    fn ahead(&mut self, next: Option<(i64, Quality)>, current: Option<(i64, Quality)>) -> bool {
        if self.ahead == next {
            return false;
        }
        if let Some(old) = self.ahead.take().filter(|old| Some(*old) != current) {
            self.urls.remove(&old);
        }
        self.ahead = next;
        next.is_some_and(|key| self.fresh(key).is_none() && self.signing.insert(key))
    }
}

/// Said each time a presign is done, for a load waiting on it.
static SIGNED_NOW: tokio::sync::Notify = tokio::sync::Notify::const_new();

async fn stream_url(track_id: i64, quality: Quality) -> anyhow::Result<String> {
    let key = (track_id, quality);
    loop {
        // Registered before looking, so a presign ending in between still
        // wakes it.
        let done = SIGNED_NOW.notified();
        tokio::pin!(done);
        done.as_mut().enable();
        {
            let mut signed = signed();
            if let Some(url) = signed.fresh(key) {
                return Ok(url);
            }
            if !signed.signing.contains(&key) {
                break;
            }
        }
        // Already on its way: a second request would only arrive later.
        done.await;
    }
    let url = backend().file_url(track_id, quality).await?;
    signed().keep(key, url.clone());
    Ok(url)
}

/// Sign the session's current track on a device that is not making the
/// sound, so that taking it over starts without a round trip to the server
/// and on to the provider. One request per track per device, the account's
/// budget can spare it.
fn presign(player: Player, track: &RemoteTrack) {
    if !track.streamable {
        return;
    }
    let key = (track.id, *player.quality.peek());
    {
        let mut signed = signed();
        if signed.fresh(key).is_some() || !signed.signing.insert(key) {
            return;
        }
    }
    sign(key);
}

/// Sign the next queued track on the output, so that the end of this one
/// goes straight to the next without waiting on the provider.
fn presign_next(player: Player, current: &RemoteTrack, next: Option<&RemoteTrack>) {
    let quality = *player.quality.peek();
    let next = next.filter(|track| track.streamable).map(|track| (track.id, quality));
    let ahead = signed().ahead(next, Some((current.id, quality)));
    if let Some(key) = next.filter(|_| ahead) {
        sign(key);
    }
}

/// Ask for `key`'s URL in the background, already marked as signing.
fn sign(key: (i64, Quality)) {
    spawn_forever(async move {
        let url = backend().file_url(key.0, key.1).await;
        {
            let mut signed = signed();
            signed.signing.remove(&key);
            if let Ok(url) = url {
                signed.keep(key, url);
            }
        }
        SIGNED_NOW.notify_waiters();
    });
}

/// Whether a queued track belongs to a hidden artist. Peeked: this runs in
/// the task loading a track, which has nothing to redraw when it changes.
fn is_hidden(artist_id: i64) -> bool {
    try_consume_context::<super::Blocklist>().is_some_and(|blocklist| {
        blocklist.artists.peek().iter().any(|entry| entry.artist_id == artist_id)
    })
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
    commit(player, Op::Load { tracks: queue, index, group: None });
}

/// Append to the queue, starting playback if nothing is going.
pub fn enqueue(player: Player, tracks: Vec<RemoteTrack>) {
    commit(player, Op::Append { tracks, group: None });
}

/// Insert directly after whatever is playing, so these come next and the rest
/// of the queue still follows. With nothing playing this is `enqueue`.
pub fn play_next(player: Player, tracks: Vec<RemoteTrack>) {
    commit(player, Op::PlayNext { tracks, group: None });
}

/// An album's tracks, the three ways in, each shown in the queue as one
/// group under the album's header.
pub fn play_album(player: Player, tracks: Vec<RemoteTrack>, index: usize, album: GroupKind) {
    commit(player, Op::Load { tracks, index, group: Some(album) });
}

pub fn play_album_next(player: Player, tracks: Vec<RemoteTrack>, album: GroupKind) {
    commit(player, Op::PlayNext { tracks, group: Some(album) });
}

pub fn enqueue_album(player: Player, tracks: Vec<RemoteTrack>, album: GroupKind) {
    commit(player, Op::Append { tracks, group: Some(album) });
}

/// Queue a group of `kind`, in place of `replace`'s rows still to come; see
/// `Op::Section`.
pub fn queue_section(player: Player, tracks: Vec<RemoteTrack>, kind: GroupKind, replace: Option<u64>) {
    commit(player, Op::Section { tracks, kind, replace });
}

/// Move a whole group to just before the row at `to`. What dragging a
/// group's header reports; see `queue-drag.js`.
pub fn move_group(player: Player, group: u64, to: usize) {
    commit(player, Op::MoveGroup { group, to });
}

/// Take what is left of a group out of the queue.
pub fn drop_group(player: Player, group: u64) {
    commit(player, Op::DropGroup { group });
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

/// Where the track is and how long it is, in seconds, and how far along as
/// the seek line's 0 to 1000. A held seek line's place stands in for the
/// position. The audio element's own duration is authoritative once it has
/// loaded; `listed`, the provider's, fills the gap before that, and on every
/// device that is not the output.
fn timeline(position: f64, duration: f64, listed: f64, held: Option<f64>) -> (f64, f64, f64) {
    let total = if duration > 0.0 { duration } else { listed };
    let progress = match held {
        Some(held) => held,
        None if total > 0.0 => (position / total * 1000.0).clamp(0.0, 1000.0),
        None => 0.0,
    };
    let position = if held.is_some() { progress / 1000.0 * total } else { position };
    (position, total, progress)
}

/// The thin seek line along the top of the player bar. Its own component,
/// with `Clock`, so the position updates, twice a second while playing,
/// redraw these two and not the whole bar.
#[component]
fn SeekLine(seeking: Signal<Option<f64>>, listed: f64) -> Element {
    let player = use_context::<Player>();
    let (position, duration) = *player.position.read();
    let (position, total, progress) = timeline(position, duration, listed, seeking());
    let filled = progress / 10.0;
    // The end of a drag. A range input says so with `change`, but not when
    // it is let go where it was taken, nor when a touch is cancelled, so the
    // pointer and touch ends say it too; whichever comes first sends.
    let mut released = move || {
        if let Some(held) = seeking.take() {
            seek(player, held / 1000.0 * total);
        }
    };

    rsx! {
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
                    seeking.set(Some(value));
                }
            },
            onchange: move |_| released(),
            onpointerup: move |_| released(),
            onpointercancel: move |_| released(),
            ontouchend: move |_| released(),
            ontouchcancel: move |_| released(),
        }
    }
}

/// The time beside the transport, the seek line's other half.
#[component]
fn Clock(seeking: Signal<Option<f64>>, listed: f64) -> Element {
    let player = use_context::<Player>();
    let (position, duration) = *player.position.read();
    let (position, total, _) = timeline(position, duration, listed, seeking());

    rsx! {
        span { class: "time muted", "{clock(position)} / {clock(total)}" }
    }
}

/// Keep the "playing" highlight on the rows of the track playing. Lists
/// read `Player::current` without subscribing, as a change of track redrawing
/// every list on screen, up to hundreds of rows each, is a lot for one class
/// on two rows. So this moves the class itself, on whatever rows carry the
/// track's `data-track`. A list drawn later reads the track as it is then.
fn use_playing_highlight(player: Player) {
    let playing = use_memo(move || {
        let index = *player.index.read();
        player.queue.read().get(index).map(|track| track.id)
    });
    use_effect(move || {
        let id = playing().map(|id| id.to_string()).unwrap_or_default();
        document::eval(&format!(
            r#"(() => {{
                const id = "{id}";
                for (const row of document.querySelectorAll("[data-track].playing")) {{
                    if (row.dataset.track !== id) row.classList.remove("playing");
                }}
                if (id) {{
                    for (const row of document.querySelectorAll(`[data-track="${{id}}"]`)) {{
                        row.classList.add("playing");
                    }}
                }}
            }})();"#
        ));
    });
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
    let player = use_context::<Player>();
    let library = use_context::<Library>();
    let selection = use_context::<Selection>().0;
    let local = use_context::<LocalIds>();

    use_playing_highlight(player);

    let current = player.current();
    let queue_length = player.queue.read().len();
    let index = *player.index.read();
    let playing = *player.playing.read();
    let volume = *player.volume.read();
    let level = volume.unwrap_or(0.0);
    // What unmuting goes back to.
    let mut unmuted = use_signal(|| 1.0);
    // Where the seek line and the volume are held, while they are: drawn
    // from here rather than from the player, so the position updates do not
    // pull the thumb back under the finger, and nothing is sent until it is
    // let go. See `SeekLine`.
    let seeking = use_signal(|| None::<f64>);
    let mut turning = use_signal(|| None::<f64>);
    let mut queue_open = player.queue_open;

    library.ensure_liked_loaded();
    let liked = current.as_ref().is_some_and(|t| library.is_liked("track", &t.id.to_string()));
    let listed = current.as_ref().and_then(|t| t.duration).unwrap_or(0) as f64;

    let level = turning().unwrap_or(level);
    // The end of a drag, as for the seek line.
    let mut volume_released = move || {
        if let Some(held) = turning.take() {
            let_go_volume(player, held);
        }
    };

    rsx! {
        footer { class: "player",
            // Hidden: the transport below drives it, and the native controls
            // would duplicate every button.
            audio { id: "player" }

            SeekLine { seeking, listed }

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
                    button {
                        class: if liked { "icon-btn now-like liked" } else { "icon-btn now-like" },
                        title: if liked { "unlike" } else { "like" },
                        onclick: {
                            let id = track.id.to_string();
                            move |_| toggle_like("track", id.clone(), liked, library, player.status)
                        },
                        {icons::heart(liked)}
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
                Clock { seeking, listed }
            }

            div { class: "player-meta",
                OutputPicker {}
                div {
                    class: "volume",
                    title: if volume.is_some() { "scroll to change the volume" } else { "the output has not said its volume" },
                    onwheel: move |event: WheelEvent| {
                        event.prevent_default();
                        let dy = event.delta().strip_units().y;
                        // Read into a local first: a guard held as a call
                        // argument lives through the call, and `set_volume`
                        // writes the same signal.
                        let Some(level) = *player.volume.peek() else {
                            return;
                        };
                        if dy != 0.0 {
                            set_volume(player, level + if dy < 0.0 { 0.05 } else { -0.05 });
                        }
                    },
                    button {
                        class: "icon-btn",
                        title: if level <= 0.0 { "unmute" } else { "mute" },
                        disabled: volume.is_none(),
                        onclick: move |_| {
                            if level > 0.0 {
                                unmuted.set(level);
                                set_volume(player, 0.0);
                            } else {
                                set_volume(player, unmuted());
                            }
                        },
                        {icons::volume(level <= 0.0)}
                    }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        step: "1",
                        disabled: volume.is_none(),
                        value: "{(level * 100.0).round() as i64}",
                        style: "--filled: {level * 100.0}%",
                        oninput: move |event| {
                            if let Ok(percent) = event.value().parse::<f64>() {
                                turning.set(Some(percent / 100.0));
                                turn_volume(percent / 100.0);
                            }
                        },
                        onchange: move |_| volume_released(),
                        onpointerup: move |_| volume_released(),
                        onpointercancel: move |_| volume_released(),
                        ontouchend: move |_| volume_released(),
                        ontouchcancel: move |_| volume_released(),
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
        let script = format!("{}{}", crate::timings::script_flag(), include_str!("../../assets/player.js"));
        let mut handle = document::eval(&script);

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
                Some("heard") => crate::timings::end("play-heard", "play", ""),
                Some("loaded") => {
                    loaded(player);
                    announce();
                }
                // Android's comes from the system instead, the element there
                // stays at full.
                #[cfg(not(target_os = "android"))]
                Some("volume") => heard_volume(player, number("level")),
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
                        mirror.loaded.filter(|_| mirror.is_output())
                    };
                    if let Some((_, track_id)) = ours {
                        // Whatever is wrong with the URL, the next try signs
                        // a new one.
                        signed().urls.retain(|&(id, _), _| id != track_id);
                        player
                            .status
                            .set(Some("playback failed, the stream URL may have expired".into()));
                    }
                }
                // A mouse's side buttons and a keyboard's media keys both
                // arrive as `previoustrack`/`nexttrack` MediaSession actions,
                // not as clicks on the transport bar, see `player.js`, and
                // the system's seeks as `seekto`. They mean "change track" or
                // "move the session", which only the session knows how to
                // do, so this is the one message type that does not just
                // mirror the `<audio>` element's own state.
                Some("transport") => match message.get("action").and_then(|v| v.as_str()) {
                    Some("previous") => step(player, -1),
                    Some("next") => step(player, 1),
                    Some("seek") => seek(player, number("position")),
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
                Button::Volume(level) => set_volume(player, level),
                Button::SystemVolume(level) => heard_volume(player, level),
            }
        }
    });

    #[cfg(target_os = "android")]
    use_hook(super::now_playing::watch_volume);
    #[cfg(target_os = "android")]
    use_hook(super::now_playing::watch_network);
}

/// Join the shared session: follow what the server pushes, send what this
/// device changes, and keep the position moving on screens that are not
/// making the sound.
pub fn use_session(player: Player) {
    use_future(move || async move {
        // A fresh start, and a fresh outbox, for each pairing. The device's
        // own volume stays, it may have been reported already.
        {
            let mut mirror = mirror();
            *mirror = Mirror {
                local: mirror.local,
                ..Mirror::default()
            };
        }
        let (outbox, mut commands) = tokio::sync::mpsc::unbounded_channel::<Command>();
        *OUTBOX.lock().unwrap_or_else(|e| e.into_inner()) = Some(outbox);

        // Last time's queue and track, so the bar is not empty until the
        // stream's first frame. Paused, whatever it was doing then, and only
        // in the signals: the mirror stays empty, so nothing plays from it
        // and the first frame replaces it all.
        spawn(async move {
            let read = tokio::task::spawn_blocking(|| crate::cache::read::<CachedSession>(SESSION_CACHE)).await;
            let Ok(Some(cached)) = read else {
                return;
            };
            let untouched = {
                let mirror = mirror();
                !mirror.shared && mirror.session().queue.is_empty()
            };
            if untouched && !cached.queue.is_empty() {
                let mut position = player.position;
                set_if_changed(player.index, cached.index.min(cached.queue.len() - 1));
                set_if_changed(player.queue, cached.queue);
                set_if_changed(player.groups, cached.groups);
                set_if_changed(player.playing, false);
                position.set((cached.position, 0.0));
            }
        });

        spawn(async move {
            let mut status = player.status;
            while let Some(command) = commands.recv().await {
                let sent = backend().session_command(&command).await;
                let mut mirror = mirror();
                mirror.pending = mirror.pending.saturating_sub(1);
                let volume = matches!(command.op, Op::Volume { .. });
                if volume && matches!(sent, Ok(Sent::Unknown)) {
                    // Each device keeps its own volume then, as before there
                    // was a shared one.
                    mirror.volume_refused = true;
                }
                drop(mirror);
                match sent {
                    Ok(Sent::Applied) => {}
                    Ok(Sent::Stale) => catch_up(player).await,
                    Ok(Sent::Unknown) if volume => catch_up(player).await,
                    Ok(Sent::Unknown) => {
                        status.set(Some("the server is older than this app".into()));
                        catch_up(player).await;
                    }
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
                // Maybe the server is gone: the pipeline poll is what says
                // so, and between jobs it might not ask for seconds.
                SessionFeed::Lost => super::pace::wake(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeline_prefers_the_elements_duration() {
        assert_eq!(timeline(30.0, 120.0, 100.0, None), (30.0, 120.0, 250.0));
        assert_eq!(timeline(30.0, 0.0, 120.0, None), (30.0, 120.0, 250.0));
        assert_eq!(timeline(30.0, 0.0, 0.0, None), (30.0, 0.0, 0.0));
    }

    #[test]
    fn a_held_seek_line_stands_in_for_the_position() {
        assert_eq!(timeline(30.0, 120.0, 0.0, Some(500.0)), (60.0, 120.0, 500.0));
    }

    const LOSSLESS: Quality = Quality::Lossless;

    #[test]
    fn only_a_new_queue_or_track_is_cached() {
        let before = Session { queue_version: 3, index: 1, position: 10.0, ..Session::default() };
        let reported = Session { position: 20.0, playing: true, version: 9, ..before.clone() };
        assert!(!moved(&before, &reported));
        assert!(moved(&before, &Session { index: 2, ..before.clone() }));
        assert!(moved(&before, &Session { queue_version: 4, ..before.clone() }));
        let group = Group {
            id: 1,
            kind: GroupKind::Generated { recipe: crate::api::Recipe::Radio { seed: 1 } },
            tracks: vec![1],
        };
        assert!(moved(&before, &Session { groups: vec![group], ..before.clone() }));
    }

    #[test]
    fn the_next_track_is_signed_once() {
        let mut signed = Signed::default();
        assert!(signed.ahead(Some((2, LOSSLESS)), Some((1, LOSSLESS))));
        assert!(signed.signing.contains(&(2, LOSSLESS)));
        // A later push does not ask again, even after a failure.
        signed.signing.clear();
        assert!(!signed.ahead(Some((2, LOSSLESS)), Some((1, LOSSLESS))));
    }

    #[test]
    fn a_track_already_signed_is_not_signed_again() {
        let mut signed = Signed::default();
        signed.keep((2, LOSSLESS), "two".into());
        assert!(!signed.ahead(Some((2, LOSSLESS)), Some((1, LOSSLESS))));
        assert_eq!(signed.fresh((2, LOSSLESS)).as_deref(), Some("two"));
    }

    #[test]
    fn a_queue_change_drops_the_old_next() {
        let mut signed = Signed::default();
        signed.ahead(Some((2, LOSSLESS)), Some((1, LOSSLESS)));
        signed.keep((2, LOSSLESS), "two".into());
        assert!(signed.ahead(Some((3, LOSSLESS)), Some((1, LOSSLESS))));
        assert_eq!(signed.fresh((2, LOSSLESS)), None);
    }

    #[test]
    fn the_next_track_once_playing_keeps_its_url() {
        let mut signed = Signed::default();
        signed.ahead(Some((2, LOSSLESS)), Some((1, LOSSLESS)));
        signed.keep((2, LOSSLESS), "two".into());
        // Track 2 ended up playing, track 3 is next.
        signed.ahead(Some((3, LOSSLESS)), Some((2, LOSSLESS)));
        assert_eq!(signed.fresh((2, LOSSLESS)).as_deref(), Some("two"));
    }

    #[test]
    fn the_end_of_the_queue_has_nothing_to_sign() {
        let mut signed = Signed::default();
        signed.ahead(Some((2, LOSSLESS)), Some((1, LOSSLESS)));
        signed.keep((2, LOSSLESS), "two".into());
        assert!(!signed.ahead(None, Some((1, LOSSLESS))));
        assert_eq!(signed.ahead, None);
        assert_eq!(signed.fresh((2, LOSSLESS)), None);
    }
}

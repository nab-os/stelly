//! The play sessions: one queue and one output per member of the family,
//! shared by all of their devices.
//!
//! The rules live in `stelly_core::session`, which the app runs too. This only
//! holds the real copies, knows which devices are listening to each, and
//! pushes every change to them. Someone else's devices are not in your output
//! picker, so nobody takes over the living room from another queue.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use crate::listens::{unix_now, Listen, Moment, Tracker};
use std::time::{Duration, Instant};
use stelly_core::session::Op;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use stelly_core::api::Device;
use stelly_core::catalogue::RemoteTrack;
use stelly_core::session::{Clocked, Command, Output, Session, Splice, Stale};

/// How long an output may be gone before the session stops waiting for it.
/// Long enough to ride out a phone switching networks, short enough that
/// another device can pick up the sound without a trip to the picker.
const GRACE: Duration = Duration::from_secs(10);

/// Everyone's session, each made the first time its user asks for it. Held
/// for as long as the server runs, which for a family is a handful.
#[derive(Default)]
pub struct Sessions {
    all: Mutex<HashMap<i64, Arc<Playback>>>,
    /// Where the sessions send each listen as it ends; none, not kept.
    listens: Option<UnboundedSender<Listen>>,
}

impl Sessions {
    pub fn recording(listens: UnboundedSender<Listen>) -> Self {
        Sessions { all: Mutex::default(), listens: Some(listens) }
    }

    pub fn of(&self, user_id: i64) -> Arc<Playback> {
        self.all
            .lock()
            .unwrap()
            .entry(user_id)
            .or_insert_with(|| Playback::listened(user_id, self.listens.clone()))
            .clone()
    }
}

pub struct Playback {
    inner: Mutex<Inner>,
    published: watch::Sender<Arc<Published>>,
    user_id: i64,
    listens: Option<UnboundedSender<Listen>>,
}

#[derive(Default)]
struct Inner {
    clocked: Clocked,
    /// The queue as last published, shared by every version since it last
    /// changed.
    queue: Arc<Queue>,
    /// Open event streams per device, with the name it was paired under.
    /// Counted rather than flagged: a device reconnecting opens its new
    /// stream before the old one is noticed closed.
    connected: HashMap<i64, (String, usize)>,
    tracker: Tracker,
}

impl Inner {
    fn devices(&self) -> Vec<Output> {
        let mut devices: Vec<Output> = self
            .connected
            .iter()
            .map(|(&device_id, (name, _))| Output {
                device_id,
                name: name.clone(),
            })
            .collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.device_id.cmp(&b.device_id)));
        devices
    }
}

/// Held by an event stream for as long as it is open.
pub struct Listening {
    playback: Arc<Playback>,
    device_id: i64,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.playback.disconnect(self.device_id);
    }
}

/// One version of the session as the event streams send it. Each part is
/// serialised once, by the first stream that needs it, however many devices
/// are listening.
pub struct Published {
    /// Its queue left empty: `queue` holds that.
    session: Session,
    queue: Arc<Queue>,
    /// The change from the queue published before, when this version changed
    /// it.
    splice: Option<Splice>,
    /// The session as JSON either side of its empty queue.
    halves: OnceLock<(String, String)>,
    splice_json: OnceLock<String>,
}

/// One version of the queue, and its JSON.
#[derive(Default)]
struct Queue {
    version: u64,
    tracks: Vec<RemoteTrack>,
    json: OnceLock<String>,
}

impl Published {
    fn new(session: Session, queue: Arc<Queue>, splice: Option<Splice>) -> Self {
        Published {
            session,
            queue,
            splice,
            halves: OnceLock::new(),
            splice_json: OnceLock::new(),
        }
    }

    pub fn queue_version(&self) -> u64 {
        self.queue.version
    }

    /// The update for a stream that last sent the queue at `sent`, as JSON.
    /// The queue goes only when it changed since; as a splice when the app
    /// reads them and has the queue it applies to, whole otherwise.
    pub fn update(&self, you: i64, sent: Option<u64>, splices: bool) -> String {
        let (head, tail) = self.halves.get_or_init(|| {
            let json = serde_json::to_string(&self.session).unwrap_or_default();
            // The first match is the field itself: no field before it holds
            // text, and a quote inside a string comes out escaped.
            let (head, tail) = json.split_once(r#""queue":[]"#).unwrap_or((&json, ""));
            (format!(r#"{head}"queue":"#), tail.to_string())
        });
        let empty = "[]";
        let (same_queue, splice, queue) = match &self.splice {
            _ if sent == Some(self.queue.version) => (true, None, empty),
            Some(splice) if splices && sent == Some(splice.base) => {
                let json = self.splice_json.get_or_init(|| serde_json::to_string(splice).unwrap_or_default());
                (false, Some(json.as_str()), empty)
            }
            _ => {
                let json = self.queue.json.get_or_init(|| serde_json::to_string(&self.queue.tracks).unwrap_or_default());
                (false, None, json.as_str())
            }
        };
        let splice = splice.map(|json| format!(r#""splice":{json},"#)).unwrap_or_default();
        format!(r#"{{"you":{you},"same_queue":{same_queue},{splice}"session":{head}{queue}{tail}}}"#)
    }
}

impl Playback {
    #[cfg(test)]
    pub fn new() -> Arc<Self> {
        Self::listened(0, None)
    }

    fn listened(user_id: i64, listens: Option<UnboundedSender<Listen>>) -> Arc<Self> {
        let (published, _) = watch::channel(Arc::new(Published::new(Session::default(), Arc::default(), None)));
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            published,
            user_id,
            listens,
        })
    }

    /// Apply `op`, telling the tracker what it did to the listen.
    fn apply(&self, inner: &mut Inner, op: Op, queue_version: u64, from: i64) -> Result<(), Stale> {
        let before = Moment::of(&inner.clocked);
        let watched = self.listens.as_ref().map(|_| op.clone());
        inner.clocked.apply(op, queue_version, from)?;
        if let (Some(op), Some(listens)) = (watched, &self.listens) {
            let after = Moment::of(&inner.clocked);
            if let Some(listen) = inner.tracker.observe(self.user_id, &op, &before, &after, Instant::now(), unix_now()) {
                let _ = listens.send(listen);
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Session {
        self.inner.lock().unwrap().clocked.snapshot()
    }

    pub fn command(&self, device: &Device, command: Command) -> Result<(), Stale> {
        let mut inner = self.inner.lock().unwrap();

        // The picker only offers connected devices, but it can be out of date
        // by the time the choice lands. Refused rather than ignored, so the
        // device that asked catches up instead of showing an output that
        // nothing is playing on.
        if let stelly_core::session::Op::Output {
            device_id: Some(target),
        } = command.op
        {
            if !inner.connected.contains_key(&target) {
                return Err(Stale);
            }
        }

        self.apply(&mut inner, command.op, command.queue_version, device.id)?;
        self.publish(&mut inner);
        Ok(())
    }

    /// Start listening for `device`. The returned guard stops it when dropped.
    pub fn connect(self: &Arc<Self>, device: &Device) -> (watch::Receiver<Arc<Published>>, Listening) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .connected
            .entry(device.id)
            .or_insert_with(|| (device.name.clone(), 0))
            .1 += 1;
        self.publish(&mut inner);

        let mut receiver = self.published.subscribe();
        // So the first `changed` resolves at once and the stream opens with
        // the whole session.
        receiver.mark_changed();

        let guard = Listening {
            playback: self.clone(),
            device_id: device.id,
        };
        (receiver, guard)
    }

    fn disconnect(self: &Arc<Self>, device_id: i64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some((_, count)) = inner.connected.get_mut(&device_id) {
            *count -= 1;
            if *count == 0 {
                inner.connected.remove(&device_id);
            }
        }
        self.publish(&mut inner);

        if inner.clocked.session.output == Some(device_id)
            && !inner.connected.contains_key(&device_id)
        {
            let playback = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(GRACE).await;
                playback.release(device_id);
            });
        }
    }

    /// Let go of an output that has not come back. The position stays, so
    /// whichever device plays next picks up where it went quiet.
    fn release(&self, device_id: i64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.connected.contains_key(&device_id) || inner.clocked.session.output != Some(device_id) {
            return;
        }
        let queue_version = inner.clocked.session.queue_version;
        let released = self.apply(&mut inner, Op::Output { device_id: None }, queue_version, device_id);
        if released.is_ok() {
            self.publish(&mut inner);
        }
    }

    /// Hand the streams the session as it is now. Only a changed queue is
    /// copied: a position report every few seconds used to copy it whole
    /// here, under the lock, and again for every listening device.
    fn publish(&self, inner: &mut Inner) {
        inner.clocked.session.devices = inner.devices();
        let mut splice = None;
        if inner.queue.version != inner.clocked.session.queue_version {
            let tracks = inner.clocked.session.queue.clone();
            splice = Some(Splice::between(inner.queue.version, &inner.queue.tracks, &tracks));
            inner.queue = Arc::new(Queue {
                version: inner.clocked.session.queue_version,
                tracks,
                json: OnceLock::new(),
            });
        }

        // The snapshot without its queue, taken out and put back rather than
        // copied.
        let position = inner.clocked.position();
        let queue = std::mem::take(&mut inner.clocked.session.queue);
        let mut session = inner.clocked.session.clone();
        inner.clocked.session.queue = queue;
        session.position = position;

        self.published.send_replace(Arc::new(Published::new(session, inner.queue.clone(), splice)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stelly_core::api::Scope;
    use stelly_core::session::{Op, Update};

    fn device(id: i64) -> Device {
        Device {
            id,
            name: format!("device {id}"),
            scope: Scope::Play,
            created_at: String::new(),
            last_seen: None,
            user_id: 1,
            user: String::new(),
        }
    }

    fn tracks(ids: std::ops::Range<i64>) -> Vec<RemoteTrack> {
        ids.map(|id| RemoteTrack { id, title: format!("\"queue\":[] {id}"), ..Default::default() })
            .collect()
    }

    fn command(playback: &Playback, op: Op) {
        let queue_version = playback.snapshot().queue_version;
        playback.command(&device(1), Command { queue_version, op }).unwrap();
    }

    /// What a stream sends next, read the way the app reads it.
    fn next(receiver: &mut watch::Receiver<Arc<Published>>, sent: &mut Option<u64>, splices: bool) -> Update {
        let published = receiver.borrow_and_update().clone();
        let update = published.update(1, *sent, splices);
        *sent = Some(published.queue_version());
        serde_json::from_str(&update).unwrap()
    }

    #[tokio::test]
    async fn the_session_tells_each_listen_as_it_ends() {
        let (listens, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let sessions = Sessions::recording(listens);
        let playback = sessions.of(1);
        let (_receiver, _listening) = playback.connect(&device(1));
        command(&playback, Op::Load { tracks: tracks(0..3), index: 0, group: None });
        command(&playback, Op::Play);
        command(&playback, Op::Step { delta: 1 });
        let listen = heard.try_recv().unwrap();
        assert_eq!((listen.user_id, listen.track_id, listen.device_id), (1, 0, Some(1)));
        assert_eq!(listen.ended, crate::listens::Ended::Skipped);
        command(&playback, Op::Clear);
        let listen = heard.try_recv().unwrap();
        assert_eq!((listen.track_id, listen.ended), (1, crate::listens::Ended::Stopped));
        assert!(heard.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_stream_sends_the_queue_only_when_it_changed() {
        let playback = Playback::new();
        let (mut receiver, _listening) = playback.connect(&device(1));
        command(&playback, Op::Load { tracks: tracks(0..5), index: 0, group: None });
        command(&playback, Op::Pause);
        let mut sent = None;

        // Joining: everything, as an app from before splices reads it too.
        let update = next(&mut receiver, &mut sent, true);
        assert!(!update.same_queue && update.splice.is_none());
        assert_eq!(update.session, playback.snapshot());

        command(&playback, Op::Seek { position: 4.0 });
        let update = next(&mut receiver, &mut sent, true);
        assert!(update.same_queue && update.session.queue.is_empty());
        assert_eq!(update.session.position, 4.0);

        command(&playback, Op::Append { tracks: tracks(5..6), group: None });
        let update = next(&mut receiver, &mut sent, true);
        let splice = update.splice.unwrap();
        assert_eq!((splice.at, splice.removed, splice.tracks), (5, 0, tracks(5..6)));
        assert!(update.session.queue.is_empty());
    }

    #[tokio::test]
    async fn an_app_without_splices_gets_what_it_always_did() {
        let playback = Playback::new();
        let (mut receiver, _listening) = playback.connect(&device(1));
        command(&playback, Op::Load { tracks: tracks(0..5), index: 0, group: None });
        command(&playback, Op::Pause);
        let mut sent = None;
        next(&mut receiver, &mut sent, false);

        for (op, same_queue) in [(Op::Seek { position: 2.0 }, true), (Op::Remove { at: 3 }, false)] {
            command(&playback, op);
            let published = receiver.borrow_and_update().clone();
            let mut session = playback.snapshot();
            if same_queue {
                session.queue = Vec::new();
            }
            let old = serde_json::to_string(&Update { you: 1, same_queue, splice: None, session }).unwrap();
            assert_eq!(published.update(1, sent, false), old);
            sent = Some(published.queue_version());
        }
    }

    #[tokio::test]
    async fn a_stream_that_missed_a_queue_gets_it_whole() {
        let playback = Playback::new();
        let (mut receiver, _listening) = playback.connect(&device(1));
        command(&playback, Op::Load { tracks: tracks(0..5), index: 0, group: None });
        let mut sent = None;
        next(&mut receiver, &mut sent, true);

        // Two changes before the stream gets to look: the splice is from the
        // one in between, which it never sent.
        command(&playback, Op::Remove { at: 4 });
        command(&playback, Op::Remove { at: 3 });
        let update = next(&mut receiver, &mut sent, true);
        assert!(update.splice.is_none() && !update.same_queue);
        assert_eq!(update.session.queue, tracks(0..3));
    }
}

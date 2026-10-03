//! The play sessions: one queue and one output per member of the family,
//! shared by all of their devices.
//!
//! The rules live in `stelly_core::session`, which the app runs too. This only
//! holds the real copies, knows which devices are listening to each, and
//! pushes every change to them. Someone else's devices are not in your output
//! picker, so nobody takes over the living room from another queue.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use stelly_core::api::Device;
use stelly_core::session::{Clocked, Command, Output, Session, Stale};

/// How long an output may be gone before the session stops waiting for it.
/// Long enough to ride out a phone switching networks, short enough that
/// another device can pick up the sound without a trip to the picker.
const GRACE: Duration = Duration::from_secs(10);

/// Everyone's session, each made the first time its user asks for it. Held
/// for as long as the server runs, which for a family is a handful.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<i64, Arc<Playback>>>);

impl Sessions {
    pub fn of(&self, user_id: i64) -> Arc<Playback> {
        self.0
            .lock()
            .unwrap()
            .entry(user_id)
            .or_insert_with(Playback::new)
            .clone()
    }
}

pub struct Playback {
    inner: Mutex<Inner>,
    published: watch::Sender<Session>,
}

#[derive(Default)]
struct Inner {
    clocked: Clocked,
    /// Open event streams per device, with the name it was paired under.
    /// Counted rather than flagged: a device reconnecting opens its new
    /// stream before the old one is noticed closed.
    connected: HashMap<i64, (String, usize)>,
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

impl Playback {
    pub fn new() -> Arc<Self> {
        let (published, _) = watch::channel(Session::default());
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            published,
        })
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

        inner
            .clocked
            .apply(command.op, command.queue_version, device.id)?;
        self.publish(&mut inner);
        Ok(())
    }

    /// Start listening for `device`. The returned guard stops it when dropped.
    pub fn connect(self: &Arc<Self>, device: &Device) -> (watch::Receiver<Session>, Listening) {
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
        let released = inner.clocked.apply(
            stelly_core::session::Op::Output { device_id: None },
            queue_version,
            device_id,
        );
        if released.is_ok() {
            self.publish(&mut inner);
        }
    }

    fn publish(&self, inner: &mut Inner) {
        inner.clocked.session.devices = inner.devices();
        self.published.send_replace(inner.clocked.snapshot());
    }
}

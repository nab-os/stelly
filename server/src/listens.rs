//! What each member of the family listened to: one row per track played,
//! how long it was heard and how it ended, kept raw for whatever is wanted
//! of it later.
//!
//! The session already sees everything this needs: the output's reports of
//! where it is while it plays, and the ops that change the track. A
//! `Tracker` watches one session through them. Heard time is what the
//! position moved by while playing, never more than the time that passed,
//! so a seek forward is not listening.

use anyhow::Result;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Double, Nullable, Text};
use crate::mix::{MixService, Reason};
use std::sync::Arc;
use std::time::Instant;
use stelly_core::session::{Clocked, Op};

/// Within this of the end, or a tenth of the track when that is less, a
/// track was heard to its end.
const FINISH_SLACK: f64 = 15.0;
/// Reports and ops arrive a little late or early: what the position may
/// move past the time that passed, and still count.
const CLOCK_SLACK: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Heard to its end, or near enough.
    Finished,
    /// Another track took its place before the end.
    Skipped,
    /// Nothing took its place: the queue cleared, or the output went.
    Stopped,
}

impl Ended {
    pub fn as_str(self) -> &'static str {
        match self {
            Ended::Finished => "finished",
            Ended::Skipped => "skipped",
            Ended::Stopped => "stopped",
        }
    }
}

/// One track played, once it has stopped being the one playing.
#[derive(Debug, Clone, PartialEq)]
pub struct Listen {
    pub user_id: i64,
    /// The output it played on when it started.
    pub device_id: Option<i64>,
    pub track_id: i64,
    /// Unix seconds.
    pub started_at: i64,
    pub listened_seconds: f64,
    pub track_seconds: Option<f64>,
    pub ended: Ended,
}

impl Listen {
    /// Heard half through, or four minutes of it, whichever comes first.
    pub fn played_through(&self) -> bool {
        let half = self.track_seconds.map_or(f64::INFINITY, |seconds| seconds / 2.0);
        self.listened_seconds >= half.min(240.0)
    }
}

/// The session as far as listening goes, at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct Moment {
    pub index: usize,
    /// The current track and its length.
    pub track: Option<(i64, Option<f64>)>,
    /// Playing, and on an output.
    pub sounding: bool,
    pub output: Option<i64>,
    pub position: f64,
}

impl Moment {
    pub fn of(clocked: &Clocked) -> Self {
        let session = &clocked.session;
        Moment {
            index: session.index,
            track: session.current().map(|t| (t.id, t.duration.filter(|&d| d > 0).map(|d| d as f64))),
            sounding: session.playing && session.output.is_some(),
            output: session.output,
            position: clocked.position(),
        }
    }

    fn track_id(&self) -> Option<i64> {
        self.track.map(|(id, _)| id)
    }
}

struct Open {
    track_id: i64,
    device_id: Option<i64>,
    started_at: i64,
    track_seconds: Option<f64>,
    listened: f64,
    /// Where it was, and when, at the last op.
    position: f64,
    at: Instant,
    sounding: bool,
}

/// One session's listen in progress.
#[derive(Default)]
pub struct Tracker {
    open: Option<Open>,
}

impl Tracker {
    /// The session went from `before` to `after` by `op`, at `now`, `unix`
    /// in Unix seconds: the listen it ended, if it ended one.
    pub fn observe(&mut self, user_id: i64, op: &Op, before: &Moment, after: &Moment, now: Instant, unix: i64) -> Option<Listen> {
        let mut closed = None;
        if let Some(open) = &mut self.open {
            // A report says where the output really is; anything else, where
            // the session reckoned it was until then.
            let heard_to = match op {
                Op::Report { .. } if after.track_id() == Some(open.track_id) => after.position,
                _ => before.position,
            };
            if open.sounding {
                let passed = now.duration_since(open.at).as_secs_f64();
                open.listened += (heard_to - open.position).clamp(0.0, passed + CLOCK_SLACK);
            }
            let starts_over = match op {
                Op::Load { .. } | Op::Jump { .. } | Op::Clear => true,
                Op::Step { .. } => after.index != before.index,
                Op::Ended { index } => *index == before.index,
                Op::Output { device_id: None } => before.output.is_some(),
                _ => false,
            };
            if starts_over || after.track_id() != Some(open.track_id) {
                let near_end = open
                    .track_seconds
                    .map_or(matches!(op, Op::Ended { .. }), |seconds| heard_to >= seconds - FINISH_SLACK.min(seconds / 10.0));
                let ended = match () {
                    _ if near_end => Ended::Finished,
                    _ if after.track.is_none() || matches!(op, Op::Clear | Op::Output { device_id: None }) => Ended::Stopped,
                    _ => Ended::Skipped,
                };
                closed = Some(Listen {
                    user_id,
                    device_id: open.device_id,
                    track_id: open.track_id,
                    started_at: open.started_at,
                    listened_seconds: open.listened,
                    track_seconds: open.track_seconds,
                    ended,
                });
                self.open = None;
            } else {
                open.position = after.position;
                open.at = now;
                open.sounding = after.sounding;
            }
        }
        if self.open.is_none() && after.sounding {
            if let Some((track_id, track_seconds)) = after.track {
                self.open = Some(Open {
                    track_id,
                    device_id: after.output,
                    started_at: unix,
                    track_seconds,
                    listened: 0.0,
                    position: after.position,
                    at: now,
                    sounding: true,
                });
            }
        }
        closed
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn store(conn: &mut SqliteConnection, listen: &Listen) -> Result<()> {
    diesel::sql_query(
        "INSERT INTO listens (user_id, device_id, track_id, started_at, listened_seconds, track_seconds, ended)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind::<BigInt, _>(listen.user_id)
    .bind::<Nullable<BigInt>, _>(listen.device_id)
    .bind::<BigInt, _>(listen.track_id)
    .bind::<BigInt, _>(listen.started_at)
    .bind::<Double, _>((listen.listened_seconds * 10.0).round() / 10.0)
    .bind::<Nullable<Double>, _>(listen.track_seconds)
    .bind::<Text, _>(listen.ended.as_str())
    .execute(conn)?;
    Ok(())
}

/// Store every listen the sessions end, as they end, off their locks; a
/// track played through is queued for its mix analysis, behind the rest.
pub fn record(db_path: std::path::PathBuf, mut listens: tokio::sync::mpsc::UnboundedReceiver<Listen>, mix: Arc<MixService>) {
    tokio::spawn(async move {
        while let Some(listen) = listens.recv().await {
            let (db_path, mix) = (db_path.clone(), mix.clone());
            let stored = tokio::task::spawn_blocking(move || {
                store(&mut crate::db::open_for_write(&db_path)?, &listen)?;
                if listen.played_through() {
                    mix.enqueue(&[listen.track_id], Reason::Played)?;
                }
                anyhow::Ok(())
            })
            .await;
            if let Ok(Err(err)) | Err(err) = stored.map_err(anyhow::Error::from) {
                eprintln!("a listen was not recorded: {err:#}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SONG: Option<(i64, Option<f64>)> = Some((7, Some(200.0)));

    fn at(index: usize, track: Option<(i64, Option<f64>)>, sounding: bool, position: f64) -> Moment {
        Moment { index, track, sounding, output: Some(1), position }
    }

    /// Feeds a tracker ops at seconds since the start.
    struct Clock {
        tracker: Tracker,
        start: Instant,
        last: Moment,
    }

    impl Clock {
        fn new() -> Self {
            Clock { tracker: Tracker::default(), start: Instant::now(), last: at(0, None, false, 0.0) }
        }

        /// `before` is what the session reckoned just before `op`.
        fn op(&mut self, second: f64, op: Op, before: Moment, after: Moment) -> Option<Listen> {
            let now = self.start + Duration::from_secs_f64(second);
            let listen = self.tracker.observe(1, &op, &before, &after, now, 1000 + second as i64);
            self.last = after;
            listen
        }
    }

    #[test]
    fn heard_to_the_end_is_finished() {
        let mut clock = Clock::new();
        assert!(clock.op(0.0, Op::Play, at(0, SONG, false, 0.0), at(0, SONG, true, 0.0)).is_none());
        clock.op(100.0, Op::Report { index: 0, playing: true, position: 100.0 }, at(0, SONG, true, 100.0), at(0, SONG, true, 100.0));
        let next = Some((8, Some(180.0)));
        let listen = clock.op(200.0, Op::Ended { index: 0 }, at(0, SONG, true, 200.0), at(1, next, true, 0.0)).unwrap();
        assert_eq!((listen.track_id, listen.ended, listen.started_at), (7, Ended::Finished, 1000));
        assert!((listen.listened_seconds - 200.0).abs() < 0.01);
        assert!(listen.played_through());
    }

    #[test]
    fn a_seek_forward_is_not_listening() {
        let mut clock = Clock::new();
        clock.op(0.0, Op::Play, at(0, SONG, false, 0.0), at(0, SONG, true, 0.0));
        clock.op(10.0, Op::Seek { position: 150.0 }, at(0, SONG, true, 10.0), at(0, SONG, true, 150.0));
        let listen = clock.op(20.0, Op::Step { delta: 1 }, at(0, SONG, true, 160.0), at(1, Some((8, None)), true, 0.0)).unwrap();
        assert_eq!(listen.ended, Ended::Skipped);
        assert!((listen.listened_seconds - 20.0).abs() < 0.01, "{}", listen.listened_seconds);
        assert!(!listen.played_through());
    }

    #[test]
    fn a_pause_is_not_listening_and_a_stall_is_corrected_by_the_report() {
        let mut clock = Clock::new();
        clock.op(0.0, Op::Play, at(0, SONG, false, 0.0), at(0, SONG, true, 0.0));
        clock.op(30.0, Op::Pause, at(0, SONG, true, 30.0), at(0, SONG, false, 30.0));
        clock.op(500.0, Op::Play, at(0, SONG, false, 30.0), at(0, SONG, true, 30.0));
        // Reckoned 60s in, but the output stalled and says 50.
        clock.op(530.0, Op::Report { index: 0, playing: true, position: 50.0 }, at(0, SONG, true, 60.0), at(0, SONG, true, 50.0));
        let listen = clock.op(540.0, Op::Clear, at(0, SONG, true, 60.0), at(0, None, false, 0.0)).unwrap();
        assert_eq!(listen.ended, Ended::Stopped);
        assert!((listen.listened_seconds - 60.0).abs() < 0.01, "{}", listen.listened_seconds);
    }

    #[test]
    fn the_next_listen_starts_with_the_next_track() {
        let mut clock = Clock::new();
        let next = Some((8, Some(180.0)));
        clock.op(0.0, Op::Play, at(0, SONG, false, 0.0), at(0, SONG, true, 0.0));
        clock.op(5.0, Op::Jump { index: 1 }, at(0, SONG, true, 5.0), at(1, next, true, 0.0));
        let listen = clock.op(185.0, Op::Ended { index: 1 }, at(1, next, true, 180.0), at(1, next, false, 180.0)).unwrap();
        assert_eq!((listen.track_id, listen.ended, listen.started_at), (8, Ended::Finished, 1005));
        // The end of the queue: nothing plays, nothing is open.
        assert!(clock.op(200.0, Op::Pause, at(1, next, false, 180.0), at(1, next, false, 180.0)).is_none());
        assert!(clock.tracker.open.is_none());
    }

    #[test]
    fn a_short_track_stopped_early_is_not_finished() {
        let mut clock = Clock::new();
        let jingle = Some((9, Some(6.0)));
        clock.op(0.0, Op::Play, at(0, jingle, false, 0.0), at(0, jingle, true, 0.0));
        let listen = clock.op(1.0, Op::Clear, at(0, jingle, true, 1.0), at(0, None, false, 0.0)).unwrap();
        assert_eq!(listen.ended, Ended::Stopped);
    }

    #[test]
    fn a_late_ended_for_another_track_ends_nothing() {
        let mut clock = Clock::new();
        clock.op(0.0, Op::Play, at(1, SONG, false, 0.0), at(1, SONG, true, 0.0));
        assert!(clock.op(3.0, Op::Ended { index: 0 }, at(1, SONG, true, 3.0), at(1, SONG, true, 3.0)).is_none());
    }

    #[test]
    fn played_through_is_half_or_four_minutes() {
        let listen = |listened, track| Listen {
            user_id: 1,
            device_id: None,
            track_id: 1,
            started_at: 0,
            listened_seconds: listened,
            track_seconds: track,
            ended: Ended::Skipped,
        };
        assert!(listen(100.0, Some(200.0)).played_through());
        assert!(!listen(99.0, Some(200.0)).played_through());
        assert!(listen(240.0, Some(900.0)).played_through());
        assert!(!listen(200.0, None).played_through());
        assert!(listen(240.0, None).played_through());
    }
}

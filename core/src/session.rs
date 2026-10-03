//! The play session one person's devices share: one queue, one position, and
//! which device the sound comes out of. The server keeps one per user.
//!
//! The server holds the real one and applies each device's `Op` to it. The
//! app applies the same op to its own copy first, so a tap shows at once and
//! the server's broadcast confirms it a moment later. Both run this code, so
//! the two agree unless another device got there first, and then the server's
//! copy wins.
//!
//! Against a server older than this, the app keeps its copy to itself and is
//! its own output, which is how playback worked before there was a session.

use crate::qobuz::{RemoteTrack, TrackIdentity};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Instant;

/// A connected device, named as it was paired, which is what the output
/// picker lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub device_id: i64,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Bumped by every change, so a copy can tell whether another is newer.
    pub version: u64,
    /// Bumped only when the queue's contents or order change. Ops that name a
    /// row by its index are checked against this, so a drag made against an
    /// order that has since changed is refused rather than moving the wrong
    /// row.
    pub queue_version: u64,
    pub queue: Vec<RemoteTrack>,
    pub index: usize,
    pub playing: bool,
    /// Seconds into the current track, as of when this copy was taken.
    pub position: f64,
    /// Bumped when the track or the position is set on purpose: a jump, a
    /// seek, a new queue, a change of output. The output follows it. What the
    /// output reports about itself never bumps it, so it does not chase its
    /// own echo.
    pub cue: u64,
    /// The device the sound comes out of, if any.
    pub output: Option<i64>,
    /// Devices connected right now, the ones the output picker offers.
    pub devices: Vec<Output>,
}

/// One change to the session, as a device asks for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Replace the queue and play from `index`, which points into `tracks`
    /// as given, before duplicates are dropped.
    Load { tracks: Vec<RemoteTrack>, index: usize },
    /// Add to the end, starting playback if the queue was empty.
    Append { tracks: Vec<RemoteTrack> },
    /// Insert right after the current track.
    PlayNext { tracks: Vec<RemoteTrack> },
    /// Replace everything after the current track, the drawer's sort.
    Upcoming { tracks: Vec<RemoteTrack> },
    Clear,
    Remove { at: usize },
    Move { from: usize, to: usize },
    Shift { at: usize, delta: i64 },
    Jump { index: usize },
    Step { delta: i64 },
    /// The output finished, or skipped, the track at `index`. Carries the
    /// index so a late or repeated report cannot skip twice.
    Ended { index: usize },
    Play,
    Pause,
    Seek { position: f64 },
    /// What the output's audio element is actually doing. Ignored from any
    /// other device.
    Report { index: usize, playing: bool, position: f64 },
    /// Send the sound to another device, or nowhere.
    Output { device_id: Option<i64> },
}

impl Op {
    /// Whether the op names a row by index, and so only means what it meant
    /// against the queue it was made from.
    fn addresses_rows(&self) -> bool {
        matches!(
            self,
            Op::Upcoming { .. }
                | Op::Remove { .. }
                | Op::Move { .. }
                | Op::Shift { .. }
                | Op::Jump { .. }
        )
    }
}

/// What a device sends: the op, and the queue it was looking at.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Command {
    pub queue_version: u64,
    pub op: Op,
}

/// The op pointed at rows of a queue that has changed since.
#[derive(Debug, PartialEq, Eq)]
pub struct Stale;

/// One pushed copy of the session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    /// The device this stream belongs to, so the app knows which entry in
    /// `devices` it is.
    pub you: i64,
    /// Whether `session.queue` was left empty because it has not changed
    /// since the previous update on this stream. A queue is most of the
    /// bytes, and a position report every few seconds should not resend it.
    pub same_queue: bool,
    pub session: Session,
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

fn identities(tracks: &[RemoteTrack]) -> HashSet<TrackIdentity> {
    tracks.iter().map(|track| track.identity()).collect()
}

/// Where the entry at `index` ends up once the one at `from` is taken out and
/// put back at `to`.
///
/// The whole of what a reorder can get wrong: the queue is only a `Vec`, but
/// `index` has to keep pointing at the track that is actually playing.
pub fn index_after_move(index: usize, from: usize, to: usize) -> usize {
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

impl Session {
    pub fn current(&self) -> Option<&RemoteTrack> {
        self.queue.get(self.index)
    }

    /// Apply `op` from device `from`, made against the queue at
    /// `queue_version`.
    pub fn apply(&mut self, op: Op, queue_version: u64, from: i64) -> Result<(), Stale> {
        if op.addresses_rows() && queue_version != self.queue_version {
            return Err(Stale);
        }

        match op {
            Op::Load { tracks, index } => {
                let wanted = tracks.get(index).map(|track| track.identity());
                let tracks = unduplicated(tracks, &mut HashSet::new());
                // Clicking the third row still plays the third row, not
                // whatever slid into its place when a duplicate dropped out.
                let start = wanted
                    .and_then(|id| tracks.iter().position(|track| track.identity() == id))
                    .unwrap_or(0);
                self.queue = tracks;
                self.queue_changed();
                self.jump(start, from);
            }
            Op::Append { tracks } => {
                let start = self.queue.len();
                let tracks = unduplicated(tracks, &mut identities(&self.queue));
                if !tracks.is_empty() {
                    self.queue.extend(tracks);
                    self.queue_changed();
                    if start == 0 {
                        self.jump(0, from);
                    }
                }
            }
            Op::PlayNext { tracks } => {
                let length = self.queue.len();
                let tracks = unduplicated(tracks, &mut identities(&self.queue));
                if !tracks.is_empty() {
                    let at = if length == 0 { 0 } else { (self.index + 1).min(length) };
                    self.queue.splice(at..at, tracks);
                    self.queue_changed();
                    if length == 0 {
                        self.jump(0, from);
                    }
                }
            }
            Op::Upcoming { tracks } => {
                let keep = (self.index + 1).min(self.queue.len());
                self.queue.truncate(keep);
                let tracks = unduplicated(tracks, &mut identities(&self.queue));
                self.queue.extend(tracks);
                self.queue_changed();
            }
            Op::Clear => {
                self.queue.clear();
                self.queue_changed();
                self.index = 0;
                self.stop();
            }
            Op::Remove { at } => self.remove(at),
            Op::Move { from: at, to } => {
                let length = self.queue.len();
                if at < length && to < length && at != to {
                    let track = self.queue.remove(at);
                    self.queue.insert(to, track);
                    self.index = index_after_move(self.index, at, to);
                    self.queue_changed();
                }
            }
            Op::Shift { at, delta } => {
                let target = at as i64 + delta;
                if at < self.queue.len() && target >= 0 && (target as usize) < self.queue.len() {
                    let target = target as usize;
                    self.queue.swap(at, target);
                    if self.index == at {
                        self.index = target;
                    } else if self.index == target {
                        self.index = at;
                    }
                    self.queue_changed();
                }
            }
            Op::Jump { index } => self.jump(index, from),
            Op::Step { delta } => {
                // Stops at the ends rather than wrapping, so a finished queue
                // stays finished.
                let next = self.index as i64 + delta;
                if next >= 0 && (next as usize) < self.queue.len() {
                    self.jump(next as usize, from);
                }
            }
            Op::Ended { index } => {
                if index == self.index {
                    if index + 1 < self.queue.len() {
                        self.jump(index + 1, from);
                    } else {
                        self.playing = false;
                    }
                }
            }
            Op::Play => {
                if !self.queue.is_empty() {
                    self.playing = true;
                    if self.output.is_none() {
                        // Nothing was sounding anywhere, so this device starts
                        // where the session left off.
                        self.output = Some(from);
                        self.cue += 1;
                    }
                }
            }
            Op::Pause => self.playing = false,
            Op::Seek { position } => {
                self.position = position.max(0.0);
                self.cue += 1;
            }
            Op::Report {
                index,
                playing,
                position,
            } => {
                if self.output == Some(from) && index == self.index {
                    self.playing = playing;
                    self.position = position.max(0.0);
                }
            }
            Op::Output { device_id } => {
                if device_id != self.output {
                    self.output = device_id;
                    if device_id.is_none() {
                        self.playing = false;
                    }
                    self.cue += 1;
                }
            }
        }

        self.version += 1;
        Ok(())
    }

    fn queue_changed(&mut self) {
        self.queue_version += 1;
    }

    /// Play the entry at `index` from its start. Claims the output for `from`
    /// when nothing is sounding anywhere, so a fresh session plays where it
    /// was started.
    fn jump(&mut self, index: usize, from: i64) {
        if index >= self.queue.len() {
            return;
        }
        self.index = index;
        self.position = 0.0;
        self.playing = true;
        self.cue += 1;
        if self.output.is_none() {
            self.output = Some(from);
        }
    }

    fn stop(&mut self) {
        self.position = 0.0;
        self.playing = false;
        self.cue += 1;
    }

    /// Drop one entry, keeping `index` on whatever is playing.
    ///
    /// Removing the playing track hands over to whatever slides into its
    /// place. When nothing does, it was last, playback stops and `index` stays
    /// on the track before it, which is where reaching the end of a queue
    /// leaves it too.
    fn remove(&mut self, at: usize) {
        let length = self.queue.len();
        if at >= length {
            return;
        }
        self.queue.remove(at);
        self.queue_changed();

        if at < self.index {
            self.index -= 1;
        } else if at == self.index {
            if at < length - 1 {
                self.position = 0.0;
                self.playing = true;
                self.cue += 1;
            } else {
                self.index = at.saturating_sub(1);
                self.stop();
            }
        }
    }
}

/// A session and the moment its `position` was true, so the position can be
/// carried forward while it plays without anyone reporting it.
#[derive(Clone, Debug)]
pub struct Clocked {
    pub session: Session,
    since: Instant,
}

impl Default for Clocked {
    fn default() -> Self {
        Self::new(Session::default())
    }
}

impl Clocked {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            since: Instant::now(),
        }
    }

    /// Where the current track is now.
    pub fn position(&self) -> f64 {
        let session = &self.session;
        if !session.playing {
            return session.position;
        }
        let moved = session.position + self.since.elapsed().as_secs_f64();
        match session.current().and_then(|track| track.duration) {
            Some(duration) if duration > 0 => moved.min(duration as f64),
            _ => moved,
        }
    }

    /// Fold the time played so far into `position`. Done before every change,
    /// so a pause freezes the position where it actually was.
    pub fn settle(&mut self) {
        self.session.position = self.position();
        self.since = Instant::now();
    }

    pub fn apply(&mut self, op: Op, queue_version: u64, from: i64) -> Result<(), Stale> {
        self.settle();
        self.session.apply(op, queue_version, from)
    }

    /// A copy with the position brought up to now.
    pub fn snapshot(&self) -> Session {
        let mut session = self.session.clone();
        session.position = self.position();
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: i64 = 1;
    const PHONE: i64 = 2;

    fn track(id: i64) -> RemoteTrack {
        RemoteTrack {
            id,
            title: format!("track {id}"),
            streamable: true,
            ..Default::default()
        }
    }

    fn tracks(ids: &[i64]) -> Vec<RemoteTrack> {
        ids.iter().copied().map(track).collect()
    }

    fn ids(session: &Session) -> Vec<i64> {
        session.queue.iter().map(|track| track.id).collect()
    }

    fn apply(session: &mut Session, op: Op) {
        let queue_version = session.queue_version;
        session.apply(op, queue_version, ME).unwrap();
    }

    fn loaded(ids: &[i64], index: usize) -> Session {
        let mut session = Session::default();
        apply(
            &mut session,
            Op::Load {
                tracks: tracks(ids),
                index,
            },
        );
        session
    }

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
        assert_eq!(index_after_move(5, 1, 3), 5);
        assert_eq!(index_after_move(5, 7, 9), 5);
    }

    #[test]
    fn dragging_past_the_playing_track_shifts_it_by_one() {
        assert_eq!(index_after_move(5, 2, 8), 4);
        assert_eq!(index_after_move(5, 8, 2), 6);
    }

    #[test]
    fn loading_starts_on_the_clicked_row_even_when_duplicates_drop_out() {
        let mut list = tracks(&[1, 2, 3]);
        list.insert(1, track(1));
        let mut session = Session::default();
        apply(&mut session, Op::Load { tracks: list, index: 3 });
        assert_eq!(ids(&session), vec![1, 2, 3]);
        assert_eq!(session.current().map(|t| t.id), Some(3));
        assert!(session.playing);
    }

    #[test]
    fn the_first_device_to_play_becomes_the_output() {
        let mut session = Session::default();
        session
            .apply(Op::Load { tracks: tracks(&[1]), index: 0 }, 0, PHONE)
            .unwrap();
        assert_eq!(session.output, Some(PHONE));

        // Another device loading a queue does not take the sound away.
        let queue_version = session.queue_version;
        session
            .apply(Op::Load { tracks: tracks(&[2]), index: 0 }, queue_version, ME)
            .unwrap();
        assert_eq!(session.output, Some(PHONE));
    }

    #[test]
    fn appending_to_an_empty_queue_starts_playing() {
        let mut session = Session::default();
        apply(&mut session, Op::Append { tracks: tracks(&[1, 2]) });
        assert!(session.playing);
        assert_eq!(session.index, 0);

        apply(&mut session, Op::Append { tracks: tracks(&[2, 3]) });
        assert_eq!(ids(&session), vec![1, 2, 3]);
    }

    #[test]
    fn play_next_goes_right_after_the_current_track() {
        let mut session = loaded(&[1, 2, 3], 1);
        apply(&mut session, Op::PlayNext { tracks: tracks(&[9, 8]) });
        assert_eq!(ids(&session), vec![1, 2, 9, 8, 3]);
        assert_eq!(session.index, 1);
    }

    #[test]
    fn removing_the_playing_track_hands_over_to_the_next() {
        let mut session = loaded(&[1, 2, 3], 1);
        let cue = session.cue;
        apply(&mut session, Op::Remove { at: 1 });
        assert_eq!(session.current().map(|t| t.id), Some(3));
        assert!(session.cue > cue);
    }

    #[test]
    fn removing_the_last_playing_track_stops() {
        let mut session = loaded(&[1, 2], 1);
        apply(&mut session, Op::Remove { at: 1 });
        assert_eq!(session.index, 0);
        assert!(!session.playing);
    }

    #[test]
    fn a_row_op_against_an_old_queue_is_refused() {
        let mut session = loaded(&[1, 2, 3], 0);
        let seen = session.queue_version;
        apply(&mut session, Op::Append { tracks: tracks(&[4]) });
        assert_eq!(session.apply(Op::Remove { at: 2 }, seen, ME), Err(Stale));
        assert_eq!(ids(&session), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_transport_op_does_not_care_about_the_queue_version() {
        let mut session = loaded(&[1, 2, 3], 0);
        assert_eq!(session.apply(Op::Pause, 0, ME), Ok(()));
        assert!(!session.playing);
    }

    #[test]
    fn a_repeated_end_skips_only_once() {
        let mut session = loaded(&[1, 2, 3], 0);
        apply(&mut session, Op::Ended { index: 0 });
        apply(&mut session, Op::Ended { index: 0 });
        assert_eq!(session.index, 1);
    }

    #[test]
    fn the_end_of_the_queue_stops_it() {
        let mut session = loaded(&[1, 2], 1);
        apply(&mut session, Op::Ended { index: 1 });
        assert_eq!(session.index, 1);
        assert!(!session.playing);
    }

    #[test]
    fn only_the_output_reports_its_position() {
        let mut session = loaded(&[1, 2], 0);
        session
            .apply(Op::Report { index: 0, playing: true, position: 40.0 }, 0, PHONE)
            .unwrap();
        assert_eq!(session.position, 0.0);

        apply(&mut session, Op::Report { index: 0, playing: true, position: 40.0 });
        assert_eq!(session.position, 40.0);
    }

    #[test]
    fn a_report_does_not_move_the_cue() {
        let mut session = loaded(&[1], 0);
        let cue = session.cue;
        apply(&mut session, Op::Report { index: 0, playing: false, position: 3.0 });
        assert_eq!(session.cue, cue);
    }

    #[test]
    fn changing_output_keeps_the_position_and_moves_the_cue() {
        let mut session = loaded(&[1], 0);
        apply(&mut session, Op::Seek { position: 30.0 });
        let cue = session.cue;
        apply(&mut session, Op::Output { device_id: Some(PHONE) });
        assert_eq!(session.output, Some(PHONE));
        assert_eq!(session.position, 30.0);
        assert!(session.cue > cue);
    }

    #[test]
    fn upcoming_replaces_only_what_follows_the_current_track() {
        let mut session = loaded(&[1, 2, 3, 4], 1);
        apply(&mut session, Op::Upcoming { tracks: tracks(&[4, 3, 1]) });
        assert_eq!(ids(&session), vec![1, 2, 4, 3]);
    }

    #[test]
    fn a_paused_position_holds_still() {
        let mut clocked = Clocked::new(loaded(&[1], 0));
        clocked.apply(Op::Pause, 0, ME).unwrap();
        let held = clocked.position();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(clocked.position(), held);
    }
}

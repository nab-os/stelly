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

use crate::api::Recipe;
use crate::catalogue::{RemoteAlbum, RemoteTrack, TrackIdentity};
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
    /// The output's volume, 0.0 to 1.0, so any device can show and turn it.
    /// `None` until the output has said what it is, which it does on taking
    /// over, since each device keeps its own level. Absent from servers older
    /// than it.
    #[serde(default)]
    pub volume: Option<f64>,
    /// Runs of the queue that came in together and are shown, moved and
    /// dropped as one: an album, a generated list. Last, after the queue:
    /// the server cuts the session's JSON at the queue, and only fields
    /// without text may come before it. Absent from servers older than it.
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// Tracks of the queue that belong together. Rows can be dragged in and out
/// of one, so its tracks need not be a single run; the queue holds a track
/// once, so its id is enough to say which row is a member.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// Unique for as long as the session lives: the session's version when
    /// the group was made.
    pub id: u64,
    pub kind: GroupKind,
    /// The members still in the queue, in no particular order: the queue's
    /// is theirs.
    pub tracks: Vec<i64>,
}

/// What brought a group in, which is what its header says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GroupKind {
    Album {
        album_id: String,
        title: String,
        artist: String,
        #[serde(default)]
        image: Option<String>,
    },
    /// What generating made, from `recipe`, which a reroll runs again.
    Generated { recipe: Recipe },
}

impl GroupKind {
    pub fn album(album: &RemoteAlbum) -> Self {
        GroupKind::Album {
            album_id: album.id.clone(),
            title: album.title.clone(),
            artist: album.artist.clone(),
            image: album.image.clone(),
        }
    }
}

/// One change to the session, as a device asks for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Replace the queue and play from `index`, which points into `tracks`
    /// as given, before duplicates are dropped. With a `group`, the new queue
    /// is one, an album played whole, say. Each `group` is left out when
    /// `None`, so a server older than groups still reads the op.
    Load {
        tracks: Vec<RemoteTrack>,
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupKind>,
    },
    /// Add to the end, starting playback if the queue was empty.
    Append {
        tracks: Vec<RemoteTrack>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupKind>,
    },
    /// Insert right after the current track.
    PlayNext {
        tracks: Vec<RemoteTrack>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupKind>,
    },
    /// Queue a group in place of the upcoming rest of `replace`, a reroll,
    /// or else after the last upcoming generated group, or after the rest of
    /// the group playing, or right after the current track: generated lists
    /// queue up one after the other, without cutting into an album. Tracks
    /// it repeats from what is still to come move into it; what has played
    /// or is playing is not queued again.
    Section {
        tracks: Vec<RemoteTrack>,
        kind: GroupKind,
        #[serde(default)]
        replace: Option<u64>,
    },
    /// Move every row of a group, in their order, to just before the row now
    /// at `to`, or to the end when `to` is the queue's length.
    MoveGroup { group: u64, to: usize },
    /// Take a group's rows still to come out of the queue, and forget it:
    /// those already played, and the one playing, stay as plain rows.
    DropGroup { group: u64 },
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
    /// Set the output's volume. From the output, what its system says it is;
    /// from any other device, a request the output follows.
    Volume { level: f64 },
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
                | Op::MoveGroup { .. }
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
    /// The queue as a change to the one this stream sent before, for an app
    /// that asked for changes: `session.queue` is left empty then. Absent
    /// from servers older than it, and from updates to apps that did not ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub splice: Option<Splice>,
    pub session: Session,
}

/// A queue as a change to an earlier one: `removed` entries at `at` replaced
/// by `tracks`. An append, a removal, a drag or a play-next each come down to
/// one, a few tracks rather than the whole queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Splice {
    /// The `queue_version` of the queue it changes. Only that queue, exactly,
    /// gives the right result, which is what makes the change safe to send.
    pub base: u64,
    pub at: usize,
    pub removed: usize,
    pub tracks: Vec<RemoteTrack>,
}

impl Splice {
    /// The change from `before`, at `base`, to `after`: whatever lies between
    /// what the two start and end with in common.
    pub fn between(base: u64, before: &[RemoteTrack], after: &[RemoteTrack]) -> Self {
        let head = before.iter().zip(after).take_while(|(a, b)| a == b).count();
        let tail = before[head..]
            .iter()
            .rev()
            .zip(after[head..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        Splice {
            base,
            at: head,
            removed: before.len() - head - tail,
            tracks: after[head..after.len() - tail].to_vec(),
        }
    }

    /// Make the change to `queue`, which has to be the one at `base`. False,
    /// and `queue` left alone, when the change does not fit it.
    pub fn apply(&self, queue: &mut Vec<RemoteTrack>) -> bool {
        let Some(end) = self.at.checked_add(self.removed).filter(|&end| end <= queue.len()) else {
            return false;
        };
        queue.splice(self.at..end, self.tracks.iter().cloned());
        true
    }
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

fn ids(tracks: &[RemoteTrack]) -> Vec<i64> {
    tracks.iter().map(|track| track.id).collect()
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

        let queue_version = self.queue_version;
        match op {
            Op::Load { tracks, index, group } => {
                let wanted = tracks.get(index).map(|track| track.identity());
                let tracks = unduplicated(tracks, &mut HashSet::new());
                // Clicking the third row still plays the third row, not
                // whatever slid into its place when a duplicate dropped out.
                let start = wanted
                    .and_then(|id| tracks.iter().position(|track| track.identity() == id))
                    .unwrap_or(0);
                self.groups.clear();
                if let Some(kind) = group {
                    self.group(kind, None, ids(&tracks));
                }
                self.queue = tracks;
                self.queue_changed();
                self.jump(start, from);
            }
            Op::Append { tracks, group } => {
                let start = self.queue.len();
                let tracks = unduplicated(tracks, &mut identities(&self.queue));
                if !tracks.is_empty() {
                    if let Some(kind) = group {
                        self.group(kind, None, ids(&tracks));
                    }
                    self.queue.extend(tracks);
                    self.queue_changed();
                    if start == 0 {
                        self.jump(0, from);
                    }
                }
            }
            Op::PlayNext { tracks, group } => {
                let length = self.queue.len();
                let tracks = unduplicated(tracks, &mut identities(&self.queue));
                if !tracks.is_empty() {
                    if let Some(kind) = group {
                        self.group(kind, None, ids(&tracks));
                    }
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
            Op::Section { tracks, kind, replace } => self.section(tracks, kind, replace, from),
            Op::MoveGroup { group, to } => self.move_group(group, to),
            Op::DropGroup { group } => self.drop_group(group),
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
                    // The new output has a level of its own, and says so.
                    self.volume = None;
                    if device_id.is_none() {
                        self.playing = false;
                    }
                    self.cue += 1;
                }
            }
            Op::Volume { level } => {
                if level.is_finite() {
                    self.volume = Some(level.clamp(0.0, 1.0));
                }
            }
        }

        if self.queue_version != queue_version {
            self.tidy_groups();
        }
        self.version += 1;
        Ok(())
    }

    fn queue_changed(&mut self) {
        self.queue_version += 1;
    }

    /// The group the track with this id is in.
    pub fn group_of(&self, track_id: i64) -> Option<&Group> {
        self.groups.iter().find(|group| group.tracks.contains(&track_id))
    }

    /// Make `tracks` a group of `kind`, or add them to `into` when it is
    /// still around. A track is in one group at most, so they leave any
    /// other.
    fn group(&mut self, kind: GroupKind, into: Option<u64>, tracks: Vec<i64>) {
        let joining: HashSet<i64> = tracks.iter().copied().collect();
        for group in &mut self.groups {
            group.tracks.retain(|id| !joining.contains(id));
        }
        match into.and_then(|id| self.groups.iter_mut().find(|group| group.id == id)) {
            Some(group) => {
                group.kind = kind;
                group.tracks.extend(tracks);
            }
            None if !tracks.is_empty() => self.groups.push(Group {
                // Bumped once per op, so no two groups share it.
                id: self.version + 1,
                kind,
                tracks,
            }),
            None => {}
        }
    }

    /// Forget members that left the queue, and groups left with none.
    fn tidy_groups(&mut self) {
        let queued: HashSet<i64> = self.queue.iter().map(|track| track.id).collect();
        for group in &mut self.groups {
            group.tracks.retain(|id| queued.contains(id));
        }
        self.groups.retain(|group| !group.tracks.is_empty());
    }

    fn members(&self, group: u64) -> HashSet<i64> {
        self.groups
            .iter()
            .find(|candidate| candidate.id == group)
            .map(|group| group.tracks.iter().copied().collect())
            .unwrap_or_default()
    }

    /// See `Op::Section`.
    fn section(&mut self, tracks: Vec<RemoteTrack>, kind: GroupKind, replace: Option<u64>, from: i64) {
        if self.queue.is_empty() {
            let tracks = unduplicated(tracks, &mut HashSet::new());
            if !tracks.is_empty() {
                self.groups.clear();
                self.group(kind, None, ids(&tracks));
                self.queue = tracks;
                self.queue_changed();
                self.jump(0, from);
            }
            return;
        }

        let index = self.index.min(self.queue.len() - 1);
        let new = unduplicated(tracks, &mut identities(&self.queue[..=index]));
        let fresh = identities(&new);
        let into = replace.filter(|group| self.groups.iter().any(|candidate| candidate.id == *group));
        let old = into.map(|group| self.members(group)).unwrap_or_default();

        let upcoming = self.queue.split_off(index + 1);
        let at = upcoming
            .iter()
            .position(|track| old.contains(&track.id))
            .unwrap_or_else(|| self.landing(&upcoming));
        let keep = |tracks: &[RemoteTrack]| -> Vec<RemoteTrack> {
            tracks
                .iter()
                .filter(|track| !old.contains(&track.id) && !fresh.contains(&track.identity()))
                .cloned()
                .collect()
        };
        let mut placed = keep(&upcoming[..at]);
        placed.extend(new.iter().cloned());
        placed.extend(keep(&upcoming[at..]));
        self.queue.extend(placed);
        // The old rows still to come have left the queue, and leave the
        // group with it, below; the played ones stay in it.
        self.group(kind, into, ids(&new));
        self.queue_changed();
    }

    /// Where in `upcoming` a new generated group goes: after the last
    /// generated one still to come, or else after the rest of the group
    /// playing.
    fn landing(&self, upcoming: &[RemoteTrack]) -> usize {
        let generated: HashSet<i64> = self
            .groups
            .iter()
            .filter(|group| matches!(group.kind, GroupKind::Generated { .. }))
            .flat_map(|group| group.tracks.iter().copied())
            .collect();
        if let Some(last) = upcoming.iter().rposition(|track| generated.contains(&track.id)) {
            return last + 1;
        }
        match self.current().and_then(|track| self.group_of(track.id)) {
            Some(playing) => upcoming.iter().take_while(|track| playing.tracks.contains(&track.id)).count(),
            None => 0,
        }
    }

    /// See `Op::MoveGroup`.
    fn move_group(&mut self, group: u64, to: usize) {
        let members = self.members(group);
        if members.is_empty() || to > self.queue.len() {
            return;
        }
        let before = self.queue[..to].iter().filter(|track| !members.contains(&track.id)).count();
        let current = self.current().map(|track| track.id);
        let (moving, mut rest): (Vec<RemoteTrack>, Vec<RemoteTrack>) =
            self.queue.iter().cloned().partition(|track| members.contains(&track.id));
        rest.splice(before..before, moving);
        if rest == self.queue {
            return;
        }
        self.queue = rest;
        if let Some(current) = current {
            self.index = self.queue.iter().position(|track| track.id == current).unwrap_or(self.index);
        }
        self.queue_changed();
    }

    /// See `Op::DropGroup`.
    fn drop_group(&mut self, group: u64) {
        let members = self.members(group);
        self.groups.retain(|candidate| candidate.id != group);
        let keep = (self.index + 1).min(self.queue.len());
        let length = self.queue.len();
        let mut at = 0;
        self.queue.retain(|track| {
            at += 1;
            at <= keep || !members.contains(&track.id)
        });
        if self.queue.len() != length {
            self.queue_changed();
        }
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
                group: None,
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
        apply(&mut session, Op::Load { tracks: list, index: 3, group: None });
        assert_eq!(ids(&session), vec![1, 2, 3]);
        assert_eq!(session.current().map(|t| t.id), Some(3));
        assert!(session.playing);
    }

    #[test]
    fn the_first_device_to_play_becomes_the_output() {
        let mut session = Session::default();
        session
            .apply(Op::Load { tracks: tracks(&[1]), index: 0, group: None }, 0, PHONE)
            .unwrap();
        assert_eq!(session.output, Some(PHONE));

        // Another device loading a queue does not take the sound away.
        let queue_version = session.queue_version;
        session
            .apply(Op::Load { tracks: tracks(&[2]), index: 0, group: None }, queue_version, ME)
            .unwrap();
        assert_eq!(session.output, Some(PHONE));
    }

    #[test]
    fn appending_to_an_empty_queue_starts_playing() {
        let mut session = Session::default();
        apply(&mut session, Op::Append { tracks: tracks(&[1, 2]), group: None });
        assert!(session.playing);
        assert_eq!(session.index, 0);

        apply(&mut session, Op::Append { tracks: tracks(&[2, 3]), group: None });
        assert_eq!(ids(&session), vec![1, 2, 3]);
    }

    #[test]
    fn play_next_goes_right_after_the_current_track() {
        let mut session = loaded(&[1, 2, 3], 1);
        apply(&mut session, Op::PlayNext { tracks: tracks(&[9, 8]), group: None });
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
        apply(&mut session, Op::Append { tracks: tracks(&[4]), group: None });
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
    fn the_volume_is_kept_in_range_and_dropped_with_the_output() {
        let mut session = loaded(&[1], 0);
        let cue = session.cue;
        apply(&mut session, Op::Volume { level: 1.5 });
        assert_eq!(session.volume, Some(1.0));
        assert_eq!(session.cue, cue);

        apply(&mut session, Op::Output { device_id: Some(PHONE) });
        assert_eq!(session.volume, None);
    }

    #[test]
    fn a_session_from_an_older_server_has_no_volume() {
        let json = r#"{"version":1,"queue_version":0,"queue":[],"index":0,"playing":false,
            "position":0.0,"cue":0,"output":null,"devices":[]}"#;
        let session: Session = serde_json::from_str(json).unwrap();
        assert_eq!(session.volume, None);
        assert!(session.groups.is_empty());
    }

    #[test]
    fn upcoming_replaces_only_what_follows_the_current_track() {
        let mut session = loaded(&[1, 2, 3, 4], 1);
        apply(&mut session, Op::Upcoming { tracks: tracks(&[4, 3, 1]) });
        assert_eq!(ids(&session), vec![1, 2, 4, 3]);
    }

    /// Every queue op, from one queue, and the splice between before and
    /// after rebuilding the after from the before.
    #[test]
    fn a_splice_rebuilds_the_queue_after_any_op() {
        let ops = [
            Op::Append { tracks: tracks(&[9, 1]), group: None },
            Op::PlayNext { tracks: tracks(&[8, 7]), group: Some(album("x")) },
            Op::Section { tracks: tracks(&[9, 4]), kind: radio(1), replace: None },
            Op::MoveGroup { group: 0, to: 0 },
            Op::DropGroup { group: 0 },
            Op::Upcoming { tracks: tracks(&[6, 5, 4]) },
            Op::Remove { at: 0 },
            Op::Remove { at: 3 },
            Op::Remove { at: 5 },
            Op::Move { from: 1, to: 4 },
            Op::Move { from: 5, to: 0 },
            Op::Shift { at: 2, delta: 1 },
            Op::Load { tracks: tracks(&[3, 2]), index: 0, group: None },
            Op::Clear,
        ];
        for op in ops {
            let mut session = loaded(&[1, 2, 3, 4, 5, 6], 2);
            let before = session.queue.clone();
            let base = session.queue_version;
            apply(&mut session, op.clone());
            let splice = Splice::between(base, &before, &session.queue);
            let mut rebuilt = before.clone();
            assert!(splice.apply(&mut rebuilt), "{op:?}");
            assert_eq!(rebuilt, session.queue, "{op:?}");
        }
    }

    #[test]
    fn a_splice_is_only_the_part_that_changed() {
        let before = tracks(&[1, 2, 3, 4, 5]);
        let splice = Splice::between(4, &before, &tracks(&[1, 2, 3, 4, 5, 6]));
        assert_eq!((splice.at, splice.removed, splice.tracks), (5, 0, tracks(&[6])));
        let splice = Splice::between(4, &before, &tracks(&[1, 4, 2, 3, 5]));
        assert_eq!((splice.at, splice.removed, splice.tracks), (1, 3, tracks(&[4, 2, 3])));
        // A repeated track at the seam is not counted twice.
        let splice = Splice::between(4, &tracks(&[1, 1]), &tracks(&[1, 1, 1]));
        assert_eq!((splice.at, splice.removed, splice.tracks.len()), (2, 0, 1));
    }

    #[test]
    fn a_splice_that_does_not_fit_leaves_the_queue_alone() {
        let splice = Splice { base: 1, at: 3, removed: 2, tracks: tracks(&[9]) };
        let mut queue = tracks(&[1, 2, 3, 4]);
        assert!(!splice.apply(&mut queue));
        assert_eq!(queue, tracks(&[1, 2, 3, 4]));
    }

    #[test]
    fn an_update_from_an_older_server_has_no_splice() {
        let json = r#"{"you":1,"same_queue":true,"session":{"version":1,"queue_version":0,
            "queue":[],"index":0,"playing":false,"position":0.0,"cue":0,"output":null,"devices":[]}}"#;
        let update: Update = serde_json::from_str(json).unwrap();
        assert_eq!(update.splice, None);
    }

    fn album(title: &str) -> GroupKind {
        GroupKind::Album {
            album_id: title.into(),
            title: title.into(),
            artist: "someone".into(),
            image: None,
        }
    }

    fn radio(seed: i64) -> GroupKind {
        GroupKind::Generated {
            recipe: Recipe::Radio { seed },
        }
    }

    /// The groups as the rows they cover, in queue order.
    fn runs(session: &Session) -> Vec<Vec<i64>> {
        session
            .groups
            .iter()
            .map(|group| ids(session).into_iter().filter(|id| group.tracks.contains(id)).collect())
            .collect()
    }

    fn last_group(session: &Session) -> u64 {
        session.groups.last().expect("a group").id
    }

    #[test]
    fn an_album_played_whole_is_one_group() {
        let mut session = Session::default();
        apply(&mut session, Op::Load { tracks: tracks(&[1, 2, 3]), index: 0, group: Some(album("a")) });
        assert_eq!(runs(&session), vec![vec![1, 2, 3]]);

        // A new queue forgets the old one's groups.
        apply(&mut session, Op::Load { tracks: tracks(&[4]), index: 0, group: None });
        assert!(session.groups.is_empty());
    }

    #[test]
    fn a_queued_album_groups_only_what_it_added() {
        let mut session = loaded(&[1, 2], 0);
        apply(&mut session, Op::Append { tracks: tracks(&[2, 3, 4]), group: Some(album("a")) });
        assert_eq!(runs(&session), vec![vec![3, 4]]);
    }

    #[test]
    fn generated_groups_queue_one_after_the_other() {
        let mut session = loaded(&[1, 2, 3], 0);
        apply(&mut session, Op::Section { tracks: tracks(&[7, 8]), kind: radio(1), replace: None });
        assert_eq!(ids(&session), vec![1, 7, 8, 2, 3]);
        apply(&mut session, Op::Section { tracks: tracks(&[9]), kind: radio(7), replace: None });
        assert_eq!(ids(&session), vec![1, 7, 8, 9, 2, 3]);
        assert_eq!(runs(&session), vec![vec![7, 8], vec![9]]);
    }

    #[test]
    fn a_generated_group_does_not_cut_into_the_album_playing() {
        let mut session = Session::default();
        apply(&mut session, Op::Load { tracks: tracks(&[1, 2, 3, 4]), index: 0, group: Some(album("a")) });
        apply(&mut session, Op::Section { tracks: tracks(&[7]), kind: radio(1), replace: None });
        assert_eq!(ids(&session), vec![1, 2, 3, 4, 7]);
    }

    #[test]
    fn a_generated_group_into_an_empty_queue_plays() {
        let mut session = Session::default();
        apply(&mut session, Op::Section { tracks: tracks(&[7, 8]), kind: radio(1), replace: None });
        assert_eq!(ids(&session), vec![7, 8]);
        assert!(session.playing);
        assert_eq!(runs(&session), vec![vec![7, 8]]);
    }

    /// Playing 1, then the user's 2, the old group's 5 and 6, then the
    /// user's 9.
    #[test]
    fn a_reroll_takes_the_old_groups_place_and_keeps_its_id() {
        let mut session = loaded(&[1, 2, 9], 0);
        apply(&mut session, Op::Section { tracks: tracks(&[5, 6]), kind: radio(1), replace: None });
        let group = last_group(&session);
        apply(&mut session, Op::Move { from: 3, to: 1 });
        assert_eq!(ids(&session), vec![1, 2, 5, 6, 9]);

        apply(&mut session, Op::Section { tracks: tracks(&[7, 8]), kind: radio(1), replace: Some(group) });
        assert_eq!(ids(&session), vec![1, 2, 7, 8, 9]);
        assert_eq!(session.groups.len(), 1);
        assert_eq!(last_group(&session), group);
        assert_eq!(runs(&session), vec![vec![7, 8]]);
    }

    #[test]
    fn a_reroll_keeps_the_rows_already_played() {
        let mut session = loaded(&[1], 0);
        apply(&mut session, Op::Section { tracks: tracks(&[5, 6]), kind: radio(1), replace: None });
        let group = last_group(&session);
        apply(&mut session, Op::Jump { index: 1 });
        apply(&mut session, Op::Section { tracks: tracks(&[7, 5]), kind: radio(1), replace: Some(group) });
        // 5 is playing, so it is not queued again behind itself.
        assert_eq!(ids(&session), vec![1, 5, 7]);
        assert_eq!(runs(&session), vec![vec![5, 7]]);
    }

    #[test]
    fn a_track_already_queued_moves_into_the_new_group() {
        let mut session = loaded(&[1, 2, 3], 0);
        apply(&mut session, Op::Section { tracks: tracks(&[3, 4]), kind: radio(1), replace: None });
        assert_eq!(ids(&session), vec![1, 3, 4, 2]);
    }

    #[test]
    fn a_track_is_in_one_group_at_most() {
        let mut session = Session::default();
        apply(&mut session, Op::Load { tracks: tracks(&[1, 2, 3]), index: 0, group: Some(album("a")) });
        apply(&mut session, Op::Section { tracks: tracks(&[3, 4]), kind: radio(1), replace: None });
        assert_eq!(runs(&session), vec![vec![1, 2], vec![3, 4]]);
    }

    #[test]
    fn moving_a_group_moves_all_its_rows_and_keeps_the_playing_track() {
        let mut session = loaded(&[1, 2, 3], 1);
        apply(&mut session, Op::Append { tracks: tracks(&[4, 5]), group: Some(album("a")) });
        let group = last_group(&session);
        apply(&mut session, Op::MoveGroup { group, to: 0 });
        assert_eq!(ids(&session), vec![4, 5, 1, 2, 3]);
        assert_eq!(session.current().map(|t| t.id), Some(2));

        // Back down, before the row now at 4.
        apply(&mut session, Op::MoveGroup { group, to: 4 });
        assert_eq!(ids(&session), vec![1, 2, 4, 5, 3]);
        assert_eq!(session.current().map(|t| t.id), Some(2));

        apply(&mut session, Op::MoveGroup { group, to: 5 });
        assert_eq!(ids(&session), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn moving_a_group_against_an_old_queue_is_refused() {
        let mut session = loaded(&[1], 0);
        apply(&mut session, Op::Append { tracks: tracks(&[4, 5]), group: Some(album("a")) });
        let group = last_group(&session);
        let seen = session.queue_version;
        apply(&mut session, Op::Append { tracks: tracks(&[6]), group: None });
        assert_eq!(session.apply(Op::MoveGroup { group, to: 0 }, seen, ME), Err(Stale));
    }

    #[test]
    fn dropping_a_group_keeps_what_has_played_and_is_playing() {
        let mut session = Session::default();
        apply(&mut session, Op::Load { tracks: tracks(&[1, 2, 3]), index: 1, group: Some(album("a")) });
        apply(&mut session, Op::Append { tracks: tracks(&[4]), group: None });
        let group = session.groups[0].id;
        apply(&mut session, Op::DropGroup { group });
        assert_eq!(ids(&session), vec![1, 2, 4]);
        assert_eq!(session.current().map(|t| t.id), Some(2));
        assert!(session.groups.is_empty());
    }

    #[test]
    fn a_group_whose_rows_all_left_is_forgotten() {
        let mut session = loaded(&[1], 0);
        apply(&mut session, Op::Append { tracks: tracks(&[4, 5]), group: Some(album("a")) });
        apply(&mut session, Op::Remove { at: 1 });
        assert_eq!(runs(&session), vec![vec![5]]);
        apply(&mut session, Op::Remove { at: 1 });
        assert!(session.groups.is_empty());
    }

    #[test]
    fn groups_outlive_a_trip_through_json() {
        let mut session = loaded(&[1], 0);
        apply(&mut session, Op::Section { tracks: tracks(&[5, 6]), kind: radio(1), replace: None });
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.groups, session.groups);
    }

    #[test]
    fn an_op_without_a_group_still_reads() {
        let op: Op = serde_json::from_str(r#"{"op":"append","tracks":[]}"#).unwrap();
        assert_eq!(op, Op::Append { tracks: vec![], group: None });
        // And is sent without one, for a server older than groups.
        assert_eq!(serde_json::to_string(&op).unwrap(), r#"{"op":"append","tracks":[]}"#);
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

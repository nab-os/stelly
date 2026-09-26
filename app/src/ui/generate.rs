//! Making a sequence of tracks out of the space.
//!
//! One panel with four modes, because they differ only in how the sequence is
//! chosen:
//!   - **neighbours**: the k most similar tracks.
//!   - **radio**: a greedy walk, pushed away from artists already used.
//!   - **path**: A to B, shortest route or evenly paced.
//!   - **drift**: away from a track, towards a phrase.

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use crate::backend::backend;
use crate::paths::{Constraints, Step};
use crate::qobuz::RemoteTrack;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Neighbours,
    Radio,
    Path,
    Drift,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Neighbours, Mode::Radio, Mode::Path, Mode::Drift];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Neighbours => "neighbours",
            Mode::Radio => "radio",
            Mode::Path => "path",
            Mode::Drift => "drift",
        }
    }

    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Mode::Neighbours => "The most similar tracks to the selection.",
            Mode::Radio => "Keep jumping to the nearest track not yet played.",
            Mode::Path => "Travel from A to B through the space.",
            Mode::Drift => "Walk away from the selection, towards a phrase.",
        }
    }
}

/// What produced the result currently on screen.
///
/// Generation used to follow the selection, so a result was never older than
/// the last click and needed no label. Now that it is asked for, it outlives
/// both the selection and the mode that made it, and twenty unlabelled
/// tracks under a tab that no longer matches them is a puzzle, not a list.
#[derive(Clone, PartialEq, Debug)]
pub enum Recipe {
    Neighbours { seed: i64 },
    Radio { seed: i64 },
    Path { a: i64, b: i64, even: bool },
    Drift { seed: i64, phrase: String },
}

#[derive(Clone, Copy)]
pub struct Generator {
    pub mode: Signal<Mode>,
    pub result: Signal<Vec<Step>>,
    /// What produced `result`, for its heading. `None` when there is none.
    pub produced_by: Signal<Option<Recipe>>,
    /// A walk is running. Set before the task is spawned so one paint gets
    /// through: the walk itself takes the engine lock and holds it, and on
    /// the desktop's single-threaded runtime that blocks the UI outright.
    /// Radio is the one that shows, it scans the corpus once per step.
    pub busy: Signal<bool>,
    /// The weights moved since `result` was produced, so the distances behind
    /// it no longer hold. Kept separate from clearing: the rows stay on
    /// screen and say they are out of date, rather than vanishing.
    pub stale: Signal<bool>,
    /// How many tracks to produce, for the modes that take a count.
    pub count: Signal<usize>,
    /// Similarity subtracted per prior use of an artist, in radio mode.
    pub artist_penalty: Signal<f32>,
    pub phrase: Signal<String>,
    pub from: Signal<Option<i64>>,
    pub to: Signal<Option<i64>>,
    /// Path shape: evenly paced interpolation rather than the shortest route.
    pub even: Signal<bool>,
    pub status: Signal<Option<String>>,
    /// Whether the backend can turn a phrase into an embedding. Half of what
    /// drift needs; the engine answers the other half.
    pub tower: Signal<bool>,
}

impl Generator {
    pub fn new() -> Self {
        Self {
            mode: Signal::new(Mode::Neighbours),
            result: Signal::new(Vec::new()),
            produced_by: Signal::new(None),
            busy: Signal::new(false),
            stale: Signal::new(false),
            count: Signal::new(20),
            // 0.10 measured out as 19.4 distinct artists per 20 tracks
            // against 14.8 unpenalised, for 0.786 similarity against 0.796.
            artist_penalty: Signal::new(0.10),
            phrase: Signal::new(String::new()),
            from: Signal::new(None),
            to: Signal::new(None),
            even: Signal::new(false),
            status: Signal::new(None),
            tower: Signal::new(false),
        }
    }

    /// Whether drift is offerable at all.
    pub(crate) fn can_steer(&self) -> bool {
        *self.tower.read() && crate::engine().lock().unwrap().has_audio_embeddings()
    }

    /// Whether the current mode has what it needs.
    pub(crate) fn ready(&self, selected: Option<i64>) -> bool {
        match *self.mode.peek() {
            Mode::Neighbours | Mode::Radio => selected.is_some(),
            Mode::Path => self.from.peek().is_some() && self.to.peek().is_some(),
            Mode::Drift => selected.is_some() && !self.phrase.peek().trim().is_empty(),
        }
    }

    /// Produce a sequence. Spawned rather than inline because drift has to
    /// await: turning a phrase into a CLAP embedding may be a round trip. The
    /// walk itself is always local.
    pub fn run(self, selected: Option<i64>) {
        let mut generator = self;
        generator.status.set(None);
        generator.busy.set(true);

        let count = *self.count.peek();
        let mode = *self.mode.peek();
        let penalty = *self.artist_penalty.peek();
        let even = *self.even.peek();
        let phrase = self.phrase.peek().clone();
        let (from, to) = (*self.from.peek(), *self.to.peek());

        // Built from the same peeked inputs the walk uses, so the label can
        // never describe a different run than the rows below it.
        let recipe = match mode {
            Mode::Neighbours => selected.map(|seed| Recipe::Neighbours { seed }),
            Mode::Radio => selected.map(|seed| Recipe::Radio { seed }),
            Mode::Path => match (from, to) {
                (Some(a), Some(b)) => Some(Recipe::Path { a, b, even }),
                _ => None,
            },
            Mode::Drift => selected.map(|seed| Recipe::Drift {
                seed,
                phrase: phrase.clone(),
            }),
        };

        // Forever, not scoped: `request` is called from the row menu, which
        // closes in the same click, and a scoped task dies with it before it
        // is ever polled, leaving `busy` set and the button on "working…".
        spawn_forever(async move {
            let produced = match mode {
                Mode::Neighbours => selected.map(|id| {
                    crate::engine()
                        .lock()
                        .unwrap()
                        .navigator
                        .neighbours(id, count, false)
                }),
                Mode::Radio => selected.map(|id| {
                    crate::engine().lock().unwrap().navigator.radio_nearest(
                        id,
                        count,
                        penalty,
                        &Constraints::default(),
                    )
                }),
                Mode::Path => match (from, to) {
                    (Some(a), Some(b)) => {
                        let mut guard = crate::engine().lock().unwrap();
                        Some(if even {
                            guard.navigator.interpolate(a, b, count, &Constraints::default())
                        } else {
                            guard.navigator.graph_path(a, b, 16)
                        })
                    }
                    _ => None,
                },
                Mode::Drift => match selected {
                    Some(id) => {
                        // Embed first, then walk. The await is outside the
                        // lock: holding the engine across a round trip would
                        // freeze every slider.
                        match backend().embed(&phrase).await {
                            Ok(embedding) => {
                                let guard = &mut *crate::engine().lock().unwrap();
                                Some(guard.navigator.drift_to_text(
                                    id,
                                    &embedding,
                                    count,
                                    5,
                                    &Constraints::default(),
                                ))
                            }
                            Err(err) => {
                                generator.status.set(Some(format!("{err:#}")));
                                None
                            }
                        }
                    }
                    None => None,
                },
            };

            let Some(produced) = produced else {
                generator.busy.set(false);
                return;
            };
            let produced = dedup_by_recording(produced);
            if produced.is_empty() {
                generator
                    .status
                    .set(Some("nothing found, try a different start".into()));
            }
            generator.result.set(produced);
            generator.produced_by.set(recipe);
            generator.stale.set(false);
            generator.busy.set(false);
        });
    }

    /// Run a mode directly, rather than whichever one the panel happens to be
    /// showing. Switches the panel to match, so its inputs describe the rows
    /// that just appeared.
    pub fn request(self, mode: Mode, seed: i64) {
        let mut generator = self;
        generator.mode.set(mode);
        generator.run(Some(seed));
    }

    /// Set one end of a path. Runs once both ends are known, two deliberate
    /// acts, so nothing is produced by accident, but naming the second end
    /// does not then need a third click to be worth anything.
    pub fn set_path_end(self, end: PathEnd, id: i64) {
        let mut generator = self;
        generator.mode.set(Mode::Path);
        match end {
            PathEnd::A => generator.from.set(Some(id)),
            PathEnd::B => generator.to.set(Some(id)),
        }
        if generator.from.peek().is_some() && generator.to.peek().is_some() {
            generator.run(None);
        }
    }

    /// Show drift's inputs without running it. Drift seeds from the selection,
    /// which the caller sets; what it still lacks is the phrase, and guessing
    /// one would be inventing the user's intent.
    pub fn aim_drift(self) {
        let mut generator = self;
        generator.mode.set(Mode::Drift);
    }

    /// Run a recipe again, restoring the inputs that produced it. Re-running
    /// "whatever the panel is showing now" would quietly answer a different
    /// question, since the mode and the selection have both been free to move
    /// since the result was made.
    pub fn rerun(self, recipe: &Recipe) {
        let mut generator = self;
        match recipe {
            Recipe::Neighbours { seed } => {
                generator.mode.set(Mode::Neighbours);
                generator.run(Some(*seed));
            }
            Recipe::Radio { seed } => {
                generator.mode.set(Mode::Radio);
                generator.run(Some(*seed));
            }
            Recipe::Path { a, b, even } => {
                generator.mode.set(Mode::Path);
                generator.from.set(Some(*a));
                generator.to.set(Some(*b));
                generator.even.set(*even);
                generator.run(None);
            }
            Recipe::Drift { seed, phrase } => {
                generator.mode.set(Mode::Drift);
                generator.phrase.set(phrase.clone());
                generator.run(Some(*seed));
            }
        }
    }

    /// The weights moved, so the distances that produced this result no longer
    /// hold. Say so and keep the rows: silently emptying the list was the old
    /// behaviour of `clear`, and it read as the app discarding your work.
    pub fn invalidate(mut self) {
        if !self.result.peek().is_empty() {
            self.stale.set(true);
        }
    }

    /// Throw the result away. Only for when the user asks, or when the ids in
    /// it have genuinely stopped existing.
    pub fn clear(mut self) {
        self.result.set(Vec::new());
        self.produced_by.set(None);
        self.stale.set(false);
        self.status.set(None);
    }
}

/// Which end of a path a track is being named as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathEnd {
    A,
    B,
}

impl Default for Generator {
    fn default() -> Self {
        Self::new()
    }
}

/// The result's heading. Says what made these rows, because with generation
/// explicit they outlive the selection and the tab that produced them.
pub(crate) fn describe(recipe: &Recipe, label: impl Fn(Option<i64>) -> String) -> String {
    match recipe {
        Recipe::Neighbours { seed } => format!("Neighbours of {}", label(Some(*seed))),
        Recipe::Radio { seed } => format!("Radio from {}", label(Some(*seed))),
        Recipe::Path { a, b, even } => format!(
            "Path {} → {}{}",
            label(Some(*a)),
            label(Some(*b)),
            if *even { ", evenly paced" } else { "" }
        ),
        Recipe::Drift { seed, phrase } => {
            format!("Drift from {} towards “{phrase}”", label(Some(*seed)))
        }
    }
}

/// Keep the first occurrence of each recording, dropping the rest.
///
/// The space has no ISRC-awareness in its distances: a single, an album cut
/// and a deluxe reissue of the same song are three rows with three track
/// ids, close enough in the space to plausibly both land in one
/// neighbours/radio/path/drift result, which reads as "the same track
/// twice", even though structurally it is not (`radio_nearest`, `interpolate`
/// and `graph_path` already refuse to revisit the same *row*; this catches
/// what they cannot see).
///
/// A post-processing step here rather than inside `paths.rs`: that module
/// mirrors `pipeline/two_khz/paths.py`, the parity oracle the Rust port is
/// checked against, and this filter has no Python equivalent to stay in step
/// with, adding it to the shared algorithm would make the two disagree
/// wherever the corpus has a duplicate ISRC.
fn dedup_by_recording(steps: Vec<Step>) -> Vec<Step> {
    let mut seen: HashSet<String> = HashSet::new();
    steps
        .into_iter()
        .filter(|step| {
            let key = step
                .track
                .isrc
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_ascii_uppercase)
                .unwrap_or_else(|| format!("id:{}", step.track.track_id));
            seen.insert(key)
        })
        .collect()
}

/// Present a space track to the player, which speaks in Qobuz terms.
pub(crate) fn as_remote(meta: &crate::db::TrackMeta) -> RemoteTrack {
    RemoteTrack {
        id: meta.track_id,
        title: meta.title.clone(),
        artist: meta.artist.clone(),
        artist_id: Some(meta.artist_id).filter(|id| *id >= 0),
        album: meta.album.clone(),
        album_id: Some(meta.album_id.clone()).filter(|id| !id.is_empty()),
        duration: None,
        streamable: true,
        hires: false,
        // The space stores no art, so the cover is derived from the album id.
        image: crate::qobuz::cover_url(&meta.album_id),
        isrc: meta.isrc.clone(),
        // Neither travels through the space, `TrackMeta` carries a release
        // *year*, folded into the era block, not the full date, and never
        // carried a credit string at all.
        released: None,
        performers: None,
        liked_at: None,
    }
}


#[cfg(test)]
mod tests {
    use super::dedup_by_recording;
    use crate::db::TrackMeta;
    use crate::paths::Step;

    fn step(track_id: i64, isrc: Option<&str>) -> Step {
        Step {
            track: TrackMeta {
                track_id,
                title: format!("track {track_id}"),
                artist: "someone".into(),
                album: String::new(),
                genre: String::new(),
                artist_id: 1,
                album_id: String::new(),
                bpm: None,
                seed_distance: 0,
                isrc: isrc.map(str::to_string),
                x: None,
                y: None,
            },
            similarity: None,
        }
    }

    /// Two different track ids, same recording, a single and an album cut,
    /// say. The second is what this filter exists to catch.
    #[test]
    fn drops_a_second_row_with_the_same_isrc() {
        let steps = vec![step(1, Some("GBAAA0000001")), step(2, Some("gbaaa0000001"))];
        let kept = dedup_by_recording(steps);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].track.track_id, 1);
    }

    /// No ISRC is not the same as a shared one, two untagged tracks must
    /// not collapse into each other just because both keys are absent.
    #[test]
    fn untagged_tracks_are_never_treated_as_duplicates() {
        let steps = vec![step(1, None), step(2, None)];
        assert_eq!(dedup_by_recording(steps).len(), 2);
    }

    #[test]
    fn distinct_recordings_all_survive() {
        let steps = vec![
            step(1, Some("GBAAA0000001")),
            step(2, Some("GBAAA0000002")),
            step(3, None),
        ];
        assert_eq!(dedup_by_recording(steps).len(), 3);
    }
}

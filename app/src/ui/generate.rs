//! Making a sequence of tracks out of the space.
//!
//! Started from a track's page, the search box or a row's menu, and landed in
//! the queue as a group of its own, after the generated ones already there or
//! else after what is playing, which can be rerolled, saved, moved or stopped
//! from its header there. The group is the session's, so it outlives a
//! reload. Five modes, because they differ only in how the sequence is
//! chosen:
//!   - **neighbours**: the k most similar tracks.
//!   - **radio**: a greedy walk, pushed away from artists already used.
//!   - **path**: A to B, shortest route or evenly paced.
//!   - **drift**: away from a track, towards a phrase.
//!   - **mood**: whatever sounds like a phrase, no track needed.

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use crate::backend::backend;
use super::icons;
use super::player::{drop_group, queue_section, Player};
pub use crate::api::Recipe;
use crate::api::{GenerateRequest, Step, TrackMeta};
use crate::catalogue::RemoteTrack;
use crate::session::GroupKind;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Neighbours,
    Radio,
    Path,
    Drift,
    Mood,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Neighbours => "neighbours",
            Mode::Radio => "radio",
            Mode::Path => "path",
            Mode::Drift => "drift",
            Mode::Mood => "mood",
        }
    }

    pub(crate) fn icon(self) -> Element {
        match self {
            Mode::Neighbours => icons::neighbours(),
            Mode::Radio => icons::radio(),
            Mode::Path => icons::path(),
            Mode::Drift => icons::drift(),
            Mode::Mood => icons::mood(),
        }
    }

    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Mode::Neighbours => "The tracks most alike",
            Mode::Radio => "Keep walking, never repeat",
            Mode::Path => "Walk here from the path's start",
            Mode::Drift => "Away, towards a phrase",
            Mode::Mood => "Whatever sounds like a phrase",
        }
    }

    /// The mode as a section of the queue is headed.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Mode::Neighbours => "Neighbours",
            Mode::Radio => "Radio",
            Mode::Path => "Path",
            Mode::Drift => "Drift",
            Mode::Mood => "Mood",
        }
    }

    /// Whether the mode needs the server to embed a phrase.
    pub(crate) fn steered(self) -> bool {
        matches!(self, Mode::Drift | Mode::Mood)
    }
}

#[derive(Clone, Copy)]
pub struct Generator {
    /// Where results go: the queue, as a group of their own.
    player: Player,
    pub mode: Signal<Mode>,
    /// The tracks the map traces: the last result, or the group whose header
    /// asked for its map.
    pub route: Signal<Vec<i64>>,
    /// A walk is running on the server.
    pub busy: Signal<bool>,
    /// The group `busy` and `status` are about: one being rerolled or saved.
    /// `None` for a new result, whose word goes where it was asked for.
    pub about: Signal<Option<u64>>,
    /// Generated groups made before the weights last moved, so the distances
    /// behind them no longer hold. The rows stay, and their header says they
    /// are out of date, rather than vanishing.
    pub stale: Signal<HashSet<u64>>,
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
    /// Whether `status` reports a failure, to be shown as one.
    pub failed: Signal<bool>,
    /// Whether the server can steer by a phrase: it has the text tower, and
    /// the corpus the audio embeddings to match it against.
    pub tower: Signal<bool>,
    /// The sliders, sent with every request: the server keeps no weights of
    /// its own for a device.
    pub weights: Signal<HashMap<String, f32>>,
    /// The track whose page should show drift's phrase box, asked for from
    /// a row's menu, which has no room to type in.
    pub drift_for: Signal<Option<i64>>,
}

impl Generator {
    pub fn new(player: Player) -> Self {
        Self {
            player,
            // The one mode that works before anything is selected.
            mode: Signal::new(Mode::Mood),
            route: Signal::new(Vec::new()),
            busy: Signal::new(false),
            about: Signal::new(None),
            stale: Signal::new(HashSet::new()),
            count: Signal::new(20),
            // 0.10 measured out as 19.4 distinct artists per 20 tracks
            // against 14.8 unpenalised, for 0.786 similarity against 0.796.
            artist_penalty: Signal::new(0.10),
            phrase: Signal::new(String::new()),
            from: Signal::new(None),
            to: Signal::new(None),
            even: Signal::new(false),
            status: Signal::new(None),
            failed: Signal::new(false),
            tower: Signal::new(false),
            weights: Signal::new(HashMap::new()),
            drift_for: Signal::new(None),
        }
    }

    /// Whether drift and mood are offerable at all.
    pub(crate) fn can_steer(&self) -> bool {
        *self.tower.read()
    }

    /// Produce a sequence, queued as a new group.
    pub fn run(self, selected: Option<i64>) {
        self.produce(selected, None);
    }

    /// Produce a sequence, in place of the rows still to come of `replace`
    /// when there is one. Spawned, because the walk is the server's: one
    /// round trip, the phrase embedded on the way for drift and mood.
    fn produce(self, selected: Option<i64>, replace: Option<u64>) {
        let mut generator = self;
        generator.status.set(None);
        generator.failed.set(false);
        generator.about.set(replace);
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
            Mode::Mood => Some(Recipe::Mood {
                phrase: phrase.clone(),
            }),
        };

        let weights = self.weights.peek().clone();

        // Forever, not scoped: `request` is called from the row menu, which
        // closes in the same click, and a scoped task dies with it before it
        // is ever polled, leaving `busy` set and the button on "working…".
        spawn_forever(async move {
            let produced = match recipe.clone() {
                Some(recipe) => {
                    let request = GenerateRequest {
                        recipe,
                        count,
                        penalty,
                        weights,
                    };
                    match backend().generate(&request).await {
                        Ok(steps) => Some(steps),
                        Err(err) => {
                            generator.status.set(Some(failure(&err)));
                            generator.failed.set(true);
                            None
                        }
                    }
                }
                None => None,
            };

            let Some(produced) = produced else {
                generator.busy.set(false);
                return;
            };
            let produced = dedup_by_recording(produced);
            super::remember(produced.iter().map(|step| &step.track));
            if produced.is_empty() {
                generator.status.set(Some(
                    if mode == Mode::Mood {
                        "nothing found, try other words"
                    } else {
                        "nothing found, try a different start"
                    }
                    .into(),
                ));
            }
            if let (Some(recipe), false) = (recipe, produced.is_empty()) {
                generator.land(&produced, recipe, replace);
            }
            if let Some(group) = replace {
                generator.stale.write().remove(&group);
            }
            generator.busy.set(false);
        });
    }

    /// Put a result in the queue as a group, see `Op::Section`. Into an empty
    /// queue it is the queue, and plays.
    fn land(self, produced: &[Step], recipe: Recipe, replace: Option<u64>) {
        let mut generator = self;
        let mut player = self.player;
        let tracks: Vec<RemoteTrack> = produced.iter().map(|step| as_remote(&step.track)).collect();
        generator.route.set(tracks.iter().map(|track| track.id).collect());
        queue_section(player, tracks, GroupKind::Generated { recipe }, replace);
        // The group has to be somewhere to look at.
        player.queue_open.set(true);
    }

    /// Make a group again from what made it. The rows still to come are
    /// swapped for the new ones; those already played stay where they are.
    pub fn reroll(self, group: u64, recipe: &Recipe) {
        self.rerun(recipe, Some(group));
    }

    /// Take what is left of a group out of the queue, and forget it. What is
    /// playing keeps playing, even if the group put it there.
    pub fn stop(self, group: u64) {
        let mut generator = self;
        drop_group(self.player, group);
        generator.stale.write().remove(&group);
        if *generator.about.peek() == Some(group) {
            generator.status.set(None);
            generator.failed.set(false);
        }
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
        match end {
            PathEnd::A => generator.from.set(Some(id)),
            PathEnd::B => generator.to.set(Some(id)),
        }
        if generator.from.peek().is_some() && generator.to.peek().is_some() {
            generator.mode.set(Mode::Path);
            generator.run(None);
        }
    }

    /// Start a path here, forgetting any end it had: a new start is a new
    /// path, and keeping the old end would walk it at once.
    pub fn start_path(self, id: i64) {
        let mut generator = self;
        generator.to.set(None);
        generator.from.set(Some(id));
    }

    /// Forget a half-built path.
    pub fn cancel_path(self) {
        let mut generator = self;
        generator.from.set(None);
        generator.to.set(None);
    }

    /// Ask for drift's phrase on a track's page, without running it. What
    /// drift still lacks is the phrase, and guessing one would be inventing
    /// the user's intent.
    pub fn aim_drift(self, id: i64) {
        let mut generator = self;
        generator.drift_for.set(Some(id));
    }

    /// Drift from a track towards a phrase.
    pub fn drift(self, seed: i64, phrase: &str) {
        let mut generator = self;
        generator.phrase.set(phrase.to_string());
        generator.drift_for.set(None);
        generator.request(Mode::Drift, seed);
    }

    /// Run a recipe again, restoring the inputs that produced it, in place
    /// of `replace`. Re-running "whatever the panel is showing now" would
    /// quietly answer a different question, since the mode and the selection
    /// have both been free to move since the result was made.
    pub fn rerun(self, recipe: &Recipe, replace: Option<u64>) {
        let mut generator = self;
        match recipe {
            Recipe::Neighbours { seed } => {
                generator.mode.set(Mode::Neighbours);
                generator.produce(Some(*seed), replace);
            }
            Recipe::Radio { seed } => {
                generator.mode.set(Mode::Radio);
                generator.produce(Some(*seed), replace);
            }
            Recipe::Path { a, b, even } => {
                generator.mode.set(Mode::Path);
                generator.from.set(Some(*a));
                generator.to.set(Some(*b));
                generator.even.set(*even);
                generator.produce(None, replace);
            }
            Recipe::Drift { seed, phrase } => {
                generator.mode.set(Mode::Drift);
                generator.phrase.set(phrase.clone());
                generator.produce(Some(*seed), replace);
            }
            Recipe::Mood { phrase } => {
                generator.mode.set(Mode::Mood);
                generator.phrase.set(phrase.clone());
                generator.produce(None, replace);
            }
        }
    }

    /// Run mood on a phrase, from a preset or the box.
    pub fn feel(self, phrase: &str) {
        let mut generator = self;
        generator.mode.set(Mode::Mood);
        generator.phrase.set(phrase.to_string());
        generator.run(None);
    }

    /// The weights moved, so the distances that produced the generated groups
    /// in the queue no longer hold. Say so and keep the rows: silently
    /// emptying them would read as the app discarding your work.
    pub fn invalidate(mut self) {
        let generated: Vec<u64> = self
            .player
            .groups
            .peek()
            .iter()
            .filter(|group| matches!(group.kind, GroupKind::Generated { .. }))
            .map(|group| group.id)
            .collect();
        if !generated.is_empty() {
            self.stale.write().extend(generated);
        }
    }
}

/// A failed request in words: a server that did not answer is said plainly,
/// anything else is the server's own message.
pub(crate) fn failure(err: &anyhow::Error) -> String {
    if crate::backend::is_unreachable(err) {
        "Couldn't reach the server. Try again once it's back.".into()
    } else {
        format!("{err:#}")
    }
}

/// Which end of a path a track is being named as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathEnd {
    A,
    B,
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
        Recipe::Mood { phrase } => format!("Sounds like “{phrase}”"),
    }
}

/// Keep the first occurrence of each recording, dropping the rest.
///
/// The server's walks already skip other releases of a song they have used
/// (see `stelly_core::recording`), which keeps their counts full. This is the
/// same rule again for a server older than that, where the duplicate is
/// dropped and the list comes out a track short instead.
fn dedup_by_recording(steps: Vec<Step>) -> Vec<Step> {
    crate::recording::first_of_each(steps, |step| &step.track)
}

/// Present a space track to the player, which speaks in Qobuz terms.
pub(crate) fn as_remote(meta: &TrackMeta) -> RemoteTrack {
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
        image: meta.image.clone(),
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
    use crate::api::{Step, TrackMeta};

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
                image: None,
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
        let mut album_cut = step(2, Some("gbaaa0000001"));
        album_cut.track.title = "track 1".into();
        let steps = vec![step(1, Some("GBAAA0000001")), album_cut];
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

    /// A remaster with an ISRC of its own is still the same song.
    #[test]
    fn drops_a_remaster_under_another_isrc() {
        let mut remaster = step(2, Some("GBAAA0000099"));
        remaster.track.title = "track 1 - 2011 Remaster".into();
        let kept = dedup_by_recording(vec![step(1, Some("GBAAA0000001")), remaster]);
        assert_eq!(kept.len(), 1);
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

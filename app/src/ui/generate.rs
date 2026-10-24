//! Making a sequence of tracks out of the space.
//!
//! Started from a track's page, the search box or a row's menu, and landed in
//! the queue as a section of its own, right after what is playing, which can
//! be rerolled, saved or stopped from its header there. Five modes, because
//! they differ only in how the sequence is chosen:
//!   - **neighbours**: the k most similar tracks.
//!   - **radio**: a greedy walk, pushed away from artists already used.
//!   - **path**: A to B, shortest route or evenly paced.
//!   - **drift**: away from a track, towards a phrase.
//!   - **mood**: whatever sounds like a phrase, no track needed.

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use crate::backend::backend;
use super::icons;
use super::player::{play_list, set_upcoming, Player};
pub use crate::api::Recipe;
use crate::api::{GenerateRequest, Step, TrackMeta};
use crate::catalogue::RemoteTrack;
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
    /// Where results go: the queue, as a section of their own.
    player: Player,
    pub mode: Signal<Mode>,
    /// The tracks of the queue's generated section, as they were placed.
    /// Rows since removed from the queue simply stop being found there;
    /// this is what the section's header and the map's route read.
    pub result: Signal<Vec<Step>>,
    /// What produced `result`, for its heading. `None` when there is none.
    pub produced_by: Signal<Option<Recipe>>,
    /// A walk is running on the server.
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

    /// Produce a sequence. Spawned, because the walk is the server's: one
    /// round trip, the phrase embedded on the way for drift and mood.
    pub fn run(self, selected: Option<i64>) {
        let mut generator = self;
        generator.status.set(None);
        generator.failed.set(false);
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
            if !produced.is_empty() {
                generator.land(&produced);
            }
            generator.result.set(produced);
            generator.produced_by.set(recipe);
            generator.stale.set(false);
            generator.busy.set(false);
        });
    }

    /// Put a result in the queue, in place of the section the last one made:
    /// where that section was, or right after what is playing. Into an empty
    /// queue it is the queue, and plays.
    fn land(self, produced: &[Step]) {
        let mut player = self.player;
        let tracks: Vec<RemoteTrack> = produced.iter().map(|step| as_remote(&step.track)).collect();
        let placement = place(&player.queue.peek(), *player.index.peek(), &self.ids(), tracks);
        match placement {
            Placement::Load(tracks) => play_list(player, tracks, 0),
            Placement::Upcoming(tracks) => set_upcoming(player, tracks),
        }
        // The section has to be somewhere to look at.
        player.queue_open.set(true);
    }

    /// The track ids of the generated section.
    pub(crate) fn ids(&self) -> HashSet<i64> {
        self.result.peek().iter().map(|step| step.track.track_id).collect()
    }

    /// Make the section again from what made it. The rows still to come are
    /// swapped for the new ones; those already played stay where they are.
    pub fn reroll(self) {
        if let Some(recipe) = self.produced_by.peek().clone() {
            self.rerun(&recipe);
        }
    }

    /// Take what is left of the section out of the queue, and forget it.
    /// What is playing keeps playing, even if the section put it there.
    pub fn stop(self) {
        let player = self.player;
        let old = self.ids();
        let queue = player.queue.peek().clone();
        let index = *player.index.peek();
        if index < queue.len() {
            let upcoming: Vec<RemoteTrack> =
                queue[index + 1..].iter().filter(|track| !old.contains(&track.id)).cloned().collect();
            if index + 1 + upcoming.len() != queue.len() {
                set_upcoming(player, upcoming);
            }
        }
        self.clear();
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
            Recipe::Mood { phrase } => {
                generator.mode.set(Mode::Mood);
                generator.phrase.set(phrase.clone());
                generator.run(None);
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
        self.failed.set(false);
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

/// Where a result goes in the queue.
#[derive(Debug, PartialEq)]
enum Placement {
    /// The queue was empty: the result becomes it.
    Load(Vec<RemoteTrack>),
    /// Everything after what is playing, with the result in it.
    Upcoming(Vec<RemoteTrack>),
}

/// Fit `new` into the queue in place of the old section's rows still to
/// come, `old`: where the first of them was, or straight after what is
/// playing when none are left. Rows the result repeats are taken from where
/// they were, so it plays them in its own order, and what is playing is
/// never queued again behind itself.
fn place(queue: &[RemoteTrack], index: usize, old: &HashSet<i64>, new: Vec<RemoteTrack>) -> Placement {
    if queue.is_empty() {
        return Placement::Load(new);
    }
    let index = index.min(queue.len() - 1);
    let current = queue[index].id;
    let new: Vec<RemoteTrack> = new.into_iter().filter(|track| track.id != current).collect();
    let fresh: HashSet<i64> = new.iter().map(|track| track.id).collect();
    let upcoming = &queue[index + 1..];
    let at = upcoming.iter().position(|track| old.contains(&track.id)).unwrap_or(0);
    let keep = |tracks: &[RemoteTrack]| -> Vec<RemoteTrack> {
        tracks
            .iter()
            .filter(|track| !old.contains(&track.id) && !fresh.contains(&track.id))
            .cloned()
            .collect()
    };
    let mut placed = keep(&upcoming[..at]);
    placed.extend(new);
    placed.extend(keep(&upcoming[at..]));
    Placement::Upcoming(placed)
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
    use super::{dedup_by_recording, place, Placement};
    use crate::api::{Step, TrackMeta};
    use crate::catalogue::RemoteTrack;
    use std::collections::HashSet;

    fn track(id: i64) -> RemoteTrack {
        super::as_remote(&step(id, None).track)
    }

    fn ids(placement: Placement) -> Vec<i64> {
        match placement {
            Placement::Load(tracks) | Placement::Upcoming(tracks) => tracks.iter().map(|t| t.id).collect(),
        }
    }

    #[test]
    fn an_empty_queue_becomes_the_result() {
        let placed = place(&[], 0, &HashSet::new(), vec![track(1), track(2)]);
        assert!(matches!(placed, Placement::Load(_)));
        assert_eq!(ids(placed), vec![1, 2]);
    }

    #[test]
    fn a_first_result_comes_straight_after_what_is_playing() {
        let queue = [track(1), track(2), track(3)];
        let placed = place(&queue, 0, &HashSet::new(), vec![track(7), track(8)]);
        assert_eq!(ids(placed), vec![7, 8, 2, 3]);
    }

    /// Playing 1, then the user's 2, the old section's 5 and 6 (4 already
    /// gone), then the user's 9.
    #[test]
    fn a_reroll_takes_the_old_sections_place() {
        let queue = [track(1), track(2), track(5), track(6), track(9)];
        let old = HashSet::from([4, 5, 6]);
        let placed = place(&queue, 0, &old, vec![track(7), track(8)]);
        assert_eq!(ids(placed), vec![2, 7, 8, 9]);
    }

    #[test]
    fn what_is_playing_is_not_queued_behind_itself() {
        let queue = [track(1), track(2)];
        let placed = place(&queue, 0, &HashSet::new(), vec![track(1), track(3)]);
        assert_eq!(ids(placed), vec![3, 2]);
    }

    #[test]
    fn a_track_already_queued_moves_into_the_section() {
        let queue = [track(1), track(2), track(3)];
        let placed = place(&queue, 0, &HashSet::new(), vec![track(3), track(4)]);
        assert_eq!(ids(placed), vec![3, 4, 2]);
    }

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

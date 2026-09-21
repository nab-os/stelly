//! Navigation: neighbours, paths, drift, radio.

use crate::db::{Catalog, TrackMeta};
use crate::space::{argsort_desc, Space, WeightedSpace};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Constraints {
    /// No repeated artist within this many steps.
    pub artist_cooldown: usize,
    pub max_bpm_delta: Option<f32>,
    pub allow_same_album_consecutive: bool,
    pub exclude: HashSet<i64>,
}

impl Default for Constraints {
    fn default() -> Self {
        Self {
            artist_cooldown: 3,
            max_bpm_delta: None,
            allow_same_album_consecutive: false,
            exclude: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    #[serde(flatten)]
    pub track: TrackMeta,
    pub similarity: Option<f32>,
}

pub struct Navigator {
    pub space: WeightedSpace,
    pub catalog: Catalog,
    pub index_of: HashMap<i64, usize>,
    knn: Option<(usize, Vec<Vec<(usize, f32)>>)>,
}

impl Navigator {
    pub fn new(space: &Space, weights: &HashMap<String, f32>, catalog: Catalog) -> Self {
        Self {
            space: space.weighted(weights),
            catalog,
            index_of: space.index_of.clone(),
            knn: None,
        }
    }

    /// Push blocked rows out of reach of any argsort over similarities.
    /// Cheaper than filtering afterwards, and matches `exclude_same_artist`.
    fn mask_blocked(&self, sims: &mut [f32]) {
        if self.catalog.blocked_artists.is_empty() {
            return;
        }
        for (j, s) in sims.iter_mut().enumerate() {
            if self.catalog.is_blocked(j) {
                *s = f32::NEG_INFINITY;
            }
        }
    }

    /// Drop the cached kNN graph. Needed when visibility changes, since the
    /// graph's edges were chosen over the old set of reachable tracks.
    pub fn invalidate_graph(&mut self) {
        self.knn = None;
    }

    fn step(&self, i: usize, similarity: Option<f32>) -> Step {
        Step {
            track: self.catalog.get(i).clone(),
            similarity,
        }
    }

    // ------------------------------------------------------------ primitives

    pub fn neighbours(&self, track_id: i64, k: usize, exclude_same_artist: bool) -> Vec<Step> {
        let Some(&i) = self.index_of.get(&track_id) else {
            return Vec::new();
        };
        let mut sims = self.space.similarities(self.space.row(i));
        sims[i] = f32::NEG_INFINITY;
        self.mask_blocked(&mut sims);

        if exclude_same_artist {
            let artist = self.catalog.get(i).artist_id;
            for (j, s) in sims.iter_mut().enumerate() {
                if artist != -1 && self.catalog.get(j).artist_id == artist {
                    *s = f32::NEG_INFINITY;
                }
            }
        }

        crate::space::top_k(&sims, k)
            .into_iter()
            .map(|j| self.step(j, Some(sims[j])))
            .collect()
    }

    /// Radio: a greedy walk that jumps to the most similar track not yet
    /// played. Deterministic: the same start gives the same walk.
    ///
    /// `artist_penalty` is subtracted once per time that artist appears in the
    /// walk, so the pull away from a discography grows. It reaches back over
    /// the whole walk, over the cooldown window alone it changes nothing.
    pub fn radio_nearest(
        &self,
        from_id: i64,
        steps: usize,
        artist_penalty: f32,
        c: &Constraints,
    ) -> Vec<Step> {
        let Some(&start) = self.index_of.get(&from_id) else {
            return Vec::new();
        };
        if self.catalog.is_blocked(start) {
            return Vec::new();
        }

        let mut chosen = vec![start];
        let mut used: HashSet<usize> = [start].into_iter().collect();
        let mut similarity: Vec<Option<f32>> = vec![None];

        while chosen.len() < steps {
            let current = *chosen.last().expect("seeded above");
            let mut sims = self.space.similarities(self.space.row(current));
            for &i in &used {
                sims[i] = f32::NEG_INFINITY;
            }
            self.mask_blocked(&mut sims);

            if artist_penalty > 0.0 {
                let mut seen: HashMap<i64, usize> = HashMap::new();
                for &i in &chosen {
                    let artist = self.catalog.get(i).artist_id;
                    if artist != -1 {
                        *seen.entry(artist).or_insert(0) += 1;
                    }
                }
                for (j, score) in sims.iter_mut().enumerate() {
                    if let Some(times) = seen.get(&self.catalog.get(j).artist_id) {
                        *score -= artist_penalty * *times as f32;
                    }
                }
            }

            // The first candidate the constraints accept, in descending order.
            let pick = argsort_desc(&sims)
                .into_iter()
                .find(|&j| sims[j].is_finite() && self.allowed(j, &chosen, c));

            let Some(pick) = pick else { break };
            similarity.push(Some(sims[pick]));
            chosen.push(pick);
            used.insert(pick);
        }

        chosen
            .into_iter()
            .zip(similarity)
            .map(|(i, sim)| self.step(i, sim))
            .collect()
    }

    fn allowed(&self, candidate: usize, chosen: &[usize], c: &Constraints) -> bool {
        if self.catalog.is_blocked(candidate) {
            return false;
        }
        let meta = self.catalog.get(candidate);
        if c.exclude.contains(&meta.track_id) {
            return false;
        }
        let Some(&previous) = chosen.last() else {
            return true;
        };

        if meta.artist_id != -1 && c.artist_cooldown > 0 {
            let start = chosen.len().saturating_sub(c.artist_cooldown);
            if chosen[start..]
                .iter()
                .any(|&p| self.catalog.get(p).artist_id == meta.artist_id)
            {
                return false;
            }
        }

        let prev_meta = self.catalog.get(previous);
        if !c.allow_same_album_consecutive
            && !meta.album_id.is_empty()
            && meta.album_id == prev_meta.album_id
        {
            return false;
        }

        if let (Some(limit), Some(a), Some(b)) = (c.max_bpm_delta, meta.bpm, prev_meta.bpm) {
            if (a - b).abs() > limit {
                return false;
            }
        }

        true
    }

    fn snap(
        &self,
        waypoint: &[f32],
        chosen: &[usize],
        used: &HashSet<usize>,
        c: &Constraints,
    ) -> Option<usize> {
        let sims = self.space.similarities(waypoint);
        argsort_desc(&sims)
            .into_iter()
            .find(|&j| !used.contains(&j) && self.allowed(j, chosen, c))
    }

    // ----------------------------------------------------------------- modes

    pub fn interpolate(
        &self,
        from_id: i64,
        to_id: i64,
        steps: usize,
        c: &Constraints,
    ) -> Vec<Step> {
        let (Some(&a), Some(&b)) = (self.index_of.get(&from_id), self.index_of.get(&to_id)) else {
            return Vec::new();
        };
        if self.catalog.is_blocked(a) || self.catalog.is_blocked(b) {
            return Vec::new();
        }
        let d = self.space.n_dims;
        let start = self.space.row(a).to_vec();
        let end = self.space.row(b).to_vec();

        let mut chosen = vec![a];
        let mut used: HashSet<usize> = [a, b].into_iter().collect();

        for step in 1..steps.saturating_sub(1) {
            let t = step as f32 / (steps - 1) as f32;
            let waypoint: Vec<f32> = (0..d).map(|k| (1.0 - t) * start[k] + t * end[k]).collect();
            match self.snap(&waypoint, &chosen, &used, c) {
                Some(pick) => {
                    chosen.push(pick);
                    used.insert(pick);
                }
                None => break,
            }
        }
        chosen.push(b);
        chosen.into_iter().map(|i| self.step(i, None)).collect()
    }

    /// kNN graph over the whole space, built once and cached.
    ///
    /// Every row against every row, so it dominates the first path after a
    /// load, weight change or block. Kept interactive by splitting rows across
    /// cores, reusing one buffer per worker, and partial selection over a sort.
    fn knn_graph(&mut self, k: usize) -> &Vec<Vec<(usize, f32)>> {
        if self.knn.as_ref().map(|(cached, _)| *cached) != Some(k) {
            let n = self.space.n_tracks;
            let space = &self.space;
            // Resolved up front: the workers cannot borrow self.catalog while
            // the graph is being written.
            let blocked: Vec<bool> = (0..n).map(|i| self.catalog.is_blocked(i)).collect();
            let any_blocked = blocked.iter().any(|&b| b);

            let mut graph: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
            let threads = std::thread::available_parallelism()
                .map_or(1, |v| v.get())
                .min(n.max(1));
            let chunk = n.div_ceil(threads.max(1));

            std::thread::scope(|scope| {
                for (index, slice) in graph.chunks_mut(chunk.max(1)).enumerate() {
                    let blocked = &blocked;
                    scope.spawn(move || {
                        let base = index * chunk;
                        let mut sims = vec![0.0f32; n];
                        for (offset, edges) in slice.iter_mut().enumerate() {
                            let i = base + offset;
                            space.similarities_into(space.row(i), &mut sims);
                            sims[i] = f32::NEG_INFINITY;
                            if any_blocked {
                                for (j, s) in sims.iter_mut().enumerate() {
                                    if blocked[j] {
                                        *s = f32::NEG_INFINITY;
                                    }
                                }
                            }
                            *edges = crate::space::top_k(&sims, k)
                                .into_iter()
                                // An edge to a masked row is unreachable
                                // anyway; dropping it keeps the graph honest.
                                .filter(|&j| sims[j].is_finite())
                                .map(|j| (j, 1.0 - sims[j]))
                                .collect();
                        }
                    });
                }
            });

            self.knn = Some((k, graph));
        }
        &self.knn.as_ref().unwrap().1
    }

    /// Order tracks so that consecutive ones sit close together in the space:
    /// a greedy nearest-neighbour tour, then 2-opt until it stops improving.
    ///
    /// An *open* path, not a tour, a playlist ends rather than returning to
    /// where it began, so the last point has no outgoing edge and reversing
    /// the final segment re-hangs one edge instead of two.
    ///
    /// `from` anchors the walk to a track already playing: it is measured from
    /// but never returned. Ids the space does not hold are dropped, since
    /// there is no distance to place them by; the caller decides where those
    /// end up.
    pub fn shortest_path_order(&self, track_ids: &[i64], from: Option<i64>) -> Vec<i64> {
        /// Above this the pairwise matrix stops being worth its memory and
        /// 2-opt stops being worth its time. `LIST_CAP` keeps a queue an order
        /// of magnitude under it in practice.
        const SORT_CAP: usize = 1024;

        // Positions, not rows: a queue may hold the same track twice, and both
        // copies have to come out the other side.
        let placeable: Vec<(usize, usize)> = track_ids
            .iter()
            .enumerate()
            .filter_map(|(pos, id)| self.index_of.get(id).map(|&row| (pos, row)))
            .collect();

        if placeable.len() < 3 || placeable.len() > SORT_CAP {
            return placeable.iter().map(|&(pos, _)| track_ids[pos]).collect();
        }

        // Node 0 is the anchor when there is one, so it can be pinned at the
        // front and stripped at the end without a second bookkeeping pass.
        let anchor = from.and_then(|id| self.index_of.get(&id).copied());
        let mut rows: Vec<usize> = Vec::with_capacity(placeable.len() + 1);
        rows.extend(anchor);
        rows.extend(placeable.iter().map(|&(_, row)| row));

        // Cosine distance, over rows the weighted space already normalised.
        let n = rows.len();
        let mut dist = vec![0.0f32; n * n];
        for a in 0..n {
            for b in (a + 1)..n {
                let (x, y) = (self.space.row(rows[a]), self.space.row(rows[b]));
                let d = 1.0 - x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
                dist[a * n + b] = d;
                dist[b * n + a] = d;
            }
        }
        let d = |a: usize, b: usize| dist[a * n + b];

        // Greedy nearest neighbour. Without an anchor the first node stays
        // first, so sorting does not also change what plays next.
        let mut tour: Vec<usize> = vec![0];
        let mut left: Vec<usize> = (1..n).collect();
        while !left.is_empty() {
            let last = *tour.last().unwrap();
            let pick = left
                .iter()
                .enumerate()
                .min_by(|a, b| d(last, *a.1).total_cmp(&d(last, *b.1)))
                .map(|(i, _)| i)
                .unwrap();
            tour.push(left.remove(pick));
        }

        // 2-opt, leaving tour[0] alone. Bounded rather than run to a fixed
        // point: the epsilon already rules out cycling on near-equal edges,
        // and this keeps a pathological queue from stalling the UI thread.
        for _ in 0..32 {
            let mut improved = false;
            for i in 1..n - 1 {
                for j in (i + 1)..n {
                    let prev = tour[i - 1];
                    let (head, tail) = (tour[i], tour[j]);
                    let (removed, added) = if j + 1 < n {
                        let next = tour[j + 1];
                        (d(prev, head) + d(tail, next), d(prev, tail) + d(head, next))
                    } else {
                        (d(prev, head), d(prev, tail))
                    };

                    if added + 1e-6 < removed {
                        tour[i..=j].reverse();
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }

        // Node k > 0 is placeable[k - 1] when an anchor took node 0.
        let offset = usize::from(anchor.is_some());
        tour.into_iter()
            .filter(|&node| !(anchor.is_some() && node == 0))
            .map(|node| track_ids[placeable[node - offset].0])
            .collect()
    }

    /// Dijkstra over the kNN graph. Usually better than `interpolate`: a
    /// straight line through a high-dimensional space crosses empty regions,
    /// whereas this stays on the data manifold.
    pub fn graph_path(&mut self, from_id: i64, to_id: i64, k: usize) -> Vec<Step> {
        let (Some(&start), Some(&goal)) = (self.index_of.get(&from_id), self.index_of.get(&to_id))
        else {
            return Vec::new();
        };
        if self.catalog.is_blocked(start) || self.catalog.is_blocked(goal) {
            return Vec::new();
        }

        let artist_of: Vec<i64> = (0..self.catalog.len())
            .map(|i| self.catalog.get(i).artist_id)
            .collect();
        let graph = self.knn_graph(k).clone();

        let mut best: HashMap<usize, f32> = HashMap::from([(start, 0.0)]);
        let mut previous: HashMap<usize, usize> = HashMap::new();
        let mut visited: HashSet<usize> = HashSet::new();
        let mut queue = BinaryHeap::new();
        queue.push(Entry {
            cost: 0.0,
            node: start,
        });

        while let Some(Entry { cost, node }) = queue.pop() {
            if !visited.insert(node) {
                continue;
            }
            if node == goal {
                break;
            }
            for &(neighbour, edge) in &graph[node] {
                if visited.contains(&neighbour) {
                    continue;
                }
                // Nudge away from same-artist chains.
                let penalty = if artist_of[neighbour] == artist_of[node] {
                    0.05
                } else {
                    0.0
                };
                let new_cost = cost + edge + penalty;
                if new_cost < *best.get(&neighbour).unwrap_or(&f32::INFINITY) {
                    best.insert(neighbour, new_cost);
                    previous.insert(neighbour, node);
                    queue.push(Entry {
                        cost: new_cost,
                        node: neighbour,
                    });
                }
            }
        }

        if goal != start && !previous.contains_key(&goal) {
            return Vec::new();
        }

        let mut path = vec![goal];
        while *path.last().unwrap() != start {
            path.push(previous[path.last().unwrap()]);
        }
        path.reverse();
        path.into_iter().map(|i| self.step(i, None)).collect()
    }

    /// Indices of the tracks whose audio best matches a text embedding.
    pub fn text_anchors(&self, clap: &[f32], k: usize) -> Vec<usize> {
        let Some(matrix) = self.catalog.clap.as_ref() else {
            return Vec::new();
        };
        let dims = self.catalog.clap_dims;
        if dims == 0 || clap.len() != dims {
            return Vec::new();
        }

        let mut sims: Vec<f32> = (0..self.catalog.len())
            .map(|i| {
                matrix[i * dims..(i + 1) * dims]
                    .iter()
                    .zip(clap)
                    .map(|(a, b)| a * b)
                    .sum()
            })
            .collect();
        self.mask_blocked(&mut sims);

        crate::space::top_k(&sims, k)
    }

    /// A destination in the space for a described sound.
    ///
    /// Not the text embedding through the audio PCA: CLAP has a modality gap,
    /// so those coordinates encode the gap rather than the content and a walk
    /// never arrives. Averaging the best-matching tracks stays inside the
    /// audio distribution.
    pub fn text_target(&self, clap: &[f32], k: usize) -> Option<Vec<f32>> {
        let anchors = self.text_anchors(clap, k);
        if anchors.is_empty() {
            return None;
        }
        let d = self.space.n_dims;
        let mut target = vec![0.0f32; d];
        for &i in &anchors {
            let row = self.space.row(i);
            for (t, v) in target.iter_mut().zip(row) {
                *t += v;
            }
        }
        let scale = 1.0 / anchors.len() as f32;
        for t in target.iter_mut() {
            *t *= scale;
        }
        Some(target)
    }

    /// Walk from a track towards a described sound.
    pub fn drift_to_text(
        &self,
        from_id: i64,
        clap: &[f32],
        steps: usize,
        k: usize,
        c: &Constraints,
    ) -> Vec<Step> {
        let Some(&start_index) = self.index_of.get(&from_id) else {
            return Vec::new();
        };
        if self.catalog.is_blocked(start_index) {
            return Vec::new();
        }
        let Some(target) = self.text_target(clap, k) else {
            return Vec::new();
        };
        let start = self.space.row(start_index).to_vec();

        let mut chosen = vec![start_index];
        let mut used: HashSet<usize> = [start_index].into_iter().collect();

        for step in 1..steps {
            let t = step as f32 / (steps - 1) as f32;
            let waypoint: Vec<f32> = start
                .iter()
                .zip(&target)
                .map(|(s, g)| (1.0 - t) * s + t * g)
                .collect();
            match self.snap(&waypoint, &chosen, &used, c) {
                Some(pick) => {
                    chosen.push(pick);
                    used.insert(pick);
                }
                None => break,
            }
        }
        chosen.into_iter().map(|i| self.step(i, None)).collect()
    }
}

/// Min-heap entry for Dijkstra. BinaryHeap is a max-heap, so the ordering is
/// reversed on cost.
struct Entry {
    cost: f32,
    node: usize,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost && self.node == other.node
    }
}
impl Eq for Entry {}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .partial_cmp(&self.cost)
            .unwrap_or(Ordering::Equal)
            .then(other.node.cmp(&self.node))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points on a unit circle, one per track id. Cosine distance between two
    /// of them grows with the angle between them, so the shortest route
    /// through a set of them is simply angle order, which makes the expected
    /// answer something the test can state rather than recompute.
    fn navigator(angles_deg: &[(i64, f32)]) -> Navigator {
        let mut unit = Vec::new();
        let mut tracks = Vec::new();
        let mut index_of = HashMap::new();

        for (row, &(id, degrees)) in angles_deg.iter().enumerate() {
            let radians = degrees.to_radians();
            unit.push(radians.cos());
            unit.push(radians.sin());
            tracks.push(TrackMeta {
                track_id: id,
                ..Default::default()
            });
            index_of.insert(id, row);
        }

        Navigator {
            space: WeightedSpace {
                unit,
                n_tracks: angles_deg.len(),
                n_dims: 2,
                scale: vec![1.0, 1.0],
            },
            catalog: Catalog {
                tracks,
                clap: None,
                clap_dims: 0,
                blocked_artists: HashSet::new(),
            },
            index_of,
            knn: None,
        }
    }

    fn fixture() -> Navigator {
        navigator(&[
            (10, 0.0),
            (11, 10.0),
            (12, 20.0),
            (13, 30.0),
            (14, 40.0),
            (15, 50.0),
        ])
    }

    #[test]
    fn orders_by_distance_from_the_anchor() {
        let nav = fixture();
        // Shuffled, and not starting at the track nearest the anchor, so
        // input order and the answer cannot coincide by luck.
        assert_eq!(
            nav.shortest_path_order(&[13, 11, 15, 12, 14], Some(10)),
            vec![11, 12, 13, 14, 15]
        );
    }

    #[test]
    fn without_an_anchor_the_first_track_stays_first() {
        let nav = fixture();
        let ordered = nav.shortest_path_order(&[13, 11, 15, 12, 14], None);
        assert_eq!(ordered[0], 13, "the anchorless start is held in place");

        // Whatever route it picks from there, nothing may be lost or repeated.
        let mut seen = ordered.clone();
        seen.sort();
        assert_eq!(seen, vec![11, 12, 13, 14, 15]);
    }

    #[test]
    fn drops_ids_the_space_does_not_hold() {
        let nav = fixture();
        assert_eq!(
            nav.shortest_path_order(&[14, 999, 12, 998, 11], Some(10)),
            vec![11, 12, 14]
        );
    }

    #[test]
    fn keeps_both_copies_of_a_repeated_track() {
        let nav = fixture();
        let ordered = nav.shortest_path_order(&[14, 12, 14, 11], Some(10));

        let mut seen = ordered.clone();
        seen.sort();
        assert_eq!(seen, vec![11, 12, 14, 14], "queued twice, played twice");

        // Two copies sit at zero distance, so nothing can come between them.
        let first = ordered.iter().position(|&id| id == 14).unwrap();
        assert_eq!(ordered[first + 1], 14);
    }

    #[test]
    fn too_few_to_reorder_passes_through() {
        let nav = fixture();
        assert_eq!(nav.shortest_path_order(&[14, 11], Some(10)), vec![14, 11]);
        assert!(nav.shortest_path_order(&[], Some(10)).is_empty());
    }
}

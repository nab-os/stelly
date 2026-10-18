//! When two rows are the same recording.
//!
//! The catalogue holds a song once per release it is on: the single, the
//! album cut, the deluxe reissue, the remaster, the explicit and the clean.
//! Each is its own track id, and the space puts them all in practically the
//! same place, so anything that walks it by nearness finds them side by side.
//! A generated list is meant to never play the same song twice, so these are
//! treated as one.
//!
//! Two rows are twins when they have the same title once the release's own
//! wording is set aside, "(Remastered 2011)", "- Single Version",
//! "[Deluxe Edition]", "(feat. …)", and either the same artist or the same
//! ISRC, which is how one song credited to two artist ids is caught. Wording
//! that names a different performance, "Live", "Acoustic", "Remix", is kept:
//! those are other recordings, and worth hearing as such.
//!
//! An artist's re-recording under the same title, untagged, counts as the
//! same song on purpose: a list holding "Ne me quitte pas" twice reads as a
//! repeat to whoever is listening, whichever session each came from.
//!
//! An ISRC alone is not trusted. Measured on a real catalogue of 158k rows,
//! some labels reuse one across a whole album of different songs, rain sounds
//! and karaoke keys mostly, and joining on it chained 342 different titles
//! into one "recording". A duplicate let through is a nuisance; a song
//! silently dropped from every list is a bug.

use crate::api::TrackMeta;
use std::collections::HashMap;

/// Words that, inside brackets or after a " - ", describe the release rather
/// than the recording.
const RELEASE_WORDS: [&str; 17] = [
    "remaster", "remastered", "version", "edit", "mono", "stereo", "deluxe", "edition",
    "anniversary", "bonus", "single", "album", "explicit", "clean", "radio", "feat", "ft",
];

/// The title as a song rather than a release of it: lowercased, with
/// bracketed and dashed release wording dropped and punctuation folded.
pub fn song_title(title: &str) -> String {
    let lower = title.to_lowercase();

    // Bracketed segments, dropped when they speak of the release.
    let mut kept = String::with_capacity(lower.len());
    let mut rest = lower.as_str();
    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') { ')' } else { ']' };
        let Some(close) = rest[open..].find(close_char).map(|at| open + at) else {
            break;
        };
        kept.push_str(&rest[..open]);
        let inside = &rest[open + 1..close];
        if !speaks_of_release(inside) {
            kept.push(' ');
            kept.push_str(inside);
            kept.push(' ');
        }
        rest = &rest[close + 1..];
    }
    kept.push_str(rest);

    // "Song - Remastered 2009", "Song - Single Version": only trailing parts,
    // and only those about the release, so "Part I - The Beginning" stays.
    let mut parts: Vec<&str> = kept.split(" - ").collect();
    while parts.len() > 1 && speaks_of_release(parts[parts.len() - 1]) {
        parts.pop();
    }
    let joined = parts.join(" ");

    joined
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words that name another performance, which win over release wording:
/// "(Live Version)" is not the studio take.
const PERFORMANCE_WORDS: [&str; 22] = [
    "live", "acoustic", "remix", "mix", "demo", "instrumental", "unplugged", "session",
    "sessions", "reprise", "orchestral", "piano", "karaoke", "rehearsal", "vip", "extended",
    "dub", "rework", "bootleg", "refix", "flip", "cover",
];

fn speaks_of_release(segment: &str) -> bool {
    let words: Vec<&str> = segment.split(|c: char| !c.is_alphanumeric()).collect();
    words.iter().any(|word| RELEASE_WORDS.contains(word))
        && !words.iter().any(|word| PERFORMANCE_WORDS.contains(word))
}

/// Who the song is by, for telling two artists' songs of the same name
/// apart: the id when the row has one, the name otherwise.
fn artist_key(track: &TrackMeta) -> String {
    if track.artist_id >= 0 {
        format!("#{}", track.artist_id)
    } else {
        track.artist.trim().to_lowercase()
    }
}

fn isrc_key(track: &TrackMeta) -> Option<String> {
    track
        .isrc
        .as_deref()
        .map(str::trim)
        .filter(|isrc| !isrc.is_empty())
        .map(str::to_ascii_uppercase)
}

/// Which recording each row is, as a group number per row: rows with the
/// same number are twins. Groups join through either key, so an ISRC shared
/// by one pair and a title shared by another make one group of three.
pub fn recordings(tracks: &[TrackMeta]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..tracks.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }

    let mut first: HashMap<String, usize> = HashMap::new();
    for (i, track) in tracks.iter().enumerate() {
        let title = song_title(&track.title);
        if title.is_empty() {
            continue;
        }
        let keys = isrc_key(track)
            .map(|isrc| format!("isrc:{isrc}:{title}"))
            .into_iter()
            .chain(Some(format!("song:{}:{title}", artist_key(track))));
        for key in keys {
            match first.get(&key) {
                Some(&j) => {
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    if a != b {
                        parent[a] = b;
                    }
                }
                None => {
                    first.insert(key, i);
                }
            }
        }
    }
    (0..tracks.len()).map(|i| root(&mut parent, i)).collect()
}

/// Keep the first row of each recording, in order.
pub fn first_of_each<T>(items: Vec<T>, meta: impl Fn(&T) -> &TrackMeta) -> Vec<T> {
    let groups = recordings(&items.iter().map(|item| meta(item).clone()).collect::<Vec<_>>());
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .zip(groups)
        .filter(|(_, group)| seen.insert(*group))
        .map(|(item, _)| item)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, artist_id: i64, title: &str, isrc: Option<&str>) -> TrackMeta {
        TrackMeta {
            track_id: id,
            title: title.into(),
            artist: format!("artist {artist_id}"),
            album: String::new(),
            genre: String::new(),
            artist_id,
            album_id: String::new(),
            image: None,
            bpm: None,
            seed_distance: 0,
            isrc: isrc.map(str::to_string),
            x: None,
            y: None,
        }
    }

    #[test]
    fn release_wording_is_set_aside() {
        assert_eq!(song_title("Heroes (2017 Remaster)"), "heroes");
        assert_eq!(song_title("Heroes - Single Version"), "heroes");
        assert_eq!(song_title("Heroes [Deluxe Edition]"), "heroes");
        assert_eq!(song_title("Heroes (feat. Someone)"), "heroes");
        assert_eq!(song_title("HEROES!"), "heroes");
    }

    #[test]
    fn other_performances_stay_apart() {
        assert_ne!(song_title("Heroes (Live)"), song_title("Heroes"));
        assert_ne!(song_title("Heroes - Acoustic"), song_title("Heroes"));
        assert_ne!(song_title("Heroes (Remix)"), song_title("Heroes"));
        assert_ne!(song_title("Heroes (Live Version)"), song_title("Heroes"));
        assert_ne!(song_title("Heroes - Radio Mix"), song_title("Heroes"));
        assert_ne!(song_title("Heroes (VIP Edit)"), song_title("Heroes"));
        assert_eq!(song_title("Heroes (Radio Edit)"), song_title("Heroes"));
        assert_eq!(song_title("Part I - The Beginning"), "part i the beginning");
    }

    #[test]
    fn twins_by_artist_or_isrc_with_the_same_song() {
        let tracks = vec![
            track(1, 7, "Heroes", Some("GBAAA0000001")),
            track(2, 7, "Heroes - 2017 Remaster", Some("GBAAA0000099")),
            // The same song under another artist id, tied by its ISRC.
            track(3, 8, "Heroes (feat. Someone)", Some("gbaaa0000001")),
            track(4, 9, "Heroes", None),
            track(5, 7, "Heroes (Live)", None),
        ];
        let groups = recordings(&tracks);
        assert_eq!(groups[0], groups[1]);
        assert_eq!(groups[0], groups[2]);
        // Another artist's song of the same name, and a live take: apart.
        assert_ne!(groups[0], groups[3]);
        assert_ne!(groups[0], groups[4]);
    }

    /// Labels reuse an ISRC across different songs; the title has to agree.
    #[test]
    fn a_shared_isrc_alone_is_not_the_same_song() {
        let tracks = vec![
            track(1, 7, "Droplets", Some("QZAAA0000001")),
            track(2, 7, "Lofi Rain", Some("QZAAA0000001")),
        ];
        let groups = recordings(&tracks);
        assert_ne!(groups[0], groups[1]);
    }

    #[test]
    fn first_of_each_keeps_order() {
        let tracks = vec![
            track(1, 7, "A", None),
            track(2, 7, "B", None),
            track(3, 7, "A (Remastered)", None),
        ];
        let kept: Vec<i64> = first_of_each(tracks, |t| t).iter().map(|t| t.track_id).collect();
        assert_eq!(kept, vec![1, 2]);
    }
}

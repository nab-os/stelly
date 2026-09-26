//! The words the mood and style blocks are made of.
//!
//! Both are zero-shot: each track's CLAP audio embedding is compared with the
//! text embeddings of these phrases, so a label is a direction in CLAP's joint
//! space rather than a trained classifier. Editing a phrase needs only
//! `build-space`, never a re-analysis, the audio embeddings are stored.
//!
//! A mood is an axis between two phrases, scored as the difference of the
//! two similarities. Contrast is what makes the score mean something: raw
//! similarity to "happy music" mostly measures how much a track resembles
//! music at all.

/// (column name, one end, the other end).
pub const MOODS: [(&str, &str, &str); 8] = [
    ("energy", "energetic, intense music", "calm, gentle music"),
    ("valence", "happy, uplifting music", "sad, melancholic music"),
    ("aggression", "aggressive, angry music", "tender, peaceful music"),
    ("danceability", "danceable music with a strong groove", "music with no beat or groove"),
    ("darkness", "dark, ominous music", "bright, sunny music"),
    ("acoustic", "acoustic instruments played by hand", "electronic music made with synthesizers"),
    ("vocals", "music with a singer and lyrics", "instrumental music with no vocals"),
    ("complexity", "complex, experimental music", "simple, repetitive music"),
];

/// Each becomes "{style} music". Broad on purpose: PCA folds these into a
/// handful of components, so what counts is covering the ground, not fine
/// distinctions between neighbours.
pub const STYLES: [&str; 60] = [
    "rock", "hard rock", "punk rock", "indie rock", "psychedelic rock", "progressive rock",
    "heavy metal", "death metal", "black metal", "post-rock", "shoegaze", "grunge",
    "pop", "synth-pop", "indie pop", "k-pop", "dream pop", "singer-songwriter",
    "folk", "country", "bluegrass", "blues", "soul", "funk",
    "r&b", "hip hop", "trap", "rap", "reggae", "dub",
    "jazz", "bebop", "free jazz", "smooth jazz", "swing", "bossa nova",
    "latin", "salsa", "flamenco", "afrobeat", "african", "indian classical",
    "classical", "baroque", "opera", "choral", "string quartet", "solo piano",
    "orchestral film score", "ambient", "drone", "new age", "techno", "house",
    "trance", "drum and bass", "dubstep", "downtempo electronic", "disco", "lo-fi",
];

pub fn style_prompt(style: &str) -> String {
    format!("{style} music")
}

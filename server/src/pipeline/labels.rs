//! The words the mood and style blocks are made of, and the ones a track is
//! described with.
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

/// What a track can be described with, as (word shown, phrase embedded).
/// Read at query time, not by `build-space`, so editing one needs no rebuild,
/// only a server restart. Experimental: the words are ones CLAP's captions
/// use, so it has a chance of knowing them, and the styles above join them.
pub const WORDS: [(&str, &str); 72] = [
    // pace and weight
    ("fast", "fast-paced music"),
    ("slow", "slow music"),
    ("energetic", "energetic, intense music"),
    ("calm", "calm, gentle music"),
    ("heavy", "heavy, loud music"),
    ("soft", "soft, quiet music"),
    ("driving", "driving, propulsive rhythm"),
    ("laid-back", "laid-back, relaxed groove"),
    ("danceable", "danceable music with a strong groove"),
    ("beatless", "music with no beat or drums"),
    // feeling
    ("happy", "happy, uplifting music"),
    ("sad", "sad, melancholic music"),
    ("dark", "dark, ominous music"),
    ("bright", "bright, sunny music"),
    ("angry", "aggressive, angry music"),
    ("tender", "tender, intimate music"),
    ("romantic", "romantic music"),
    ("nostalgic", "nostalgic music"),
    ("dreamy", "dreamy, hazy music"),
    ("tense", "tense, suspenseful music"),
    ("epic", "epic, cinematic music"),
    ("playful", "playful, quirky music"),
    ("hypnotic", "hypnotic, repetitive music"),
    ("spiritual", "spiritual, meditative music"),
    ("sensual", "sensual, sultry music"),
    ("triumphant", "triumphant, anthemic music"),
    // texture and sound
    ("warm", "warm, mellow sound"),
    ("cold", "cold, clinical sound"),
    ("lush", "lush, layered sound"),
    ("sparse", "sparse, minimal arrangement"),
    ("distorted", "distorted, noisy guitars"),
    ("clean", "clean, polished production"),
    ("lo-fi", "lo-fi, crackly recording"),
    ("atmospheric", "atmospheric, spacious reverb"),
    ("groovy", "groovy bassline"),
    ("glitchy", "glitchy electronic sounds"),
    ("raw", "raw, live recording"),
    ("orchestral", "full orchestra"),
    ("acoustic", "acoustic instruments played by hand"),
    ("electronic", "electronic music made with synthesizers"),
    ("vintage", "vintage recording from the 1960s"),
    ("modern", "modern, contemporary production"),
    // instruments
    ("piano", "solo piano"),
    ("acoustic guitar", "acoustic guitar"),
    ("electric guitar", "electric guitar"),
    ("bass", "prominent bass guitar"),
    ("drums", "loud drums"),
    ("drum machine", "drum machine beat"),
    ("synthesizer", "analog synthesizer"),
    ("strings", "string section, violins and cellos"),
    ("brass", "brass section, trumpets and trombones"),
    ("saxophone", "saxophone"),
    ("organ", "hammond organ"),
    ("flute", "flute"),
    ("harp", "harp"),
    ("percussion", "hand percussion, congas and shakers"),
    ("808", "808 bass and hi-hats"),
    ("sitar", "sitar"),
    // voice
    ("instrumental", "instrumental music with no vocals"),
    ("male vocals", "male singer"),
    ("female vocals", "female singer"),
    ("choir", "choir singing"),
    ("rapping", "rapping"),
    ("whispered", "whispered, breathy vocals"),
    ("screamed", "screamed vocals"),
    ("falsetto", "falsetto singing"),
    ("spoken word", "spoken word"),
    // setting
    ("night drive", "music for driving at night"),
    ("rainy day", "music for a rainy day"),
    ("party", "party music"),
    ("study", "music to study and focus"),
    ("sleep", "music to fall asleep to"),
];

/// Every word a track can be described with: `WORDS`, then the styles.
pub fn vocabulary() -> Vec<(String, String)> {
    WORDS
        .iter()
        .map(|(word, phrase)| (word.to_string(), phrase.to_string()))
        .chain(STYLES.iter().map(|style| (style.to_string(), style_prompt(style))))
        .collect()
}

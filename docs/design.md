# Design notes

Why things are the way they are. The [README](../README.md) covers how to run
it; this covers the reasoning and the measurements behind it.

## Why the audio has to be analysed

Qobuz's API returns metadata only: genre, label, release date, performers.
There is no BPM, no key, no energy. Spotify's `/audio-features`, which everyone
used to build this kind of thing, was removed in Nov 2024, and AcousticBrainz has
been frozen since Jul 2022. So the acoustic half of every vector is computed
from the audio itself, which a Qobuz subscription makes possible via
`track/getFileUrl`.

## Extract once, keep what cannot be recomputed

Audio excerpts are disposable, so analysis stores what would cost a download
to get back: the descriptors and the full 512-d CLAP embedding. Everything else
is derived at `build-space` time, which costs seconds and no network, the
mood and style blocks included, since they are computed from the stored
embedding (see below). Changing a label, a weight or the PCA is a rebuild,
never a re-analysis.

## One binary, client and server

Everything that is not the screen is `two-khz-server`: the Qobuz client and
its credentials, the SQLite corpus, the crawl, `analyse`, `build-space`,
`layout`, the CLAP models. The desktop and Android apps are both clients of it
and hold nothing but a synced copy of the space and the slim catalogue. There
is no local mode; a desktop user runs the server on the same machine and
pairs with it like any other device.

The pipeline used to be Python, Essentia's descriptors and EffNet heads, CLAP
under torch, UMAP, driven as `uv run` subprocesses. It was ported so that
deploying meant one binary instead of a multi-GB x86-only image with its own
Python, torch, TensorFlow and ffmpeg. The port made three substitutions:

- **CLAP runs in ONNX Runtime.** Xenova's export of the same checkpoint, pinned
  by revision and checked by hash on download. The Rust mel front end matches
  transformers' `ClapFeatureExtractor` to within 0.01 dB, and the embedding's
  cosine to the torch model's is above 0.9999.
- **Essentia is gone entirely.** Its mood and style classifiers are replaced by
  zero-shot scores against CLAP's text tower; its classical descriptors by
  plain signal processing (`pipeline/descriptors.rs`), EBU R128 loudness, a
  spectral-flux onset envelope for tempo and onset rate, chroma against
  Krumhansl-Kessler profiles for key.
- **UMAP is a port** of the parts of umap-learn the map needs: cosine kNN,
  fuzzy union, the same curve fit and epoch sampling, PCA rather than spectral
  initialisation. Deterministic.

What the zero-shot labels cost: Essentia's heads were trained classifiers, and
a similarity to "sad, melancholic music" is noisier than one. The mood and
style blocks are also now linear views of the same embedding the semantic
block holds, so they reweight what CLAP heard rather than adding an
independent opinion. What they buy: no second model, no second front end,
labels that are plain text in `pipeline/labels.rs`, and a permissive licence.

## The space

| block | dims | source |
|---|---|---|
| tempo | 2 | folded `log2(bpm)`, onset rate |
| key | 3 | circle-of-fifths position, scaled by detection strength |
| dynamics | 3 | integrated loudness, dynamic complexity, loudness range |
| timbre | 4 | spectral centroid, rolloff, flatness, zero-crossing rate |
| mood | 8 | CLAP contrasts: energy, valence, aggression, danceability, darkness, acoustic, vocals, complexity |
| style | 20 | CLAP similarity to 60 style phrases → PCA |
| era | 1 | release year |
| semantic | 40 | CLAP audio embedding → PCA |

Each block is z-scored per column and L2-normalised per row **before** being
written out. Both halves matter: column z-scoring stops one wide-range
descriptor swamping its neighbours, and row normalisation stops the 40-d
semantic block silently dominating the 2-d tempo block regardless of any weight
you set.

Weights are applied by the client at query time, which is what makes the
sliders live.

Decisions worth knowing about:

- **Tempo is folded into one octave.** Drum & bass detected at 86 instead of 172
  is the classic beat-tracker failure. Folding makes the metric immune to it
  rather than patching it per genre. Raw BPM is still stored for display.
- **A mood is a contrast, not a similarity.** Each is scored as similarity to
  one phrase minus similarity to its opposite ("energetic, intense music" against
  "calm, gentle music"). Raw similarity to a single mood phrase mostly measures
  how much a track sounds like music at all.

**Checkpoint choice.** `laion/clap-htsat-unfused`, deliberately not
`laion/larger_clap_music`, the latter's text tower returns near-constant
embeddings (mean pairwise cosine ≈ 0.999), which would destroy text steering
and, now, every mood and style score with it.

## How fast `analyse` goes

One 90s excerpt costs about **1.3 core-seconds**: CLAP 0.84s, resampling to
48kHz 0.41s, the descriptors 0.09s (Ryzen 9 9950X3D, one thread). The Python
pipeline paid about 19. Giving one ONNX session four threads only brings it to
0.9s wall, so `analyse` runs several single-threaded workers instead, one per
four hardware threads, at most four by default, each with its own session,
fed excerpts fetched on the stage's own thread through the one rate-limited
client. Only that thread writes to SQLite. A worker holds about 600MB, so four
is ~2.5GB.

```sh
two-khz-server analyse                 # one worker per 4 hardware threads, up to 4
two-khz-server analyse --workers 12    # override, e.g. re-analysing from the cache
two-khz-server analyse --fetchers 16   # if fetching is the laggard
```

The ceiling is Qobuz's rate limit, not the CPU: one signed URL per track at the
default 2 req/s is ~7,200 tracks/hour, and three workers already keep up with
it, measured at ~2 tracks/s from a warm cache with four. The excerpt itself
comes from the CDN and does not count against the account.

**Analysis uses MP3 320, not FLAC.** Roughly 10× less bandwidth, and CLAP
resamples to 48kHz anyway. It is also a constant bitrate, which is what makes
"the middle 90 seconds" a byte range: the analyser requests just that slice,
finds the first frame boundary itself, and decodes it with symphonia, no
ffmpeg, and about 3.6MB per track. FLAC is reserved for listening.

## Crawl ordering

The frontier is ordered artists-before-albums within a hop, so the early phase of
a large crawl discovers thousands of artists before the track count moves. That
is working as intended, not a stall.

In-app crawling is stepped one frontier item at a time, and the Qobuz client is
handed back between steps, so searching and playback keep working throughout.
Measured: searches returned in 0.2 to 2.4s with a crawl running.

## Artist pull

Without it a radio walk tends to sit inside one discography, because a prolific
artist occupies a tight cluster: 14.8 distinct artists per 20 tracks. The
penalty is subtracted from a candidate's similarity once per time that artist
already appears in the walk, so the push outward grows the longer you stay. At
the default 0.10 that is 19.4 distinct artists per 20, and it costs almost
nothing in similarity between jumps, 0.786 against 0.796.

It has to reach back over the whole walk rather than a few steps: the path
constraints already forbid an artist repeating within three, so a penalty over
that same window measurably does nothing at any strength.

## Stopping a stage

Stages run inside the server, each on a thread of its own. **Stop** sets a flag
the stage checks as it goes: `analyse` between tracks, letting the excerpts
already in a worker finish (about a second each); `layout` every 25 epochs;
`build-space` is seconds anyway. A stage started from a client deliberately
outlives it, that is the point of starting `analyse` from a phone and walking
away. It does not outlive the server: stopping the server stops the stage, and
everything stored so far stays stored.

## Testing

```sh
cd server
cargo test                                         # unit tests, no network
cargo test --release -- --ignored smoke            # the demo corpus, end to end
```

Neither needs Qobuz credentials. The smoke test synthesises eight albums with
known character (beatless drones, 174 BPM breaks, a bright pop pulse) runs
them through the real analyser, space and layout, and checks tracks from one
album land together (30 of 32 are each other's nearest neighbour) and that
tempo comes out where it was put. It fetches the CLAP weights on first run.

Two more ignored tests need something from outside: `front_end_matches_transformers`
compares the mel front end and the embedding against reference files written by
transformers, and `excerpt_from_the_middle_of_a_served_file` runs the byte-range
fetch against a real MP3 (`TWO_KHZ_MP3_SAMPLE`).

## Not yet verified

Whether Qobuz's signed file URLs are IP-bound. `track/getFileUrl` returns an
opaque CDN URL and whether they tie it to the requesting address is their
policy, not ours. If they do, playback has to proxy through the server and the
bandwidth story changes. Ten-minute test: mint a URL on the server, then
`curl -r 0-1000` it from a host on another network.

## Android: the open questions

- **The layout is still desktop-shaped.** Three columns and a canvas map do not
  belong on a 1080px-wide screen; it is usable but not designed. The panels are
  ordinary flexbox, so this is CSS work, not architecture.
- **Background playback.** Audio is an `<audio>` element in a WebView, which
  Android throttles when backgrounded, with no MediaSession, lockscreen controls
  or audio focus. For a music app that is the product, not a rough edge, it
  wants a native `MediaSessionService` fed the signed URL, with Rust keeping
  only the queue.
- **The applicationId must differ from `dev.dioxus.main`.** The CLI emits
  `typealias BuildConfig = <applicationId>.BuildConfig` into a file that is
  itself in `package dev.dioxus.main`, so reusing the id makes that typealias
  refer to itself and Kotlin fails to compile.

## Web

Not working, and honestly characterised rather than promised.

`cargo check --target wasm32-unknown-unknown --features web` gets further than
expected: **every dependency compiles, including rusqlite and memmap2.**

Compiling is not the hard part. The hard part is that both of those crates
compile and then cannot *work*: there is no filesystem to `std::fs::read` a
`catalog.db` from and nothing to `mmap`. A real web client needs the space
fetched into memory rather than mapped, and the catalogue served as a flat
buffer instead of SQLite, which would also simplify Android. That is a week-ish
of work on the data layer, not an afternoon of cfg attributes.

## Licensing

The CLAP checkpoint is Apache-2.0. With Essentia's CC BY-NC-SA 4.0 weights
gone, nothing in the pipeline restricts commercial use any more.

## Packaging

Each Ubuntu release is built on its own runner, because each links against its
own webkit and glibc, a 24.04 build is not safe to hand to a 26.04 machine.
`ubuntu-latest` is deliberately unused: it migrates from 24.04 to 26.04 during
October to November 2026, which would quietly collapse the matrix into two
identical legs.

The desktop and server packages are separate because the server takes the app
crate with default features off, which keeps dioxus, wry and GTK out of it
entirely: the desktop `.deb` depends on twelve libraries including webkit, the
server `.deb` on three. A server box should not be made to install a browser
engine.

Dependencies are computed by `dpkg-shlibdeps` on the release being built for
rather than hardcoded, because 24.04's 64-bit `time_t` transition renamed
several of them, `libgtk-3-0` became `libgtk-3-0t64`, `libssl3` became
`libssl3t64`, so a fixed list would be wrong on one release or the other.

# Mix plan

Stelly is where music is found, day to day; stelly-mix is where it is mixed,
at home, from what was found. So the mix analysis, beats, bars, phrases, key
and each phrase's embedding, has to be there for what was found, without
anyone asking for it. Each step is one PR; the PR that does a step marks it
here. Cost: **S** hours, **M** a day or more, **L** several days or more.
Paths are `stelly` unless the row names `mix` (stelly-mix).

## The shape

- **One download, both analyses.** A whole track decoded for mixing already
  holds the 90s excerpt the space is built from: cut it from the samples, as
  `excerpt_of_whole` does for files that are not constant-bitrate MP3, and
  write `features` too when it is missing or outdated. The other way round
  saves nothing: an excerpt cannot make a whole-track analysis.
- **The tables stay apart.** `features` and `mix` carry versions of their
  own, and `build-space` reads only `features`: bumping one never redoes the
  other.
- **A queue, not a job.** `mix_queue(track_id, priority, reason, queued_at)`,
  kept like `frontier` so it outlives a restart, drained by a worker that
  runs for as long as the server does, as the crawl does. Most urgent first:

  | Priority | Why it is there | Pace |
  |---|---|---|
  | asked | stelly-mix or the app wants it now, and waits | at once, ahead of all |
  | liked | a track or album liked in the last day | every worker |
  | selection | `server-analyse` and the pipeline controls' box | every worker |
  | played | listened through, see below; older likes, and an artist's, its 20 catalogued tracks nearest the likes | one worker, when nothing else waits |

  Played tracks are the slow, passive growth of the library: one at a time,
  behind everything else, and only those with no mix row yet.
- **Listens are recorded, for later.** One row per track played:
  `listens(user_id, device_id, track_id, started_at, listened_seconds,
  track_seconds, ended)`, `ended` one of `finished`, `skipped`, `stopped`.
  Kept raw, nothing aggregated yet, so whatever is wanted of them later can
  be computed from all of them. The server's session sees everything needed:
  the output's `Report`s while it plays, and `Ended`, `Step`, `Jump` and
  `Load` when the track changes. Listened time is what the reports say was
  played, a seek is not listening. A *play-through* is half the track or
  four minutes listened, whichever comes first.
- **stelly-mix opens on what was found.** Its library is what the server has
  analysed, newest likes first, each with its BPM, key and phrases already
  there; a `stelly:` track with no entry is asked for, not refused.

## Phase 1: one analysis

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 1.1 | A whole-track analysis writes `features` too, when it is missing or outdated, from the excerpt cut out of the samples it already has | `server/src/mix.rs`, `pipeline/analyse.rs`, `pipeline/audio.rs` | S | #172 |
| 1.2 | `mix_queue` and the worker that drains it, replacing `MixService`'s one-off job; `server-analyse` and its selections queue at `selection`; stop drops the queued selection | `schema.sql`, `server/src/mix.rs`, `hub.rs`, `routes.rs` | M | #173 |
| 1.3 | Tracks analysed this way join the space: the rebuild `analyse` triggers, triggered here too | `server/src/hub.rs` | S | #174 |

## Phase 2: listens and likes fill it

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 2.1 | `listens`, recorded from the session: opened on a new current track, closed with what was listened and how it ended | `schema.sql`, `server/src/playback.rs` | M | #175 |
| 2.2 | A play-through queues the track at `played`, run one at a time when nothing else waits | `server/src/playback.rs`, `mix.rs` | S | #176 |
| 2.3 | Each like queued once, newest first: a track or album liked in the last day at `liked`, older ones and artists' 20 catalogued tracks at `played`; with no provider answering, the queue waits rather than failing everything | `server/src/mix.rs`, `hub.rs`, `provider.rs` | S | #177 |
| 2.4 | `POST /api/mix/tracks/{id}/analyse`: queued at `asked`, answers when it is done | `server/src/routes.rs`, `mix.rs` | S | #178 |

## Phase 3: the app shows it

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 3.1 | Mix types of its own in `stelly-core`, so the app never links stelly-mix, and a track's mix summary from the server | `core/src/api.rs`, `server/src/routes.rs` | S | #179 |
| 3.2 | Track details: BPM and whether it sits on a grid, Camelot key and strength, first downbeat, bars, phrases, sections, over the excerpt's tempo and key; queued or analysing while it is not there yet | `app/` | M | #180 |
| 3.3 | BPM and key in album track lists | `app/` | S | #181 |
| 3.4 | Pipeline controls: the queue, what it is on and why each track is in it, the selection box, stop | `app/`, `server/src/routes.rs` | M | #182 |

## Phase 4: stelly-mix opens on what was found

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 4.1 | The family's likes with when they were liked, `GET /api/mix/liked?since=` | `server/src/routes.rs` | S | |
| 4.2 | `sync` brings them, and the app opens on the last liked | mix `src/stelly.rs`, `app/` | M | |
| 4.3 | `next`, `set` and `mix` on a `stelly:` track with no entry ask for it and wait | mix `src/stelly.rs`, `src/cli.rs` | S | |

## Later

- The nearest neighbours of each like, queued at `played`: a mix needs what
  goes with a find, not only the find.
- The server's memory after a burst: ~5 GB resident on a 32-core machine once
  the workers are gone, what the allocator keeps of their ONNX sessions and
  decoded tracks. Measured with 1.2; the one-off job before it kept the same.
- The rest of the catalogue, when the server is idle, off by default: a whole
  track is about ten times the download and the CPU of an excerpt.
- Whatever the listens are wanted for: skips against the space, what a radio
  should leave out, favourites ordered by plays.

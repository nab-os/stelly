# Performance plan

Everything found profiling Stelly, its server and its two providers on
2026-10-19, in the order to do it. Each step is one PR; the PR that does a
step marks it here. Cost: **S** hours, **M** a day or more, **L** several days or more.
Paths are `stelly` unless the row names `qobuz` (stelly-qobuz), `ytmusic`
(stelly-ytmusic) or `provider` (stelly-provider).

## What was measured

On the local corpus copy (23,465 tracks in the space, 32 cores); production
holds 142,292, so walks there scale about 6×.

| What | Result |
|---|---|
| `/api/me`, `/api/session`, favourites, pipeline status | ~0.5 ms |
| neighbours / path / even path / mood / drift | 2 / 14 / 44 / 13 / 58 ms |
| radio, 50 → 200 → 2,000 tracks | 83 ms → 337 ms → 3.3 s, `count` uncapped |
| 16 radios at once | served one at a time: 0.34 s … 5.4 s |
| space search while 4 radios run | 1.29 s (3 ms idle) |
| first `/api/space` / first describe | 483 ms / 1.49 s |
| `/api/corpus`, polled by every app every ~2 s | 70 ms |
| any authenticated route during a 3 s SQLite write lock | 2.7–2.9 s |
| Qobuz calls alone / back to back | 90–250 ms / 0.44–0.52 s (2 req/s) |
| Qobuz album behind 6 interactive-lane calls | 1.47 s |
| Qobuz first stream URL after start / next | 1.14 s / 0.53 s |
| YouTube Music stream URL (yt-dlp) | 1.54 s, 79 MB per run |
| YouTube Music relay with yt-dlp 2026.03.17 | 0 bytes after 1.7 s: YouTube serves its URLs' first ~1 MB only (2026.08.19 is fine, 2.9 s an extraction) |

On production (2026-10-19, 142,292 tracks, behind nginx), one request at a time:

| What | Result |
|---|---|
| health, me, session, space, pipeline, crawl | 9–13 ms |
| favourites tracks (242 KB) | 28 ms |
| `/api/corpus` | **3.6 s**, and every app polls it every ~2 s |
| index / points / meta | 2.1 / 2.6 / 4.7 MB on the wire, **uncompressed**: nginx does not pass `Accept-Encoding` on |
| neighbours / path / even path / mood / drift | 80 / 496 / 753 / 148 / 978 ms |
| radio, 50 tracks | **1.79 s**, holding the space for everyone |
| provider search, cold / cached | 3.5 s / 27 ms |

## Phase 0: measure first

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 0.1 | `bench/` scripts replaying the measurements above: routes, concurrent radios, search under load, a held write lock, providers, relay ranges | `bench/` | S | #95 |
| 0.2 | App baselines: launch to favourites, page open, search, time to sound, map open (`STELLY_TIMINGS=1`, `debug.stelly.timings`) | app | S | #96, baseline to record |
| 0.3 | The same route timings on the production server, read-only, sequential | prod | S | done, above |

## Phase 1: broken or dangerous

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 1.1 | A stream YouTube refuses is a 502 that says why, not a promised body never sent | ytmusic `routes.rs` | S | ytmusic#3 |
| 1.2 | A URL found under a minute ago is not re-extracted on refusal | ytmusic `streams.rs` | S | ytmusic#3 |
| 1.3 | The release watcher on the bulk lane | `server/src/hub.rs:182` | S | #91 |
| 1.4 | Cap `count` on generate (500, clamped); a 500 ms budget on the 768-wide path retry; order already bounded by `SORT_CAP` | `server/src/routes.rs`, `core/src/paths.rs:453` | S | #94 |
| 1.5 | Auth answers 503, not 401, on a database error; a failed `last_seen` write lets the request through | `server/src/auth.rs:332` | S | #93 |
| 1.6 | A timeout and one connect-error retry on session commands; pool idle timeout and TCP keepalive | `app/src/backend/remote.rs:254, 677` | S | #92 |

## Phase 2: server quick wins

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 2.1 | Device tokens held in memory (TTL or `PRAGMA data_version`); `last_seen` written at most once a minute, off the runtime | `server/src/auth.rs` | S–M | #100 |
| 2.2 | Text model loaded at start in `spawn_blocking`; `can_steer` an `AtomicBool` | `server/src/hub.rs:462–501` | S | #101 |
| 2.3 | Embeds in `spawn_blocking`; phrase → embedding LRU; mix `BASELINE` precomputed | `server/src/hub.rs:470`, `routes.rs:880` | S | #102 |
| 2.4 | Describe vocabulary and baseline built in the background after a reload | `server/src/hub.rs:484`, `space.rs:270` | S | #103 |
| 2.5 | `/api/corpus` cached 10–30 s, cleared when a stage ends; `db::connect`, not `open_for_write` | `server/src/stages.rs:74` | S | #97 |
| 2.6 | Partial index on `features(track_id) WHERE clap_f32 IS NOT NULL`, covering `(track_id, extractor_version)` | `schema.sql` | S | #98 |
| 2.7 | Crawl counts taken before the status mutex | `server/src/hub.rs:860` | S | #99 |
| 2.8 | Compression on every route; TCP_NODELAY on the listener | `server/src/routes.rs`, `main.rs:446` | S | #107 |
| 2.9 | Partial selection instead of full sorts in neighbours, radio, snap | `core/src/paths.rs:104, 168, 232` | S | #104 |
| 2.10 | Hidden artists as a per-row bitset; artist ids in their own array; radio penalty as a `Vec<u16>` | `core/src/db.rs:193`, `paths.rs:54–165` | S | #105 |
| 2.11 | Legacy conversion marked done; space load reads only the space's ids and only the bpm | `server/src/legacy.rs:47`, `core/src/db.rs:88` | S | #108 |
| 2.12 | Mix reuses the loaded space; CLAP load without the doubled map | `server/src/mix.rs:149`, `core/src/db.rs:45` | S | #109 |
| 2.13 | Layout, dedupe and analyse on all cores but one, or at a lower priority | `server/src/pipeline/layout.rs`, `dedupe.rs` | S | #110 |
| 2.14 | Pipeline log published through `Notify`, not a 200 ms poll per subscriber | `server/src/routes.rs:572` | S | #111 |
| 2.15 | nginx in front of production: pass `Accept-Encoding` through (or `gzip on; gzip_proxied any;` with the JSON and octet-stream types) | prod nginx | S | done, nginx passes Accept-Encoding |

## Phase 3: app quick wins

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 3.1 | Poll every 5–10 s unless the pipeline view is open or a job runs; a slow heartbeat for offline | `app/src/ui/crawler.rs:68`, `pipeline.rs:128` | S | #112 |
| 3.2 | Status set only when it changed; the "add" badge its own component | `app/src/ui/pipeline.rs:141`, `crawler.rs:73`, `screens.rs:105` | S | #113 |
| 3.3 | Pipeline log stream open only while the log is shown | `app/src/ui/pipeline.rs:157` | S | #114 |
| 3.4 | The output presigns the next track | `app/src/ui/player.rs:288, 357` | S | #115 |
| 3.5 | Play album from a row menu reuses tracks already held | `app/src/ui/menu.rs:320` | S | #116 |
| 3.6 | Likes flip at once, roll back on error | `app/src/ui/screens.rs:26` | S | #117 |
| 3.7 | Liked ids from the home answer and `favourites.json`, not fetched twice | `app/src/ui/library.rs:428` | S | #118 |
| 3.8 | Map points loaded on the first map open, not at launch | `app/src/app.rs:353, 752`, `assets/map.js:150` | S | #119 |
| 3.9 | Space cache written only on a new stamp, off the UI thread; parsed off it too | `app/src/app.rs:354, 1065` | S | #120 |
| 3.10 | Rows keyed by id, not index | `app/src/ui/library.rs`, `queue.rs:160`, `screens.rs` | S | |
| 3.11 | Search keeps its results while refining; a cancellable 150 ms debounce | `app/src/ui/library.rs:692`, `app.rs:698, 830` | S | |
| 3.12 | Seek and volume commit on release | `app/src/ui/player.rs:787, 904` | S | |
| 3.13 | Android: non-passive `touchmove` only during a queue drag | `app/assets/queue-drag.js:194, 283` | S | |
| 3.14 | Map: `resize()` skipped when unchanged; cached rect on touch; card placed with `transform` | `app/assets/map.js:179, 759, 1027` | S | |
| 3.15 | Android HTTP client built once, reused by the update check | `app/src/backend/remote.rs:729`, `update/mod.rs:81` | S | #121 |
| 3.16 | Artist albums and similar fetched together; capabilities and providers too | `app/src/ui/library.rs:916`, `app.rs:525` | S | |
| 3.17 | "Liked" read without cloning the sets per tick; seek line in its own component | `app/src/ui/player.rs:755`, `library.rs:462` | S | |
| 3.18 | Covers decoded before shown, applied at once when cached, off-screen ones skipped, `data-cover` changes observed, larger heroes | `app/assets/covers.js` | S | |
| 3.19 | `transform`/`opacity` animations instead of `box-shadow` and `left` | `app/assets/style.css:1430, 1654` | S | |
| 3.20 | The playing highlight follows the track (`current()` peeks); a class toggled in JS | `app/src/ui/player.rs:51` | S | |
| 3.21 | Hidden artists and the session cached on disk | `app/src/app.rs:476` | S | |
| 3.22 | The initial view opened synchronously, not in `use_future` | `app/src/app.rs:563` | S | |

## Phase 4: provider quick wins

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 4.1 | Relay forwards each piece as it arrives; 403 renewal before any byte | ytmusic `routes.rs:257` | S–M | |
| 4.2 | A missing size read from `clen=` or a 1-byte range, never by re-running yt-dlp | ytmusic `routes.rs:231` | S | |
| 4.3 | yt-dlp single-flight, 2–3 at a time, 20 s timeout, `x-priority` honoured on `/file` | ytmusic `streams.rs` | S–M | |
| 4.4 | ids.db in WAL with `synchronous=NORMAL`; one transaction per answer; known ids in memory | ytmusic `ids.rs` | S | |
| 4.5 | Artist "more" shelves fetched together | ytmusic `routes.rs:327` | S | |
| 4.6 | The working app secret probed at start and remembered | qobuz `qobuz.rs:377`, `main.rs:134` | S | |
| 4.7 | Pages after the first fetched together once the total is known | qobuz `qobuz.rs:206, 268, 304` | S–M | |
| 4.8 | gzip/brotli from Qobuz | qobuz `Cargo.toml` | S | |
| 4.9 | Warm connections: pool idle, TCP and HTTP/2 keepalive, connect timeout | qobuz, ytmusic | S | |
| 4.10 | `getReleasesList` falls back only on 404 or a parse error | qobuz `routes.rs:264` | S | |
| 4.11 | `find` stops at the first match | ytmusic `parse.rs:78` | S | |

## Phase 5: server structure

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 5.1 | No `Mutex<Engine>`: immutable catalogue, haystacks and manifest behind `Arc`, weighted spaces in a small LRU keyed by the weights | `server/src/space.rs:31`, `core/src/engine.rs` | M–L | |
| 5.2 | SIMD-friendly dot: rows padded to 88, 8 accumulators, AVX2/FMA by multiversioning | `core/src/space.rs:206`, `paths.rs:534, 736` | S–M | |
| 5.3 | One persistent pool (rayon) for corridor, kNN and CLAP scans | `core/src/paths.rs:284–339` | M | |
| 5.4 | A DB layer: `on_db` over `spawn_blocking`, a read pool with mmap, cache_size and temp_store, one writer | `server/src/db.rs` and callers | M | |
| 5.5 | Refs: a read pass without a transaction, `immediate_transaction` only for new items, short busy timeout | `server/src/provider.rs:142, 280`, `refs.rs` | M | |
| 5.6 | Dedupe and layout commit in chunks | `server/src/dedupe.rs:269`, `pipeline/layout.rs:69` | S–M | |
| 5.7 | Provider calls under a 10–15 s timeout; capabilities probed in the background, concurrently | `server/src/provider.rs:53–265`, provider | M | |
| 5.8 | Stale-while-revalidate on the catalogue and account caches | `server/src/hub.rs:154`, `cache.rs` | M | |
| 5.9 | Index, points and meta serialised and compressed once in `derive`; `ETag` = stamp, 304 | `server/src/space.rs:299`, `routes.rs:620` | S–M | |
| 5.10 | Map labels binary instead of 6.3 MB of JSON | `core/src/map.rs:345`, `app/assets/map.js` | M | |
| 5.11 | SSE: a session serialised once per version; queue diffs | `server/src/routes.rs:328`, `playback.rs:434`, `core/src/session.rs` | M | |
| 5.12 | Crawl, analyse and mix through a shared cache too | `server/src/cli.rs`, `pipeline/analyse.rs`, `mix.rs` | M | |

## Phase 6: app structure

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 6.1 | Every long list rendered in growing windows, or `content-visibility: auto` | `app/src/ui/library.rs:1533, 1977`, `screens.rs:522, 743` | S–M | |
| 6.2 | Queue rows mounted once the drawer has opened | `app/src/ui/queue.rs:122` | S–M | |
| 6.3 | Transitions: visible clone only, cover flown with `transform`, no per-frame mask, page-in on `.hero-meta` | `app/assets/transitions.js` | M | |
| 6.4 | Scroll position kept in JS, not sent to Rust per event | `app/src/ui/library.rs:1065` | M | |
| 6.5 | Shelves shared by `Rc`, disk writes in the background | `app/src/ui/library.rs:664, 680, 736` | S–M | |
| 6.6 | Recent album and artist pages, portrait and bio cached on disk | `app/src/ui/library.rs`, `screens.rs:626` | M | |
| 6.7 | Map card filled in JS from the labels; selection highlight a CSS class | `app/src/app.rs:775`, `ui/mod.rs:210` | M | |
| 6.8 | SSE silence ~20 s; reconnect at once on an Android network change | `app/src/backend/remote.rs:623` | S | |
| 6.9 | webkit2gtk hardware acceleration policy | `app/src/main.rs:43` | S | |

## Phase 7: provider structure

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 7.1 | Gateway cache with single-flight: metadata 24 h+, discographies 6–24 h, search 5 min, file URLs until near expiry | qobuz | M | |
| 7.2 | Browse cache with single-flight; stream URLs kept in ids.db with their real `expire=` | ytmusic | M | |
| 7.3 | One shared pause on a 429; interactive calls get one short retry, then fail | qobuz `queue.rs`, `qobuz.rs:197` | M | |
| 7.4 | Superseded searches dropped from the lane | qobuz `queue.rs` | S–M | |
| 7.5 | Stream URLs warmed ahead: next queued track, first tracks of an opened album | stelly, qobuz, ytmusic | M | |
| 7.6 | Docker: yt-dlp cache on a volume, the zipapp rather than PyInstaller | ytmusic `Dockerfile` | S | |
| 7.7 | A resident yt-dlp instead of a process per track | ytmusic | L | |
| 7.8 | Compression between gateway and server, for gateways off loopback | qobuz, provider | S | |
| 7.9 | yt-dlp kept current: a scheduled image rebuild on new releases, and a start-up check that a stream answers past its first MB | ytmusic | S–M | |

## Phase 8: scale, before the space passes ~150k

| # | Step | Where | Cost | Status |
|---|---|---|---|---|
| 8.1 | Per-track top-128 neighbours per weights hash, filled lazily | core | M | |
| 8.2 | Vectors in f16 or i8, CLAP in f16 or truncated; no heap copy of the mmap | core, server | L | |
| 8.3 | An ANN index (HNSW/IVF) for the default weights, exact re-rank | core | L | |
| 8.4 | The map rasterised in a Worker with OffscreenCanvas | `app/assets/map.js` | L | |

## Order

Phase 0 and 1, then 2.5, 2.6 and 3.1 (corpus is 3.6 s on production and
polled every 2 s by each app), 2.15, 2.1, 2.2, 3.2, 2.9, 2.10 (radio is
1.8 s there), 3.4, 3.6, 4.6, which clear most of the measured stalls; then the rest of phases 2 to 4; then 5.1
and 5.4, phases 6 and 7; phase 8 as the corpus grows. Each phase ends by
re-running 0.1 and 0.2 and recording before and after in `docs/design.md`.

# Benchmarks

Timings to take before and after a change, on the same corpus, so a change
meant to be faster can be shown to be. Plain bash and curl, python3 for JSON.

## `server.sh`

Against a running `stelly-server`, given `STELLY_SERVER` and `STELLY_TOKEN`.
Each request is made three times (`-n` to change it) and the median shown,
with the spread:

- the light routes the apps call at start and on every view: health, me,
  session, space, favourites, corpus, pipeline, crawl, blocked;
- the space's payloads, the index, the points and their meta and a search,
  plain and with `--compressed`, sizes as they went over the wire;
- every generate recipe at 50, and radio at 200, from two tracks a search for
  "love" finds (`SEED_A` and `SEED_B` to fix them).

`--cold` times the first `/api/space` and `/api/space/describe/{id}`, which
load what the space needs on first use: restart the server just before.

`--load` sends sixteen radio-200 at once and a search among them, and shows
when each came back. `--write-lock` holds `BEGIN IMMEDIATE` on `stelly.db`
for three seconds and times health, me, session and space meanwhile, to see
which routes wait on the database.

**`--load` stalls the server for everyone using it, and `--write-lock` blocks
its writes.** Only ever against a local copy, never a shared or production
server. `--write-lock` refuses unless the server is on loopback and
`STELLY_DATA_DIR` is set.

## `providers.sh`

Against a provider, given `PROVIDER_URL` and `PROVIDER_TOKEN`: each route of
the provider protocol once, search and the file URL twice to see a cache,
then the file itself, a first range, a seek, 1 MB, 2 MB and the whole file,
with the bytes received beside the bytes asked for. `--lanes` sends eight
bulk-lane requests and an interactive one, then six interactive and one more,
to see whether the interactive lane gets ahead.

These requests reach a real account with a shared budget of about two a
second, so nothing is repeated and the counts stay small.

## On a copy of the data

Never the data the server uses day to day: the first `serve` on a copy
migrates it, and the modes above write to it or starve it.

```sh
cargo build --release -p stelly-server
mkdir -p /tmp/bench/data /tmp/bench/cache
cp data/{stelly.db,catalog.db,space.bin,space.json,semantic_pca.bin} /tmp/bench/data/
export STELLY_ENV_DIR=/tmp/bench STELLY_DATA_DIR=/tmp/bench/data \
       STELLY_CACHE_DIR=/tmp/bench/cache STELLY_MODEL_DIR=$PWD/data/models \
       STELLY_PROVIDERS=http://127.0.0.1:59999
target/release/stelly-server pair --name bench --scope admin --user <user>
target/release/stelly-server serve --bind 127.0.0.1:7802 &

STELLY_SERVER=http://127.0.0.1:7802 STELLY_TOKEN=<token> bench/server.sh --load --write-lock
```

`STELLY_PROVIDERS` pointed at a port nothing listens on keeps the copy off
the real providers; the space routes do not need them.

A data directory from before providers is migrated by the first command that
opens it, and the space can try to load before that is done: if `serve` says
`no space loaded yet: no such column`, restart it.

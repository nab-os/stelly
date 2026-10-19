#!/usr/bin/env bash
# Times stelly-server's routes, one line per measurement, so a change to the
# server can be checked before and after on the same corpus.
#
#   STELLY_SERVER=http://127.0.0.1:7802 STELLY_TOKEN=<token> bench/server.sh
#
#   -n N          repeats per measurement, the median reported (default 3)
#   --cold        first /api/space and /api/space/describe after a restart
#   --load        16 radio-200 at once, and a search among them
#   --write-lock  the light routes while another writer holds stelly.db
#
# SEED_A and SEED_B fix the tracks the recipes start from; otherwise they are
# the first two a search for "love" finds, so any corpus will do.
set -u

: "${STELLY_SERVER:?set STELLY_SERVER, e.g. http://127.0.0.1:7802}"
: "${STELLY_TOKEN:?set STELLY_TOKEN, what stelly-server pair printed}"
BASE=${STELLY_SERVER%/}
N=3
COLD=0 LOAD=0 LOCK=0
while [ $# -gt 0 ]; do
    case $1 in
        -n) N=$2; shift ;;
        --cold) COLD=1 ;;
        --load) LOAD=1 ;;
        --write-lock) LOCK=1 ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

# --write-lock takes the write lock on the server's own database, so only on
# a local copy: refused before anything runs, for a server not on loopback or
# without STELLY_DATA_DIR naming where its database is.
if [ $LOCK = 1 ]; then
    host=${BASE#*://}; host=${host%%/*}
    case $host in \[*) host=${host#[}; host=${host%%]*} ;; *) host=${host%%:*} ;; esac
    case $host in
        127.*|localhost|::1) ;;
        *) echo "refusing --write-lock: $host is not loopback" >&2; exit 1 ;;
    esac
    db="${STELLY_DATA_DIR:?refusing --write-lock: set STELLY_DATA_DIR to where the server keeps its copy}/stelly.db"
    [ -f "$db" ] || { echo "refusing --write-lock: no $db" >&2; exit 1; }
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
FMT='%{http_code} %{time_total} %{size_download}\n'

# One request; prints "code seconds bytes". A refused connection is code 000
# rather than an exit, so one dead route does not end the run.
once() {
    curl -s -o /dev/null -w "$FMT" -H "Authorization: Bearer $STELLY_TOKEN" "$@" \
        || true
}

# Seconds as curl gives them, in milliseconds.
ms() { awk -v t="$1" 'BEGIN { printf "%.1fms", t * 1000 }'; }

line() { printf '%-34s %3s %9s %10s  %s\n' "$@"; }

# Repeats a request N times and reports the median time, with the spread.
measure() {
    local name=$1; shift
    local i
    : > "$TMP/runs"
    for i in $(seq "$N"); do once "$@" >> "$TMP/runs"; done
    sort -k2 -g "$TMP/runs" | awk -v name="$name" '
        { code[NR] = $1; t[NR] = $2; b[NR] = $3 }
        END {
            m = int((NR + 1) / 2)
            printf "%-34s %3s %7.1fms %10d  (%.1f-%.1f)\n",
                name, code[m], t[m] * 1000, b[m], t[1] * 1000, t[NR] * 1000
        }'
}

# The JSON body of a generate request.
recipe() { printf '{"recipe":%s,"count":%s,"penalty":0.1,"weights":{}}' "$1" "${2:-50}"; }

generate() {
    measure "$1" -X POST -H 'Content-Type: application/json' -d "$2" "$BASE/api/space/generate"
}

first=$(once "$BASE/api/health")
if [ "${first%% *}" = 000 ]; then
    echo "no server answering at $BASE" >&2
    exit 1
fi

if [ $COLD = 1 ]; then
    # Only means something as the first requests after a restart: the space
    # and the describe vocabulary load on first use. Not repeated, a second
    # run would be warm.
    echo "# cold: restart the server just before this"
    cold_seed=${SEED_A:-}
    read -r code t b < <(once "$BASE/api/space")
    line "cold /api/space" "$code" "$(ms "$t")" "$b"
    if [ -z "$cold_seed" ]; then
        cold_seed=$(curl -s -H "Authorization: Bearer $STELLY_TOKEN" "$BASE/api/space/search?q=love&limit=1" \
            | python3 -c 'import json,sys; print(json.load(sys.stdin)["tracks"][0]["track_id"])' 2>/dev/null)
    fi
    read -r code t b < <(once "$BASE/api/space/describe/$cold_seed")
    line "cold /api/space/describe/$cold_seed" "$code" "$(ms "$t")" "$b"
    echo
fi

# Seeds from the corpus being measured, unless given.
if [ -z "${SEED_A:-}" ] || [ -z "${SEED_B:-}" ]; then
    seeds=$(curl -s -H "Authorization: Bearer $STELLY_TOKEN" "$BASE/api/space/search?q=love&limit=50" \
        | python3 -c '
import json, sys
ids = []
for t in json.load(sys.stdin).get("tracks", []):
    if t["track_id"] not in ids:
        ids.append(t["track_id"])
print(*ids[:2])' 2>/dev/null)
    read -r found_a found_b <<< "$seeds"
    SEED_A=${SEED_A:-${found_a:-}}
    SEED_B=${SEED_B:-${found_b:-}}
fi
if [ -z "${SEED_A:-}" ] || [ -z "${SEED_B:-}" ]; then
    echo "no two tracks found for \"love\" in the space; set SEED_A and SEED_B" >&2
    exit 1
fi

echo "# $BASE, median of $N, seeds $SEED_A $SEED_B"
line name code time bytes "(min-max)"

echo "# light routes"
for r in health me session space favourites/tracks favourites/albums favourites/artists \
         corpus pipeline crawl blocked; do
    measure "/api/$r" "$BASE/api/$r"
done

echo "# space payloads, plain then --compressed"
for r in index points points/meta 'search?q=love'; do
    measure "/api/space/$r" "$BASE/api/space/$r"
    measure "/api/space/$r gz" --compressed "$BASE/api/space/$r"
done

echo "# generate"
generate "neighbours 50" "$(recipe "{\"mode\":\"neighbours\",\"seed\":$SEED_A}")"
generate "radio 50" "$(recipe "{\"mode\":\"radio\",\"seed\":$SEED_A}")"
generate "radio 200" "$(recipe "{\"mode\":\"radio\",\"seed\":$SEED_A}" 200)"
generate "path 50" "$(recipe "{\"mode\":\"path\",\"a\":$SEED_A,\"b\":$SEED_B,\"even\":false}")"
generate "path even 50" "$(recipe "{\"mode\":\"path\",\"a\":$SEED_A,\"b\":$SEED_B,\"even\":true}")"
generate "drift 50" "$(recipe "{\"mode\":\"drift\",\"seed\":$SEED_A,\"phrase\":\"dark ambient\"}")"
generate "mood 50" "$(recipe '{"mode":"mood","phrase":"dark ambient"}')"

if [ $LOAD = 1 ]; then
    # STALLS THE SERVER FOR EVERYONE ON IT for as long as it runs: sixteen
    # radio-200 at once is meant to fill the space's workers. Never run this
    # against a shared or production server, only a local copy.
    echo "# load: 16 radio 200 at once, a search fired among them"
    body=$(recipe "{\"mode\":\"radio\",\"seed\":$SEED_A}" 200)
    for i in $(seq 16); do
        once -X POST -H 'Content-Type: application/json' -d "$body" \
            "$BASE/api/space/generate" > "$TMP/load.$i" &
    done
    sleep 0.2
    once "$BASE/api/space/search?q=love" > "$TMP/load.search" &
    wait
    for i in $(seq 16); do
        read -r code t b < "$TMP/load.$i"
        line "radio 200 #$i" "$code" "$(ms "$t")" "$b"
    done
    read -r code t b < "$TMP/load.search"
    line "search during load (+0.2s)" "$code" "$(ms "$t")" "$b"
fi

if [ $LOCK = 1 ]; then
    echo "# write lock: BEGIN IMMEDIATE held 3 s on stelly.db"
    python3 - "$db" "$TMP/locked" <<'EOF' &
import sqlite3, sys, time
db = sqlite3.connect(sys.argv[1], isolation_level=None, timeout=10)
db.execute("BEGIN IMMEDIATE")
open(sys.argv[2], "w").close()
time.sleep(3)
db.execute("ROLLBACK")
EOF
    holder=$!
    while [ ! -e "$TMP/locked" ] && kill -0 $holder 2>/dev/null; do sleep 0.05; done
    for r in health me session space; do
        once "$BASE/api/$r" > "$TMP/lock.$r" &
    done
    wait
    for r in health me session space; do
        read -r code t b < "$TMP/lock.$r"
        line "/api/$r under lock" "$code" "$(ms "$t")" "$b"
    done
fi

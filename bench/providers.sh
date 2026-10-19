#!/usr/bin/env bash
# Times a provider's routes and the file behind a track, one line per request,
# to tell the provider's latency from the server's.
#
#   PROVIDER_URL=http://127.0.0.1:7711 PROVIDER_TOKEN=<token> bench/providers.sh
#
#   -q PHRASE   what to search for (default "love")
#   --lanes     the bulk and interactive lanes against each other
#
# Every request here reaches a real account, and the providers share a budget
# of about 2 requests a second between everything on the machine. So each
# route is asked once (search and the file URL twice, to see a cache), nothing
# is repeated for a median, and --lanes stays opt-in.
set -u

: "${PROVIDER_URL:?set PROVIDER_URL, e.g. http://127.0.0.1:7711}"
BASE=${PROVIDER_URL%/}
Q=love
LANES=0
while [ $# -gt 0 ]; do
    case $1 in
        -q) Q=$2; shift ;;
        --lanes) LANES=1 ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
AUTH=()
[ -n "${PROVIDER_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer $PROVIDER_TOKEN")
FMT='%{http_code} %{time_starttransfer} %{time_total} %{size_download}'

# Seconds as curl gives them, in milliseconds.
ms() { awk -v t="$1" 'BEGIN { printf "%.1fms", t * 1000 }'; }

line() { printf '%-30s %3s %9s %9s %10s  %s\n' "$@"; }

# One request; the body kept in $TMP/body. Prints "code ttfb total bytes".
fetch() {
    curl -s -o "$TMP/body" -w "$FMT" ${AUTH[@]+"${AUTH[@]}"} "$@" || true
}

# The same with the body dropped, for requests run side by side.
quiet() {
    curl -s -o /dev/null -w "$FMT" ${AUTH[@]+"${AUTH[@]}"} "$@" || true
}

# One request, one line; "$1" names it.
measure() {
    local name=$1; shift
    local code ttfb t b
    read -r code ttfb t b < <(fetch "$@")
    line "$name" "$code" "$(ms "$ttfb")" "$(ms "$t")" "$b"
}

field() {
    python3 -c '
import json, sys
try:
    v = json.load(open(sys.argv[1]))
    for k in sys.argv[2:]:
        v = v[int(k)] if isinstance(v, list) else v[k]
    print("" if v is None else v)
except Exception:
    pass' "$TMP/body" "$@"
}

read -r code _ < <(fetch "$BASE/v1/health")
if [ "$code" = 000 ]; then
    echo "no provider answering at $BASE" >&2
    exit 1
fi

q=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$Q")
echo "# $BASE"
line name code ttfb total bytes
measure /v1/health "$BASE/v1/health"
measure /v1/capabilities "$BASE/v1/capabilities"
measure /v1/account "$BASE/v1/account"
measure "/v1/search cold" "$BASE/v1/search?q=$q&limit=50"
measure "/v1/search repeat" "$BASE/v1/search?q=$q&limit=50"

id=$(field tracks 0 id)
album=$(field tracks 0 album_id)
artist=$(field tracks 0 artist_id)
if [ -z "$id" ]; then
    echo "no track found for \"$Q\"; try -q" >&2
    exit 1
fi
echo "# track $id, album ${album:-none}, artist ${artist:-none}"

measure "/v1/tracks/{id}" "$BASE/v1/tracks/$id"
[ -n "$album" ] && measure "/v1/albums/{id}" "$BASE/v1/albums/$album"
if [ -n "$artist" ]; then
    measure "/v1/artists/{id}" "$BASE/v1/artists/$artist"
    measure "/v1/artists/{id}/releases" "$BASE/v1/artists/$artist/releases?own=false&cap=500"
    measure "/v1/artists/{id}/similar" "$BASE/v1/artists/$artist/similar?limit=20"
fi
measure "/v1/tracks/{id}/file" "$BASE/v1/tracks/$id/file?quality=lossy"
measure "/v1/tracks/{id}/file repeat" "$BASE/v1/tracks/$id/file?quality=lossy"

url=$(field url)
if [ -z "$url" ]; then
    echo "no file url for track $id" >&2
else
    # The file is fetched as it is, with no credentials, as a client would.
    # Bytes received against bytes asked for: a relay that answers 206 with
    # nothing in it looks fine on the status alone.
    echo "# the file: received / requested"
    # The size from a one-byte range rather than a HEAD, which a URL signed
    # for GET alone can refuse.
    size=$(curl -s -o /dev/null -D - -r 0-0 "$url" | tr -d '\r' \
        | awk 'tolower($1) == "content-range:" { sub(/.*\//, "", $NF); print $NF }')
    range() {
        local name=$1 from=$2 to=$3 code ttfb t b
        read -r code ttfb t b < <(curl -s -o /dev/null -w "$FMT" -r "$from-$to" "$url" || true)
        line "$name" "$code" "$(ms "$ttfb")" "$(ms "$t")" "$b" "/ $((to - from + 1))"
    }
    range "range 0-65535" 0 65535
    if [ -n "$size" ] && [ "$size" -gt 0 ]; then
        mid=$((size / 2))
        range "seek to the middle, 64 KB" $mid $((mid + 65535))
        range "range 1 MB" 0 $((1048576 - 1))
        range "range 2 MB" 0 $((2097152 - 1))
        range "whole file" 0 $((size - 1))
    else
        echo "# no size in a Content-Range, so no seek or whole-file ranges"
        range "range 1 MB" 0 $((1048576 - 1))
        range "range 2 MB" 0 $((2097152 - 1))
    fi
fi

if [ $LANES = 1 ]; then
    # Whether an interactive request overtakes queued bulk ones: the album
    # fired last should not wait behind the bulk tracks, and should behind
    # the interactive ones only as long as the budget says.
    lanes() {
        local lane=$1 n=$2 i
        for i in $(seq "$n"); do
            quiet -H "x-priority: $lane" "$BASE/v1/tracks/$id" > "$TMP/lane.$i" &
        done
        sleep 0.1
        quiet -H 'x-priority: interactive' "$BASE/v1/albums/$album" > "$TMP/lane.album" &
        wait
        for i in $(seq "$n"); do
            read -r code ttfb t b < "$TMP/lane.$i"
            line "$lane track #$i" "$code" "$(ms "$ttfb")" "$(ms "$t")" "$b"
        done
        read -r code ttfb t b < "$TMP/lane.album"
        line "interactive album (+0.1s)" "$code" "$(ms "$ttfb")" "$(ms "$t")" "$b"
    }
    if [ -z "$album" ]; then
        echo "no album for track $id, no --lanes" >&2
    else
        echo "# lanes: 8 bulk tracks, then an interactive album"
        lanes bulk 8
        echo "# lanes: 6 interactive tracks, then an interactive album"
        lanes interactive 6
    fi
fi

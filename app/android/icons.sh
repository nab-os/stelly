#!/usr/bin/env bash
# Puts Stelly's launcher icon in the APK, after `dx bundle --release
# --platform android`. dx 0.7 writes its own logo into the generated gradle
# project on every build and has no setting for it, so the icon files are
# swapped there and gradle runs again, which only repackages.
#
#   android/icons.sh OUT_DIR    # the --out-dir given to dx bundle
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
out=${1:?usage: android/icons.sh OUT_DIR}
project=${CARGO_TARGET_DIR:-$here/../../target}/dx/stelly/release/android/app
res=$project/app/src/main/res

[ -d "$res" ] || { echo "no gradle project at $project, run dx bundle first" >&2; exit 1; }

# dx's icon, every file of it: what is left over would clash with ours, the
# webps by name and the vector foreground by being picked over the PNGs.
rm -f "$res"/mipmap-*/ic_launcher.webp \
      "$res"/drawable/ic_launcher_background.xml \
      "$res"/drawable-v24/ic_launcher_foreground.xml \
      "$res"/mipmap-anydpi-v26/ic_launcher.xml
cp -r "$here"/res/. "$res"/

# dx assembles release only when Dioxus.toml has signing settings, and the
# APK is signed in CI instead; whichever it built is the one to build again.
if [ -f "$project/app/build/outputs/apk/release/app-release.apk" ]; then
    task=assembleRelease apk=$project/app/build/outputs/apk/release/app-release.apk
else
    task=assembleDebug apk=$project/app/build/outputs/apk/debug/app-debug.apk
fi

(cd "$project" && ./gradlew --quiet "$task")
cp "$apk" "$out"/
echo "icon in place: $out/$(basename "$apk")"

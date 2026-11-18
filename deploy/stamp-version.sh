#!/bin/sh
# Stamp web/version.json in the rendered tree with the deployed build info:
# the aria commit, the pinned MOSAIC_REV (the mosaic that rendered the app),
# and the UTC build time. The topbar badge shows it — so you can always see
# what IS deployed, and (the same value your browser fetched) what YOUR
# browser is running.
#
# The placeholder committed in web/version.json says UNSTAMPED; a deploy that
# skips this step is visible in the UI (and the li7 deploy script greps for
# the real sha before building).
#
# Run AFTER `mosaic build` + `stamp-sw.sh`, BEFORE taring the tree:
#   mosaic build . -o /tmp/aria-outNN
#   deploy/stamp-sw.sh /tmp/aria-outNN <BTAG>
#   deploy/stamp-version.sh /tmp/aria-outNN <BTAG> <MOSAIC_REV>
#   (cd /tmp/aria-outNN && cargo build -p aria)   # compile gate
#
# Usage: stamp-version.sh <rendered-out-dir> <aria-sha> <mosaic-rev>
set -eu
dir="${1:?usage: stamp-version.sh <rendered-out-dir> <aria-sha> <mosaic-rev>}"
aria="${2:?usage: stamp-version.sh <rendered-out-dir> <aria-sha> <mosaic-rev>}"
mosaic="${3:?usage: stamp-version.sh <rendered-out-dir> <aria-sha> <mosaic-rev>}"
f="$dir/web/version.json"
[ -f "$f" ] || { echo "stamp-version: no $f (is web/version.json committed?)" >&2; exit 1; }
built="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf '{"aria":"%s","mosaic":"%s","built":"%s"}\n' "$aria" "$mosaic" "$built" > "$f"
grep -q "\"aria\":\"$aria\"" "$f" || { echo "stamp-version: stamp not applied to $f" >&2; exit 1; }
echo "stamp-version: $f -> aria=$aria mosaic=$mosaic built=$built"

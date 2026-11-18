#!/bin/sh
# Stamp the rendered service worker's VERSION with a per-deploy tag so the
# worker's BYTES change on every deploy.
#
# Why this exists: the mosaic template hardcodes
#   const VERSION = 'mosaic-shell-v3';
# in web/sw.js. A browser only reinstalls a service worker when the sw.js
# bytes change. Because VERSION was static, every deploy served a byte-
# identical sw.js, so a client that had already precached the app shell
# (lane A, cache-first) NEVER picked up a new app.js — the "deployed but I
# still see the old UI" failure. Stamping VERSION with the deploy tag makes
# each deploy's sw.js unique, so the worker reinstalls, precaches the new
# shell, and activate() drops the old cache.
#
# Run AFTER `mosaic build` (render) and BEFORE taring the tree for li7, with
# the same tag that will be the brain image tag (BTAG):
#   mosaic build . -o /tmp/aria-outNN
#   deploy/stamp-sw.sh /tmp/aria-outNN <BTAG>
#   (cd /tmp/aria-outNN && cargo build -p aria)   # compile gate
#
# Usage: stamp-sw.sh <rendered-out-dir> <deploy-tag>
set -eu
dir="${1:?usage: stamp-sw.sh <rendered-out-dir> <deploy-tag>}"
tag="${2:?usage: stamp-sw.sh <rendered-out-dir> <deploy-tag>}"
sw="$dir/web/sw.js"
[ -f "$sw" ] || { echo "stamp-sw: no $sw" >&2; exit 1; }
sed -i "s/const VERSION = 'mosaic-shell-v3';/const VERSION = 'mosaic-shell-v3-${tag}';/" "$sw"
grep -q "mosaic-shell-v3-${tag}" "$sw" || { echo "stamp-sw: stamp not applied to $sw" >&2; exit 1; }
echo "stamp-sw: $sw -> mosaic-shell-v3-${tag}"

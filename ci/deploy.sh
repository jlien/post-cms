#!/usr/bin/env bash
# Build and rsync dist/ to the origin target directory.
#
# Env:
#   ORIGIN_TARGET  destination dir (default: ./deploy-target)
set -euo pipefail
cd "$(dirname "$0")/.."

ORIGIN_TARGET="${ORIGIN_TARGET:-./deploy-target}"

bash ci/build.sh

mkdir -p "$ORIGIN_TARGET"
rsync -a --delete dist/ "$ORIGIN_TARGET/"
echo "done: rsynced dist/ → $ORIGIN_TARGET"

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
rsync -a --delete --exclude=preview/ dist/ "$ORIGIN_TARGET/"
# Purge in the same breath: a deploy that leaves stale HTML behind (HTML is
# cached with s-maxage=86400) is a broken deploy. One call = built, shipped
# and visible.
if [[ -f ci/purge.sh ]]; then
  bash ci/purge.sh
fi
echo "done: rsynced dist/ → $ORIGIN_TARGET (cache purged)"

#!/usr/bin/env bash
# Build and rsync dist/ to the origin target directory.
#
# The target is a public origin, so this script never ships drafts or theme
# previews. It refuses (exit 1, nothing copied) when DRAFTS or
# SITE_PREVIEW_THEMES is set, and again when the built dist/ holds anything
# only a drafts or preview build writes: a top-level `preview` entry or the
# `.post-cms-drafts` marker, in any letter case. The origin server makes the
# same check, but only at startup; a running origin serves whatever lands in
# its directory, so the check has to happen before the copy too.
#
# Env:
#   ORIGIN_TARGET  destination dir (default: ./deploy-target)
set -euo pipefail
cd "$(dirname "$0")/.."

ORIGIN_TARGET="${ORIGIN_TARGET:-./deploy-target}"

refuse() {
  echo "deploy: refusing to deploy: $1. $ORIGIN_TARGET is a public origin, which must never serve drafts or previews; build those for a preview origin (IAP_AUDIENCE set) instead." >&2
  exit 1
}

for v in DRAFTS SITE_PREVIEW_THEMES; do
  if [[ -n "${!v:-}" ]]; then
    refuse "$v is set"
  fi
done

bash ci/build.sh

# Any letter case: a case-insensitive file system serves dist/Preview/ for
# /preview/.
found=()
shopt -s nullglob
for path in dist/* dist/.*; do
  name=$(printf '%s' "${path#dist/}" | tr '[:upper:]' '[:lower:]')
  case "$name" in
    preview | .post-cms-drafts) found+=("$path") ;;
  esac
done
shopt -u nullglob
if ((${#found[@]})); then
  refuse "dist/ holds a drafts or preview build (${found[*]})"
fi

mkdir -p "$ORIGIN_TARGET"
rsync -a --delete --exclude=preview/ dist/ "$ORIGIN_TARGET/"
# Purge in the same breath: a deploy that leaves stale HTML behind (HTML is
# cached at the edge for s-maxage, 300 s by default; see site.yaml `cache:`)
# is a broken deploy. One call = built, shipped
# and visible.
if [[ -f ci/purge.sh ]]; then
  bash ci/purge.sh
fi
echo "done: rsynced dist/ → $ORIGIN_TARGET (cache purged)"

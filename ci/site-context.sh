#!/usr/bin/env bash
# Stage a site for `docker build --build-context site=<out>`: the files git
# tracks at HEAD in <site-dir>, and nothing else.
#
#   ci/site-context.sh <site-dir> <out-dir>
#
# A named build context is sent to BuildKit whole (its cache, or a remote
# builder) and the engine's .dockerignore does not filter it, so the site
# directory itself must hold no secrets. `git archive HEAD` leaves out .git,
# untracked and ignored files (a local config/master.key, .env, node_modules,
# dist/) and uncommitted edits. Tracked files named like secrets (.env*,
# *.key, *.enc, *.pem; for example an encrypted credentials file) are removed
# too: build-site does not read them, and the image build refuses a site
# that has them. <site-dir> may be a subdirectory of a repository. <out-dir>
# must not exist or be empty.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <site-dir> <out-dir>" >&2
  exit 2
fi
site=$1
out=$2

git -C "$site" rev-parse --verify -q HEAD >/dev/null \
  || { echo "$site: not in a git repository with a commit" >&2; exit 1; }
[ -f "$site/site.yaml" ] || { echo "$site: no site.yaml" >&2; exit 1; }
if [ -e "$out" ] && [ -n "$(ls -A "$out")" ]; then
  echo "$out: exists and is not empty" >&2
  exit 1
fi
mkdir -p "$out"

# Archive from the top level: run inside a subdirectory, git archive also
# filters by that directory's path, which the subtree has already stripped.
prefix=$(git -C "$site" rev-parse --show-prefix)
top=$(git -C "$site" rev-parse --show-toplevel)
git -C "$top" archive --format=tar "HEAD:$prefix" | tar -x -C "$out"

removed=$(find "$out" \( -name .env -o -name '.env.*' -o -name '*.key' -o -name '*.enc' -o -name '*.pem' \) -print)
if [ -n "$removed" ]; then
  printf '%s\n' "$removed" | while read -r f; do rm -rf "$f"; echo "left out ${f#"$out"/}"; done
fi
[ -f "$out/site.yaml" ] || { echo "$site: site.yaml is not committed" >&2; exit 1; }
echo "staged $(find "$out" -type f | wc -l | tr -d ' ') tracked files of $site at $(git -C "$site" rev-parse --short HEAD) in $out"

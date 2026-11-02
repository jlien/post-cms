#!/usr/bin/env bash
# Build the origin image and check it: no secrets in any layer or in the
# history, no shell, runs as non-root, honors PORT, serves /health with a
# read-only root file system, and exits 0 within 10 s of SIGTERM.
#
#   ci/container-check.sh            # builds `git archive HEAD` of this repo
#
# Needs docker with BuildKit (buildx), curl, git, tar, gzip. Builds from a
# copy of the committed tree with canary secrets planted in it (and in a
# second, named site context), so a .dockerignore or Dockerfile change that
# lets a secret into the image fails here. Removes every container, image and
# temp directory it creates. Env: KEEP_IMAGE=1 keeps the image for inspection;
# DOCKER (default docker) picks the CLI.
set -euo pipefail

DOCKER=${DOCKER:-docker}
REPO=$(cd "$(dirname "$0")/.." && pwd)
TAG="post-cms-check-$$-$(date +%s)"
IMG="$TAG:demo"
IMG_SITE="$TAG:site"
CANARY="postcms-canary-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/post-cms-container-check.XXXXXX")
CONTAINERS=()
FAILS=0

cleanup() {
  for c in "${CONTAINERS[@]+"${CONTAINERS[@]}"}"; do "$DOCKER" rm -f "$c" >/dev/null 2>&1 || true; done
  if [ "${KEEP_IMAGE:-}" != 1 ]; then
    "$DOCKER" rmi -f "$IMG" "$IMG_SITE" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

ok()   { echo "ok   $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }

# ---- contexts with planted secrets ------------------------------------------
CTX="$WORK/context"
mkdir -p "$CTX"
git -C "$REPO" archive HEAD | tar -x -C "$CTX"
plant() { mkdir -p "$(dirname "$1")"; printf '%s\n' "$CANARY" > "$1"; }
plant "$CTX/config/master.key"
plant "$CTX/.env"
plant "$CTX/config/credentials.json.enc"
plant "$CTX/content/.env"
plant "$CTX/content/posts/.env.local"
plant "$CTX/themes/server.pem"
plant "$CTX/templates/master.key"

SITE="$WORK/site"
mkdir -p "$SITE"
for p in site.yaml redirects.yaml content templates themes plugins; do cp -R "$CTX/$p" "$SITE/"; done
# The site context is bind-mounted, not filtered by this repo's .dockerignore:
# anything here that build-site does not publish must still stay out.
plant "$SITE/config/master.key"
plant "$SITE/.env"
plant "$SITE/config/credentials.json.enc"
rm -f "$SITE/content/.env" "$SITE/content/posts/.env.local" "$SITE/themes/server.pem" "$SITE/templates/master.key"
printf '%s' "$CANARY" > "$WORK/sanity-token"

# ---- build ------------------------------------------------------------------
echo "building $IMG (engine context, demo site)"
build() { # log-name args...
  local log="$WORK/$1.log"; shift
  if ! "$DOCKER" build --progress=plain "$@" >"$log" 2>&1; then
    tail -n 60 "$log"
    echo "FAIL docker build ($log)"
    exit 1
  fi
}
build build-demo -t "$IMG" --secret id=SANITY_READ_TOKEN,src="$WORK/sanity-token" "$CTX"
ok "image builds from the committed tree"
echo "building $IMG_SITE (site from a named build context)"
build build-site -t "$IMG_SITE" --build-context site="$SITE" \
  --secret id=SANITY_READ_TOKEN,src="$WORK/sanity-token" "$CTX"
ok "image builds with --build-context site=<dir>"

size=$("$DOCKER" image inspect -f '{{.Size}}' "$IMG")
echo "info image size: $size bytes ($((size / 1024 / 1024)) MiB)"

# ---- static checks on each image -------------------------------------------
# Every layer as a tar stream, whatever the store compressed it with.
layer_streams() { # image outdir
  local tarball="$2/image.tar" n=0
  mkdir -p "$2/blobs"
  "$DOCKER" save -o "$tarball" "$1"
  tar -x -f "$tarball" -C "$2/blobs"
  find "$2/blobs" -type f ! -name '*.json' ! -name 'repositories' | while read -r f; do
    n=$((n + 1))
    case "$(od -An -N4 -tx1 "$f" | tr -d ' \n')" in
      1f8b*)    gzip -dc "$f" > "$2/layer.$n.tar" 2>/dev/null || true ;;
      28b52ffd) if command -v zstd >/dev/null; then zstd -dcq "$f" > "$2/layer.$n.tar" || true; else echo "zstd layer and no zstd: $f" >&2; exit 1; fi ;;
      *)        cp "$f" "$2/layer.$n.tar" ;;
    esac
    tar -t -f "$2/layer.$n.tar" >/dev/null 2>&1 || rm -f "$2/layer.$n.tar"
  done
}

check_image() { # image label
  local img="$1" label="$2" dir="$WORK/scan-$2"
  layer_streams "$img" "$dir"
  local layers
  layers=$(find "$dir" -maxdepth 1 -name 'layer.*.tar' | wc -l | tr -d ' ')
  [ "$layers" -gt 0 ] || { fail "$label: found no layers to scan"; return; }

  if grep -a -l -F "$CANARY" "$dir"/layer.*.tar >/dev/null 2>&1; then
    fail "$label: a planted secret is in an image layer"
  else
    ok "$label: no planted secret in $layers layers"
  fi
  if "$DOCKER" history --no-trunc --format '{{.CreatedBy}}' "$img" | grep -q -F "$CANARY"; then
    fail "$label: a planted secret is in docker history"
  else
    ok "$label: no planted secret in docker history"
  fi
  if "$DOCKER" image inspect "$img" | grep -q -F "$CANARY"; then
    fail "$label: a planted secret is in the image config"
  else
    ok "$label: no planted secret in the image config"
  fi

  local listing="$dir/files.txt"
  for t in "$dir"/layer.*.tar; do tar -t -f "$t"; done | sed 's|^\./||' | sort -u > "$listing"
  if grep -E '(^|/)(\.env(\..*)?|master\.key|[^/]*\.enc|[^/]*\.pem|id_rsa|id_ed25519)$' "$listing" >/dev/null; then
    fail "$label: secret-like files in the image: $(grep -E '(^|/)(\.env(\..*)?|master\.key|[^/]*\.enc|[^/]*\.pem|id_rsa|id_ed25519)$' "$listing" | tr '\n' ' ')"
  else
    ok "$label: no secret-like file names in the image"
  fi
  if grep -E '^(usr/)?(s?bin)/(sh|bash|dash|ash|busybox|zsh)$' "$listing" >/dev/null; then
    fail "$label: a shell is in the image: $(grep -E '^(usr/)?(s?bin)/(sh|bash|dash|ash|busybox|zsh)$' "$listing" | tr '\n' ' ')"
  else
    ok "$label: no shell in the image"
  fi
  grep -q -E '^site/dist/index\.html$' "$listing" && ok "$label: ships site/dist/index.html" \
    || fail "$label: site/dist/index.html missing"
  grep -q -E '^usr/local/bin/post-cms-server$' "$listing" && ok "$label: ships the server binary" \
    || fail "$label: server binary missing"
  if grep -q -E '^(site/(content|templates|themes|plugins)/|usr/local/bin/build-site$)' "$listing"; then
    fail "$label: site sources or build-site in the runtime image"
  else
    ok "$label: only dist/, site.yaml and redirects.yaml from the site"
  fi
  local user
  user=$("$DOCKER" image inspect -f '{{.Config.User}}' "$img")
  case "$user" in
    ""|0|0:*|root|root:*) fail "$label: image user is '${user:-root}'" ;;
    *) ok "$label: image user is $user" ;;
  esac
}
check_image "$IMG" demo
check_image "$IMG_SITE" site

# ---- run: read-only root, no capabilities, non-default PORT ---------------
PORT_IN=9090
cid=$("$DOCKER" run -d --read-only --tmpfs /tmp:rw,mode=1777,size=256m \
  --cap-drop ALL --security-opt no-new-privileges \
  -e PORT=$PORT_IN -p "127.0.0.1::$PORT_IN" "$IMG")
CONTAINERS+=("$cid")
hostport=$("$DOCKER" port "$cid" "$PORT_IN/tcp" | head -n1)
base="http://${hostport/0.0.0.0/127.0.0.1}"
healthy=""
for _ in $(seq 1 60); do
  if body=$(curl -fsS --max-time 2 "$base/health" 2>/dev/null); then healthy=1; break; fi
  sleep 0.5
done
if [ -n "$healthy" ] && printf '%s' "$body" | grep -q '"status":"ok"'; then
  ok "GET /health on PORT=$PORT_IN: $body"
else
  fail "GET /health did not answer; logs: $("$DOCKER" logs "$cid" 2>&1 | tail -n 5)"
fi
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$base/" || true)
[ "$code" = 200 ] && ok "GET / is 200" || fail "GET / is $code"

uids=$("$DOCKER" top "$cid" -eo uid 2>/dev/null | tail -n +2 | tr -d ' ' | sort -u | tr '\n' ' ')
if [ -z "$uids" ]; then
  fail "could not read the process uid (docker top)"
elif printf ' %s' "$uids" | grep -q -E '(^| )0( |$)'; then
  fail "a process runs as uid 0 (uids: $uids)"
else
  ok "runs as non-root (uid $uids)"
fi

# ---- SIGTERM ----------------------------------------------------------------
start=$(date +%s)
"$DOCKER" kill --signal TERM "$cid" >/dev/null
stopped=""
for _ in $(seq 1 100); do
  if [ "$("$DOCKER" inspect -f '{{.State.Running}}' "$cid")" = false ]; then stopped=1; break; fi
  sleep 0.1
done
elapsed=$(( $(date +%s) - start ))
exit_code=$("$DOCKER" inspect -f '{{.State.ExitCode}}' "$cid")
if [ -n "$stopped" ] && [ "$exit_code" = 0 ]; then
  ok "SIGTERM: exited 0 after ${elapsed}s"
else
  fail "SIGTERM: running=$([ -n "$stopped" ] && echo no || echo yes) exit=$exit_code after ${elapsed}s"
fi
"$DOCKER" logs "$cid" 2>&1 | grep -q 'origin: SIGTERM, shutting down' \
  && ok "logged the shutdown" || fail "no shutdown line in the log"

echo
if [ "$FAILS" -gt 0 ]; then
  echo "container check: $FAILS failed"
  exit 1
fi
echo "container check: all passed"

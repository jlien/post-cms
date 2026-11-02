# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
#
# Origin image: the post-cms server binary plus one site's built dist/.
# Build inputs, runtime settings and checks: README "Container image".
#
#   docker build -t my-site .                                  # the demo site in this repo
#   docker build -t my-site --build-context site=../my-site .  # any other site
#
# Base images are pinned by digest. To update one, resolve the new digest of
# the same tag (`docker buildx imagetools inspect <image>:<tag>`) and replace
# both the tag and the digest in the ARG.

# Builder: Debian 12 (bookworm), so the binary links against the same glibc
# as the runtime. The full image already has gcc, g++, pkg-config and the
# OpenSSL headers the native dependencies need, so the build runs no apt.
ARG RUST_IMAGE=rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1
# Runtime: distroless cc on Debian 12, nonroot tag (uid 65532). glibc,
# libstdc++ (the ONNX runtime behind /api/ask is C++), OpenSSL and CA
# certificates, and no shell or package manager.
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

# ---- site: the default site is the demo in this repo ------------------------
# Replace it with `--build-context site=<dir>`. The site is bind-mounted into
# the build step below, never copied into a layer, so files in it that the
# build does not write to dist/ never reach any image layer.
FROM scratch AS site
COPY site.yaml redirects.yaml /
COPY content /content
COPY templates /templates
COPY themes /themes
COPY plugins /plugins

# ---- engine: dependency-cached Rust build ----------------------------------
FROM ${RUST_IMAGE} AS engine
WORKDIR /src
ENV CARGO_TERM_COLOR=never
# 1. Manifests plus stub sources: this layer compiles every dependency and is
#    reused until Cargo.toml or Cargo.lock changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/build/Cargo.toml crates/build/Cargo.toml
COPY crates/server/Cargo.toml crates/server/Cargo.toml
RUN mkdir -p crates/build/src/bin crates/server/src \
 && echo 'fn main() {}' > crates/build/src/main.rs \
 && echo 'fn main() {}' > crates/build/src/bin/pt_render.rs \
 && : > crates/build/src/lib.rs \
 && echo 'fn main() {}' > crates/server/src/main.rs \
 && cargo build --release --locked -p server -p build --bins \
 && rm -rf crates/build/src crates/server/src
# 2. The real sources. Touch them so cargo rebuilds the workspace crates even
#    when the copied files are older than the stub build.
COPY crates crates
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --locked -p server -p build --bins \
 && mkdir -p /out \
 && cp target/release/server target/release/build-site /out/

# ---- site-build: run build-site over the site ------------------------------
FROM ${RUST_IMAGE} AS site-build
# Optional Tailwind CLI (standalone binary) for themes that compile CSS.
# Both or neither: the download is checked against the SHA-256.
ARG TAILWINDCSS_URL=""
ARG TAILWINDCSS_SHA256=""
RUN if [ -n "$TAILWINDCSS_URL" ]; then \
      [ -n "$TAILWINDCSS_SHA256" ] || { echo "TAILWINDCSS_SHA256 is required with TAILWINDCSS_URL" >&2; exit 1; }; \
      curl -fsSL --proto '=https' -o /usr/local/bin/tailwindcss "$TAILWINDCSS_URL" \
      && echo "$TAILWINDCSS_SHA256  /usr/local/bin/tailwindcss" | sha256sum -c - \
      && chmod 0755 /usr/local/bin/tailwindcss; \
    elif [ -n "$TAILWINDCSS_SHA256" ]; then \
      echo "TAILWINDCSS_URL is required with TAILWINDCSS_SHA256" >&2; exit 1; \
    fi
COPY --from=engine /out/build-site /usr/local/bin/build-site
# Build settings (never secrets: build args are visible in `docker history`).
ARG SITE_BASE_URL=""
ARG SITE_THEME=""
ARG DRAFTS=""
ARG POSTCMS_EMBED_BATCH=""
ARG IMAGES_OFFLINE=""
ARG SANITY_OFFLINE=""
# The site is mounted read-write so build-site can write dist/ and .cache/;
# writes to the mount are discarded after this step. Only dist/, site.yaml and
# redirects.yaml are copied out. The Sanity read token, when the site uses
# Sanity, comes from a BuildKit secret: mounted for this step only, never in
# a layer, an ARG or an ENV. The model and image caches live in cache mounts.
RUN --mount=type=bind,from=site,target=/site,rw \
    --mount=type=secret,id=SANITY_READ_TOKEN,required=false \
    --mount=type=cache,id=post-cms-fastembed,target=/cache/home \
    --mount=type=cache,id=post-cms-images,target=/cache/images \
    set -eu; \
    for v in SITE_BASE_URL SITE_THEME DRAFTS POSTCMS_EMBED_BATCH IMAGES_OFFLINE SANITY_OFFLINE; do \
      [ -n "$(printenv "$v")" ] || unset "$v"; \
    done; \
    if [ -s /run/secrets/SANITY_READ_TOKEN ]; then \
      SANITY_READ_TOKEN="$(cat /run/secrets/SANITY_READ_TOKEN)"; export SANITY_READ_TOKEN; \
    fi; \
    rm -rf /site/dist; \
    SITE_ROOT=/site HOME=/cache/home IMAGES_CACHE_DIR=/cache/images build-site; \
    mkdir -p /out/site; \
    cp -R /site/dist /out/site/dist; \
    cp /site/site.yaml /out/site/site.yaml; \
    if [ -f /site/redirects.yaml ]; then cp /site/redirects.yaml /out/site/redirects.yaml; fi; \
    chmod -R a+rX,go-w /out/site

# ---- runtime ---------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=engine /out/server /usr/local/bin/post-cms-server
COPY --from=site-build /out/site /site
# PORT is the listen port (platforms such as Cloud Run set it). The server
# listens on 127.0.0.1 unless told otherwise; in a container that is
# unreachable, so the image listens on every interface.
# HOME only locates the /api/ask model cache ($HOME/.cache/post-cms-fastembed),
# the one path the server writes. It is under /tmp so a read-only root file
# system works with a tmpfs on /tmp.
ENV SITE_ROOT=/site \
    PORT=8080 \
    POSTCMS_HOST=0.0.0.0 \
    HOME=/tmp/post-cms
USER 65532:65532
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/post-cms-server"]

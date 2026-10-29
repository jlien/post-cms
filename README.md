# Post-CMS

A CMS-free publishing stack: content lives as files in git, a Rust build step
renders static HTML, a tiny axum origin serves it, and an MCP tool server lets
an AI agent do all content operations.

## Architecture

```text
                    ┌──────────────────────────────────────────────┐
                    │                    git                       │
                    │  content/posts/<slug>/{meta.yaml,index.mdx}  │
                    └──────────────┬───────────────────────────────┘
                                   │ edits (agent via MCP, or human)
                                   ▼
┌──────────────┐        ┌────────────────────┐        ┌───────────────┐
│ MCP server   │───────▶│ build (Rust)       │──────▶│ dist/          │
│ (mcp/        │  cargo │ pulldown-cmark +   │ write  │ index.html     │
│  server.py)  │  run   │ tera templates     │        │ posts/<slug>/  │
└──────────────┘        └────────────────────┘        │ rss.xml        │
                                                      │ sitemap.xml    │
                                                      │ robots.txt     │
                                                      │ 404.html       │
                                                      └───────┬────────┘
                                                              ▼
                                        ┌──────────────────────────────┐
                                        │ origin (crates/server, axum) │
                                        │ :8787 · Cache-Control tuned  │
                                        └───────────────┬──────────────┘
                                                        ▼
                                        ┌──────────────────────────────┐
                                        │ CDN (simulated locally)      │
                                        │ ci/purge.sh on deploy        │
                                        └──────────────────────────────┘
```

## Layout

| Path | Purpose |
|---|---|
| `content/posts/<slug>/` | `meta.yaml` + `index.mdx` per post |
| `crates/build` | static site generator → `dist/` |
| `crates/server` | axum static origin, 127.0.0.1:8787 (`PORT`, `--host` / `POSTCMS_HOST` to override) |
| `mcp/server.py` | MCP stdio server (stdlib only) |
| `templates/` | tera templates (base, post, index, 404) |
| `ci/` | build, deploy, purge scripts |

## Build

```bash
cargo run -p build      # renders dist/
cargo run -p build -- --version   # prints the engine and linked libwebp versions
SITE_ROOT=/path/to/site build-site
```

The site root is `SITE_ROOT`, else the current directory. Guards against building the wrong directory:

- The site root must contain `site.yaml`. Anything else (an unrelated repo, a home directory) exits non-zero before `dist/` is touched.
- Every build writes `dist/.post-cms-build`. An existing `dist/` is deleted and recreated only when it has that marker. A `dist/` without it is refused with an error naming the directory, and a `dist` that is a symlink or a file is always refused. A missing or empty `dist/` is used as is.
- A `dist/` from a build-site older than the marker is refused once: check it is build output, then run `build-site --force-clean` (or `cargo run -p build -- --force-clean`). Later builds find the marker. The engine does not guess from contents such as `sitemap.xml`, since other generators write those too.
- One build at a time per site: build-site takes an advisory lock on the site root directory (released when it exits, even on a crash). A second build-site on the same site exits non-zero with `another build-site is already building <root>` before `dist/` is touched. Build a separate copy of the site to run builds in parallel; separate copies may share `IMAGES_CACHE_DIR`.
- Unknown flags and arguments (`-V`, a path) are a usage error (exit 2) with nothing built or deleted. `--help` prints the usage and `--version` the versions, both without building. The server takes only `--host` and `--help` (`server --help` prints its usage and exits without listening), and the AVIF decode-only mode takes exactly its three numbers.

Temporary files: the Tailwind CLI writes the compiled theme CSS into a private directory created for that build (random name under the system temp directory, `TMPDIR` on Unix, mode 0700, never an existing path or symlink) and removed when the build finishes or fails, so builds of separate site copies on one machine never read each other's CSS. An interrupted build (SIGTERM, SIGINT, SIGHUP, or SIGKILL) can leave an empty or partly written `postcms-theme-*` directory there; it is private to your user and safe to delete. The embedding model is cached in `~/.cache/post-cms-fastembed`; when `HOME` is unset or empty it goes to a private per-run directory instead, downloaded again on each build.

The origin server never serves the marker file.

### Embeddings

`dist/embeddings.json` holds one vector per text chunk (about 700 characters) of every published post and indexable collection item. The build sends the chunks to the model `POSTCMS_EMBED_BATCH` at a time (default 32, a whole number from 1 to 1024), so peak memory follows the batch size, not the number of chunks. Larger batches can be a little faster and use more memory; the vectors are the same within float rounding. An invalid `POSTCMS_EMBED_BATCH` fails the build before `dist/` is touched.

### Tailwind CLI

The theme CSS is compiled by `tailwindcss` from `PATH` (the npm `@tailwindcss/cli` or the standalone binary). Only absolute `PATH` entries are searched: an empty or relative entry (`::`, `.`, `node_modules/.bin`) is skipped, since it would resolve against the site root and run a `tailwindcss` placed in the site. The CLI's own `PATH` is filtered the same way, so a `node` placed in the site never runs either. Put `$PWD/node_modules/.bin` on `PATH`, not the relative form. When it is not on `PATH` or exits non-zero, the build inlines the raw `theme.css` and prints a warning.

The CLI is third-party code, so it does not inherit the build's environment (which can hold secrets such as `SANITY_READ_TOKEN`). It gets only these variables, when set:

| Variable | Why |
|---|---|
| `PATH` | finds `node` for the npm CLI's `#!/usr/bin/env node` line; only its absolute entries, in order |
| `HOME` | Node and the standalone binary expect a home directory |
| `TMPDIR` | Node's temp directory (the CLI's config loader caches there) |
| `NODE_PATH` | extra directories the CLI searches for `@import` and `@plugin` packages |
| `LANG`, `LC_ALL`, `LC_CTYPE` | locale, so non-ASCII file names and messages decode as in the build |

On Windows `SYSTEMROOT`, `TEMP`, `TMP` and `USERPROFILE` are passed too. Everything else (`NODE_OPTIONS`, tokens, CI variables) is dropped.

The CLI gets no stdin, and its stdout and stderr are discarded. It runs in its own process group. When it exits, anything it left running in that group is killed. It is killed, with everything it started, after `POSTCMS_TAILWIND_TIMEOUT_SECS` (default 120, whole seconds from 1 to 3600). The build then fails with `tailwindcss did not finish within <n>s and was killed`; a timeout never falls back to raw CSS. An invalid `POSTCMS_TAILWIND_TIMEOUT_SECS` fails the build before `dist/` is touched. SIGINT, SIGTERM or SIGHUP to `build-site` while the CLI runs kills the CLI's group too, then ends the build with that signal; after a SIGKILL to `build-site` the CLI runs until it finishes on its own.

The only other child process, the AVIF decoder (a copy of `build-site`, see Images), also starts with an empty environment, in its own process group, and is killed after `decode_timeout_secs`.

## Template data

Every template (pages, collection items and indexes, home, blog, contact, 404) gets `base_url`: the site origin with no trailing slash, from the `SITE_BASE_URL` env var, then site.yaml `base_url`, then `http://localhost:8787`. It must be `http(s)://host[:port]` with no path, quotes, spaces, or control characters; anything else fails the build. For example, a canonical tag: `<link rel="canonical" href="{{ base_url }}{{ url }}">`.

Pages (`content/pages/<slug>/meta.yaml`) accept any extra keys beyond `title`, `slug`, `description`, `path`, `template`, `lang`, and `published`. Templates see every key, known and extra, as `page.fields`, the same way collection items see `fields`:

```yaml
title: About
slug: about
seo_title: About Example
og_image: https://example.com/og.png
no_index: true
```

```html
<title>{{ page.fields.seo_title | default(value=title) }}</title>
```

Collection item and index templates get `collection`: `{name, title, url}` (`url` is the index path). Index templates also keep `coll_name` and `coll_title`.

A schema can declare reference fields whose values are item slugs in another collection. The build fails, naming the file, when a value is not an item of the target collection:

```yaml
# content/articles/schema.yaml
references:
  author: authors      # author: ada
  reviewers: authors   # reviewers: [ada, lin]
```

Item templates of that collection get `refs.<field>`: `{slug, title, description, url, published, fields}` for a single slug, or a list of those for a list of slugs. Rows in `items` on the collection index carry the same `refs`, unless an item has its own `refs` field. A published item may only reference published items, or items of an `expose_to_pages` collection (see below): a reference to any other unpublished item fails the build, naming the item, field, and slug. Drafts (`published: false`) may reference drafts, for previews. Resolution is one level deep: a referenced item's own references stay slugs in its `fields`. Collections without `references` get no `refs`.

```html
{% if refs.author %}<a href="{{ refs.author.url }}">{{ refs.author.title }}</a>{% endif %}
```

Every template (pages, built-in pages, collection items and indexes, 404) also gets `collections`, keyed by collection name: `collections.<name>` is `{name, title, url, items}`. `url` is the index path, or null when the schema sets `index: false`. `items` lists the published items, newest first by `date`, then undated items in slug order. Each row is `{slug, title, description, url, date, published, fields, refs}`: `url` is null for an item with no page, `date` is null when unset, and `refs` appears only when the schema declares references. The map is built once per build, and templates that never read it render exactly as before.

```html
{% for post in collections.articles.items | slice(end=3) %}
  <a href="{{ post.url }}">{{ post.title }}</a>
{% endfor %}
```

Drafts stay hidden: an item with `published: false` is not in `collections`, so a page cannot leak an unpublished title, date, or field. Data collections (people, logos, quotes) whose items never get pages of their own opt in with `expose_to_pages: true`, which puts every item in `collections.<name>`, drafts included:

```yaml
# content/people/schema.yaml
title: People
index: false
expose_to_pages: true
```

Use it only for collections whose every item is safe to show on a live page, including any `agent_notes`. An exposed draft has `url: null`, since it has no page. Published items may reference items of an `expose_to_pages` collection (the reference's `url` is null for a draft target). Any item pages can see, published or exposed, may only reference published or exposed items, so no page reaches a hidden draft through `refs`; the build fails naming the item, field, and slug otherwise.

`sitemap.xml` and `llms.txt` leave out any page or collection item whose meta sets `no_index: true` (a YAML boolean), and `search-index.json` and `embeddings.json` leave out such collection items; one predicate decides all four. `sitemap.xml` also leaves out any path that is a source in `redirects.yaml`. Every `<loc>` is XML-escaped. The page itself still builds; emitting a `noindex` robots tag is up to the template (`page.fields.no_index`, `fields.no_index`).

## Portable Text templates

Collection items stored as `index.pt.json` render through `crates/build/src/pt.rs`. Custom block types need `templates/pt/<_type>.html` (the block is `block`; nested Portable Text fields arrive as `<field>_html`). Built-ins render with engine defaults unless the site adds an override:

| Template | Context |
|---|---|
| `pt/image.html` | `src`, `alt`, `caption`, `width`, `height` (from the asset reference, else null), `block` |
| `pt/table.html` | `rows` (arrays of cell strings), `has_header`, `block` |
| `pt/divider.html` | `block` |
| `pt/blockquote.html` | `text_html`, `block` |
| `pt/list.html` | `kind` (`bullet`, `number`, ...), `tag` (`ul`/`ol`), `level` (1 = outermost), `items` (item HTML, nested lists included) |
| `pt/marks/link.html` | `href`, `blank`, `text`, `text_html`, `mark` |

Templates autoescape. Use `| safe` only for `*_html` values, which the engine rendered itself; never on `href`, `src`, `alt`, `caption`, or `block` fields. Link `href` values must be http(s), `mailto:`, `tel:`, a site path (`/x`), `#fragment`, or `?query`, and image `src` values must be http(s) or a site path; anything else is dropped (link text kept, image omitted) and printed as a warning naming the item; the build still succeeds. Unknown custom block types still fail the build. Templates can apply the same checks to custom block fields with the `safe_href` and `safe_src` filters, for example `{{ block.url | safe_href }}`, which print the URL or an empty string. Custom blocks nest at most 32 levels. `pt-render --templates templates` uses the same overrides.

## Images

`site.yaml` `images:` turns on a build-time image pipeline (`crates/build/src/images.rs`). Without the block nothing changes: the helpers below print a plain `<img>` with the original URL, and the build output is byte-identical.

```yaml
images:
  allowed_hosts: [cdn.example.com, "*.media.example.org"]   # remote hosts images may come from
  # every key below is optional; these are the defaults
  widths: [320, 480, 640, 768, 1024, 1280, 1600, 1920]
  formats: [webp]          # add avif for AVIF too (slower: rav1e, pure Rust)
  webp_quality: 75
  avif_quality: 55
  avif_speed: 8            # 1 slow/small to 10 fast
  jpeg_quality: 82        # fallback JPEGs are progressive
  fallback_width: 1280     # the single <img src> fallback when a modern format is on
  max_bytes: 26214400      # per response
  max_pixels: 50000000     # width x height, checked from the header before decoding
  timeout_secs: 30         # per attempt (1 to 600)
  retries: 3               # extra attempts after a timeout, dropped connection, 429 or 5xx (0 to 10)
  retry_total_secs: 120    # most time one source may take across attempts (1 to 600)
  decode_timeout_secs: 60  # most time one AVIF decode may take before its process is killed (1 to 600)
  animated_gifs: webp      # or copy: keep animated GIFs as they are
  animation_max_width: 640 # widest animated variant (1 to 16383); no srcset entry is larger
  max_frames: 1000         # per animated GIF, counted before decoding
  max_animation_pixels: 1000000000  # width x height x frames, checked before decoding
  cache_dir: .cache/images # relative to the site root; IMAGES_CACHE_DIR overrides
  output_dir: assets/img   # under dist/
```

Templates:

```jinja
{{ image(src=item.cover, alt=item.title, sizes="(min-width: 60em) 33vw, 100vw", class="card__img") }}
{{ image(src=hero, alt="", loading="eager", fetchpriority="high") }}
<div style="background-image: url({{ bg | image_url }})"></div>
<meta property="og:image" content="{{ cover | image_url(width=1200, absolute=true) }}">
```

`image()` takes `src`, `alt`, `sizes` (default `auto, 100vw` when lazy, else `100vw`), `class`, `loading` (`lazy` default, or `eager`), `fetchpriority`, and optional `width`/`height` to override the attributes. It prints a `<picture>` with one `<source>` per modern format and an `<img>` with `width`, `height` (the largest variant's size, for layout stability), `loading`, and `decoding="async"`. When the `<picture>` wrapper must not affect layout, add `picture { display: contents; }`. If CSS sets the image's `width: auto`, the browser sizes it from `sizes`, so keep `sizes` at least as wide as the slot. `image_url` returns the local URL of the largest fallback variant, or of `width` (clamped to the source); `absolute=true` prefixes `base_url`. Portable Text image blocks without a `pt/image.html` override render through `image()`; an override can call `image(src=src, alt=alt, width=width, height=height)` itself.

How it works: rendering records each call and prints a placeholder. After every page is rendered, the build fetches each distinct source once (in parallel), reads its size from the header, writes `/<output_dir>/<hash>-<width>.<ext>` variants (in parallel), and replaces the placeholders, before precompression. Only images a template uses are fetched and written.

- Placeholders never ship: replacing them fails the build when a page or directory cannot be read, or a placeholder has no markup. After precompression, every text file in `dist/` and every `.gz` copy is checked for `__postcms_img_`. Any hit fails the build (non-zero exit) with the page and the image source named, or says the placeholder came from another build writing the same `dist/`. A build that fails here removes the `dist/` it wrote, so no partial site is served.
- Sources: `https://` URLs on `allowed_hosts`, and site paths (`/assets/...`) resolved inside `dist/`. Other hosts and plain http keep their URL silently.
- AVIF sources are decoded and re-encoded like JPEG, PNG and WebP. Decoding is pure Rust (`avif-parse` for the container, `rav1d` without assembly for AV1), so it needs no system library. The size is read from the AV1 sequence header and checked against `max_pixels` before any pixel is decoded (the decoder enforces the same limit). 8 to 12 bit, 4:0:0 to 4:4:4, and alpha items are supported; YUV is converted with the matrix and range the stream declares (BT.601 when unspecified). Container transforms (`irot`, `imir`, `clap`), grid images and image sequences are not: those keep their original URL or their stored orientation. The AV1 data is decoded in a separate process: `build-site` starts a copy of itself in a decode-only mode for each AVIF, sends the file on stdin and reads the pixels back. rav1d can abort its process on malformed input (a panic inside its C API cannot unwind), so a crash, a hang past `decode_timeout_secs`, or malformed output fails that one image, which keeps its original URL like any failed image, and the build carries on. The decoder process gets no core dumps, a CPU-time limit, and on Linux a data-size limit of its estimated memory plus 256 MB. Programs that embed the pipeline call `images::decoder_child()` first in `main` (or set `ImagesCfg::decoder_exe` to a binary that does). AVIF decodes count against the per-image memory budget by format: rav1d's working set (about twice the decoded picture: 1 or 2 bytes per sample, plus the chroma planes the layout keeps) plus the 8-bit output held by both processes while it is sent. That is 6 bytes per pixel for 8-bit 4:2:0 and 17 for 12-bit 4:4:4 with alpha, so a large high-bit-depth 4:4:4 AVIF can be refused under `max_pixels` (a 7000x7000 12-bit 4:4:4 file with alpha needs about 800 MB against the default 381 MB).
- Retries: a fetch that times out, loses its connection, or gets HTTP 429 or a 5xx is retried up to `retries` times with exponential backoff (0.5 s, 1 s, 2 s, ... plus jitter), each source within `retry_total_secs`. Other errors (403, 404, a refused host) are not retried. A source that still fails keeps its URL as below.
- Failures: a source that fails (an HTTP error such as 403, a refused or oversized image, an unsupported format, truncated or corrupt data that fails to decode, or a cache miss offline) keeps its original URL and the build still succeeds. Each prints `warning: image <url>: <reason>` when it fails, and the build summary lists them all again (`kept original URL: <url> (<reason>)`) after the `images:` totals line. Bad data is removed from the cache, so the next online build fetches it again.
- Variants: the configured widths below the source width plus the source width itself (never upscaled), in each modern format, plus one fallback at `fallback_width`: a progressive JPEG, or a PNG only when some pixel is actually transparent (the alpha channel is scanned, not just declared; the answer is cached). EXIF orientation is applied, then all metadata is dropped. Animated PNG/WebP are copied unchanged under a hashed name.
- WebP encoding (stills and animations) calls libwebp directly through `libwebp-sys`, which compiles its vendored libwebp (1.6.0) into the binary; no system library is used. `build-site --version` prints the linked version. Stills are lossy at `webp_quality` with libwebp's default settings (method 4, alpha kept and compressed losslessly). Decoding WebP sources is pure Rust (the `image` crate).
- Animated GIFs: resized to the same widths (never upscaled) and encoded as animated WebP (libwebp's animation encoder; each frame lossy at `webp_quality` or lossless, whichever is smaller), printed as an `<img>` with a WebP `srcset`; `image_url` returns the animated WebP too. Widths stop at `animation_max_width` (default 640), because animations cost many times a still's bytes and a phone at DPR 3 would otherwise pick a variant about 3x its slot. The `src`, `width` and `height` are the capped largest variant, and an `image_url` width is clamped to the cap. A slot wider than the cap shows the animation upscaled; raise the cap where that matters. Stills are not capped. There is no GIF fallback: every browser in use decodes animated WebP (Chrome 32, Firefox 65, Safari 14, Edge 18 and later), so shipping the GIF would only add bytes. Frame timing and the loop count are kept, using browser rules: a delay of 10 ms or less plays at 100 ms, a GIF without a loop extension plays once, and a loop count of n repeats n times after the first play. Frames are resized with premultiplied alpha and keep GIF's on/off transparency. Before any frame is decoded, a scan of the GIF's blocks (no decompression) counts frames against `max_frames`, checks width x height x frames against `max_animation_pixels`, checks every frame's own size against `max_pixels` and refuses frames that fall outside the screen (the decoder would fill them anyway), counts the sum of every frame's width x height against `max_animation_pixels` too, and estimates decode and encode memory against the per-image budget (`max_pixels` x 8 bytes, see Memory). Frames are decoded and encoded one at a time, all widths in one pass. Anything over a limit, malformed, or failing to decode keeps its original URL with a warning, like any failed image. A single-frame GIF is a still image. `animated_gifs: copy` keeps the old behavior (the GIF copied unchanged). Social crawlers vary in WebP support, so avoid an animated GIF as `og:image`. SVG is copied only from site paths (files the site already serves); a remote SVG keeps its original URL and is listed, because SVG is a document that can carry script and no markup filter reliably makes an untrusted one safe to serve from the site.
- Cache: fetched bytes are stored by sha256 under `cache_dir/src/`, URLs map to them in `cache_dir/urls/`, and encoded variants live in `cache_dir/out/`, each with `<name>.sha256` holding the hash of its bytes. Nothing is trusted by name: source bytes are checked against their hash when read and before encoding, variants against their recorded hash, and a mismatch is evicted with a warning and fetched or encoded again (variants cached before hashes were recorded are encoded once more). This catches corrupt or swapped files; it cannot catch a writer who replaces both a file and its record, or points a URL at other valid content, so restore the cache only from builds you trust. Remote URLs are treated as immutable: delete the cache to refetch. `IMAGES_OFFLINE=1` builds from the cache only. Output names include a fingerprint of the quality settings, so changing them never reuses a cached name. `IMAGES_JOBS` sets the worker threads (default: all cores). Add the cache directory to the site's `.gitignore` (`.cache/`): it holds fetched originals and is rebuilt on demand.
- Memory: at most `IMAGES_DECODES` images (default: the smaller of the core count and 4) are decoded and held at once, each up to `max_pixels` x 8 bytes while resizing, so the default caps image memory near 1.6 GB (4 x 50 MP x 8 B); an animated GIF or an AVIF holds one slot and must fit the same per-image budget, with its decode estimated from its header (see AVIF above); lower `max_pixels` or `IMAGES_DECODES` on small runners.
- Security: https only, the host must be allowed on every redirect hop (at most 3), the resolver drops loopback, private, link-local, CGNAT, site-local, 6to4, Teredo, NAT64-wrapped private, and other reserved addresses (so DNS cannot point an allowed name inward), no proxy, a global timeout, a body cap, and a pixel cap before decode. Output and cache names are hex digests and numbers only; site paths cannot leave `dist/` (`..`, encoded separators, and symlinks are refused). Remote SVG is never copied (see Variants). The origin server sends SVG with `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox`.

## Serve

```bash
cargo run -p server     # serves dist/ on http://127.0.0.1:8787
```

The server reads `SITE_ROOT`, else the current directory, the same way as the build.

It listens on 127.0.0.1 only by default, so a dev or CI run is not reachable from the network. Settings:

| Setting | Default | Meaning |
|---|---|---|
| `PORT` | `8787` | Port to listen on. `0` lets the OS pick a free one; the startup line prints the address actually bound. |
| `--host ADDR` | `127.0.0.1` | IP address to listen on (`--host 0.0.0.0` or `--host=::`). Only IP literals and `localhost` are accepted, never other host names. |
| `POSTCMS_HOST` | unset | Same as `--host`, for environments configured by env vars; `--host` wins when both are set. |

A host that is not an IP address (including a `POSTCMS_HOST` or `--host` value that is not valid UTF-8), or an unknown flag, is a usage error (exit 2) and nothing listens. Binding a non-loopback address prints `warning: listening on ...: other machines can reach this server` on stderr. IPv4-mapped IPv6 addresses count as the IPv4 address they carry, so `::ffff:127.0.0.1` is loopback and does not warn.

**Upstream errors.** When the model call fails, `/api/ask` returns a fixed message and never the upstream's own error text, which can name `LLM_BASE_URL`, internal hosts or headers:

| Failure | Status | Body |
|---|---|---|
| Connect, DNS or TLS failure | 502 | `{"error":"upstream unreachable"}` |
| No response within 60 s | 504 | `{"error":"upstream timed out"}` |
| Upstream returns a non-2xx status, or a 2xx body that is not JSON | 502 | `{"error":"upstream error"}` |

The detail goes to the server log (stderr) as one line starting `ask: `: the request URL with any username, password, query string and fragment removed (or `<unparseable LLM_BASE_URL>` when the value has no `http` or `https` scheme), the error chain, and for a non-2xx status the status code and the first 300 characters of the upstream body, with every occurrence of `LLM_API_KEY` replaced by `[redacted]`.

**Embeddings without HOME.** `/api/ask` embeds the question with the model cached in `$HOME/.cache/post-cms-fastembed`. When `HOME` is unset or empty, the server never falls back to a shared path such as `/tmp/.cache`, where another local user could plant model files. It turns semantic retrieval off instead, logs `embeddings: HOME is not set; semantic retrieval is off ...` at startup, and `/api/ask` inlines the corpus (the same fallback as a site without `embeddings.json`). Set `HOME` to turn retrieval back on. The build uses a private per-run directory in this case; the server does not, because a long-running process stopped by a signal never removes that directory, and each start would download the model again.

**Containers and deploys.** Inside a container, 127.0.0.1 is the container's own loopback, so a published port will not reach the server. Set the address explicitly:

```bash
POSTCMS_HOST=0.0.0.0 PORT=8080 server       # or: server --host 0.0.0.0
```

Earlier versions always listened on `0.0.0.0`; a deployment that relied on that must now set `POSTCMS_HOST=0.0.0.0` (or pass `--host 0.0.0.0`). Behind a reverse proxy on the same machine, keep the default and point the proxy at `127.0.0.1:$PORT`.

Routes: `/`, `/posts/<slug>`, `/rss.xml`, `/sitemap.xml`, `/robots.txt`,
`/health`, 404 fallback → `404.html`.

Cache-Control: HTML → `public, max-age=0, s-maxage=86400,
stale-while-revalidate=604800`; RSS/sitemap/robots → `s-maxage=3600`.

## Response headers

`site.yaml` `headers:` adds response headers per path. The origin server (`crates/server`) reads it at startup.

```yaml
headers:
  default:                         # every page, asset, redirect, and 404
    X-Frame-Options: SAMEORIGIN
  rules:                           # in order; a later rule wins per header
    - path: /signup
      headers:
        Content-Security-Policy: "default-src 'self'; script-src 'self' forms.example.com"
    - path: [/docs/*, /guides/*]
      headers:
        X-Robots-Tag: noindex
```

- `path` is a pattern or a list of patterns. Each starts with `/`. `*` matches any run of characters, `/` included, so `/docs/*` matches `/docs/a` and `/docs/a/b` but not `/docs`.
- Matching ignores leading and trailing slashes, a trailing `.gz` (the precompressed sibling), a trailing `index.html`, and ASCII case, so `/signup`, `/signup/`, `/SignUp/Index.html`, and `/signup/index.html.gz` get the same headers. Query strings are not part of the match.
- A response gets the `default` headers, then the headers of every matching rule in file order. A later value replaces an earlier one for the same header name.
- Headers apply to every response from the static fallback: pages, assets, redirects (`redirects.yaml` and slash redirects), and 404s, all matched on the request path. `/health`, `/api/ask`, and `/mcp` get none.
- The server always sends `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`, and a `Cache-Control` per file type. Site headers apply last, so a site can replace `Referrer-Policy` or `Cache-Control` for a path. A `Cache-Control` in `default` also replaces the server's value on redirects and 404s.
- Validation at startup, which refuses to start on any error: names must be tokens (letters, digits, and ``!#$%&'*+-.^_`|~``); values must be non-empty strings with no control characters (CR, LF, NUL) and no surrounding whitespace; a name may appear once per map. These headers are rejected because the server owns them: hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Connection`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`), `Content-Length`, `Content-Type`, `Content-Encoding`, `Content-Range`, `Accept-Ranges`, `Location`, `Date`, `Server`, `Set-Cookie`, `Vary`, and `X-Content-Type-Options`.
- The build does not emit a CDN `_headers` file. CDNs that read one combine every matching block instead of overriding per header, so the same config would mean something different there. Put the CDN in front of this origin and let it pass the headers through.

## MCP self-test

```bash
python3 mcp/server.py --self-test
```

Tools: `list_posts`, `get_post`, `create_draft`, `update_post`, `build_site`,
`publish_status`. All mutations are git commits.

## Deploy + CDN purge

```bash
ORIGIN_TARGET=./deploy-target bash ci/deploy.sh   # build + rsync
bash ci/purge.sh                                  # stub purge, appends ci/purge.log
```

## Agent workflow

See [agents/CONVENTIONS.md](agents/CONVENTIONS.md) — in particular the
publish gate: never merge to production without explicit human confirmation.

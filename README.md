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
| `ci/` | build, deploy, purge scripts; `container-check.sh` for the image |
| `Dockerfile` | origin image: server binary plus one site's built `dist/` (see Container image) |

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

`dist/embeddings.json` holds one vector per text chunk (about 700 characters) of every published post and indexable collection item. The build sends the chunks to the model `POSTCMS_EMBED_BATCH` at a time (default 32, a whole number from 1 to 128), so peak memory follows the batch size, not the number of chunks. An invalid `POSTCMS_EMBED_BATCH` fails the build before `dist/` is touched. The vectors are the same at any batch size within float rounding (under 1e-6 per component).

Memory grows about linearly with the batch, and above 16 the speed stays about the same. Peak RSS of a whole build of a 2,879-chunk site on an Apple Silicon Mac (`/usr/bin/time -l`):

| `POSTCMS_EMBED_BATCH` | 1 | 8 | 16 | 32 (default) | 64 | 128 |
|---|---|---|---|---|---|---|
| peak RSS | 0.44 GB | 0.57 GB | 0.72 GB | 1.04 GB | 1.83 GB | 3.27 GB |
| wall time | 45 s | 42 s | 31 s | 30 s | 31 s | 31 s |

Lower the batch on small CI runners. Raising it above 32 costs memory and gains no speed.

### Tailwind CLI

The theme CSS is compiled by `tailwindcss` from `PATH` (the npm `@tailwindcss/cli` or the standalone binary). Only absolute `PATH` entries are searched: an empty or relative entry (`::`, `.`, `node_modules/.bin`) is skipped, since it would resolve against the site root and run a `tailwindcss` placed in the site. The CLI's own `PATH` is filtered the same way, so a `node` placed in the site never runs either. Put `$PWD/node_modules/.bin` on `PATH`, not the relative form. By default, when it is not on `PATH`, exits non-zero or writes empty CSS, the build inlines the raw `theme.css` and prints a warning (`warning: ...; inlining raw theme.css`), so a site still builds without Node. To make any of those fail the build instead, pass `build-site --require-css-compiler` or set `POSTCMS_REQUIRE_TAILWIND=1`; use one of them in CI and in any check that measures the CSS, since raw `theme.css` is not what production serves. `POSTCMS_REQUIRE_TAILWIND` accepts `1`, `0` or unset; any other value fails the build before `dist/` is touched.

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

Item templates get `title`, `slug`, `description`, `date` (when the item has one), `fields`, `collection`, `refs` (see below), `body`, and `url`, the item's own path. A `DRAFTS=1` build renders every item again under `/preview/drafts/<collection>/<slug>/` with the same data, so a template that renders on the site renders as a draft too. There `url` is that preview path (following `trailing_slash`), never the public one, `draft` is `true`, and `agent_notes` and `date` are empty strings when the item has none.

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

### Template sandbox

Templates see only their context; they cannot read the build environment or the file system. In `build-site` and `pt-render`:

- `get_env(...)` always fails the build with `get_env is disabled: templates cannot read the build environment`; pass values through `site.yaml` or page and item fields instead.
- `throw(...)` fails the build with `a template called throw()`; its `message` is never printed.
- `now()` is always UTC (`utc=` is accepted and ignored), so the host time zone never reaches the output. `now(timestamp=true)` and `now() | date(format="%Y")` work as before.
- `include`, `extends` and `import` resolve only among the loaded templates.
- A theme name (`site.yaml` `theme:`, `SITE_THEME`, each `SITE_PREVIEW_THEMES` entry) must match `^[a-z0-9_-]{1,64}$`; anything else, such as `../../x`, fails the build. `SITE_PREVIEW_THEMES=all` skips directories whose names do not match.
- `build-site` checks `templates/` and the theme directory (`theme.css`, `assets/`, `templates/`) before reading them: each must resolve inside the site root, and a symlink under them that resolves outside that directory, or does not resolve, fails the build. `pt-render --templates <dir>` applies the symlink check to `<dir>`.

Every other Tera builtin function, filter and test works as documented.

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
- Draft previews (`DRAFTS=1`): drafts render under `dist/preview/drafts/`, and an image that only drafts use is written to `dist/preview/drafts/<output_dir>/` and linked there, so it sits behind the same access control as the drafts: whatever gates `/preview/` gates it too, and `ci/deploy.sh` excludes `preview/`. An image any published page uses stays in `/<output_dir>/`, and drafts link that copy. Names stay content-addressed and come from the same cache, so when a draft publishes, the next build writes the same files to `/<output_dir>/`. A build without `DRAFTS` never renders drafts, so no draft-only image is written at all. Search index, embeddings, sitemap, RSS, `llms.txt` and per-post markdown list published items only.
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
| `IAP_AUDIENCE` | unset | Turns on the draft preview behind Identity-Aware Proxy (IAP): the preview backend service, `/projects/<number>/global/backendServices/<id>`. Unset: `/preview/` is always 404, and a dist holding drafts refuses to start (see Startup below). |
| `IAP_JWKS_URL` | `https://www.gstatic.com/iap/verify/public_key-jwk` | IAP signing keys (JWK set). Must be `https`. |
| `IAP_JWKS_MAX_STALE_SECS` | `86400` | How long past the 1 h cache life the key set is still used while every refresh fails. After that every preview token is refused and the server logs an error. |
| `IAP_JWKS_ALLOW_HTTP` | unset | `1` allows an `http` `IAP_JWKS_URL` on a loopback host only. For tests; never set it in a deploy. |

A host that is not an IP address (including a `POSTCMS_HOST` or `--host` value that is not valid UTF-8), or an unknown flag, is a usage error (exit 2) and nothing listens. Binding a non-loopback address prints `warning: listening on ...: other machines can reach this server` on stderr. IPv4-mapped IPv6 addresses count as the IPv4 address they carry, so `::ffff:127.0.0.1` is loopback and does not warn.

**Draft preview behind IAP.** A `DRAFTS=1` build writes unpublished items under `dist/preview/` (theme previews from `SITE_PREVIEW_THEMES` land there too). The server serves `/preview` and everything under it only when the request carries an `x-goog-iap-jwt-assertion` header that verifies:

- signed ES256 by a key from `IAP_JWKS_URL`, found by the token's `kid` (`alg` must be exactly `ES256`, so `none` and `HS256` are refused; any `crit` header is refused);
- `aud` equal to `IAP_AUDIENCE` and `iss` equal to `https://cloud.google.com/iap`;
- `exp` not passed, and `iat` and `nbf` (when present) not in the future, each with 30 s of clock skew; each must be an integer from 1 to 9999999999 (floats, strings and null are refused);
- an `email` claim.

The `Host` header plays no part: when the public and preview hosts reach the same service, any client can send any header to the public host, so only the signature decides. Every `/preview/` response, served or refused and for any method (a `POST` gets a 405 with them too), sends `Cache-Control: private, no-store` and `X-Robots-Tag: noindex, nofollow`, applied after `site.yaml` headers so no site rule can make a draft cacheable. A refused request gets the site's normal 404 page.

Keys are fetched with a 5 s timeout, a 64 KB cap, no redirects, and https only, one fetch at a time. They are cached for an hour. Two separate budgets decide when to fetch again:

- Stale set: the first request after the hour fetches at once; while fetches fail, retries are at least 30 s apart and the cached keys keep working for up to `IAP_JWKS_MAX_STALE_SECS`.
- Unknown `kid` on a fresh set: at most one fetch per 30 s, and a `kid` the last fetch showed missing is remembered for 5 minutes and fetches nothing. A flood of forged `kid`s therefore cannot hammer the key endpoint or delay the stale-set refresh.

Startup: a server without `IAP_AUDIENCE` is a public origin, and a public origin refuses to start (exit 1, before it listens) over a dist that holds drafts: a `dist/preview` entry of any kind or letter case (`DRAFTS=1` and `SITE_PREVIEW_THEMES` builds write it), or `dist/.post-cms-drafts`, the marker every `DRAFTS=1` build writes so a deploy that strips `preview/` is still caught (the public pages of a drafts build differ too: plugins are skipped). The message names what it found: `refusing to start: <dist> holds a drafts or preview build (dist/preview) and IAP_AUDIENCE is unset, so this is a public origin, which must never serve drafts. ...`. Public or preview comes from configuration only, never a request, and anything short of a valid `IAP_AUDIENCE` (unset, empty, a misspelled name) counts as public, so a config mistake fails closed. Set `IAP_AUDIENCE` on the preview service only. A set but malformed `IAP_AUDIENCE`, or an `IAP_JWKS_URL` that is not https, refuses to start. Handlers that need the signed-in user (for example an approval step) take the `iap::VerifiedIdentity` extractor, which holds the verified `email` and `sub`, or `None`.


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

**Stopping.** SIGTERM or SIGINT stops the server accepting connections, logs `origin: SIGTERM, shutting down` (or `SIGINT`), lets open requests finish for up to `POSTCMS_SHUTDOWN_TIMEOUT_SECS` (default 25, so it fits a 30 s platform grace period), then drops whatever is still open (logging `origin: <n> connection(s) still open after <s>s ...`), logs `origin stopped`, and exits 0. Idle keep-alive connections close at once. Set the timeout below your platform's grace period (Cloud Run and `docker stop` give 10 s).

**Timeouts and limits.** A client has `POSTCMS_HEADER_READ_TIMEOUT_SECS` (default 10) to send a complete request head; a connection that sends part of one and stalls is closed. `POST /api/ask` and `POST /mcp` must deliver the whole body within `POSTCMS_BODY_READ_TIMEOUT_SECS` (default 15, a total, so a slow drip does not reset it), else `408`. Bodies are capped at 16 KiB for `/api/ask` and 256 KiB for `/mcp` (`413`, refused up front when `Content-Length` is larger); a body that is not `application/json` gets `415`, and malformed JSON `400`. `/api/ask` applies its per-client rate limit before reading the body and takes one of its `ASK_MAX_CONCURRENT` model slots only once the whole body has arrived, so stalled bodies cannot hold the slots. A response write that makes no progress for `POSTCMS_WRITE_TIMEOUT_SECS` (default 15; the clock restarts on every byte) drops the connection, so a client that stops reading cannot hold it. At most `POSTCMS_MAX_CONNECTIONS` (default 512) connections are open at once; further connections are closed as soon as they are accepted, with one log line per 10 s. The cap is global, not per client: behind a load balancer such as Google Cloud Load Balancing or Cloud Run's front end, which hold client connections and open their own to the origin, that is the intended shape; exposed directly, one client can use all the slots. Timeouts take whole seconds from 1 to 3600 and the cap a whole number from 1 to 65535; any other value refuses to start (exit 2).

**Client address for the rate limit.** By default (`POSTCMS_TRUSTED_PROXY_HOPS=0`) the `/api/ask` rate limit keys on the socket peer and ignores `X-Forwarded-For`, which any client can forge. Behind `n` proxies that each append to `X-Forwarded-For`, set `POSTCMS_TRUSTED_PROXY_HOPS=n`: the client is then the `n`th entry from the right (all `X-Forwarded-For` headers joined in order), and entries to its left are ignored. Google Cloud Load Balancing in front of Cloud Run appends `<client>, <load balancer>`, so use 2. A request whose header has fewer entries, or a non-IP entry there, falls back to the peer. Whole number from 0 to 10.

Routes: `/`, `/posts/<slug>`, `/rss.xml`, `/sitemap.xml`, `/robots.txt`,
`/health`, 404 fallback → `404.html`.

Cache-Control: see [Caching](#caching).

## Caching

`site.yaml` `cache:` sets the `Cache-Control` the origin sends per class of response. Every key is optional; these are the defaults:

```yaml
cache:
  html: "public, max-age=0, s-maxage=300, stale-while-revalidate=86400"
  hashed: "public, max-age=31536000, immutable"
  static: "public, max-age=3600, s-maxage=300, stale-while-revalidate=86400"
  feeds: "public, max-age=0, s-maxage=300, stale-while-revalidate=3600"
  not_found: "public, max-age=0, s-maxage=60"
  redirects: "public, max-age=3600, s-maxage=300"
```

| Class | Responses |
|---|---|
| `html` | Pages, and CSS, JS and SVG that are not hashed, plus unknown file types. Browsers always revalidate; the CDN keeps a copy 5 minutes and may serve it stale for a day while it refetches, so a deploy shows within minutes even when a purge is slow or rate-limited. |
| `hashed` | Files the image pipeline wrote under content-addressed names: directly in `images.output_dir` (default `assets/img`), named `<16 hex>.<ext>` or `<16 hex>-<width>.<ext>` with `avif`, `webp`, `jpg`, `png`, `gif` or `svg`. Only when `images:` is on. A changed image gets a new name, so these never go stale. |
| `static` | Other images, fonts, media and PDFs (theme and plugin assets keep their own names and can change in place, so they are never `immutable`). |
| `feeds` | `.xml`, `.json`, `.txt`, `.md` (sitemap, RSS, robots.txt, llms.txt, search index). |
| `not_found` | Every 404 from the static fallback. |
| `redirects` | `redirects.yaml` hits and slash redirects. |

- Values are checked at startup, which refuses to start (exit 1, naming the key) on any error: known response directives only (`public`, `private`, `no-cache`, `no-store`, `no-transform`, `must-revalidate`, `proxy-revalidate`, `must-understand`, `immutable`, and `max-age`, `s-maxage`, `stale-while-revalidate`, `stale-if-error` with whole seconds up to 2147483648), each at most once, not both `public` and `private`. `private` and `no-cache` take no field list. The same check applies to a `Cache-Control` set under `headers:`.
- A `Cache-Control` in a `headers:` rule (below) applies after the class value, so a rule can still set one path's policy. `headers.default` cannot set `Cache-Control`.
- Not configurable: every `/preview` response (served, refused, 404 or 405) gets `Cache-Control: private, no-store`, `X-Robots-Tag: noindex, nofollow` and `Vary` naming `x-goog-iap-jwt-assertion`; every `/mcp` and `/api/ask` response gets `private, no-store`; `/health` gets `no-store`. A router layer sets these after everything `site.yaml` sets.
- The server logs the active `html` and `hashed` values at startup (`cache: html ...`).

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
- Headers apply to every response from the static fallback: pages, assets, redirects (`redirects.yaml` and slash redirects), and 404s, all matched on the request path. `/health`, `/api/ask`, and `/mcp` get none, and no site header changes a `/preview` response's `Cache-Control` or `X-Robots-Tag`.
- The server sends `X-Content-Type-Options: nosniff` on every response (pages, assets, previews, redirects, 404s, `/health`, `/mcp`, `/api/ask`, and method errors), and `Referrer-Policy: strict-origin-when-cross-origin` and a `Cache-Control` per class (see [Caching](#caching)) on files. Site headers apply last, so a site can replace `Referrer-Policy` or `Cache-Control` for a path. `Cache-Control` is allowed in rules only: under `default` it refuses to start, because it would override 404s and redirects (use `cache:` for a class).
- Every SVG (public assets, draft images under `/preview`, plain or precompressed) is sent as `image/svg+xml` with `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox`. An SVG opened directly is a document that could run script as the site; this policy allows only its inline styles and `data:` images, and `sandbox` gives it an opaque origin with no scripts, forms or popups. Browsers ignore a response CSP on an SVG used as `<img>` or a CSS image, so those render the same. A `Content-Security-Policy` that site.yaml sets for an SVG path is sent as a second header after this one: browsers enforce both, so a site policy can tighten an SVG but never loosen it.
- Validation at startup, which refuses to start on any error: names must be tokens (letters, digits, and ``!#$%&'*+-.^_`|~``); values must be non-empty strings with no control characters (CR, LF, NUL) and no surrounding whitespace; a name may appear once per map. These headers are rejected because the server owns them: hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Connection`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`), `Content-Length`, `Content-Type`, `Content-Encoding`, `Content-Range`, `Accept-Ranges`, `Location`, `Date`, `Server`, `Set-Cookie`, `Vary`, and `X-Content-Type-Options`.
- The build does not emit a CDN `_headers` file. CDNs that read one combine every matching block instead of overriding per header, so the same config would mean something different there. Put the CDN in front of this origin and let it pass the headers through.

## MCP self-test

```bash
python3 mcp/server.py --self-test
```

Tools: `list_posts`, `get_post`, `create_draft`, `update_post`, `build_site`,
`publish_status`. All mutations are git commits.

## Container image

`Dockerfile` builds an image holding the origin server and one site's built `dist/`. It needs BuildKit (`docker buildx`, the default builder in current Docker).

```bash
docker build -t demo-site --build-arg SITE_CACHE_ID=demo .     # the demo site in this repo
ci/site-context.sh ../my-site /tmp/my-site-ctx                 # stage a site: tracked files only
docker build -t my-site --build-context site=/tmp/my-site-ctx --build-arg SITE_CACHE_ID=my-site .
docker run --rm --read-only --tmpfs /tmp -p 8080:8080 my-site  # http://127.0.0.1:8080/health
```

Stages: `engine` compiles `server` and `build-site` (dependencies in their own layer, reused until `Cargo.toml` or `Cargo.lock` changes), `site-build` runs `build-site` over the site, and `runtime` gets only the server binary at `/usr/local/bin/post-cms-server` and `dist/`, `site.yaml` and `redirects.yaml` under `/site`. `build-site`, the site sources and the Rust toolchain are not in the runtime image.

### Build inputs

| Input | How | Meaning |
|---|---|---|
| engine | build context (`.`) | A post-cms checkout. `.dockerignore` is an allowlist: only `Cargo.*`, `crates/` and the demo site files enter the context, and `.env*`, `*.key`, `*.enc`, `*.pem`, `credentials*.json`, `.cache`, `node_modules`, `target` and `dist` are dropped even inside those. |
| site | `--build-context site=<dir>` (default: the demo site in this repo) | A directory staged by `ci/site-context.sh <site-checkout> <dir> [extra-path...]`: only `site.yaml`, `redirects.yaml`, `content/`, `templates/`, `themes/`, `plugins/` and any extra paths named, as git tracks them at `HEAD`. No `.git`, untracked or ignored files (a local `master.key`, `.env`, `node_modules`), uncommitted edits or other tracked paths (`config/`, `scripts/`), and tracked secret-named files inside the allowed paths are removed (`.env*`, `*.key`, `*.enc`, `*.pem`, `*.p12`, `*.pfx`, `id_rsa*`, `id_ed25519*`, `credentials*.json`, `.npmrc`, `.netrc`). Stage it because a named context is sent to BuildKit whole (its cache, or a remote builder) and this repo's `.dockerignore` does not filter it; the build refuses a site context holding `.git`, `.env*`, `*.key`, `*.enc` or `*.pem`. The site is bind-mounted into the `build-site` step, never copied into a layer; only `dist/`, `site.yaml` and `redirects.yaml` reach the image. |
| `SITE_CACHE_ID` | `--build-arg`, required | One id per site (letters, digits, `.`, `_`, `-`); the build fails without it. Scopes the build's model and image cache mounts, so on a shared builder one site's build never reuses cache entries another site wrote. Never share an id between sites. |
| `SANITY_READ_TOKEN` | `--secret id=SANITY_READ_TOKEN,env=SANITY_READ_TOKEN` (or `src=<file>`) | Read token for sites that pull from Sanity. Mounted for the `build-site` step only; never in a layer, the history or the image config. |
| `SITE_BASE_URL`, `SITE_THEME`, `DRAFTS`, `POSTCMS_EMBED_BATCH`, `IMAGES_OFFLINE`, `SANITY_OFFLINE` | `--build-arg` | The `build-site` settings of the same names (see Build). Unset or empty means the engine default. Build args are visible in `docker history`: never pass a secret this way. |
| `TAILWINDCSS_URL`, `TAILWINDCSS_SHA256` | `--build-arg`, both or neither | Standalone Tailwind CLI for themes that compile CSS (see Tailwind CLI). The download must match the SHA-256 and be https. |
| `RUST_IMAGE`, `RUNTIME_IMAGE` | `--build-arg` | Base images, pinned by digest in the Dockerfile. To update one, resolve the tag's new digest (`docker buildx imagetools inspect <image>:<tag>`) and replace tag and digest together. |

`build-site` downloads the embedding model during the build (network access to Hugging Face), cached in a BuildKit cache mount (per `SITE_CACHE_ID`) between builds, as are processed images.

A site repository builds its own image from a post-cms checkout at a pinned commit (a submodule or a CI checkout), for example `path/to/post-cms/ci/site-context.sh . "$CTX" && docker build --build-context site="$CTX" --build-arg SITE_CACHE_ID=my-site -t site:$SHA path/to/post-cms`.

### Runtime

Base image: `gcr.io/distroless/cc-debian12:nonroot`, chosen over `debian-slim` because it has no shell, package manager or setuid binaries, and still carries what the server links against: glibc, libstdc++ (the ONNX runtime behind `/api/ask` is C++), OpenSSL and CA certificates (model download, the LLM call). The builder is Debian 12 too, so the binary's glibc matches. Debugging is through logs and `/health`; there is nothing to `exec` into.

The image runs as uid/gid 65532 (`nonroot`), and `/site` is owned by root and not writable by it. The only path the server writes is the `/api/ask` model cache, `$HOME/.cache/post-cms-fastembed` with `HOME=/tmp/post-cms`, so a read-only root file system works with a writable `/tmp` (`--read-only --tmpfs /tmp`, or an `emptyDir` on `/tmp` in Kubernetes). The model (over 100 MB) is downloaded on the first `/api/ask` after each start, so size the tmpfs for it; without network access to Hugging Face, or without a writable `/tmp`, `/api/ask` falls back to inlining the corpus. There is no `HEALTHCHECK` (no shell or curl in the image): point the platform's probe at `GET /health`.

Runtime environment, set when the container runs and never baked into the image (secrets from the platform's secret store):

| Variable | Image default | Meaning |
|---|---|---|
| `PORT` | `8080` | Listen port. Platforms that assign one (Cloud Run) set it. |
| `POSTCMS_HOST` | `0.0.0.0` | Listen address. The server's own default is 127.0.0.1, which a published port cannot reach. |
| `SITE_ROOT` | `/site` | Site directory holding `dist/`. |
| `HOME` | `/tmp/post-cms` | Locates the model cache. |
| `POSTCMS_SHUTDOWN_TIMEOUT_SECS` | unset (25) | Seconds open connections get after SIGTERM (see Serve, Stopping). |
| `POSTCMS_HEADER_READ_TIMEOUT_SECS` | unset (10) | Seconds a client has to send a request head. |
| `POSTCMS_BODY_READ_TIMEOUT_SECS` | unset (15) | Seconds a client has to send a whole POST body. |
| `POSTCMS_MAX_CONNECTIONS` | unset (512) | Connections open at once; more are closed on accept. |
| `LLM_API_KEY`, `LLM_BASE_URL`, `LLM_MODEL`, `LLM_MAX_RAG_CHARS` | unset | `/api/ask` upstream. `LLM_API_KEY` is a secret. |
| `RATE_LIMIT_PER_MIN`, `ASK_MAX_CONCURRENT` | unset (10, 4) | `/api/ask` limits. |
| `POSTCMS_TRUSTED_PROXY_HOPS` | unset (0) | Proxies that append `X-Forwarded-For` (see Serve, Client address); 2 behind Google Cloud Load Balancing. |
| `POSTCMS_WRITE_TIMEOUT_SECS` | unset (15) | Seconds a response write may make no progress. |
| `IAP_AUDIENCE`, `IAP_JWKS_URL` | unset | Identity-Aware Proxy check for `/preview/`, in servers that have it: the backend service audience, and the key set URL (default Google's). Per deployment, so never in the image. |

The image's stop signal is SIGTERM; the server exits 0 on it (see Serve, Stopping).

### Checks

`ci/container-check.sh` builds `git archive HEAD` of this repo twice: the demo site, and a site checkout (a git repo with a tracked `*.enc` and an untracked `config/master.key` and `.env`) staged by `ci/site-context.sh` as a named build context. Canary secrets are planted in the engine context (`config/master.key`, `.env`, `*.enc`, `*.pem`), the site checkout and a `SANITY_READ_TOKEN` build secret. It fails when staging leaves `.git` or a secret; when the build accepts the unstaged checkout; when the build step does not read the token secret; when a canary appears in the build output, any layer (every layer is unpacked, gzip or zstd, and counted against the manifest), the history or the image config; when the image has a secret-like file name, a shell, the site sources or `build-site`; when the image user is root; or when the container, run with `--read-only --tmpfs /tmp --cap-drop ALL` and a non-default `PORT`, does not answer `/health` and `/`, runs a process as uid 0, or does not exit 0 within 10 s of SIGTERM. It removes the containers, images and temp files it creates. Run it in CI on any runner with Docker and BuildKit.

## Hosted MCP behind an identity-aware proxy

`mcp/http_server.py` serves the MCP tools over HTTP. `OAUTH_MODE` picks the
identity source: `github`, `oidc` (bearer tokens), or `iap`.

With `OAUTH_MODE=iap` the server sits behind Google Cloud Identity-Aware
Proxy and trusts only the proxy's signed `x-goog-iap-jwt-assertion` header.
The `Authorization` header is ignored, so there is one identity source.

| Variable | Default | Meaning |
|---|---|---|
| `IAP_AUDIENCE` | none, required | Expected `aud`, for example `/projects/<number>/global/backendServices/<id>`. The server refuses to start without it. |
| `IAP_JWKS_URL` | `https://www.gstatic.com/iap/verify/public_key-jwk` | Proxy signing keys (JWKS). Must be `https://`; redirects are not followed. |
| `IAP_JWKS_MAX_STALE_SECS` | `86400` | How long past the 1-hour TTL the held keys stay valid while fetches fail. After that every token is refused, with one logged error, until a fetch succeeds. |

Each request's assertion must be ES256 (exactly), signed by a key in the
JWKS, with `iss` `https://cloud.google.com/iap`, `aud` equal to
`IAP_AUDIENCE`, and `exp`, `iat` and `nbf` integers in [1, 9999999999] within
30 seconds of skew. Every segment must be canonical base64url and its JSON at
most 32 levels deep. The
keys are cached for an hour; an unknown `kid` triggers at most one refresh
every 30 seconds, and a failed fetch keeps the keys already held. The
identity's stable `id` is `https://cloud.google.com/iap|<sub>`, which is the
`id` to list in the roles file; `email` (the
claim, lowercased) is kept for display and the commit record. A missing or
invalid assertion gets a bare `401 {"error": "unauthorized"}`; the reason goes
to the server log only.

In every mode, request bodies are capped at 1 MiB (an admin's `add_media`
call may be larger, see MCP roles; over the cap is a 413), `Transfer-Encoding`
and malformed or conflicting `Content-Length` get a 400, a body on GET or HEAD
gets a 400, and every error response closes the connection, so an unread body
is never parsed as the next request. Faults inside a tool or the connector
return a generic 500; details go to the server log.

Approvals (`approve_proposal`, `merge=true`) take the approver from this
identity, and the distinct-approver check compares it with the proposal's
commit author. The verifier is `mcp/iap.py`, standard library only. Tests:

```bash
python3 -m unittest discover -s mcp -p 'test_*.py'
```

## MCP forge: GitHub or GitLab

Proposals become pull requests (GitHub) or merge requests (GitLab). The
propose, approve, reject, revert and queue tools reach the forge only
through `mcp/forge.py`.

| Variable | Meaning |
|---|---|
| `FORGE` | `github` (default) or `gitlab`. |
| `GITLAB_URL` | Base URL, `https://` only (for example `https://gitlab.example.com`, or a path prefix for a self-managed install). No credentials, query or fragment. |
| `GITLAB_PROJECT` | Numeric project id, or the project path (`group/project`, plain or url-encoded). |
| `GITLAB_TOKEN` | Token with the `api` scope. Sent only in the `PRIVATE-TOKEN` header, never in a URL, log line or error. Maintainer role to merge into a protected branch. |
| `GITLAB_ALLOW_HTTP_LOOPBACK` | Tests only: `1` allows `http://` for `127.0.0.1`, `localhost` or `::1`. |

Both forges:

- Every change the server opens or revises is recorded as branch, change id and the head commit the server pushed, in `changes.json` in the server state dir: `$PCMS_STATE_DIR` (an absolute path outside the repository, mode 0700, required over HTTP), default `.tenants/.state` (gitignored). Writes hold an `flock` on `changes.lock` and replace the file through an `O_EXCL|O_NOFOLLOW` temp file.
- `approve_proposal` needs `reviewed_head` (or `reviewed_heads` `{slug: sha}` for a batch): the `head` that `list_proposals` and the propose and revise results show. It merges the recorded change id only, pinned to that head, and refuses when the head is missing, differs from the recorded head, or the change moved, was retargeted away from `main`, or was not opened or revised by this server. `list_proposals` also returns `forge_head` and `moved`. `merge=true` on a propose tool pins to the head that same call pushed.
- `revise_proposal` only revises a change the server recorded, starting from the recorded head: if `origin/<branch>` has moved (someone else pushed), it refuses. Its push leases on the recorded head.
- Changes from forks are refused everywhere (look up, merge, close, the queue, revert): a GitHub PR must have `isCrossRepository` false and its head repository and owner equal to the base repository; a GitLab MR must have source and target project equal to `GITLAB_PROJECT`. Branch lookups match the exact head ref.
- `revert_proposal` takes the same gate as every merge (`MCP_ALLOW_APPROVE`). On GitLab, when merging is not allowed, the revert merge request still opens and is left for a person to merge.

`FORGE=github` runs the `gh` CLI (approval identity from `gh api user`).
`FORGE=gitlab` needs no CLI:

- Proposals open a merge request into `main` that deletes its source branch on merge.
- Approve merges through the merge endpoint. A pinned head goes in its `sha` parameter, so GitLab refuses the merge (409) when a commit landed after the check.
- A project whose merge method is fast-forward (or semi-linear) only merges a branch that sits on `main`. A branch that is behind fails with a clear error; the server never rebases, because a rebase makes a new head that nobody approved.
- Reject posts a `REJECTED:` note, closes the merge request and deletes the branch.
- The commit list and every author and committer of a change come from the merge request commits API, all pages.
- Revert opens a revert merge request: a new `revert/<branch>-<time>` branch from `main`, GitLab's commit revert endpoint applied there (the merge commit, or each commit newest first for fast-forward merges), then a merge pinned to the revert head. Nothing is pushed to `main` directly, so protected-branch rules, pipelines and approval rules apply to a revert too. If GitLab cannot merge it (for example a required pipeline), the tool fails and names the open revert merge request.
- On stdio the approver identity is the token's user (`GET /user`); over HTTP it is the OAuth identity, as before. The email recorded in notes is the user's public email or GitLab's noreply form (`<id>-<username>@users.noreply.<host>`), never the private primary address.
- Each read waits at most 20 seconds and each request at most 60 seconds overall; a response is capped at 4 MB, a paged read at 16 MB in total, JSON nesting at 64 levels. Proxy settings (`HTTP_PROXY` and similar) are ignored for the API, redirects are refused, and 401, 403, 404, 405, 406, 409 and 422 map to plain tool errors.

## MCP roles

Every tool call is checked against the caller's role (`mcp/authz.py`). Roles
build on each other:

| Role | Tools |
| --- | --- |
| reader | `list_posts`, `get_post`, `list_proposals`, `publish_status` |
| author | reader, plus `propose_change`, `propose_template`, `propose_plugin`, `revise_proposal`, `build_preview` |
| approver | author, plus `approve_proposal`, `reject_proposal`, `revert_proposal`, and `merge=true` on a `propose_*` tool |
| admin | everything, plus `create_draft`, `update_post`, `set_theme`, `build_site`, `add_media` |

`create_draft`, `update_post` and `set_theme` commit straight to the shared
tree with no review. Over HTTP they are off, even for admins, unless
`MCP_ALLOW_DIRECT_COMMIT=1`. `tools/list` shows only the tools the caller may
call.

`add_media` reads the file type from the content (magic bytes), never the
name, and accepts PNG, JPEG, GIF, WebP, PDF, MP4, MOV, WebM, MP3, WAV and M4A
(not SVG). The size cap is `MCP_MEDIA_MAX_BYTES` (default 25 MiB). Send the
file as `content_base64` (with an optional `filename`). Over HTTP that is the
only way: a server `file_path` is refused. On stdio `file_path` also works,
but only for a regular file inside `MCP_UPLOADS_DIR` (default `uploads/`,
gitignored), opened one directory at a time with no symlink followed.
Images are probed with ffprobe first and refused over `MCP_MEDIA_MAX_PIXELS`
(default 50,000,000) or `MCP_MEDIA_MAX_SECONDS` (default 600); every
ffmpeg/ffprobe run has a timeout (`MCP_FFMPEG_TIMEOUT`, default 60 s), no
stdin and only `PATH` in its environment.

Request bodies over HTTP: 1 MiB, except an admin's `add_media` call, which may
be up to `ceil(MCP_MEDIA_MAX_BYTES * 4 / 3) + 64 KiB`. `Content-Length` is
checked before the body is read (413, connection closed); chunked bodies and
malformed lengths get 400; a body that stalls for 30 s gets 408. A cached
OIDC token is dropped at its `exp`.

**stdio** (`python3 mcp/server.py`): one local user, who already holds the
checkout. The default role is admin; set `MCP_STDIO_ROLE=reader|author|approver`
to narrow it. No roles file is read.

**HTTP** (`mcp/http_server.py`): roles come from a JSON file,
`MCP_ROLES_FILE` (default `mcp/roles.json`, gitignored):

```json
{
  "allowed_domains": ["example.com"],
  "users": [
    {"id": "https://issuer.example.com|0f3c9a", "email": "alice@example.com", "role": "admin"},
    {"id": "github:583231", "email": "octocat@example.com", "role": "author"},
    {"email": "bob@example.com", "role": "reader"}
  ]
}
```

- Prefer `id`, the stable actor id: `<iss>|<sub>` for OIDC (the token's
  issuer and subject), `github:<numeric user id>` for GitHub (`id` from
  `GET /user`). Logins and emails can be renamed and re-registered; ids
  cannot. On an entry with `id`, `email` is for display only.
- An email-only entry matches only an OIDC or IAP identity whose `email_verified`
  claim is true, whose email is plain ASCII on `allowed_domains`, and which
  equals the entry (ASCII case-insensitive). Non-ASCII emails are refused,
  so Unicode lookalikes cannot match.
- `OAUTH_MODE=iap` matches by `id` (`https://cloud.google.com/iap|<sub>`), or
  an email-only entry by the email the proxy signed (treated as verified).
- `OAUTH_MODE=github` matches by numeric id only. A GitHub login or profile
  email never grants a role.
- An identity with no entry gets HTTP 403 on every request, including
  `initialize`. A valid GitHub or OIDC token alone grants nothing.
- The server refuses to start when the file is missing, is not a regular
  file, is world-writable, is not valid JSON, repeats a JSON key, has an
  unknown key or role, has a malformed id or a non-ASCII email, lists one
  identity twice, lists an email outside `allowed_domains`, or has no
  non-empty `allowed_domains` (fail closed; the list is required in GitHub
  mode too).
- The file is read once at startup. Restart the server to apply a change,
  including a revocation; a validated token is also cached for up to five
  minutes.
- Approver rights come only from this file: the server refuses to start when
  `MCP_APPROVERS` is set. With no approver or admin listed it warns at
  startup, and approve, reject and revert are refused for everyone.

## Deploy + CDN purge

```bash
ORIGIN_TARGET=./deploy-target bash ci/deploy.sh   # build + rsync
bash ci/purge.sh                                  # stub purge, appends ci/purge.log
```

## Security

Dependency audit commands and the accepted unmaintained-crate warnings are in
[SECURITY.md](SECURITY.md).

## Agent workflow

See [agents/CONVENTIONS.md](agents/CONVENTIONS.md) — in particular the
publish gate: never merge to production without explicit human confirmation.

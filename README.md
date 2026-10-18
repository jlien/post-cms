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
| `crates/server` | axum static origin, port 8787 (`PORT` env to override) |
| `mcp/server.py` | MCP stdio server (stdlib only) |
| `templates/` | tera templates (base, post, index, 404) |
| `ci/` | build, deploy, purge scripts |

## Build

```bash
cargo run -p build      # renders dist/
```

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

`sitemap.xml` leaves out any page or collection item whose meta sets `no_index: true` (a YAML boolean), and any path that is a source in `redirects.yaml`. Every `<loc>` is XML-escaped. The page itself still builds; emitting a `noindex` robots tag is up to the template (`page.fields.no_index`, `fields.no_index`).

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
  timeout_secs: 30
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

- Sources: `https://` URLs on `allowed_hosts`, and site paths (`/assets/...`) resolved inside `dist/`. Other hosts and plain http keep their URL silently.
- Failures: a source that fails (an HTTP error such as 403, a refused or oversized image, an unsupported format, truncated or corrupt data that fails to decode, or a cache miss offline) keeps its original URL and the build still succeeds. Each prints `warning: image <url>: <reason>` when it fails, and the build summary lists them all again (`kept original URL: <url> (<reason>)`) after the `images:` totals line. Bad data is removed from the cache, so the next online build fetches it again.
- Variants: the configured widths below the source width plus the source width itself (never upscaled), in each modern format, plus one fallback at `fallback_width`: a progressive JPEG, or a PNG only when some pixel is actually transparent (the alpha channel is scanned, not just declared; the answer is cached). EXIF orientation is applied, then all metadata is dropped. Animated PNG/WebP are copied unchanged under a hashed name.
- Animated GIFs: resized to the same widths (never upscaled) and encoded as animated WebP (libwebp's animation encoder; each frame lossy at `webp_quality` or lossless, whichever is smaller), printed as an `<img>` with a WebP `srcset`; `image_url` returns the animated WebP too. Widths stop at `animation_max_width` (default 640), because animations cost many times a still's bytes and a phone at DPR 3 would otherwise pick a variant about 3x its slot. The `src`, `width` and `height` are the capped largest variant, and an `image_url` width is clamped to the cap. A slot wider than the cap shows the animation upscaled; raise the cap where that matters. Stills are not capped. There is no GIF fallback: every browser in use decodes animated WebP (Chrome 32, Firefox 65, Safari 14, Edge 18 and later), so shipping the GIF would only add bytes. Frame timing and the loop count are kept, using browser rules: a delay of 10 ms or less plays at 100 ms, a GIF without a loop extension plays once, and a loop count of n repeats n times after the first play. Frames are resized with premultiplied alpha and keep GIF's on/off transparency. Before any frame is decoded, a scan of the GIF's blocks (no decompression) counts frames against `max_frames`, checks width x height x frames against `max_animation_pixels`, checks every frame's own size against `max_pixels` and refuses frames that fall outside the screen (the decoder would fill them anyway), counts the sum of every frame's width x height against `max_animation_pixels` too, and estimates decode and encode memory against the per-image budget (`max_pixels` x 8 bytes, see Memory). Frames are decoded and encoded one at a time, all widths in one pass. Anything over a limit, malformed, or failing to decode keeps its original URL with a warning, like any failed image. A single-frame GIF is a still image. `animated_gifs: copy` keeps the old behavior (the GIF copied unchanged). Social crawlers vary in WebP support, so avoid an animated GIF as `og:image`. SVG is copied only from site paths (files the site already serves); a remote SVG keeps its original URL and is listed, because SVG is a document that can carry script and no markup filter reliably makes an untrusted one safe to serve from the site.
- Cache: fetched bytes are stored by sha256 under `cache_dir/src/`, URLs map to them in `cache_dir/urls/`, and encoded variants live in `cache_dir/out/`. Remote URLs are treated as immutable: delete the cache to refetch. `IMAGES_OFFLINE=1` builds from the cache only. Output names include a fingerprint of the quality settings, so changing them never reuses a cached name. `IMAGES_JOBS` sets the worker threads (default: all cores). Add the cache directory to the site's `.gitignore` (`.cache/`): it holds fetched originals and is rebuilt on demand.
- Memory: at most `IMAGES_DECODES` images (default: the smaller of the core count and 4) are decoded and held at once, each up to `max_pixels` x 8 bytes while resizing, so the default caps image memory near 1.6 GB (4 x 50 MP x 8 B); an animated GIF holds one slot and must fit the same per-image budget; lower `max_pixels` or `IMAGES_DECODES` on small runners.
- Security: https only, the host must be allowed on every redirect hop (at most 3), the resolver drops loopback, private, link-local, CGNAT, site-local, 6to4, Teredo, NAT64-wrapped private, and other reserved addresses (so DNS cannot point an allowed name inward), no proxy, a global timeout, a body cap, and a pixel cap before decode. Output and cache names are hex digests and numbers only; site paths cannot leave `dist/` (`..`, encoded separators, and symlinks are refused). Remote SVG is never copied (see Variants). The origin server sends SVG with `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox`.

## Serve

```bash
cargo run -p server     # serves dist/ on http://localhost:8787
```

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

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
- Matching ignores leading and trailing slashes, a trailing `index.html`, and ASCII case, so `/signup`, `/signup/`, and `/signup/index.html` get the same headers. Query strings are not part of the match.
- A response gets the `default` headers, then the headers of every matching rule in file order. A later value replaces an earlier one for the same header name.
- Headers apply to every response from the static fallback: pages, assets, redirects (`redirects.yaml` and slash redirects), and 404s, all matched on the request path. `/health`, `/api/ask`, and `/mcp` get none.
- The server always sends `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`, and a `Cache-Control` per file type. Site headers apply last, so a site can replace `Referrer-Policy` or `Cache-Control` for a path.
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

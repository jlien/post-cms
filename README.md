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

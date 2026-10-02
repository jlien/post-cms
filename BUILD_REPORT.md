# Build Report — Post-CMS Publishing Stack v0.1

Date: 2026-10-02. All deliverables built; every verification step from TASK.md executed with actual output recorded below.

## Deliverables

| Deliverable | Status | Files |
|---|---|---|
| Content format + 3 seed posts | ✅ | `content/posts/{welcome,this-architecture,static-first-rendering}/{meta.yaml,index.mdx}` |
| Static build pipeline (Rust) | ✅ | `crates/build` (serde, serde_yaml, pulldown-cmark, tera, chrono, anyhow, serde_json) |
| Origin server (Rust, axum 0.8) | ✅ | `crates/server` |
| MCP tool server (stdlib JSON-RPC 2.0) | ✅ | `mcp/server.py` |
| Deploy + CDN purge scripts | ✅ | `ci/build.sh`, `ci/deploy.sh`, `ci/purge.sh` |
| Repo hygiene | ✅ | `README.md`, `agents/CONVENTIONS.md`, `.gitignore`, git history |

Templates: `templates/{base,post,index,404}.html` — dark theme, system fonts, fully inline CSS, self-contained (no external assets). Inline CSS carries a `<!-- css-sha256: … -->` content-hash comment (SHA-256 of the template CSS, computed in-crate) and a `Cache-Control` guidance comment.

## Verification results (actual outputs)

### 1. `cargo build` — PASS

```
Finished `dev` profile [unoptimized + debuginfo] in 0.24s   (workspace, both crates)
```

Compile errors hit and fixed during development:
- Crate binary named `build` conflicts with cargo's build dir → package kept as `build`, bin renamed `build-site` (`cargo run -p build` still works).
- Axum 0.8 route syntax: `/posts/:slug` → `/posts/{slug}` (panicked at startup with 0.7 syntax).
- Missing `serde_json` dep in `server`, `StaticDir` missing `#[derive(Clone)]`, `String` move errors in RSS generation.

### 2. `cargo run -p build` → dist/ — PASS

```
built dist/: 3 posts (1 drafts skipped), index, 404, rss.xml, sitemap.xml, robots.txt
```

Actual files:
```
dist/404.html
dist/index.html
dist/posts/static-first-rendering/index.html
dist/posts/this-architecture/index.html
dist/posts/welcome/index.html
dist/robots.txt
dist/rss.xml
dist/sitemap.xml
```

### 3. Server curl checks — PASS

Server started in background on port 8787.

| Route | Status | Evidence |
|---|---|---|
| `/` | 200 | — |
| `/posts/welcome` | 200 | body contains `<h1>Welcome to Post-CMS</h1>` |
| `/rss.xml` | 200 | body contains `<rss version="2.0">`, 3 `<item>`s |
| `/sitemap.xml` | 200 | — |
| `/robots.txt` | 200 | — |
| `/health` | 200 | `{"status":"ok"}` |
| `/nonexistent` | 404 | serves `dist/404.html` |

Cache-Control headers observed via `curl -I`:
- `/`, `/posts/welcome`, 404 fallback: `public, max-age=0, s-maxage=86400, stale-while-revalidate=604800`
- `/rss.xml`: `public, s-maxage=3600`

### 4. MCP self-test — PASS

```
$ python3 mcp/server.py --self-test
== post-cms mcp self-test ==
[PASS] initialize
[PASS] tools/list
[PASS] list_posts
[PASS] get_post
[PASS] create_draft
[PASS] create_draft appears in list_posts(draft)
[PASS] update_post diff
[PASS] publish_status
[PASS] build_site
[PASS] unknown tool errors
== ALL PASS ==
```

### 5. create_draft → list_posts → rebuild — PASS (live stdio JSON-RPC session)

Drove the real stdio server with piped JSON-RPC lines:

1. `create_draft(agent-field-notes)` → `{"created": true, "path": "content/posts/agent-field-notes/meta.yaml", "published": false}` — git commit `c7aa5f8 draft: create agent-field-notes`.
2. `list_posts(status=draft)` → count 1, `agent-field-notes`.
3. `build_site()` → `built dist/: 3 posts (1 drafts skipped)` — draft correctly absent from `dist/posts/`, RSS items, and index links.
4. `update_post(published: true)` → diff `{"field": "published", "from": false, "to": true}`; rebuild → `4 posts (0 drafts skipped)`.
5. Flipped back to draft + rebuild → page removed from dist again. Post left in draft state per the publish gate (no explicit user confirmation to publish).

Git log shows one commit per mutation as required (`draft: create …`, `update …: published`, `update …: tags`).

### 6. Server killed — PASS

Background server process stopped after verification.

## Bugs found and fixed during verification

1. **MCP YAML mini-parser dropped block-list tags** — `_read_meta` ignored `  - item` lines, and `update_post`'s read-modify-write then rewrote `tags: []` into files (real data loss). Fixed the parser to accumulate block-list items; repaired the affected draft's tags via `update_post`.
2. Axum 0.8 path-capture syntax (startup panic) — fixed to `{slug}`.
3. Build emitted HTML pages for unpublished drafts — changed to skip drafts (consistent with `agents/CONVENTIONS.md`: drafts invisible on the site); build now reports skipped draft count.

## Verified vs not verified

**Verified:** all six verification steps above, end to end, with real outputs.

**Not verified / limitations:**
- `ci/purge.sh` is a stub by design (writes `ci/purge.log`, prints `purged`); the Cloudflare curl is a comment only — no real CDN was purged.
- `ci/deploy.sh` rsynced to the local default `./deploy-target/` only; no remote target was exercised.
- MCP server was not exercised by a real MCP client library (e.g. Claude Desktop); protocol shape follows JSON-RPC 2.0 with `initialize`/`tools/list`/`tools/call` and was tested with raw stdio frames.
- Draft-page 404 behavior: the origin serves files from `dist/` and never sees draft state; because drafts produce no HTML file, `/posts/<draft-slug>` correctly 404s — verified by construction (file absent), not via curl.

## Repo state

- 4 post directories (3 published seeds + 1 agent-created draft), all mutations committed.
- `target/`, `dist/`, `deploy-target/`, `ci/purge.log` gitignored.

# Agent conventions for Post-CMS

How an AI agent should operate this stack.

## Content model

- One post = `content/posts/<slug>/` containing `meta.yaml` + `index.mdx`.
- `slug` must be lowercase, digits and hyphens only, and match the directory name.
- `published: false` means draft. Drafts are invisible on the site and in RSS/sitemap.
- MDX is treated as plain markdown — no JSX.

## Draft → preview → publish flow

1. **Draft** — `create_draft(slug, title, body, tags, description)`. Always
   creates with `published: false`. One git commit per mutation.
2. **Preview** — `build_site()` and run the origin
   (`cargo run -p server`); drafts never render publicly, but the build must
   succeed before claiming anything works. Check `publish_status()`.
3. **Publish** — `update_post(slug, published: true)`, rebuild, then
   `ci/deploy.sh` and `ci/purge.sh`.

## The publish gate

**Never merge to production without explicit human confirmation.** Concretely:

- The agent may create drafts, edit content, and rebuild freely.
- The agent MUST NOT flip `published: true` on a post that was never shown to
  the user, and MUST NOT run `ci/deploy.sh` against a real target without an
  explicit "publish it" instruction in the conversation.
- If the user asks to "publish", confirm which slug(s) before flipping.

## Build hygiene

- Run `build_site()` after every content mutation; report build failures verbatim.
- Templates and Rust code are code — propose a diff, don't silently rewrite.
- `dist/`, `target/`, `deploy-target/` are build artifacts; never commit them.

## Tool reference

| Tool | Effect |
|---|---|
| `list_posts(status)` | summary, filtered by all/published/draft |
| `get_post(slug)` | full meta + markdown body |
| `create_draft(...)` | new draft + git commit |
| `update_post(slug, ...)` | field edits + diff summary + git commit |
| `build_site()` | `cargo run -p build`, returns output tail |
| `publish_status()` | live/pending counts and lists |

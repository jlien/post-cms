# Agent conventions for Post-CMS

How an AI agent should operate this stack. Git is the ledger, not the
interface — every human-visible change goes through the **proposal flow**.

## Content model

- One post = `content/posts/<slug>/` containing `meta.yaml` + `index.mdx`.
  Typed collections (e.g. `content/integrations/<slug>/`) work the same way
  with collection-specific `fields`.
- `slug` must be lowercase, digits and hyphens only, and match the directory name.
- `published: false` means draft. Drafts are invisible on the site and in RSS/sitemap.
- MDX is treated as plain markdown — no JSX.

## The proposal flow (the only write path)

1. **Propose** — `propose_change(...)` (or `propose_template`). This creates
   an `agent/<slug>` branch, runs a REAL build to validate before the commit
   (schema violations are rejected with the exact error, no PR), and opens a
   PR — the proposal's ID. Extras that ride on the proposal:
   - `agent_notes` — context for the reviewer, rendered on the draft preview.
   - `fields` — typed collection fields, validated against the collection schema.
   - `published: true` — publish-on-approve: the item goes live the moment
     the proposal is approved, not before.
   - Every proposal gets a **risk class** (`copy`, `new-item`, `schema`,
     `publish-flip`) recorded on the PR.
2. **Review** — render the draft preview so the reviewer sees the artifact,
   not a diff: `cd <worktree> && git fetch origin <branch> && git switch
   <branch> && DRAFTS=1 cargo run -q -p build && python3 -m http.server 8000
   -d dist`, then open `http://localhost:8000/preview/drafts/<collection>/<slug>/`.
   `list_proposals` returns the queue with risk class and a field-level diff
   (e.g. `tier: pro -> enterprise`) per proposal.
3. **Iterate** — `revise_proposal(slug, ...)` updates the SAME proposal (same
   PR, version 2+). Never open a competing PR for the same item.
4. **Approve** — `approve_proposal(slug, approver_name, approver_email)`.
   Identity is recorded as a PR comment (the audit receipt), then merge →
   build → deploy → purge in one action. Batches of low-risk copy proposals
   can ride one call: `approve_proposal(slugs: [...], ...)`.
5. **Reject** — `reject_proposal(slug, reason, reviewer_name, reviewer_email)`.
   The reason is recorded as a `REJECTED:` comment — agents read it and
   revise instead of re-proposing blind. `list_proposals(include_rejected: true)`
   surfaces recent rejections.
6. **Undo** — `revert_proposal(slug, approver_name, approver_email)` reverts
   the merge commit, rebuilds, redeploys and purges in one action.

## The publish gate

**The agent cannot publish directly — the gate is structural.**

- `update_post` REJECTS `published` — publishing decisions only move through
  `propose_change` + `approve_proposal`. There is no side door.
- A new item with `published: true` stays invisible until its proposal is
  approved (the build renders it at its live path only in draft previews).
- `approve_proposal` / `revert_proposal` REQUIRE a human's name and email —
  that identity goes on the permanent record.
- The agent MUST NOT run `ci/deploy.sh` against a real target without an
  explicit instruction in the conversation.

## Build hygiene

- Validation happens BEFORE the commit: if `propose_change` fails a build,
  report the exact error and fix the proposal — do not commit broken content.
- `dist/`, `target/`, `deploy-target/` are build artifacts; never commit them.
- Templates and Rust code are code — propose a diff, don't silently rewrite.

## Tool reference

| Tool | Effect |
|---|---|
| `list_posts(status)` | summary, filtered by all/published/draft |
| `get_post(slug)` | full meta + markdown body |
| `update_post(slug, ...)` | field edits (title/description/tags/body) — cannot touch `published` |
| `propose_change(...)` | proposal PR w/ build validation, risk class, `agent_notes`, `fields`, `published` |
| `propose_template(...)` | template proposal, same flow |
| `revise_proposal(slug, ...)` | iterate the SAME proposal (same PR) |
| `list_proposals()` | the queue: risk class, field diff, preview command; `include_rejected` for reasons |
| `approve_proposal(slug\|slugs, ...)` | identity on record → merge → build → deploy → purge |
| `reject_proposal(slug, reason, ...)` | REJECTED: comment + close |
| `revert_proposal(slug, ...)` | one-action undo → revert → build → deploy → purge |
| `build_site()` | `cargo run -p build`, returns output tail |
| `build_preview()` | DRAFTS=1 draft build for review |
| `publish_status()` | live/pending counts and lists |

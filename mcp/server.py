#!/usr/bin/env python3
"""Post-CMS MCP tool server.

Speaks MCP (JSON-RPC 2.0) over stdio using only the Python standard library.
Every mutation is written to content/posts/<slug>/ and committed to git
(silently skipped when the repo has no .git directory).

Run:   python3 mcp/server.py            # JSON-RPC server on stdio
       python3 mcp/server.py --self-test
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
CONTENT = ROOT / "content" / "posts"
TEMPLATES_FOR_BUILD = "cargo run -p build"
PROTOCOL_VERSION = "2024-11-05"

TOOLS = [
    {
        "name": "list_posts",
        "description": "List all posts with status summary.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["all", "published", "draft"],
                    "description": "Filter by publish status.",
                }
            },
        },
    },
    {
        "name": "get_post",
        "description": "Get full metadata and markdown body for one post.",
        "inputSchema": {
            "type": "object",
            "properties": {"slug": {"type": "string"}},
            "required": ["slug"],
        },
    },
    {
        "name": "create_draft",
        "description": "Create a new draft post (published: false).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string"},
                "title": {"type": "string"},
                "body": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "description": {"type": "string"},
            },
            "required": ["slug", "title", "body"],
        },
    },
    {
        "name": "update_post",
        "description": "Update fields of an existing post; returns a diff summary. Cannot change `published` — use propose_change + approve_proposal.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string"},
                "title": {"type": "string"},
                "body": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "description": {"type": "string"},
            },
            "required": ["slug"],
        },
    },
    {
        "name": "build_site",
        "description": "Rebuild the static site (cargo run -p build); returns output tail.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "build_preview",
        "description": (
            "Build every theme (or a comma list) under /preview/<theme>/ without "
            "changing the live site. themes: 'all' or e.g. 'astra,neve'."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "themes": {"type": "string", "description": "'all' or a comma list of theme names."},
            },
        },
    },
    {
        "name": "add_media",
        "description": (
            "Upload a binary file (image/video/audio/pdf) to the media bucket at "
            "upload-time with resized variants for images; returns markdown/HTML "
            "snippets to embed in a post. Binaries never go in git."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Post slug to namespace media under."},
                "file_path": {"type": "string", "description": "Local path of the file to upload."},
                "alt": {"type": "string", "description": "Alt text / link label."},
            },
            "required": ["slug", "file_path"],
        },
    },
    {
        "name": "set_theme",
        "description": (
            "Switch the active theme (a directory under themes/), writes site.yaml, "
            "and rebuilds. Returns the new theme and available themes."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "theme": {"type": "string", "description": "Theme name, e.g. 'default' or 'paper'."},
            },
            "required": ["theme"],
        },
    },
    {
        "name": "propose_change",
        "description": (
            "Propose a content change WITHOUT touching the shared working "
            "tree: writes it on an agent/<slug> branch, commits with the "
            "caller's git identity, pushes to origin and opens a pull "
            "request with a preview command in the body. Use this instead "
            "of create_draft/update_post when the repo is shared."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Item slug (new or existing)."},
                "title": {"type": "string", "description": "Title (required for new items)."},
                "collection": {"type": "string", "description": "content/ subfolder: 'posts' (default) or a typed collection name."},
                "description": {"type": "string"},
                "body": {"type": "string", "description": "Full markdown body to set."},
                "agent_notes": {"type": "string", "description": "Agent commentary rendered only in the draft preview (why the draft is written this way, open questions)."},
                "fields": {"type": "object", "description": "Extra meta keys, e.g. typed-collection fields (vendor, tier). Schema-validated by a real build before the PR opens."},
                "published": {"type": "boolean", "description": "Target published state of a NEW item (default false). true = publish on approve."},
                "tenant": {"type": "string", "description": "Isolated working copy (.tenants/<tenant>) for multi-tenant simulation."},
                "author_name": {"type": "string", "description": "Git author name for the commit."},
                "author_email": {"type": "string", "description": "Git author email for the commit."},
                "merge": {"type": "boolean", "description": "Merge the PR immediately (default false)."},
            },
            "required": ["slug", "author_name", "author_email"],
        },
    },
    {
        "name": "revise_proposal",
        "description": (
            "Iterate on an existing proposal: same branch, same PR, version "
            "N+1. Pass slug plus any of body/title/description/agent_notes/"
            "fields/collection. The push updates the PR in place — feedback "
            "revises the artifact instead of opening a new PR."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Slug of the proposed item."},
                "collection": {"type": "string", "description": "content/ subfolder (default 'posts')."},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "body": {"type": "string"},
                "agent_notes": {"type": "string"},
                "fields": {"type": "object", "description": "Extra meta keys to set/overwrite (e.g. {\"published\": true} to publish on approve)."},
                "author_name": {"type": "string"},
                "author_email": {"type": "string"},
            },
            "required": ["slug", "author_name", "author_email"],
        },
    },
    {
        "name": "list_proposals",
        "description": (
            "The proposal queue: open PRs on agent/* branches with number, "
            "title, slug, author, risk class, field-level diff, URL and the "
            "DRAFTS=1 draft-preview command for each. include_rejected adds "
            "recent rejections (with reasons) so agents can revise."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "include_rejected": {"type": "boolean",
                                     "description": "Also return recently rejected proposals with their reasons."},
            },
        },
    },
    {
        "name": "approve_proposal",
        "description": (
            "Approve one proposal (slug) or a batch (slugs): records the "
            "approver's identity as PR comments, merges, then builds, deploys "
            "and purges once — 'approve' means live. deploy=false skips deploy."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Slug of the proposed item (or use slugs for a batch)."},
                "slugs": {"type": "array", "items": {"type": "string"},
                          "description": "Batch of low-risk proposals to approve in one action (one build+deploy)."},
                "approver_name": {"type": "string"},
                "approver_email": {"type": "string"},
                "deploy": {"type": "boolean", "description": "Build+deploy+purge after merge (default true)."},
            },
            "required": ["approver_name", "approver_email"],
        },
    },
    {
        "name": "reject_proposal",
        "description": (
            "Reject a proposal: records WHO and WHY as a REJECTED: comment on "
            "the PR (agent-readable state), then closes the PR."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Slug of the proposed item."},
                "reason": {"type": "string", "description": "Why — the proposing agent reads this to revise."},
                "reviewer_name": {"type": "string"},
                "reviewer_email": {"type": "string"},
            },
            "required": ["slug", "reason", "reviewer_name", "reviewer_email"],
        },
    },
    {
        "name": "revert_proposal",
        "description": (
            "One-action undo: reverts the merge commit that shipped a proposal, "
            "then rebuilds, redeploys and purges. Records the revert on the PR."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Slug of the shipped proposal to undo."},
                "approver_name": {"type": "string"},
                "approver_email": {"type": "string"},
                "deploy": {"type": "boolean", "description": "Build+deploy+purge after revert (default true)."},
            },
            "required": ["slug", "approver_name", "approver_email"],
        },
    },
    {
        "name": "propose_template",
        "description": (
            "Create or update a Tera template (templates/<name>.html) via the "
            "review flow. Validates the template with a real build before "
            "opening a PR, so a broken template never reaches a merge request. "
            "Collections select templates with item_template/index_template in "
            "their schema.yaml."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Template name (no ext) — becomes templates/<name>.html."},
                "body": {"type": "string", "description": "Full template HTML. Should extend base and define {% block content %}."},
                "tenant": {"type": "string", "description": "Isolated working copy (.tenants/<tenant>) for multi-tenant simulation."},
                "author_name": {"type": "string", "description": "Git author name for the commit."},
                "author_email": {"type": "string", "description": "Git author email for the commit."},
                "merge": {"type": "boolean", "description": "Merge the PR immediately (default false)."},
            },
            "required": ["name", "body", "author_name", "author_email"],
        },
    },
    {
        "name": "publish_status",
        "description": "Counts and lists of live vs pending (draft) posts.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


# ---------------------------------------------------------------- content io

def _slug_ok(slug: str) -> bool:
    return bool(slug) and all(c.isalnum() or c == "-" for c in slug) and slug == slug.lower()


def _post_dir(slug: str) -> Path:
    return CONTENT / slug


def _read_meta(slug: str) -> dict[str, Any]:
    path = _post_dir(slug) / "meta.yaml"
    meta: dict[str, Any] = {"title": "", "slug": slug, "tags": [], "lang": "en",
                            "published": False, "date": "", "description": ""}
    if not path.exists():
        return meta
    current_key: str | None = None
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if stripped.startswith("- ") and current_key == "tags":
            meta["tags"].append(stripped[2:].strip().strip("'\""))
            continue
        if ":" not in line:
            continue
        key, _, raw = line.partition(":")
        key, raw = key.strip(), raw.strip()
        current_key = key
        if raw == "":
            continue  # block list or nested value; items handled above
        if raw.startswith("[") and raw.endswith("]"):
            meta[key] = [t.strip().strip("'\"") for t in raw[1:-1].split(",") if t.strip()]
        elif raw.lower() in ("true", "false"):
            meta[key] = raw.lower() == "true"
        else:
            meta[key] = raw.strip("'\"")
    return meta


def _read_body(slug: str) -> str:
    path = _post_dir(slug) / "index.mdx"
    return path.read_text() if path.exists() else ""


def _yaml_value(v: Any) -> str:
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, list):
        if not v:
            return "[]"
        return "\n" + "\n".join(f"  - {t}" for t in v)
    return json.dumps(str(v), ensure_ascii=False)


def _write_meta(slug: str, meta: dict[str, Any]) -> None:
    order = ["title", "slug", "tags", "lang", "published", "date", "description"]
    lines = []
    for key in order:
        if key in meta:
            lines.append(f"{key}: {_yaml_value(meta[key])}")
    _post_dir(slug).mkdir(parents=True, exist_ok=True)
    (_post_dir(slug) / "meta.yaml").write_text("\n".join(lines) + "\n")


def _load_post(slug: str) -> dict[str, Any]:
    meta = _read_meta(slug)
    meta["body"] = _read_body(slug)
    return meta


def _summary(meta: dict[str, Any]) -> dict[str, Any]:
    return {k: meta.get(k) for k in ("slug", "title", "date", "published", "tags", "description")}


def _all_slugs() -> list[str]:
    if not CONTENT.is_dir():
        return []
    return sorted(p.name for p in CONTENT.iterdir() if p.is_dir())


def _parse_date_or_today() -> str:
    import datetime
    return datetime.date.today().isoformat()


# ------------------------------------------------------------------- git

def _git(*args: str, env_extra: dict[str, str] | None = None,
         worktree: Path | None = None) -> subprocess.CompletedProcess:
    """Run git in ROOT or a linked worktree (multi-tenant isolation)."""
    env = {**os.environ, **(env_extra or {})}
    base = str(worktree) if worktree else str(ROOT)
    return subprocess.run(["git", "-C", base, *args], cwd=str(ROOT),
                          capture_output=True, text=True, env=env)


def tenant_worktree(tenant: str) -> Path:
    """Per-tenant working copy (simulates multi-tenant locally).

    Each tenant gets a persistent git worktree under .tenants/<tenant>/
    with its own index and checked-out files, so two tenants can propose
    changes concurrently without touching each other or the shared tree.
    The worktree is refreshed to the default branch head on every use.
    """
    if not tenant or "/" in tenant or ".." in tenant or tenant.startswith("."):
        raise ValueError("invalid tenant name")
    if not (ROOT / ".git").exists():
        raise ValueError("not a git repository")
    wt = ROOT / ".tenants" / tenant
    if (wt / ".git").exists():
        _git("fetch", "origin", worktree=wt)
        _git("switch", "--detach", "origin/main", worktree=wt)
        return wt
    fetch = _git("fetch", "origin")
    if fetch.returncode != 0:
        raise ValueError(f"git fetch failed: {fetch.stderr.strip()[:300]}")
    wt.parent.mkdir(parents=True, exist_ok=True)
    res = _git("worktree", "add", "--detach", str(wt), "origin/main")
    if res.returncode != 0:
        raise ValueError(f"git worktree add failed: {res.stderr.strip()[:300]}")
    return wt


def _reset_worktree(wt: Path, created: list[Path]) -> None:
    """Undo uncommitted writes in a worktree (new files removed, tracked
    restored), then leave it detached on origin/main — the proposal branch
    ref must keep pointing at the pushed commits, so reset --hard is only
    ever run against HEAD, never against origin/main."""
    for p in created:
        p.unlink(missing_ok=True)
    _git("reset", "--hard", worktree=wt)
    _git("checkout", "--detach", "origin/main", worktree=wt)


def _open_pr(wt: Path, branch: str, title: str, pr_body: str) -> tuple[bool, str]:
    pr = subprocess.run(["gh", "pr", "create", "--title", title, "--body", pr_body],
                        cwd=str(wt), capture_output=True, text=True)
    if pr.returncode != 0:
        return False, pr.stderr.strip()[:300]
    return True, pr.stdout.strip().splitlines()[-1]


def tool_propose_template(args: dict[str, Any]) -> dict[str, Any]:
    """Create a Tera template via the review flow.

    Writes templates/<name>.html on an agent/template-<name> branch,
    validates it by running a real build (a template that does not compile
    fails before any PR is opened), pushes, and opens a PR with a preview
    command. Collections select templates via schema.yaml item_template /
    index_template.
    """
    if not (ROOT / ".git").exists():
        raise ValueError("not a git repository")
    author_name = (args.get("author_name") or "").strip()
    author_email = (args.get("author_email") or "").strip()
    if not author_name or not author_email:
        raise ValueError("author_name and author_email are required (identity on the MR)")
    name = (args.get("name") or "").strip()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", name):
        raise ValueError("template name must match [A-Za-z0-9][A-Za-z0-9_-]*")
    body = args.get("body")
    if not body or not body.strip():
        raise ValueError("body (HTML) is required")
    if "{% block content %}" not in body:
        raise ValueError("template must extend a base and define a {% block content %}")
    tenant = (args.get("tenant") or "").strip()
    wt = tenant_worktree(tenant) if tenant else ROOT

    tpl_rel = Path("templates") / f"{name}.html"
    tpl_path = wt / tpl_rel
    existed = tpl_path.exists()
    if not tpl_path.parent.exists():
        raise ValueError(f"templates/ directory not found in {'worktree' if tenant else 'repo'}")
    tpl_path.write_text(body if body.endswith("\n") else body + "\n")

    try:
        # Validation: a real build compiles every template in templates/.
        build = subprocess.run(["cargo", "run", "-q", "-p", "build"], cwd=str(wt),
                               capture_output=True, text=True)
        if build.returncode != 0:
            tail = "\n".join(build.stderr.strip().splitlines()[-15:])
            _reset_worktree(wt, [] if existed else [tpl_path])
            raise ValueError(f"template failed build validation:\n{tail}")

        branch = f"agent/template-{name}"
        _git("switch", "-C", branch, worktree=wt)
        commit_env = {
            "GIT_AUTHOR_NAME": author_name,
            "GIT_AUTHOR_EMAIL": author_email,
            "GIT_COMMITTER_NAME": author_name,
            "GIT_COMMITTER_EMAIL": author_email,
        }
        verb = "update" if existed else "add"
        res = _git("add", str(tpl_rel), worktree=wt)
        res = _git("commit", "-m", f"propose: {verb} template {name}", env_extra=commit_env,
                   worktree=wt)
        if res.returncode != 0:
            if "nothing to commit" in (res.stdout + res.stderr):
                _reset_worktree(wt, [] if existed else [tpl_path])
                raise ValueError("template content unchanged — nothing to propose")
            raise ValueError(f"git commit failed: {res.stderr.strip()[:300]}")
        push = _git("push", "-u", "origin", branch, worktree=wt)
        if push.returncode != 0:
            _reset_worktree(wt, [] if existed else [tpl_path])
            raise ValueError(f"git push failed: {push.stderr.strip()[:300]}")

        root_arg = "" if tenant else "  # or pass the same tenant to render from its worktree"
        preview_cmd = (f"cd {wt} && cargo run -q -p build && "
                       f"python3 -m http.server 8000 -d dist{root_arg}")
        pr_body = (f"Agent-authored template by {author_name}.\n\n"
                   f"Use: set `item_template: {name}.html` (or index_template) in a "
                   f"collection's schema.yaml.\n\nPreview: {preview_cmd}")
        ok, pr_or_err = _open_pr(wt, branch, f"Template: {name}", pr_body)

        result: dict[str, Any] = {
            "branch": branch,
            "template": f"templates/{name}.html",
            "existed": existed,
            "tenant": tenant or None,
            "pr_url": pr_or_err if ok else None,
            "preview_command": preview_cmd,
        }
        if not ok:
            result["pr_error"] = pr_or_err
        if ok and args.get("merge"):
            # merge=true is an approval — it requires a human's identity on
            # the record, exactly like approve_proposal. No anonymous merges.
            apn = (args.get("approver_name") or "").strip()
            ape = (args.get("approver_email") or "").strip()
            if not apn or not ape:
                raise ValueError(
                    "merge=true requires approver_name and approver_email "
                    "(an approval is on the record) — or call approve_proposal "
                    "after review"
                )
            comment = subprocess.run(
                ["gh", "pr", "comment", branch, "--body",
                 f"Approved by {apn} <{ape}> via propose_change(merge=true)."],
                cwd=str(wt), capture_output=True, text=True)
            result["approval_recorded"] = comment.returncode == 0
            merge = subprocess.run(["gh", "pr", "merge", branch, "--merge", "--delete-branch"],
                                   cwd=str(wt), capture_output=True, text=True)
            result["merged"] = merge.returncode == 0
            if merge.returncode != 0:
                result["merge_error"] = merge.stderr.strip()[:300]
            else:
                result["deploy"] = ("skipped: propose_change does not deploy; "
                                    "use approve_proposal for build+deploy+purge")
        _reset_worktree(wt, [])
        return result
    except Exception:
        _reset_worktree(wt, [] if existed else [tpl_path])
        raise

def tool_propose_change(args: dict[str, Any]) -> dict[str, Any]:
    """Collaboration flow: one branch + one commit + one PR per change.

    Instead of committing to the shared working tree, this writes the change
    on an agent/<slug>-<n> branch, commits with the caller's git identity
    (author_name/author_email — required, so the MR carries a human), pushes
    to origin and opens a merge request with `gh`. The PR body carries a
    DRAFTS=1 preview command: it builds unpublished items to
    dist/preview/drafts/ so the reviewer sees the rendered page (with the
    proposal's agent_notes embedded) without touching git. `merge: true`
    merges via gh immediately.

    With `tenant`, all writes happen inside that tenant's worktree
    (.tenants/<tenant>/) instead of the repo root — multiple agents can
    propose concurrently without touching each other's files.

    `agent_notes` (string) rides on the item and renders only in the draft
    preview — the agent's voice next to its work. `fields` (object) writes
    extra meta keys, e.g. typed-collection fields like vendor/tier; schema
    validation happens in a real build BEFORE the commit, so a proposal that
    would fail the build is rejected with the exact error and no PR opens.
    """
    if not (ROOT / ".git").exists():
        raise ValueError("not a git repository")
    author_name = (args.get("author_name") or "").strip()
    author_email = (args.get("author_email") or "").strip()
    if not author_name or not author_email:
        raise ValueError("author_name and author_email are required (identity on the MR)")
    slug = (args.get("slug") or "").strip()
    if not slug or "/" in slug or ".." in slug:
        raise ValueError("slug required (existing post slug or new draft slug)")
    raw_title = (args.get("title") or "").strip()
    title = raw_title or f"Update {slug}"
    body_md = args.get("body")  # optional new markdown body
    description = args.get("description")
    agent_notes = args.get("agent_notes")
    if agent_notes is not None and not isinstance(agent_notes, str):
        agent_notes = json.dumps(agent_notes, ensure_ascii=False)
    fields = args.get("fields") or {}
    if not isinstance(fields, dict):
        raise ValueError("fields must be an object of extra meta keys")
    published = bool(args.get("published"))
    collection = (args.get("collection") or "posts").strip()
    if "/" in collection or ".." in collection:
        raise ValueError("invalid collection")
    tenant = (args.get("tenant") or "").strip()
    wt = tenant_worktree(tenant) if tenant else ROOT

    # Resolve target files. Existing item → update in place; new → draft item.
    item_dir = wt / "content" / collection / slug
    meta_path = item_dir / "meta.yaml"
    md_path = item_dir / "index.mdx"
    is_new = not item_dir.exists()

    # Branch per change, from current default branch state. Version suffixes
    # are -rN (so hyphenated slugs parse unambiguously); a slug's first
    # proposal is agent/<slug>, competing re-proposals get -r2, -r3, ...
    branch = f"agent/{slug}"
    for i in range(2, 21):
        if (_git("rev-parse", "--verify", "--quiet", f"agent/{slug}-r{i}").returncode == 0
                or _git("rev-parse", "--verify", "--quiet",
                        f"origin/agent/{slug}-r{i}").returncode == 0):
            branch = f"agent/{slug}-r{i + 1}"
    res = _git("switch", "-C", branch, worktree=wt)
    if res.returncode != 0:
        raise ValueError(f"git switch failed: {res.stderr.strip()[:300]}")

    try:
        if is_new:
            item_dir.mkdir(parents=True, exist_ok=True)
            lines = [
                f'title: "{raw_title or slug}"',
                f'slug: "{slug}"',
                f"published: {str(published).lower()}",
            ]
            if description:
                lines.append(f'description: "{description}"')
            if collection == "posts":
                lines.append(f"date: {datetime.now(timezone.utc).date().isoformat()}")
            if agent_notes is not None:
                lines.append(f"agent_notes: {_yaml_value(agent_notes)}")
            for k, v in fields.items():
                lines.append(f"{k}: {_yaml_value(v)}")
            meta_path.write_text("\n".join(lines) + "\n")
            if body_md:
                md_path.write_text(body_md if body_md.endswith("\n") else body_md + "\n")
            summary = f"propose: new {collection}/{slug} draft"
        else:
            summary = f"propose: update {collection}/{slug}"
            _apply_meta_edits(meta_path,
                              title=raw_title or None,
                              description=description,
                              agent_notes=agent_notes,
                              fields=fields or None)
        if body_md and not is_new:
            md_path.write_text(body_md if body_md.endswith("\n") else body_md + "\n")

        # Validate with a real build BEFORE committing anything: a schema
        # violation or broken item fails here with the exact compiler error,
        # so a proposal never opens a PR it can't render.
        build = subprocess.run(
            ["cargo", "run", "-q", "-p", "build"], cwd=str(wt),
            capture_output=True, text=True,
            env={**os.environ, "CARGO_TARGET_DIR": str(ROOT / "target")})
        if build.returncode != 0:
            tail = "\n".join(build.stderr.strip().splitlines()[-15:])
            raise ValueError(f"proposal failed build validation:\n{tail}")

        _git("add", str(item_dir.relative_to(wt)), worktree=wt)
        commit_env = {
            "GIT_AUTHOR_NAME": author_name,
            "GIT_AUTHOR_EMAIL": author_email,
            "GIT_COMMITTER_NAME": author_name,
            "GIT_COMMITTER_EMAIL": author_email,
        }
        res = _git("commit", "-m", summary, env_extra=commit_env, worktree=wt)
        if res.returncode != 0:
            if "nothing to commit" in (res.stdout + res.stderr):
                raise ValueError(
                    "nothing to commit — for updates pass body/description; "
                    "if the item already has this content, no MR is needed")
            raise ValueError(f"git commit failed: {res.stderr.strip()[:300]}")

        push = _git("push", "-u", "origin", branch, worktree=wt)
        if push.returncode != 0:
            raise ValueError(f"git push failed: {push.stderr.strip()[:300]}")

        preview_cmd = (f"cd {wt} && git fetch origin {branch} && git switch {branch} && "
                       f"DRAFTS=1 cargo run -q -p build && "
                       f"python3 -m http.server 8000 -d dist")
        preview_url = f"http://localhost:8000/preview/drafts/{collection}/{slug}/"
        # Risk class: the queue uses this to batch copy edits and page a
        # human for publish flips / schema changes.
        target_published = published
        if not is_new:
            try:
                cur = {}
                for line in meta_path.read_text().splitlines():
                    if ":" in line and not line.startswith((" ", "#", "-")):
                        k, _, v = line.partition(":")
                        cur[k.strip()] = v.strip()
                raw_pub = fields.get("published") if "published" in fields else cur.get("published", False)
                target_published = str(raw_pub).strip().lower() in ("true", "yes", "1")
            except Exception:
                pass
        if is_new and target_published:
            risk = "publish-flip"
        elif is_new:
            risk = "new-item"
        elif fields:
            risk = "schema"
        else:
            risk = "copy"
        pr_body = (f"Agent-authored change by {author_name}.\n\n"
                   f"risk: {risk}\n\n"
                   f"Preview URL (after running the command): {preview_url}\n"
                   f"Preview: {preview_cmd}")
        ok, pr_or_err = _open_pr(wt, branch, title, pr_body)

        result: dict[str, Any] = {
            "branch": branch,
            "proposed": True,
            "is_new": is_new,
            "risk": risk,
            "tenant": tenant or None,
            "summary": summary,
            "pr_url": pr_or_err if ok else None,
            "preview_command": preview_cmd,
        }
        if not ok:
            result["pr_error"] = pr_or_err
        if ok and args.get("merge"):
            # merge=true is an approval — it requires a human's identity on
            # the record, exactly like approve_proposal. No anonymous merges.
            apn = (args.get("approver_name") or "").strip()
            ape = (args.get("approver_email") or "").strip()
            if not apn or not ape:
                raise ValueError(
                    "merge=true requires approver_name and approver_email "
                    "(an approval is on the record) — or call approve_proposal "
                    "after review"
                )
            comment = subprocess.run(
                ["gh", "pr", "comment", branch, "--body",
                 f"Approved by {apn} <{ape}> via propose_template(merge=true)."],
                cwd=str(wt), capture_output=True, text=True)
            result["approval_recorded"] = comment.returncode == 0
            merge = subprocess.run(["gh", "pr", "merge", branch, "--merge", "--delete-branch"],
                                   cwd=str(wt), capture_output=True, text=True)
            result["merged"] = merge.returncode == 0
            if merge.returncode != 0:
                result["merge_error"] = merge.stderr.strip()[:300]
            else:
                result["deploy"] = ("skipped: propose_template does not deploy; "
                                    "use approve_proposal for build+deploy+purge")
        # Reset the worktree to the default branch head: it must be clean for
        # the next proposal (the repo root is returned to main only when no
        # tenant was used, since the shared tree must not sit on a branch).
        if wt != ROOT:
            _reset_worktree(wt, [])
        else:
            _git("switch", "-")
        return result
    except Exception:
        if wt != ROOT:
            _reset_worktree(wt, [])
        else:
            _git("switch", "-")
        raise


def _apply_meta_edits(meta_path: Path, *, title: str | None = None,
                      description: str | None = None,
                      agent_notes: str | None = None,
                      fields: dict[str, Any] | None = None) -> None:
    """Rewrite keys in an existing item's meta.yaml, appending missing ones."""
    cur = meta_path.read_text()
    changed = False
    for key, val in (("title", title), ("description", description),
                     ("agent_notes", agent_notes)):
        if val is None:
            continue
        line = f"{key}: {_yaml_value(val)}"
        if re.search(rf"(?m)^{re.escape(key)}:", cur):
            cur = re.sub(rf"(?m)^{re.escape(key)}:.*$", line, cur)
        else:
            cur = cur.rstrip("\n") + "\n" + line + "\n"
        changed = True
    for k, v in (fields or {}).items():
        line = f"{k}: {_yaml_value(v)}"
        if re.search(rf"(?m)^{re.escape(k)}:", cur):
            cur = re.sub(rf"(?m)^{re.escape(k)}:.*$", line, cur)
        else:
            cur = cur.rstrip("\n") + "\n" + line + "\n"
        changed = True
    if changed:
        meta_path.write_text(cur)


def _proposal_branch(slug: str) -> str | None:
    """Latest agent/<slug>[-rN] branch, local or on origin. The version
    suffix is -rN so hyphenated slugs parse unambiguously."""
    for n in range(20, 1, -1):
        name = f"agent/{slug}-r{n}"
        if (_git("rev-parse", "--verify", "--quiet", name).returncode == 0
                or _git("rev-parse", "--verify", "--quiet",
                        f"origin/{name}").returncode == 0):
            return name
    if (_git("rev-parse", "--verify", "--quiet", f"agent/{slug}").returncode == 0
            or _git("rev-parse", "--verify", "--quiet",
                    f"origin/agent/{slug}").returncode == 0):
        return f"agent/{slug}"
    return None


def _open_pr_for_branch(branch: str) -> str | None:
    """URL of the open PR whose head is `branch`, or None."""
    res = subprocess.run(
        ["gh", "pr", "list", "--head", branch, "--state", "open",
         "--json", "url", "--limit", "1"],
        cwd=str(ROOT), capture_output=True, text=True)
    if res.returncode != 0:
        return None
    try:
        prs = json.loads(res.stdout or "[]")
        return prs[0]["url"] if prs else None
    except (ValueError, KeyError, IndexError):
        return None


def tool_revise_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """Iterate on an existing proposal: same branch, same PR, version N+1.

    This is the artifact-model iteration unit — feedback revises the artifact
    instead of opening a new PR. Writes happen in a throwaway worktree of the
    proposal branch, so the shared tree and other tenants are never touched.
    The push updates the open PR in place; a new DRAFTS=1 preview is available
    from the same PR body command.
    """
    author_name = (args.get("author_name") or "").strip()
    author_email = (args.get("author_email") or "").strip()
    if not author_name or not author_email:
        raise ValueError("author_name and author_email are required (identity on the revision)")
    slug = (args.get("slug") or "").strip()
    if not slug or "/" in slug or ".." in slug:
        raise ValueError("slug required")
    branch = _proposal_branch(slug)
    if branch is None:
        raise ValueError(f"no open proposal branch for slug {slug!r} (propose_change first)")
    collection = (args.get("collection") or "posts").strip()
    if "/" in collection or ".." in collection:
        raise ValueError("invalid collection")

    tmp = ROOT / ".tenants" / f"revise-{slug}-{int(time.time() * 1000)}"
    tmp.parent.mkdir(parents=True, exist_ok=True)
    # origin/<branch> is the source of truth — a local ref can be stale
    # (behind a revision pushed by an earlier session).
    fres = _git("fetch", "origin",
                f"+refs/heads/{branch}:refs/remotes/origin/{branch}", worktree=ROOT)
    if fres.returncode != 0:
        raise ValueError(f"git fetch failed: {fres.stderr.strip()[:300]}")
    res = _git("worktree", "add", "--detach", str(tmp), f"origin/{branch}")
    if res.returncode != 0:
        raise ValueError(f"worktree add failed: {res.stderr.strip()[:300]}")
    try:
        item_dir = tmp / "content" / collection / slug
        if not item_dir.exists():
            raise ValueError(f"{collection}/{slug} not on {branch}")
        meta_path = item_dir / "meta.yaml"
        md_path = item_dir / "index.mdx"
        body_md = args.get("body")
        if body_md:
            md_path.write_text(body_md if body_md.endswith("\n") else body_md + "\n")
        agent_notes = args.get("agent_notes")
        if agent_notes is not None and not isinstance(agent_notes, str):
            agent_notes = json.dumps(agent_notes, ensure_ascii=False)
        fields = args.get("fields")
        if not isinstance(fields, dict):
            fields = None
        raw_title = (args.get("title") or "").strip() or None
        _apply_meta_edits(meta_path, title=raw_title,
                          description=args.get("description"),
                          agent_notes=agent_notes, fields=fields)
        _git("add", str(item_dir.relative_to(tmp)), worktree=tmp)
        env = {"GIT_AUTHOR_NAME": author_name, "GIT_AUTHOR_EMAIL": author_email,
               "GIT_COMMITTER_NAME": author_name, "GIT_COMMITTER_EMAIL": author_email}
        res = _git("commit", "-m", f"revise: {collection}/{slug} from review feedback",
                   env_extra=env, worktree=tmp)
        if res.returncode != 0 and "nothing to commit" not in (res.stdout + res.stderr):
            raise ValueError(f"git commit failed: {res.stderr.strip()[:300]}")
        push = _git("push", "origin", f"HEAD:refs/heads/{branch}", worktree=tmp)
        if push.returncode != 0:
            raise ValueError(f"git push failed: {push.stderr.strip()[:300]}")
        return {"revised": True, "branch": branch,
                "pr_url": _open_pr_for_branch(branch),
                "preview_command": (f"cd {tmp} && DRAFTS=1 cargo run -q -p build && "
                                    f"python3 -m http.server 8000 -d dist")}
    finally:
        _git("worktree", "remove", "--force", str(tmp))
        _git("worktree", "prune")
        shutil.rmtree(tmp, ignore_errors=True)


def tool_list_proposals(args: dict[str, Any]) -> dict[str, Any]:
    """The proposal queue: open PRs whose head branch is agent/*, with the
    draft-preview command for each. This is the human- and agent-readable
    review surface git was hiding."""
    res = subprocess.run(
        ["gh", "pr", "list", "--state", "open", "--json",
         "number,title,headRefName,url,author,createdAt,body"],
        cwd=str(ROOT), capture_output=True, text=True)
    if res.returncode != 0:
        raise ValueError(f"gh pr list failed: {res.stderr.strip()[:300]}")
    try:
        prs = json.loads(res.stdout or "[]")
    except ValueError:
        raise ValueError(f"gh pr list returned non-JSON: {res.stdout[:200]}")
    proposals = []
    for p in prs:
        head = str(p.get("headRefName") or "")
        if not head.startswith("agent/"):
            continue
        slug = re.sub(r"-r\d+$", "", head.removeprefix("agent/"))
        entry: dict[str, Any] = {
            "number": p.get("number"),
            "title": p.get("title"),
            "branch": head,
            "slug": slug,
            "url": p.get("url"),
            "author": (p.get("author") or {}).get("name"),
            "preview_command": f"cd {ROOT} && git fetch origin {head} && "
                               f"git switch {head} && DRAFTS=1 cargo run -q -p build && "
                               f"python3 -m http.server 8000 -d dist",
        }
        body = str(p.get("body") or "")
        m = re.search(r"^risk:\s*(\S+)", body, re.MULTILINE)
        entry["risk"] = m.group(1) if m else "unknown"
        entry["field_diff"] = _proposal_field_diff(head)
        proposals.append(entry)
    result: dict[str, Any] = {"proposals": proposals, "count": len(proposals)}
    if args.get("include_rejected"):
        result["recently_rejected"] = _recent_rejections()
    return result


def _proposal_field_diff(branch: str) -> list[dict[str, Any]]:
    """Field-level diff of meta.yaml between origin/main and the proposal
    branch — so a reviewer sees `tier: pro -> enterprise` without reading a
    unified diff. Empty when the item is new (nothing to compare against)."""
    fetch = _git("fetch", "origin", f"+refs/heads/{branch}:refs/remotes/origin/{branch}")
    if fetch.returncode != 0:
        return [{"error": fetch.stderr.strip()[:200]}]
    diff = _git("diff", "origin/main...origin/" + branch, "--", "content/*/meta.yaml")
    if diff.returncode != 0:
        return []
    out: list[dict[str, Any]] = []
    for line in diff.stdout.splitlines():
        if line.startswith(("--- ", "+++ ")) or "@@" in line:
            continue
        if line.startswith("-") and line[1:].strip() and ":" in line:
            k, _, v = line[1:].partition(":")
            out.append({"field": k.strip(), "from": v.strip()})
        elif line.startswith("+") and line[1:].strip() and ":" in line:
            k, _, v = line[1:].partition(":")
            for entry in out:
                if entry.get("field") == k.strip() and "to" not in entry:
                    entry["to"] = v.strip()
                    break
            else:
                out.append({"field": k.strip(), "added": v.strip()})
    return out


def _recent_rejections(limit: int = 10) -> list[dict[str, Any]]:
    """Rejections as agent-readable state: recently closed agent PRs whose
    last comment starts with REJECTED: <reason>."""
    res = subprocess.run(
        ["gh", "pr", "list", "--state", "closed", "--json",
         "number,title,headRefName,url,closedAt", "--limit", "30"],
        cwd=str(ROOT), capture_output=True, text=True)
    if res.returncode != 0:
        return []
    try:
        prs = json.loads(res.stdout or "[]")
    except ValueError:
        return []
    rejected = []
    for p in prs:
        head = str(p.get("headRefName") or "")
        if not head.startswith("agent/"):
            continue
        comments = subprocess.run(
            ["gh", "pr", "view", str(p["number"]), "--json", "comments",
             "--jq", ".comments[-1].body // \"\""],
            cwd=str(ROOT), capture_output=True, text=True)
        body = comments.stdout.strip()
        if not body.startswith("REJECTED:"):
            continue
        rejected.append({
            "number": p.get("number"),
            "slug": re.sub(r"-r\d+$", "", head.removeprefix("agent/")),
            "url": p.get("url"),
            "reason": body.removeprefix("REJECTED:").strip(),
            "closed_at": p.get("closedAt"),
        })
        if len(rejected) >= limit:
            break
    return rejected


def _ship_and_purge(result: dict[str, Any]) -> None:
    """Bring the shared tree to origin/main and run build + deploy + purge.
    One call = the whole 'approve/revert means live' tail, atomically from
    the caller's point of view."""
    _git("fetch", "origin")
    switch = _git("switch", "main")
    if switch.returncode != 0:
        switch = _git("switch", "-")
    dirty = _git("status", "--porcelain", "--untracked-files=no")
    if dirty.stdout.strip():
        result["deploy"] = ("merged, but the shared tree is dirty — run the "
                            "deploy manually after committing or stashing")
        result["dirty"] = dirty.stdout.strip()[:200]
        return
    pull = _git("pull", "--ff-only", "origin", "main")
    if pull.returncode != 0:
        raise ValueError(f"pull after merge failed: {pull.stderr.strip()[:300]}")
    deploy = subprocess.run(["bash", "ci/deploy.sh"], cwd=str(ROOT),
                            capture_output=True, text=True, timeout=600)
    if deploy.returncode != 0:
        raise ValueError(f"deploy failed: {(deploy.stdout + deploy.stderr)[-400:]}")
    result["deploy"] = "ok"
    purge = subprocess.run(["bash", "ci/purge.sh"], cwd=str(ROOT),
                           capture_output=True, text=True)
    result["purge"] = "ok" if purge.returncode == 0 else purge.stderr.strip()[:200]


def _approve_one(slug: str, approver_name: str, approver_email: str) -> dict[str, Any]:
    """Merge one proposal PR with the approver's identity on the record."""
    branch = _proposal_branch(slug)
    if branch is None:
        raise ValueError(f"no proposal branch for slug {slug!r}")
    pr_number = None
    res = subprocess.run(
        ["gh", "pr", "list", "--head", branch, "--state", "open",
         "--json", "number", "--limit", "1"],
        cwd=str(ROOT), capture_output=True, text=True)
    if res.returncode == 0:
        try:
            prs = json.loads(res.stdout or "[]")
            pr_number = prs[0]["number"] if prs else None
        except (ValueError, KeyError, IndexError):
            pr_number = None
    if pr_number is None:
        raise ValueError(f"no open PR on {branch} — nothing to approve")
    out: dict[str, Any] = {"slug": slug, "branch": branch, "pr": pr_number}
    comment = subprocess.run(
        ["gh", "pr", "comment", str(pr_number), "--body",
         f"Approved by {approver_name} <{approver_email}> via approve_proposal."],
        cwd=str(ROOT), capture_output=True, text=True)
    out["approval_recorded"] = comment.returncode == 0
    merge = subprocess.run(
        ["gh", "pr", "merge", str(pr_number), "--merge", "--delete-branch"],
        cwd=str(ROOT), capture_output=True, text=True)
    if merge.returncode != 0:
        raise ValueError(f"merge failed for {slug}: {merge.stderr.strip()[:300]}")
    out["merged"] = True
    return out


def tool_approve_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """Approve one proposal (slug) or a batch (slugs): merge the PR(s), then
    build + deploy + purge once, so 'approve' means live. The approver's
    identity is recorded as a comment on each PR — the audit receipt. Low-risk
    copy batches can ride one approval."""
    approver_name = (args.get("approver_name") or "").strip()
    approver_email = (args.get("approver_email") or "").strip()
    if not approver_name or not approver_email:
        raise ValueError("approver_name and approver_email are required (identity on the record)")
    slugs = args.get("slugs")
    if not slugs:
        slug = (args.get("slug") or "").strip()
        if not slug or "/" in slug or ".." in slug:
            raise ValueError("slug required")
        slugs = [slug]
    if not isinstance(slugs, list) or not all(isinstance(s, str) and s.strip() for s in slugs):
        raise ValueError("slugs must be a list of proposal slugs")
    slugs = [s.strip() for s in slugs]

    merged: list[dict[str, Any]] = []
    result: dict[str, Any] = {}
    try:
        for s in slugs:
            merged.append(_approve_one(s, approver_name, approver_email))
    finally:
        result["approved"] = merged
        result["count"] = len(merged)
    if not args.get("deploy", True):
        result["deploy"] = "skipped (deploy=false)"
        return result
    _ship_and_purge(result)
    return result


def tool_reject_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """Reject a proposal: record WHO and WHY on the PR (agent-readable state —
    the proposing agent can read the reason and revise instead of re-proposing
    blind), then close the PR and delete its branch."""
    slug = (args.get("slug") or "").strip()
    reason = (args.get("reason") or "").strip()
    reviewer_name = (args.get("reviewer_name") or "").strip()
    reviewer_email = (args.get("reviewer_email") or "").strip()
    if not slug or "/" in slug or ".." in slug:
        raise ValueError("slug required")
    if not reason:
        raise ValueError("reason required — a rejection without a reason teaches nothing")
    if not reviewer_name or not reviewer_email:
        raise ValueError("reviewer_name and reviewer_email are required (identity on the record)")
    branch = _proposal_branch(slug)
    if branch is None:
        raise ValueError(f"no proposal branch for slug {slug!r}")
    pr_number = None
    res = subprocess.run(
        ["gh", "pr", "list", "--head", branch, "--state", "open",
         "--json", "number", "--limit", "1"],
        cwd=str(ROOT), capture_output=True, text=True)
    if res.returncode == 0:
        try:
            prs = json.loads(res.stdout or "[]")
            pr_number = prs[0]["number"] if prs else None
        except (ValueError, KeyError, IndexError):
            pr_number = None
    if pr_number is None:
        raise ValueError(f"no open PR on {branch} — nothing to reject")
    comment = subprocess.run(
        ["gh", "pr", "comment", str(pr_number), "--body",
         f"REJECTED: {reason}\n\nBy {reviewer_name} <{reviewer_email}> via reject_proposal."],
        cwd=str(ROOT), capture_output=True, text=True)
    out: dict[str, Any] = {
        "slug": slug, "branch": branch, "pr": pr_number,
        "reason": reason, "rejected": True,
        "recorded": comment.returncode == 0,
    }
    close = subprocess.run(
        ["gh", "pr", "close", str(pr_number), "--delete-branch"],
        cwd=str(ROOT), capture_output=True, text=True)
    if close.returncode != 0:
        out["closed"] = False
        out["close_error"] = close.stderr.strip()[:300]
    else:
        out["closed"] = True
    return out


def tool_revert_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """One-action undo: revert the merge commit that shipped a proposal,
    rebuild, redeploy, purge. The revert itself is a commit — the ledger keeps
    both the mistake and the correction."""
    approver_name = (args.get("approver_name") or "").strip()
    approver_email = (args.get("approver_email") or "").strip()
    if not approver_name or not approver_email:
        raise ValueError("approver_name and approver_email are required (identity on the record)")
    slug = (args.get("slug") or "").strip()
    if not slug or "/" in slug or ".." in slug:
        raise ValueError("slug required")
    _git("fetch", "origin")
    log = _git("log", "origin/main", "--merges", "--format=%H %s", "--")
    needle = f"agent/{slug}"
    merge_sha = None
    merge_pr = None
    for line in log.stdout.splitlines():
        sha, _, subject = line.partition(" ")
        if needle in subject:
            m = re.search(r"#(\d+)", subject)
            merge_pr = int(m.group(1)) if m else None
            merge_sha = sha
            break
    if merge_sha is None:
        raise ValueError(f"no merge commit found for proposal {slug!r} on main")
    out: dict[str, Any] = {"slug": slug, "merge_commit": merge_sha, "pr": merge_pr}
    if merge_pr is not None:
        subprocess.run(
            ["gh", "pr", "comment", str(merge_pr), "--body",
             f"Reverted by {approver_name} <{approver_email}> via revert_proposal."],
            cwd=str(ROOT), capture_output=True, text=True)
    switch = _git("switch", "main")
    if switch.returncode != 0:
        _git("switch", "-")
    dirty = _git("status", "--porcelain", "--untracked-files=no")
    if dirty.stdout.strip():
        raise ValueError(f"shared tree is dirty: {dirty.stdout.strip()[:200]}")
    pull = _git("pull", "--ff-only", "origin", "main")
    if pull.returncode != 0:
        raise ValueError(f"pull failed: {pull.stderr.strip()[:300]}")
    revert = _git("revert", "--no-edit", "-m", "1", merge_sha)
    if revert.returncode != 0:
        raise ValueError(f"revert failed: {revert.stderr.strip()[:400]}")
    push = _git("push", "origin", "main")
    if push.returncode != 0:
        raise ValueError(f"push of revert failed: {push.stderr.strip()[:300]}")
    out["reverted"] = True
    if not args.get("deploy", True):
        out["deploy"] = "skipped (deploy=false)"
        return out
    _ship_and_purge(out)
    return out


def git_commit(message: str) -> dict[str, Any]:
    """Stage content/ and commit. No-ops when not a git repo."""
    if not (ROOT / ".git").exists():
        return {"committed": False, "reason": "not a git repository"}
    _git("add", "content")
    res = _git("commit", "-m", message)
    if res.returncode != 0:
        # nothing to commit is fine
        if "nothing to commit" in (res.stdout + res.stderr):
            return {"committed": False, "reason": "nothing to commit"}
        return {"committed": False, "reason": res.stderr.strip()[:300]}
    return {"committed": True, "message": message}


# ------------------------------------------------------------------ tools

def tool_list_posts(args: dict[str, Any]) -> dict[str, Any]:
    status = args.get("status", "all")
    posts = [_summary(_read_meta(s)) for s in _all_slugs()]
    if status == "published":
        posts = [p for p in posts if p["published"]]
    elif status == "draft":
        posts = [p for p in posts if not p["published"]]
    return {"count": len(posts), "status": status, "posts": posts}


def tool_get_post(args: dict[str, Any]) -> dict[str, Any]:
    slug = args["slug"]
    if not _post_dir(slug).is_dir():
        raise ValueError(f"post not found: {slug}")
    return _load_post(slug)


def tool_create_draft(args: dict[str, Any]) -> dict[str, Any]:
    slug = args["slug"]
    if not _slug_ok(slug):
        raise ValueError(f"invalid slug: {slug!r} (lowercase letters, digits, hyphens)")
    d = _post_dir(slug)
    if d.exists():
        raise ValueError(f"post already exists: {slug}")
    meta = {
        "title": args["title"],
        "slug": slug,
        "tags": args.get("tags", []),
        "lang": "en",
        "published": False,
        "date": _parse_date_or_today(),
        "description": args.get("description", ""),
    }
    d.mkdir(parents=True, exist_ok=True)
    _write_meta(slug, meta)
    (d / "index.mdx").write_text(args["body"] if args["body"].endswith("\n") else args["body"] + "\n")
    git_commit(f"draft: create {slug}")
    return {"created": True, "path": str((d / "meta.yaml").relative_to(ROOT)),
            "body_path": str((d / "index.mdx").relative_to(ROOT)), "published": False}


def tool_update_post(args: dict[str, Any]) -> dict[str, Any]:
    slug = args["slug"]
    # GATE: the published flag is the one field that changes what the world
    # sees without a build. It must go through the proposal gate
    # (propose_change + approve_proposal) like any other publish decision —
    # a direct flip here would be a side door around "approve means live".
    if "published" in args:
        raise ValueError(
            "update_post cannot flip `published` — publishing is a gated "
            "decision. Use propose_change (with published/fields) and then "
            "approve_proposal; that path builds, deploys and purges."
        )
    meta = _read_meta(slug)
    if not _post_dir(slug).is_dir():
        raise ValueError(f"post not found: {slug}")
    changes: list[dict[str, Any]] = []
    for field in ("title", "description", "tags"):
        if field in args:
            old = meta.get(field)
            new = args[field]
            if old != new:
                changes.append({"field": field, "from": old, "to": new})
                meta[field] = new
    _write_meta(slug, meta)
    if "body" in args:
        old_body = _read_body(slug)
        new_body = args["body"]
        if old_body != new_body:
            changes.append({"field": "body", "from_len": len(old_body), "to_len": len(new_body)})
            (_post_dir(slug) / "index.mdx").write_text(
                new_body if new_body.endswith("\n") else new_body + "\n")
    if changes:
        git_commit(f"update {slug}: {', '.join(c['field'] for c in changes)}")
    return {"slug": slug, "changes": changes}


def tool_build_site(_args: dict[str, Any]) -> dict[str, Any]:
    res = subprocess.run(TEMPLATES_FOR_BUILD, shell=True, cwd=ROOT,
                         capture_output=True, text=True, timeout=600)
    output = (res.stdout + res.stderr)[-2000:]
    if res.returncode != 0:
        raise RuntimeError(f"build failed (exit {res.returncode}): {output[-500:]}")
    return {"ok": True, "tail": output.strip().splitlines()[-5:]}


def tool_build_preview(args: dict[str, Any]) -> dict[str, Any]:
    """Build every theme (or a comma list) under /preview/<theme>/ without
    changing the live site. `themes`: 'all' or e.g. 'astra,neve'."""
    spec = str(args.get("themes", "all"))
    env = {**os.environ, "SITE_PREVIEW_THEMES": spec}
    res = subprocess.run(TEMPLATES_FOR_BUILD, shell=True, cwd=ROOT,
                         capture_output=True, text=True, timeout=600, env=env)
    output = (res.stdout + res.stderr)[-2000:]
    if res.returncode != 0:
        raise RuntimeError(f"preview build failed (exit {res.returncode}): {output[-500:]}")
    return {"ok": True, "urls": ["/preview/<theme>/", "/preview/"],
            "tail": output.strip().splitlines()[-5:]}


def tool_publish_status(_args: dict[str, Any]) -> dict[str, Any]:
    slugs = _all_slugs()
    metas = [_read_meta(s) for s in slugs]
    live = [m["slug"] for m in metas if m["published"]]
    pending = [m["slug"] for m in metas if not m["published"]]
    return {"live": {"count": len(live), "posts": live},
            "pending": {"count": len(pending), "posts": pending}}


def tool_add_media(args: dict[str, Any]) -> dict[str, Any]:
    import media as media_mod

    if not _post_dir(args["slug"]).is_dir():
        raise ValueError(f"post not found: {args['slug']} (create the draft first)")
    return media_mod.add_media(
        args["slug"], args["file_path"], args.get("alt", "")
    )


def tool_set_theme(args: dict[str, Any]) -> dict[str, Any]:
    theme = args["theme"]
    themes_dir = ROOT / "themes"
    available = sorted(p.name for p in themes_dir.iterdir() if p.is_dir()) if themes_dir.is_dir() else []
    if theme not in available:
        raise ValueError(f"unknown theme {theme!r}; available: {available}")
    site_cfg = ROOT / "site.yaml"
    if site_cfg.exists():
        lines = [ln for ln in site_cfg.read_text().splitlines()
                 if not ln.strip().startswith("theme:")]
    else:
        lines = []
    lines.append(f"theme: {theme}")
    site_cfg.write_text("\n".join(lines) + "\n")
    git_commit(f"theme: switch to {theme}")
    build = tool_build_site({})
    return {"theme": theme, "available": available, "build": build}


HANDLERS = {
    "list_posts": tool_list_posts,
    "get_post": tool_get_post,
    "create_draft": tool_create_draft,
    "update_post": tool_update_post,
    "propose_change": tool_propose_change,
    "propose_template": tool_propose_template,
    "revise_proposal": tool_revise_proposal,
    "list_proposals": tool_list_proposals,
    "approve_proposal": tool_approve_proposal,
    "reject_proposal": tool_reject_proposal,
    "revert_proposal": tool_revert_proposal,
    "build_site": tool_build_site,
    "build_preview": tool_build_preview,
    "add_media": tool_add_media,
    "set_theme": tool_set_theme,
    "publish_status": tool_publish_status,
}


# --------------------------------------------------------------- protocol

def handle_request(req: dict[str, Any]) -> dict[str, Any] | None:
    method = req.get("method")
    req_id = req.get("id")
    if method == "initialize":
        return {"jsonrpc": "2.0", "id": req_id, "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "post-cms-mcp", "version": "0.1.0"},
        }}
    if method == "notifications/initialized":
        return None
    if method == "ping":
        return {"jsonrpc": "2.0", "id": req_id, "result": {}}
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}}
    if method == "tools/call":
        params = req.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        handler = HANDLERS.get(name)
        if handler is None:
            return {"jsonrpc": "2.0", "id": req_id, "error": {
                "code": -32601, "message": f"unknown tool: {name}"}}
        try:
            result = handler(args)
        except Exception as exc:  # noqa: BLE001 — surfaced as tool error content
            return {"jsonrpc": "2.0", "id": req_id, "result": {
                "content": [{"type": "text", "text": f"error: {exc}"}],
                "isError": True}}
        return {"jsonrpc": "2.0", "id": req_id, "result": {
            "content": [{"type": "text", "text": json.dumps(result, indent=2,
                                                            ensure_ascii=False)}],
            "isError": False}}
    if req_id is not None:
        return {"jsonrpc": "2.0", "id": req_id, "error": {
            "code": -32601, "message": f"method not found: {method}"}}
    return None


def serve() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            sys.stdout.write(json.dumps({
                "jsonrpc": "2.0", "id": None,
                "error": {"code": -32700, "message": "parse error"}}) + "\n")
            sys.stdout.flush()
            continue
        resp = handle_request(req)
        if resp is not None:
            sys.stdout.write(json.dumps(resp, ensure_ascii=False) + "\n")
            sys.stdout.flush()
        if req.get("method") == "exit":
            break


# -------------------------------------------------------------- self-test

def _rpc(req: dict[str, Any]) -> dict[str, Any] | None:
    return handle_request(req)


def self_test() -> int:
    global CONTENT, ROOT  # noqa: PLW0603
    import shutil
    import tempfile

    print("== post-cms mcp self-test ==")
    failures = 0

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal failures
        print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" — {detail}" if detail and not cond else ""))
        if not cond:
            failures += 1

    # 1. initialize handshake
    r = _rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    check("initialize", r is not None and r["result"]["serverInfo"]["name"] == "post-cms-mcp")

    # 2. tools/list
    r = _rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    names = {t["name"] for t in r["result"]["tools"]}
    check("tools/list", names == set(HANDLERS), str(names))

    # 3. list_posts
    r = _rpc({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
              "params": {"name": "list_posts", "arguments": {"status": "published"}}})
    payload = json.loads(r["result"]["content"][0]["text"])
    check("list_posts", payload["count"] >= 1, json.dumps(payload)[:200])

    # 4. get_post
    slug = payload["posts"][0]["slug"] if payload["posts"] else "welcome"
    r = _rpc({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
              "params": {"name": "get_post", "arguments": {"slug": slug}}})
    got = json.loads(r["result"]["content"][0]["text"])
    check("get_post", got["slug"] == slug and "body" in got)

    # 5-7. create_draft / list / build in an isolated copy so the repo stays clean
    tmp = Path(tempfile.mkdtemp(prefix="post-cms-selftest-"))
    saved_content, saved_root = CONTENT, ROOT
    shutil.copytree(saved_content, tmp / "content")
    CONTENT = tmp / "content"
    ROOT = tmp
    try:
        r = _rpc({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
            "name": "create_draft", "arguments": {
                "slug": "self-test-draft", "title": "Self Test Draft",
                "body": "Hello from the self-test.",
                "tags": ["test"], "description": "temporary draft"}}})
        created = json.loads(r["result"]["content"][0]["text"])
        check("create_draft", created.get("created") is True
              and (CONTENT / "self-test-draft" / "meta.yaml").exists())

        r = _rpc({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {
            "name": "list_posts", "arguments": {"status": "draft"}}})
        drafts = json.loads(r["result"]["content"][0]["text"])
        check("create_draft appears in list_posts(draft)",
              any(p["slug"] == "self-test-draft" for p in drafts["posts"]))

        # update_post: flip published, then verify it lands in live list
        r = _rpc({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
            "name": "update_post", "arguments": {
                "slug": "self-test-draft", "published": True}}})
        upd = json.loads(r["result"]["content"][0]["text"])
        check("update_post diff", any(c["field"] == "published" for c in upd["changes"]))

        r = _rpc({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {
            "name": "publish_status"}})
        status = json.loads(r["result"]["content"][0]["text"])
        check("publish_status", "self-test-draft" in status["live"]["posts"])
    finally:
        CONTENT, ROOT = saved_content, saved_root
        shutil.rmtree(tmp, ignore_errors=True)

    # 8. build_site — real cargo build against the actual repo
    r = _rpc({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
              "params": {"name": "build_site", "arguments": {}}})
    if r["result"].get("isError"):
        check("build_site", False, r["result"]["content"][0]["text"][:300])
    else:
        tail = json.loads(r["result"]["content"][0]["text"])
        check("build_site", tail.get("ok") is True, str(tail)[:200])

    # 9. unknown tool → JSON-RPC error
    r = _rpc({"jsonrpc": "2.0", "id": 10, "method": "tools/call",
              "params": {"name": "nope", "arguments": {}}})
    check("unknown tool errors", r.get("error", {}).get("code") == -32601)

    print(f"== {'ALL PASS' if failures == 0 else str(failures) + ' FAILURE(S)'} ==")
    return 1 if failures else 0


if __name__ == "__main__":
    os.chdir(ROOT)
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    serve()

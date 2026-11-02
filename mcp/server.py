#!/usr/bin/env python3
"""Post-CMS MCP tool server.

Speaks MCP (JSON-RPC 2.0) over stdio using only the Python standard library.
Every mutation is written to content/posts/<slug>/ and committed to git
(silently skipped when the repo has no .git directory).

Run:   python3 mcp/server.py            # JSON-RPC server on stdio
       python3 mcp/server.py --self-test
"""

from __future__ import annotations

import contextlib
import contextvars
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
CONTENT = ROOT / "content" / "posts"
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
        "description": "Rebuild the static site (cargo run -p build --bin build-site); returns output tail.",
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
                "merge": {"type": "boolean", "description": "Merge the PR immediately (default false). An approval: runs the approve_proposal gate, so it is refused where the author may not approve their own change (over HTTP by default)."},
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
            "Approve one proposal (slug) or a batch (slugs): merges, then "
            "builds, deploys and purges once — 'approve' means live. Approver "
            "identity is taken from the authenticated GitHub session (`gh api "
            "user`); free-text approver_name/approver_email are optional and "
            "must MATCH that session identity. Refused without a gh session."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Slug of the proposed item (or use slugs for a batch)."},
                "slugs": {"type": "array", "items": {"type": "string"},
                          "description": "Batch of low-risk proposals to approve in one action (one build+deploy)."},
                "approver_name": {"type": "string", "description": "Optional; must match the gh session identity."},
                "approver_email": {"type": "string", "description": "Optional; must match the gh session identity."},
                "deploy": {"type": "boolean", "description": "Build+deploy+purge after merge (default true)."},
            },
            "required": [],
        },
    },
    {
        "name": "reject_proposal",
        "description": (
            "Reject a proposal: records WHO and WHY as a REJECTED: comment on "
            "the PR (agent-readable state), then closes the PR. Reviewer "
            "identity comes from the authenticated GitHub session; free-text "
            "reviewer_name/reviewer_email must MATCH it."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string", "description": "Slug of the proposed item."},
                "reason": {"type": "string", "description": "Why — the proposing agent reads this to revise."},
                "reviewer_name": {"type": "string", "description": "Optional; must match the gh session identity."},
                "reviewer_email": {"type": "string", "description": "Optional; must match the gh session identity."},
            },
            "required": ["slug", "reason"],
        },
    },
    {
        "name": "revert_proposal",
        "description": (
            "One-action undo: reverts the merge commit that shipped a proposal, "
            "then rebuilds, redeploys and purges. Records the revert on the PR. "
            "Needs approver rights (MCP_ALLOW_APPROVE, MCP_APPROVERS); the "
            "change's own author cannot revert it over HTTP."
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
                "merge": {"type": "boolean", "description": "Merge the PR immediately (default false). An approval: runs the approve_proposal gate, so it is refused where the author may not approve their own change (over HTTP by default)."},
            },
            "required": ["name", "body", "author_name", "author_email"],
        },
    },
    {
        "name": "propose_plugin",
        "description": (
            "Propose a client-side plugin (analytics pixel, ad tag) via the "
            "review flow: writes plugins/<name>/snippet.html + the site.yaml "
            "plugins: entry, validates with a real build, opens a PR with "
            "risk: tracking. remove=true deletes an existing plugin. The PR "
            "diff IS the pixel being added."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Plugin name [A-Za-z0-9-] — becomes plugins/<name>/."},
                "snippet": {"type": "string", "description": "Raw snippet HTML (mounted verbatim). Required unless remove=true."},
                "mounts": {"type": "array", "items": {"type": "string", "enum": ["head", "body_end"]}, "description": "Where the snippet mounts (default [head])."},
                "csp_script_src": {"type": "array", "items": {"type": "string"}, "description": "Script-src hosts the snippet loads from (feeds the CSP meta tag)."},
                "require_consent": {"type": "boolean", "description": "Run only after visitor consent (pcms_consent=granted / pcms:grant event)."},
                "remove": {"type": "boolean", "description": "Remove the plugin instead of adding it."},
                "tenant": {"type": "string", "description": "Isolated working copy (.tenants/<tenant>) for multi-tenant simulation."},
                "author_name": {"type": "string", "description": "Git author name for the commit."},
                "author_email": {"type": "string", "description": "Git author email for the commit."},
                "merge": {"type": "boolean", "description": "Merge the PR immediately (default false). An approval: runs the approve_proposal gate, so it is refused where the author may not approve their own change (over HTTP by default)."},
                "approver_name": {"type": "string", "description": "Required with merge=true."},
                "approver_email": {"type": "string", "description": "Required with merge=true."},
            },
            "required": ["name", "author_name", "author_email"],
        },
    },
    {
        "name": "publish_status",
        "description": "Counts and lists of live vs pending (draft) posts.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


# ---------------------------------------------------------------- content io

# ---- worktree file access: never follow a symlink ----------------------------
#
# A proposal branch's content is controlled by whoever can push it. The
# server therefore reads and writes worktree files through directory fds,
# component by component, with O_NOFOLLOW: a committed symlink anywhere on
# the path makes the operation refuse instead of escaping the worktree.

_ID_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}")


def _check_id(value: Any, what: str) -> str:
    """Slugs, collections and proposal IDs: ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$."""
    v = value.strip() if isinstance(value, str) else ""
    if not _ID_RE.fullmatch(v):
        raise ValueError(f"{what} must match ^[A-Za-z0-9][A-Za-z0-9_-]{{0,63}}$")
    return v


def _rel_parts(root: Path, path: Path | str) -> list[str]:
    p = Path(path)
    rel = p.relative_to(root) if p.is_absolute() else p
    parts = list(rel.parts)
    if not parts or any(x in ("", ".", "..") or "/" in x for x in parts):
        raise ValueError(f"unsafe worktree path {str(rel)!r}")
    return parts


def _open_dir_nofollow(root: Path, parts: list[str], create: bool) -> int:
    fd = os.open(str(root), os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts:
            try:
                nfd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            except FileNotFoundError:
                if not create:
                    raise
                os.mkdir(part, 0o755, dir_fd=fd)
                nfd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = nfd
        return fd
    except OSError:
        os.close(fd)
        raise


def _wt_write(root: Path, path: Path | str, data: str) -> None:
    """Write a worktree file without following any symlink on its path."""
    parts = _rel_parts(root, path)
    try:
        dfd = _open_dir_nofollow(root, parts[:-1], create=True)
        try:
            fd = os.open(parts[-1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW,
                         0o644, dir_fd=dfd)
        finally:
            os.close(dfd)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(data)
    except OSError as exc:
        raise ValueError(f"refusing to write {'/'.join(parts)}: {exc.strerror or exc} "
                         "(symlinks are never followed in a worktree)") from exc


def _wt_read(root: Path, path: Path | str) -> str | None:
    """Read a worktree file without following symlinks; None when missing."""
    parts = _rel_parts(root, path)
    try:
        dfd = _open_dir_nofollow(root, parts[:-1], create=False)
        try:
            fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW, dir_fd=dfd)
        finally:
            os.close(dfd)
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise ValueError(f"refusing to read {'/'.join(parts)}: {exc.strerror or exc} "
                         "(symlinks are never followed in a worktree)") from exc
    with os.fdopen(fd, encoding="utf-8") as fh:
        return fh.read()


def _wt_is_dir(root: Path, path: Path | str) -> bool:
    """True for a real directory reached without following symlinks; raises
    when a symlink is in the way."""
    parts = _rel_parts(root, path)
    try:
        os.close(_open_dir_nofollow(root, parts, create=False))
        return True
    except FileNotFoundError:
        return False
    except OSError as exc:
        raise ValueError(f"refusing {'/'.join(parts)}: {exc.strerror or exc} "
                         "(symlinks are never followed in a worktree)") from exc


def _refuse_symlinks_in_tree(rev: str) -> None:
    """Refuse a tree that contains any symlink (mode 120000) anywhere."""
    res = _git("ls-tree", "-r", "-z", "--full-tree", rev)
    if res.returncode != 0:
        raise ValueError(f"cannot list the tree of {rev}: {res.stderr.strip()[:200]}")
    for rec in res.stdout.split("\x00"):
        if not rec:
            continue
        meta, _, name = rec.partition("\t")
        if meta.split(" ", 1)[0] == "120000":
            raise ValueError(f"{rev} contains a symlink ({name!r}); refusing to work on it")


def _slug_ok(slug: str) -> bool:
    return bool(slug) and all(c.isalnum() or c == "-" for c in slug) and slug == slug.lower()


def _post_dir(slug: str) -> Path:
    return CONTENT / _check_id(slug, "slug")


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


def tenant_worktree_name_ok(tenant: str) -> None:
    if not tenant or "/" in tenant or ".." in tenant or tenant.startswith("."):
        raise ValueError("invalid tenant name")


def tenant_worktree(tenant: str) -> Path:
    """Per-tenant working copy (simulates multi-tenant locally).

    Each tenant gets a persistent git worktree under .tenants/<tenant>/
    with its own index and checked-out files, so two tenants can propose
    changes concurrently without touching each other or the shared tree.
    The worktree is refreshed to the default branch head on every use.
    """
    tenant_worktree_name_ok(tenant)
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


# Builds render templates and content, and the template engine can read the
# process environment (get_env) and echo it into an error (throw). So every
# build runs with a cleared environment plus this allowlist: no tokens, no
# server secrets, HOME pointed at a scratch dir. Error output returned to a
# caller is sanitized and truncated.
BUILD_CMD = ["cargo", "run", "-q", "-p", "build", "--bin", "build-site"]
BUILD_SHELL = "cargo run -q -p build --bin build-site"
_BUILD_ENV_PASS = ("PATH", "LANG", "LC_ALL", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME",
                   "RUSTUP_TOOLCHAIN", "CARGO_BUILD_JOBS", "CARGO_TARGET_DIR",
                   "SITE_BASE_URL", "SITE_THEME", "IMAGES_OFFLINE", "IMAGES_CACHE_DIR",
                   "SANITY_OFFLINE")
_SCRATCH_HOME: Path | None = None


def _scratch_home() -> str:
    global _SCRATCH_HOME
    if _SCRATCH_HOME is None or not _SCRATCH_HOME.is_dir():
        _SCRATCH_HOME = Path(tempfile.mkdtemp(prefix="pcms-build-home-"))
    return str(_SCRATCH_HOME)


def _build_env(site: Path | None = None, **extra: str) -> dict[str, str]:
    """A cleared environment for cargo builds: only _BUILD_ENV_PASS from the
    server's environment, HOME at a scratch dir (with CARGO_HOME and
    RUSTUP_HOME kept on the real toolchain), SITE_ROOT set explicitly."""
    real_home = Path(os.environ.get("HOME") or str(Path.home()))
    env = {k: os.environ[k] for k in _BUILD_ENV_PASS if os.environ.get(k)}
    env.setdefault("PATH", "/usr/local/bin:/usr/bin:/bin")
    env.setdefault("CARGO_HOME", str(real_home / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(real_home / ".rustup"))
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / "target"))
    env["HOME"] = _scratch_home()
    if site is not None:
        env["SITE_ROOT"] = str(site)
    env.update(extra)
    return env


def _run_build(site: Path, **extra: str) -> subprocess.CompletedProcess:
    return subprocess.run(BUILD_CMD, cwd=str(site), capture_output=True, text=True,
                          env=_build_env(site, **extra), timeout=1200)


_CTRL_RE = re.compile(r"[\x00-\x08\x0b-\x1f\x7f]|\x1b\[[0-9;?]*[A-Za-z]")


def _build_error_tail(res: subprocess.CompletedProcess, lines: int = 15,
                      limit: int = 2000) -> str:
    """Last lines of a failed build's output, safe to return to a caller:
    control and escape sequences stripped, any value of the server's own
    environment redacted, truncated."""
    text = _CTRL_RE.sub("", (res.stderr or "") + (res.stdout or ""))
    for k, v in os.environ.items():
        if v and len(v) >= 8 and k not in ("PATH", "HOME", "PWD", "SHELL", "TMPDIR"):
            text = text.replace(v, "[redacted]")
    tail = "\n".join(text.strip().splitlines()[-lines:])
    return tail[-limit:]


def _open_pr(wt: Path, branch: str, title: str, pr_body: str, head: str | None = None):
    """Open a PR. head: source branch name, needed when the worktree is
    detached (throwaway worktrees) so gh can name the head ref."""
    cmd = ["gh", "pr", "create", "--title", title, "--body", pr_body]
    if head:
        cmd += ["--head", head]
    pr = subprocess.run(cmd, cwd=str(wt), capture_output=True, text=True)
    if pr.returncode != 0:
        return False, pr.stderr.strip()[:300]
    return True, pr.stdout.strip().splitlines()[-1]


def _artifact_payload(html_path: str | None) -> dict[str, Any]:
    """Artifact as CONTENT, not a filesystem path: over the hosted HTTP
    transport the reviewer's chat client cannot read the MCP server's /tmp,
    so the rendered HTML travels in the tool result itself (capped)."""
    payload: dict[str, Any] = {"path": html_path}
    if not html_path:
        return payload
    try:
        html = Path(html_path).read_text()
    except OSError:
        return payload
    if len(html) > 400_000:
        html = html[:400_000] + "\n<!-- truncated (artifact path has the full page) -->"
    payload["html"] = html
    return payload


def _save_artifact(name: str, html: str) -> str | None:
    out_dir = Path(tempfile.gettempdir()) / "pcms-artifacts"
    out_dir.mkdir(exist_ok=True)
    out = out_dir / name
    out.write_text(html)
    return str(out)


def _render_preview_artifact(wt: Path, collection: str, slug: str) -> str | None:
    """Per-PR preview as a chat artifact, not a hosted URL: DRAFTS=1 build in
    the proposal worktree renders the item AS IT WILL APPEAR (site template,
    draft banner, agent_notes), and the rendered HTML is copied to a stable
    path the agent attaches in the chat window. Returns the artifact path or
    None when the item did not render (never blocks the proposal)."""
    build = _run_build(wt, DRAFTS="1")
    if build.returncode != 0:
        return None
    try:
        html = _wt_read(wt, f"dist/preview/drafts/{collection}/{slug}/index.html")
    except ValueError:
        return None
    if html is None:
        return None
    return _save_artifact(f"{collection}-{slug}.html", html)


# ---- throwaway worktrees: capped, swept ----------------------------------

_WORK_IN_USE: set[str] = set()
_WORK_LOCK = threading.Lock()


def _max_worktrees() -> int:
    try:
        return max(1, int(os.environ.get("PCMS_MAX_WORKTREES", "4")))
    except ValueError:
        return 4


def _add_work_worktree(kind: str, rev: str, fetch: list[str]) -> Path:
    """Create a throwaway worktree of `rev` under .tenants/.work-<kind>-<pid>-<id>
    after fetching `fetch` refspecs. Refuses with a busy error when
    PCMS_MAX_WORKTREES (default 4) are already in use by this process."""
    with _WORK_LOCK:
        if len(_WORK_IN_USE) >= _max_worktrees():
            raise ValueError(f"server busy: {len(_WORK_IN_USE)} proposal worktrees in "
                             "use (PCMS_MAX_WORKTREES); retry shortly")
        tmp = ROOT / ".tenants" / f".work-{kind}-{os.getpid()}-{uuid.uuid4().hex[:12]}"
        _WORK_IN_USE.add(str(tmp))
    try:
        tmp.parent.mkdir(parents=True, exist_ok=True)
        with _flock("repo"):
            for spec in fetch:
                f = _git("fetch", "--quiet", "origin", spec)
                if f.returncode != 0:
                    raise ValueError(f"git fetch failed: {f.stderr.strip()[:300]}")
            res = _git("worktree", "add", "--detach", str(tmp), rev)
        if res.returncode != 0:
            raise ValueError(f"worktree add failed: {res.stderr.strip()[:300]}")
        return tmp
    except BaseException:
        _remove_work_worktree(tmp)
        raise


def _remove_work_worktree(tmp: Path) -> None:
    try:
        with _flock("repo"):
            _git("worktree", "remove", "--force", str(tmp))
            _git("worktree", "prune")
        shutil.rmtree(tmp, ignore_errors=True)
    finally:
        with _WORK_LOCK:
            _WORK_IN_USE.discard(str(tmp))


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def sweep_stale_worktrees() -> list[str]:
    """Remove .tenants/.work-* left by a server that died (its pid is gone)
    or by this process but no longer in use. Run at startup and on a timer."""
    base = ROOT / ".tenants"
    removed: list[str] = []
    if not base.is_dir():
        return removed
    for p in base.iterdir():
        m = re.fullmatch(r"\.work-[a-z]+-(\d+)-[0-9a-f]{12}", p.name)
        if not m or p.is_symlink():
            continue
        pid = int(m.group(1))
        with _WORK_LOCK:
            busy = str(p) in _WORK_IN_USE
        if busy or (pid != os.getpid() and _pid_alive(pid)):
            continue
        with _flock("repo"):
            _git("worktree", "remove", "--force", str(p))
        shutil.rmtree(p, ignore_errors=True)
        removed.append(p.name)
    with _flock("repo"):
        _git("worktree", "prune")
    return removed


def start_worktree_sweeper(interval: float = 600.0) -> None:
    """Sweep now, then every `interval` seconds on a daemon thread."""
    def tick() -> None:
        try:
            sweep_stale_worktrees()
        except Exception as exc:  # never let the sweeper kill the server
            print(f"post-cms-mcp: worktree sweep failed: {exc}", file=sys.stderr)
        t = threading.Timer(interval, tick)
        t.daemon = True
        t.start()
    tick()


# ---- proposal workspaces ---------------------------------------------------
#
# Over HTTP every proposal runs in its own throwaway worktree created from
# origin/main and deleted afterwards: no two callers ever share an index, so
# one caller's staged files can never ride in another caller's commit. On
# stdio (one local user) proposals keep using the shared checkout or a
# tenant worktree, serialized with a lock. Commits always name their own
# paths, and the ledger refuses a commit that touches anything else.


@contextlib.contextmanager
def _flock(name: str):
    """Cross-thread and cross-process lock on <repo>/.git/pcms-<name>.lock.
    Not re-entrant: never nest the same name."""
    import fcntl
    path = ROOT / ".git" / f"pcms-{name}.lock"
    fd = os.open(str(path), os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


class _ProposalWorkspace:
    """Where one proposal call writes and commits. Opened lazily (open())
    so identity and gate checks still run before any side effect; closed by
    the tool wrapper whatever happens."""

    def __init__(self, args: dict[str, Any], kind: str) -> None:
        self.kind = kind
        self.requested_tenant = (args.get("tenant") or "").strip()
        self.http = os.environ.get("PCMS_TRANSPORT") == "http"
        self.throwaway = self.http or (kind == "plugin" and not self.requested_tenant)
        self.tenant = "" if self.throwaway else self.requested_tenant
        self.path: Path | None = None
        self._lock: Any = None

    def open(self) -> Path:
        if self.path is not None:
            return self.path
        if self.throwaway:
            self.path = _add_work_worktree(
                self.kind, "origin/main", ["+refs/heads/main:refs/remotes/origin/main"])
        else:
            if self.tenant:
                tenant_worktree_name_ok(self.tenant)
            key = "tenant-" + self.tenant if self.tenant else "root"
            self._lock = _flock(key)
            self._lock.__enter__()
            self.path = tenant_worktree(self.tenant) if self.tenant else ROOT
        return self.path

    def close(self) -> None:
        if self.throwaway and self.path is not None:
            _remove_work_worktree(self.path)
        if self._lock is not None:
            self._lock.__exit__(None, None, None)
            self._lock = None
        self.path = None

    def note(self, result: dict[str, Any]) -> None:
        if self.http and self.requested_tenant:
            result["tenant_ignored"] = ("over HTTP every proposal runs in its own "
                                        "fresh worktree from origin/main")


def _in_workspace(kind: str, body: Any, args: dict[str, Any]) -> dict[str, Any]:
    if not (ROOT / ".git").exists():
        raise ValueError("not a git repository")
    ws = _ProposalWorkspace(args, kind)
    try:
        result = body(args, ws)
        ws.note(result)
        return result
    finally:
        ws.close()


def _commit_own_paths(wt: Path, paths: list[str], message: str,
                      env: dict[str, str], nothing: str | None) -> str | None:
    """Stage and commit exactly `paths` (pathspec commit: anything else
    staged in this index stays out of the commit). On any failure our paths
    are unstaged again, so they cannot ride in a later commit. Returns the
    commit's parent sha, or None when nothing changed and `nothing` is None
    (otherwise raises `nothing`)."""
    before = _git("rev-parse", "HEAD", worktree=wt).stdout.strip()
    try:
        present = [p for p in paths if (wt / p).exists()]
        if present:
            add = _git("add", "-A", "--", *present, worktree=wt)
            if add.returncode != 0:
                raise ValueError(f"git add failed: {add.stderr.strip()[:300]}")
        if _git("diff", "--cached", "--quiet", "--", *paths, worktree=wt).returncode == 0:
            if nothing is None:
                return None
            raise ValueError(nothing)
        res = _git("commit", "-m", message, "--", *paths, env_extra=env, worktree=wt)
        if res.returncode != 0:
            raise ValueError(f"git commit failed: {res.stderr.strip()[:300]}")
    except Exception:
        _git("reset", "-q", "--", *paths, worktree=wt)
        raise
    return before


def tool_propose_template(args: dict[str, Any]) -> dict[str, Any]:
    return _in_workspace("template", _propose_template, args)


def _propose_template(args: dict[str, Any], ws: _ProposalWorkspace) -> dict[str, Any]:
    """Create a Tera template via the review flow.

    Writes templates/<name>.html on an agent/template-<name> branch,
    validates it by running a real build (a template that does not compile
    fails before any PR is opened), pushes, and opens a PR with a preview
    command. Collections select templates via schema.yaml item_template /
    index_template.
    """
    if not (ROOT / ".git").exists():
        raise ValueError("not a git repository")
    # Author identity comes from the authenticated session, not from a
    # typed name: an agent could otherwise propose as "Alice", approve as
    # itself, and launder self-approval past the distinct-approver check.
    # Free-text author_name/email stay OPTIONAL but must MATCH the session
    # (same contract as approve_proposal).
    author_i = _check_declared_identity(
        (args.get("author_name") or "").strip(),
        (args.get("author_email") or "").strip())
    author_name = author_i["name"] or author_i["login"]
    author_email = author_i["email"] or f"{author_i['login']}@users.noreply.github.com"
    # merge=true is an approval of what this session just authored: it
    # runs the full publish gate (MCP_ALLOW_APPROVE, approver rights,
    # distinct approver) BEFORE any side effect, so a refused merge
    # leaves no pushed branch or open PR behind.
    apn = ape = apn_src = ""
    apn_i: dict[str, Any] = {}
    if args.get("merge"):
        apn, ape, apn_src, apn_i = _merge_now_gate(args, "propose_template", author_i,
                                                   author_name, author_email)
    _stable_id(author_i)  # the ledger needs it; fail before any side effect
    name = (args.get("name") or "").strip()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", name):
        raise ValueError("template name must match [A-Za-z0-9][A-Za-z0-9_-]*")
    body = args.get("body")
    if not body or not body.strip():
        raise ValueError("body (HTML) is required")
    if "{% block content %}" not in body:
        raise ValueError("template must extend a base and define a {% block content %}")
    wt = ws.open()
    tenant = ws.tenant

    tpl_rel = Path("templates") / f"{name}.html"
    tpl_path = wt / tpl_rel
    if not _wt_is_dir(wt, "templates"):
        raise ValueError(f"templates/ directory not found in {'worktree' if tenant else 'repo'}")
    existed = _wt_read(wt, tpl_rel) is not None
    _wt_write(wt, tpl_rel, body if body.endswith("\n") else body + "\n")

    try:
        # Validation: a real build compiles every template in templates/.
        build = _run_build(wt)
        if build.returncode != 0:
            tail = _build_error_tail(build)
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
        own = [str(tpl_rel)]
        before = _commit_own_paths(wt, own, f"propose: {verb} template {name}", commit_env,
                                   "template content unchanged — nothing to propose")
        assert before is not None
        _ledger_record_commit("propose_template", branch, wt, before, author_i, own)
        push = _git("push", "-u", "origin", branch, worktree=wt)
        if push.returncode != 0:
            _reset_worktree(wt, [] if existed else [tpl_path])
            raise ValueError(f"git push failed: {push.stderr.strip()[:300]}")

        root_arg = "" if tenant else "  # or pass the same tenant to render from its worktree"
        if ws.throwaway:
            preview_cmd = (f"cd {shlex.quote(str(ROOT))} && git fetch origin "
                           f"{shlex.quote(branch)} && git switch {shlex.quote(branch)} && "
                           f"cargo run -q -p build --bin build-site && python3 -m http.server 8000 -d dist")
        else:
            preview_cmd = (f"cd {shlex.quote(str(wt))} && cargo run -q -p build --bin build-site && "
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
        if ok:
            # The template source IS the diff under review — it travels in
            # the result so remote reviewers see it without the server fs.
            try:
                result["template_artifact"] = _artifact_payload(
                    _save_artifact(f"template-{name}.html",
                                   _wt_read(wt, tpl_rel) or ""))
            except OSError:
                pass
        if not ok:
            result["pr_error"] = pr_or_err
        if ok and args.get("merge"):
            # Gate already passed up front; record the approval, merge.
            # Pinned to the commit made here; the approval note is posted
            # only after that merge succeeded.
            merge = _merge_pinned(branch, _git("rev-parse", "HEAD", worktree=wt).stdout.strip())
            result["merged"] = merge.returncode == 0
            if merge.returncode != 0:
                result["merge_error"] = merge.stderr.strip()[:300]
            else:
                comment = subprocess.run(
                    ["gh", "pr", "comment", branch, "--body",
                     f"Approved by {apn} <{ape}> via propose_template(merge=true) "
                     f"[identity: {apn_src}]"],
                    cwd=str(wt), capture_output=True, text=True)
                result["approval_recorded"] = comment.returncode == 0
                _ledger_record_merge("propose_template", branch, branch, wt, apn_i)
                result["deploy"] = ("skipped: propose_change does not deploy; "
                                    "use approve_proposal for build+deploy+purge")
        _reset_worktree(wt, [])
        return result
    except Exception:
        _reset_worktree(wt, [] if existed else [tpl_path])
        raise

def tool_propose_change(args: dict[str, Any]) -> dict[str, Any]:
    return _in_workspace("change", _propose_change, args)


def _propose_change(args: dict[str, Any], ws: _ProposalWorkspace) -> dict[str, Any]:
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
    # Author identity comes from the authenticated session, not from a
    # typed name: an agent could otherwise propose as "Alice", approve as
    # itself, and launder self-approval past the distinct-approver check.
    # Free-text author_name/email stay OPTIONAL but must MATCH the session
    # (same contract as approve_proposal).
    author_i = _check_declared_identity(
        (args.get("author_name") or "").strip(),
        (args.get("author_email") or "").strip())
    author_name = author_i["name"] or author_i["login"]
    author_email = author_i["email"] or f"{author_i['login']}@users.noreply.github.com"
    # merge=true is an approval of what this session just authored: it
    # runs the full publish gate (MCP_ALLOW_APPROVE, approver rights,
    # distinct approver) BEFORE any side effect, so a refused merge
    # leaves no pushed branch or open PR behind.
    apn = ape = apn_src = ""
    apn_i: dict[str, Any] = {}
    if args.get("merge"):
        apn, ape, apn_src, apn_i = _merge_now_gate(args, "propose_change", author_i,
                                                   author_name, author_email)
    _stable_id(author_i)  # the ledger needs it; fail before any side effect
    slug = _check_id(args.get("slug"), "slug")
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
    collection = _check_id(args.get("collection") or "posts", "collection")
    wt = ws.open()
    tenant = ws.tenant

    # Resolve target files. Existing item → update in place; new → draft item.
    item_dir = wt / "content" / collection / slug
    meta_path = item_dir / "meta.yaml"
    md_path = item_dir / "index.mdx"
    is_new = not _wt_is_dir(wt, item_dir)

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

    # Files this proposal created or overwrote in wt. On ANY failure they are
    # removed/restored — a rejected proposal must leave the tree exactly as
    # it found it (the shared tree is not a scratchpad).
    created: list[Path] = []
    overwritten: dict[Path, str | None] = {}

    try:
        if is_new:
            created.append(meta_path)
            if body_md:
                created.append(md_path)
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
            _wt_write(wt, meta_path, "\n".join(lines) + "\n")
            if body_md:
                _wt_write(wt, md_path, body_md if body_md.endswith("\n") else body_md + "\n")
            summary = f"propose: new {collection}/{slug} draft"
        else:
            summary = f"propose: update {collection}/{slug}"
            for f in (meta_path, md_path):
                overwritten[f] = _wt_read(wt, f)
            _apply_meta_edits(meta_path, root=wt,
                              title=raw_title or None,
                              description=description,
                              agent_notes=agent_notes,
                              fields=fields or None)
        if body_md and not is_new:
            _wt_write(wt, md_path, body_md if body_md.endswith("\n") else body_md + "\n")

        # Validate with a real build BEFORE committing anything: a schema
        # violation or broken item fails here with the exact compiler error,
        # so a proposal never opens a PR it can't render.
        build = _run_build(wt)
        if build.returncode != 0:
            tail = _build_error_tail(build)
            raise ValueError(f"proposal failed build validation:\n{tail}")

        commit_env = {
            "GIT_AUTHOR_NAME": author_name,
            "GIT_AUTHOR_EMAIL": author_email,
            "GIT_COMMITTER_NAME": author_name,
            "GIT_COMMITTER_EMAIL": author_email,
        }
        own = [str(item_dir.relative_to(wt))]
        before = _commit_own_paths(
            wt, own, summary, commit_env,
            "nothing to commit — for updates pass body/description; "
            "if the item already has this content, no MR is needed")
        assert before is not None
        _ledger_record_commit("propose_change", branch, wt, before, author_i, own)

        push = _git("push", "-u", "origin", branch, worktree=wt)
        if push.returncode != 0:
            raise ValueError(f"git push failed: {push.stderr.strip()[:300]}")

        preview_cmd = (f"cd {shlex.quote(str(ROOT if ws.throwaway else wt))} && "
                       f"git fetch origin {shlex.quote(branch)} && "
                       f"git switch {shlex.quote(branch)} && "
                       f"DRAFTS=1 cargo run -q -p build --bin build-site && "
                       f"python3 -m http.server 8000 -d dist")
        preview_url = f"http://localhost:8000/preview/drafts/{collection}/{slug}/"
        # Risk class: the queue uses this to batch copy edits and page a
        # human for publish flips / schema changes.
        target_published = published
        if not is_new:
            try:
                cur = {}
                for line in (_wt_read(wt, meta_path) or "").splitlines():
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
        if ok:
            # Per-PR preview as a chat artifact: the rendered page travels
            # INTO the result (path + html), not to a server-local path.
            result["preview_artifact"] = _artifact_payload(
                _render_preview_artifact(wt, collection, slug))
        if not ok:
            result["pr_error"] = pr_or_err
        if ok and args.get("merge"):
            # Gate already passed up front; record the approval, merge.
            # Pinned to the commit made here; the approval note is posted
            # only after that merge succeeded.
            merge = _merge_pinned(branch, _git("rev-parse", "HEAD", worktree=wt).stdout.strip())
            result["merged"] = merge.returncode == 0
            if merge.returncode != 0:
                result["merge_error"] = merge.stderr.strip()[:300]
            else:
                comment = subprocess.run(
                    ["gh", "pr", "comment", branch, "--body",
                     f"Approved by {apn} <{ape}> via propose_change(merge=true) "
                     f"[identity: {apn_src}]"],
                    cwd=str(wt), capture_output=True, text=True)
                result["approval_recorded"] = comment.returncode == 0
                _ledger_record_merge("propose_change", branch, branch, wt, apn_i)
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
            # Restore overwritten files, remove created ones, then prune the
            # now-empty dirs — the shared tree ends exactly as it started.
            for f, content in overwritten.items():
                if content is None:
                    if f.exists():
                        f.unlink()
                else:
                    f.write_text(content)
            for f in created:
                if f.exists():
                    f.unlink()
            for d in sorted({f.parent for f in created}, reverse=True):
                if d != ROOT and d.is_dir() and not any(d.iterdir()):
                    d.rmdir()
            _git("switch", "-")
        raise


# ------------------------------------------------------------------- plugins

def _read_site_yaml_plugins(site_path: Path, root: Path) -> list[dict[str, Any]]:
    """Parse the top-level `plugins:` block of a site.yaml (stdlib only)."""
    lines = (_wt_read(root, site_path) or "").splitlines()
    try:
        start = next(i for i, ln in enumerate(lines) if ln.startswith("plugins:"))
    except StopIteration:
        return []
    end = start + 1
    while end < len(lines) and (lines[end].startswith((" ", "\t")) or not lines[end].strip()):
        end += 1
    entries: list[dict[str, Any]] = []
    cur: dict[str, Any] | None = None
    for ln in lines[start + 1:end]:
        if ln.strip().startswith("- name:"):
            cur = {"name": ln.split(":", 1)[1].strip()}
            entries.append(cur)
        elif cur is not None and ":" in ln:
            k, v = ln.split(":", 1)
            k, v = k.strip(), v.strip()
            if k == "mounts":
                cur["mounts"] = [t.strip() for t in v.strip("[]").split(",") if t.strip()]
            elif k == "csp_script_src":
                cur["csp_script_src"] = [t.strip().strip('"')
                                         for t in v.strip("[]").split(",") if t.strip()]
            elif k == "require_consent":
                cur["require_consent"] = v.lower() in ("true", "yes", "1")
    return entries


def _site_yaml_plugins_block(entries: list[dict[str, Any]]) -> str:
    lines = ["plugins:"]
    for e in entries:
        lines.append(f"  - name: {e['name']}")
        lines.append(f"    mounts: [{', '.join(e.get('mounts') or ['head'])}]")
        if e.get("csp_script_src"):
            domains = ", ".join(f'"{d}"' for d in e["csp_script_src"])
            lines.append(f"    csp_script_src: [{domains}]")
        if e.get("require_consent"):
            lines.append("    require_consent: true")
    return "\n".join(lines)


def _set_site_yaml_plugins(site_path: Path, new_entries: list[dict[str, Any]],
                           root: Path) -> None:
    """Replace the top-level `plugins:` block in site.yaml (or append it, or
    remove it entirely when the list is empty). Rest of the file untouched."""
    lines = (_wt_read(root, site_path) or "").splitlines()
    try:
        start = next(i for i, ln in enumerate(lines) if ln.startswith("plugins:"))
    except StopIteration:
        start = None
    if start is not None:
        end = start + 1
        while end < len(lines) and (lines[end].startswith((" ", "\t")) or not lines[end].strip()):
            end += 1
        del lines[start:end]
    block = _site_yaml_plugins_block(new_entries)
    if block:
        if lines and lines[-1].strip():
            lines.append("")
        lines.extend(block.splitlines())
    _wt_write(root, site_path, "\n".join(lines) + "\n")


def tool_propose_plugin(args: dict[str, Any]) -> dict[str, Any]:
    return _in_workspace("plugin", _propose_plugin, args)


def _propose_plugin(args: dict[str, Any], ws: _ProposalWorkspace) -> dict[str, Any]:
    """Propose a client-side plugin (analytics pixel, ad tag) via the review
    flow. Writes plugins/<name>/snippet.html and registers the plugin in
    site.yaml `plugins:`, validates with a real build (a snippet whose
    domains are undeclared or whose file is missing fails the build), pushes,
    and opens a PR with risk: tracking — the PR diff IS the pixel being
    added. `remove: true` deletes the snippet and drops the declaration.
    """
    if not (ROOT / ".git").exists():
        raise ValueError("not a git repository")
    # Author identity comes from the authenticated session, not from a
    # typed name: an agent could otherwise propose as "Alice", approve as
    # itself, and launder self-approval past the distinct-approver check.
    # Free-text author_name/email stay OPTIONAL but must MATCH the session
    # (same contract as approve_proposal).
    author_i = _check_declared_identity(
        (args.get("author_name") or "").strip(),
        (args.get("author_email") or "").strip())
    author_name = author_i["name"] or author_i["login"]
    author_email = author_i["email"] or f"{author_i['login']}@users.noreply.github.com"
    name = (args.get("name") or "").strip()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*", name):
        raise ValueError("plugin name must match [A-Za-z0-9][A-Za-z0-9-]*")
    remove = bool(args.get("remove"))
    mounts = args.get("mounts") or ["head"]
    for m in mounts:
        if m not in ("head", "body_end"):
            raise ValueError(f"unknown mount {m!r} (head / body_end)")
    csp_script_src = [d.strip() for d in (args.get("csp_script_src") or []) if d.strip()]
    require_consent = bool(args.get("require_consent"))
    tenant = (args.get("tenant") or "").strip()
    apn = ape = apn_src = ""  # bound when merge=true (gate below); Pyright-narrowing aid
    apn_i: dict[str, Any] = {}
    if args.get("merge"):
        # Full publish gate BEFORE any side effect (MCP_ALLOW_APPROVE,
        # approver rights, distinct approver against this session as the
        # author); a refused merge leaves no pushed branch or open PR.
        apn, ape, apn_src, apn_i = _merge_now_gate(args, "propose_plugin", author_i,
                                                   author_name, author_email)
    _stable_id(author_i)  # the ledger needs it; fail before any side effect

    # A throwaway worktree on origin/main (always over HTTP, and without a
    # tenant) — the shared tree is never touched, so a failed proposal
    # cannot leave uncommitted writes behind. The wrapper deletes it.
    wt = ws.open()
    tenant = ws.tenant
    tmp: Path | None = wt if ws.throwaway else None

    try:
        site_path = wt / "site.yaml"
        if _wt_read(wt, "site.yaml") is None:
            raise ValueError("site.yaml not found in repo")
        plugin_dir = wt / "plugins" / name
        snippet_path = plugin_dir / "snippet.html"

        current = _read_site_yaml_plugins(site_path, wt)
        if remove:
            if not any(e["name"] == name for e in current):
                raise ValueError(f"plugin {name!r} is not declared in site.yaml — nothing to remove")
            if _wt_read(wt, snippet_path) is not None:
                _git("rm", "-rqf", str(plugin_dir.relative_to(wt)), worktree=wt)
            new_entries = [e for e in current if e["name"] != name]
            _set_site_yaml_plugins(site_path, new_entries, wt)
            verb = "remove"
        else:
            snippet = args.get("snippet")
            if not snippet or not snippet.strip():
                raise ValueError("snippet (HTML) is required unless remove=true")
            _wt_write(wt, snippet_path, snippet if snippet.endswith("\n") else snippet + "\n")
            entry = {"name": name, "mounts": mounts, "csp_script_src": csp_script_src,
                     "require_consent": require_consent}
            new_entries = [e for e in current if e["name"] != name] + [entry]
            _set_site_yaml_plugins(site_path, new_entries, wt)
            verb = "add"

        # Validation: a real build. The build crate rejects declared plugins
        # with no snippet, unknown mounts, and bad names — so a broken plugin
        # never reaches a PR. The build output doubles as the review artifact:
        # dist/index.html has the plugin injected at its declared mount, so
        # the reviewer sees exactly what a visitor's browser would run.
        build = _run_build(wt)
        if build.returncode != 0:
            tail = _build_error_tail(build)
            raise ValueError(f"plugin failed build validation:\n{tail}")
        rendered_artifact = None
        if not remove:
            built = wt / "dist" / "index.html"
            if built.exists():
                rendered_artifact = _artifact_payload(
                    _save_artifact(f"plugin-{name}.html", built.read_text()))

        branch = f"agent/plugin-{name}"
        commit_env = {
            "GIT_AUTHOR_NAME": author_name,
            "GIT_AUTHOR_EMAIL": author_email,
            "GIT_COMMITTER_NAME": author_name,
            "GIT_COMMITTER_EMAIL": author_email,
        }
        own = [str(plugin_dir.relative_to(wt)), "site.yaml"]
        before = _commit_own_paths(wt, own, f"propose: {verb} plugin {name}", commit_env,
                                   "plugin unchanged — nothing to propose")
        assert before is not None
        _ledger_record_commit("propose_plugin", branch, wt, before, author_i, own)
        if tmp is not None:
            push = _git("push", "origin", f"HEAD:refs/heads/{branch}",
                        "--force-with-lease", worktree=wt)
        else:
            push = _git("push", "-u", "origin", branch, worktree=wt)
        if push.returncode != 0:
            raise ValueError(f"git push failed: {push.stderr.strip()[:300]}")

        if remove:
            detail = ("Removes the tracking snippet and its site.yaml declaration. "
                      "After merge, revert_proposal can restore it — the ledger keeps both.")
        else:
            detail = (f"mounts: {mounts}\ncsp_script_src: {csp_script_src or ['(none)']}\n"
                      f"require_consent: {require_consent}\n\n"
                      "The PR diff IS the pixel being added — review the script URLs.")
        pr_body = (f"Agent-authored plugin by {author_name}.\n\n"
                   f"risk: tracking\n\n{detail}")
        ok, pr_or_err = _open_pr(wt, branch, f"Plugin ({verb}): {name}", pr_body,
                                 head=None if tmp is None else branch)

        result: dict[str, Any] = {
            "branch": branch,
            "plugin": f"plugins/{name}/snippet.html",
            "proposed": True,
            "risk": "tracking",
            "tenant": tenant or None,
            "pr_url": pr_or_err if ok else None,
        }
        if ok and rendered_artifact:
            # The rendered page (plugin injected at its mount) travels in the
            # result — reviewable over HTTP without touching the server's fs.
            result["rendered_artifact"] = rendered_artifact
        if not ok:
            result["pr_error"] = pr_or_err
        if ok and args.get("merge"):
            # Gate already validated up-front; record the approval, merge.
            # Pinned to the commit made here; the approval note is posted
            # only after that merge succeeded.
            merge = _merge_pinned(branch, _git("rev-parse", "HEAD", worktree=wt).stdout.strip())
            result["merged"] = merge.returncode == 0
            if merge.returncode != 0:
                result["merge_error"] = merge.stderr.strip()[:300]
            else:
                comment = subprocess.run(
                    ["gh", "pr", "comment", branch, "--body",
                     f"Approved by {apn} <{ape}> via propose_plugin(merge=true) "
                     f"[identity: {apn_src}]"],
                    cwd=str(wt), capture_output=True, text=True)
                result["approval_recorded"] = comment.returncode == 0
                _ledger_record_merge("propose_plugin", branch, branch, wt, apn_i)
                result["deploy"] = ("skipped: propose_plugin does not deploy; "
                                    "use approve_proposal for build+deploy+purge")
        if wt != ROOT:
            _reset_worktree(wt, [])
        else:
            _git("switch", "-")
        return result
    except Exception:
        if wt != ROOT:
            _reset_worktree(wt, [])
        raise


def _apply_meta_edits(meta_path: Path, *, root: Path, title: str | None = None,
                      description: str | None = None,
                      agent_notes: str | None = None,
                      fields: dict[str, Any] | None = None) -> None:
    """Rewrite keys in an existing item's meta.yaml, appending missing ones.
    Reads and writes inside worktree `root` without following symlinks."""
    cur = _wt_read(root, meta_path)
    if cur is None:
        raise ValueError(f"{meta_path.relative_to(root)} not found")
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
        _wt_write(root, meta_path, cur)


def _proposal_branch(slug: str) -> str | None:
    """Latest agent/<slug>[-rN] branch, local or on origin. The version
    suffix is -rN so hyphenated slugs parse unambiguously. Accepts the bare
    proposal ID and resolves kind-prefixed branches too: `acme-pixel`
    finds agent/plugin-acme-pixel (plugins) / agent/template-<name>
    (templates); the queue always shows the full branch-derived ID."""
    candidates = [slug]
    if not slug.startswith(("plugin-", "template-")):
        candidates += [f"plugin-{slug}", f"template-{slug}"]
    for s in candidates:
        for n in range(20, 1, -1):
            name = f"agent/{s}-r{n}"
            if (_git("rev-parse", "--verify", "--quiet", name).returncode == 0
                    or _git("rev-parse", "--verify", "--quiet",
                            f"origin/{name}").returncode == 0):
                return name
        if (_git("rev-parse", "--verify", "--quiet", f"agent/{s}").returncode == 0
                or _git("rev-parse", "--verify", "--quiet",
                        f"origin/agent/{s}").returncode == 0):
            return f"agent/{s}"
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
    # Same contract as propose_change: the reviser's identity comes from the
    # authenticated session, not free text.
    author_i = _check_declared_identity(
        (args.get("author_name") or "").strip(),
        (args.get("author_email") or "").strip())
    author_name = author_i["name"] or author_i["login"]
    author_email = author_i["email"] or f"{author_i['login']}@users.noreply.github.com"
    slug = _check_id(args.get("slug"), "slug")
    collection = _check_id(args.get("collection") or "posts", "collection")
    branch = _proposal_branch(slug)
    if branch is None:
        raise ValueError(f"no open proposal branch for slug {slug!r} (propose_change first)")

    # origin/<branch> is the source of truth — a local ref can be stale
    # (behind a revision pushed by an earlier session). Its content is
    # controlled by whoever can push the branch, so before anything runs
    # in it: no symlink anywhere in the tree, and the branch may change
    # nothing but this item (the preview below runs the repo's build code).
    # Resolve the branch and main ONCE: every check, the build and the push
    # use these exact shas, so a concurrent fetch moving origin/<branch>
    # cannot swap the tree after it was checked.
    with _flock("repo"):
        for spec in ("+refs/heads/main:refs/remotes/origin/main",
                     f"+refs/heads/{branch}:refs/remotes/origin/{branch}"):
            f = _git("fetch", "--quiet", "origin", spec)
            if f.returncode != 0:
                raise ValueError(f"git fetch failed: {f.stderr.strip()[:300]}")
        head_sha = _git("rev-parse", "--verify", "--quiet", f"origin/{branch}^{{commit}}").stdout.strip()
        main_sha = _git("rev-parse", "--verify", "--quiet", "origin/main^{commit}").stdout.strip()
    if not _SHA_RE.fullmatch(head_sha) or not _SHA_RE.fullmatch(main_sha):
        raise ValueError(f"cannot resolve {branch} or main")
    tmp = _add_work_worktree("revise", head_sha, [])
    try:
        if _git("rev-parse", "HEAD", worktree=tmp).stdout.strip() != head_sha:
            raise ValueError("revise worktree is not at the checked commit")
        _refuse_symlinks_in_tree(head_sha)
        item_rel = f"content/{collection}/{slug}"
        _refuse_branch_outside(branch, [item_rel], head=head_sha, base=main_sha)
        item_dir = tmp / item_rel
        if not _wt_is_dir(tmp, item_rel):
            raise ValueError(f"{collection}/{slug} not on {branch}")
        meta_path = item_dir / "meta.yaml"
        md_path = item_dir / "index.mdx"
        body_md = args.get("body")
        if body_md:
            _wt_write(tmp, md_path, body_md if body_md.endswith("\n") else body_md + "\n")
        agent_notes = args.get("agent_notes")
        if agent_notes is not None and not isinstance(agent_notes, str):
            agent_notes = json.dumps(agent_notes, ensure_ascii=False)
        fields = args.get("fields")
        if not isinstance(fields, dict):
            fields = None
        raw_title = (args.get("title") or "").strip() or None
        _apply_meta_edits(meta_path, root=tmp, title=raw_title,
                          description=args.get("description"),
                          agent_notes=agent_notes, fields=fields)
        env = {"GIT_AUTHOR_NAME": author_name, "GIT_AUTHOR_EMAIL": author_email,
               "GIT_COMMITTER_NAME": author_name, "GIT_COMMITTER_EMAIL": author_email}
        own = [str(item_dir.relative_to(tmp))]
        before = _commit_own_paths(tmp, own,
                                   f"revise: {collection}/{slug} from review feedback",
                                   env, None)
        if before is not None:
            # Only the commit made here is recorded, never the branch head
            # it started from (which may hold commits made outside).
            _ledger_record_commit("revise_proposal", branch, tmp, before, author_i, own)
        # Push only if the branch is still at the commit that was checked.
        push = _git("push", f"--force-with-lease=refs/heads/{branch}:{head_sha}",
                    "origin", f"HEAD:refs/heads/{branch}", worktree=tmp)
        if push.returncode != 0:
            raise ValueError(f"git push failed (the branch may have moved since it "
                             f"was checked): {push.stderr.strip()[:300]}")
        return {"revised": True, "branch": branch,
                "pr_url": _open_pr_for_branch(branch),
                "preview_artifact": _artifact_payload(
                    _render_preview_artifact(tmp, collection, slug)),
                "preview_command": (
                    f"cd {shlex.quote(str(ROOT))} && git fetch origin {shlex.quote(branch)} "
                    f"&& git switch {shlex.quote(branch)} && DRAFTS=1 cargo run -q -p build --bin build-site "
                    f"&& python3 -m http.server 8000 -d dist")}
    finally:
        _remove_work_worktree(tmp)


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
            # Quoted: the branch name comes from the forge. Running it builds
            # the branch's own code on the reviewer's machine.
            "preview_command": f"cd {shlex.quote(str(ROOT))} && git fetch origin "
                               f"{shlex.quote(head)} && git switch {shlex.quote(head)} && "
                               f"DRAFTS=1 {BUILD_SHELL} && python3 -m http.server 8000 -d dist",
            "preview_note": "the preview command builds and runs this branch's code",
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
    with _flock("root"):
        _ship_and_purge_locked(result)


def _ship_and_purge_locked(result: dict[str, Any]) -> None:
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


# ---- approver identity: from the login, not from free text ----------------
#
# Over the hosted HTTP transport (mcp/http_server.py), identity comes from
# the validated OAuth token bound into the request context — a standard
# connector (GitHub token introspection or any OIDC issuer via discovery +
# JWKS). Locally (stdio), it comes from the authenticated gh session.

_HTTP_IDENTITY: "contextvars.ContextVar[dict[str, Any] | None]" = (
    contextvars.ContextVar("pcms_http_identity", default=None))

_GH_IDENTITY_CACHE: dict[str, Any] | None = None


def _gh_identity() -> dict[str, Any]:
    """Resolve the approver's identity — not from free text. Over HTTP this
    is the authenticated OAuth principal; locally it is the gh session
    (`gh api user`). Cached per process for the local path. Raises when no
    authenticated identity exists: approvals are refused, not guessed."""
    http_ident = _HTTP_IDENTITY.get()
    if http_ident is not None:
        return http_ident
    global _GH_IDENTITY_CACHE
    if _GH_IDENTITY_CACHE is not None:
        return _GH_IDENTITY_CACHE
    res = subprocess.run(["gh", "api", "user"], cwd=str(ROOT),
                         capture_output=True, text=True)
    if res.returncode != 0:
        raise ValueError(
            "approver identity could not be resolved: `gh api user` failed "
            f"({res.stderr.strip()[:200]}). Approvals require an "
            "authenticated identity (OAuth token over HTTP, gh session "
            "locally) — identity is taken from the login, never from "
            "free-text arguments."
        )
    try:
        u = json.loads(res.stdout)
    except ValueError:
        raise ValueError("approver identity: could not parse `gh api user` output")
    _GH_IDENTITY_CACHE = {
        "login": u.get("login") or "",
        "name": u.get("name") or "",
        "email": u.get("email") or "",
        # Stable id: the numeric GitHub user id, which survives login,
        # name and email changes. Approvals and the ledger key on this.
        "id": f"github:{u['id']}" if u.get("id") else "",
        "source": f"github:{u.get('login')} via gh session",
    }
    return _GH_IDENTITY_CACHE


def _approval_allowed() -> None:
    """Raise unless merging is allowed in this transport.

    MCP_ALLOW_APPROVE=0: hard block everywhere (agent-facing installs).
    Unset + PCMS_TRANSPORT=http: refuse by default — the token holder may
    be an agent, and agents must not merge. Humans approve in the PR UI
    or Slack; set MCP_ALLOW_APPROVE=1 to re-enable.
    stdio: allowed (a solo loop would freeze without it).
    """
    if os.environ.get("MCP_ALLOW_APPROVE") == "0":
        raise ValueError(
            "merging is disabled (MCP_ALLOW_APPROVE=0). Approvals happen "
            "through a human-authenticated channel — an agent-facing "
            "server must not merge.")
    if os.environ.get("MCP_ALLOW_APPROVE") != "1" \
            and os.environ.get("PCMS_TRANSPORT") == "http":
        raise ValueError(
            "merging is disabled over HTTP by default: the token holder "
            "may be an agent, and agents must not merge. Humans approve "
            "in the PR UI or Slack; set MCP_ALLOW_APPROVE=1 to re-enable.")


def _check_declared_identity(declared_name: str, declared_email: str) -> dict[str, Any]:
    """Free-text approver args (legacy) are allowed only when they MATCH the
    authenticated session identity — a name typed into a tool call is no
    longer the identity. Returns the canonical identity record."""
    ident = _gh_identity()
    candidates = {c for c in (ident["name"], ident["login"], ident["email"]) if c}
    name_ok = (not declared_name) or declared_name in candidates
    email_ok = (not declared_email) or (not ident["email"]) or declared_email == ident["email"]
    if not name_ok or not email_ok:
        raise ValueError(
            f"declared approver {declared_name!r} <{declared_email!r}> does not "
            f"match the authenticated GitHub session (login: {ident['login']!r}, "
            f"name: {ident['name']!r}). Identity comes from the login — a typed "
            "name is not accepted as someone else."
        )
    return ident


# ---- proposals ledger: who made each commit, recorded by the server ---------
#
# Git author and committer fields are free text: an author can change them
# and anyone with push access can forge them. So the server keeps its own
# append-only record of every commit it makes on a proposal branch, keyed by
# the caller's stable id (GitHub numeric user id, or OIDC issuer + subject),
# and of every merge it performs. Approve, merge=true and revert read the
# ledger; git identities are only a secondary check.
#
# Location: $PCMS_STATE_DIR/proposals-ledger.jsonl (absolute, required over
# HTTP), default on stdio $XDG_STATE_HOME/post-cms/<repo hash>/. Never inside
# the repository or a worktree. The state dir must be a real directory owned
# by the server user, mode 0700; the file is opened O_NOFOLLOW and must be a
# regular file the server user owns. One JSON object per line:
#   {"v": 1, "event": "commit", "tool": "propose_change", "branch": "agent/x",
#    "sha": "<40 hex>", "actor": "github:123", "email": "...", "time": "..."}
#   {"v": 1, "event": "merge", "tool": "approve_proposal", "branch": "agent/x",
#    "pr": 7, "sha": "<merge sha or null>", "actor": "github:456", ...}
# The file is opened O_APPEND, locked, fsynced; a failed write fails the
# operation. A malformed line makes every gate that reads it refuse.

_SHA_RE = re.compile(r"[0-9a-f]{40}([0-9a-f]{24})?")


def _state_dir() -> Path:
    """Where server state (the proposals ledger) lives: never inside the repo
    or any of its worktrees, whose content a branch pusher controls.
    PCMS_STATE_DIR (absolute) wins and is required over HTTP. On stdio the
    default is $XDG_STATE_HOME/post-cms/<repo hash> (~/.local/state/...)."""
    base = os.environ.get("PCMS_STATE_DIR", "").strip()
    if base:
        d = Path(base)
        if not d.is_absolute():
            raise ValueError("PCMS_STATE_DIR must be an absolute path")
    elif os.environ.get("PCMS_TRANSPORT") == "http":
        raise ValueError("PCMS_STATE_DIR is required over HTTP: an absolute path "
                         "outside the repository and its worktrees")
    else:
        xdg = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local" / "state")
        tag = hashlib.sha256(str(ROOT.resolve()).encode()).hexdigest()[:12]
        d = Path(xdg) / "post-cms" / tag
    real = Path(os.path.realpath(d))
    roots = {Path(os.path.realpath(ROOT))}
    wl = _git("worktree", "list", "--porcelain")
    for line in wl.stdout.splitlines():
        if line.startswith("worktree "):
            roots.add(Path(os.path.realpath(line[len("worktree "):])))
    for r in roots:
        if real == r or r in real.parents:
            raise ValueError(f"state dir {d} is inside {r}; server state must live "
                             "outside the repository and its worktrees")
    return d


def check_state_dir() -> Path:
    """Startup check: resolve the state dir and create or validate it."""
    _ledger_dir(create=True)
    return _state_dir()


def _ledger_dir(create: bool) -> Path | None:
    """The ledger's state dir (see _state_dir). It must be a real directory
    (not a symlink) owned by the server user with mode 0700; anything else
    refuses. Created 0700 when missing and `create`."""
    import stat as st_mod
    d = _state_dir()
    try:
        st = os.lstat(d)
    except FileNotFoundError:
        if not create:
            return None
        d.parent.mkdir(parents=True, exist_ok=True)
        os.mkdir(d, 0o700)
        st = os.lstat(d)
    if st_mod.S_ISLNK(st.st_mode) or not st_mod.S_ISDIR(st.st_mode):
        raise ValueError(f"ledger state dir {d} is not a real directory")
    if st.st_uid != os.getuid():
        raise ValueError(f"ledger state dir {d} is not owned by the server user")
    if st.st_mode & 0o077:
        raise ValueError(f"ledger state dir {d} must be mode 0700 "
                         f"(is {oct(st.st_mode & 0o777)})")
    return d


def _ledger_path() -> Path:
    return _state_dir() / "proposals-ledger.jsonl"


def _ledger_open(flags: int) -> int:
    """Open the ledger without following symlinks; it must be a regular file
    owned by the server user."""
    import stat as st_mod
    path = _ledger_path()
    try:
        fd = os.open(str(path), flags | os.O_NOFOLLOW, 0o600)
    except OSError as exc:
        raise ValueError(f"cannot open proposals ledger {path} without following "
                         f"links: {exc}") from exc
    st = os.fstat(fd)
    if not st_mod.S_ISREG(st.st_mode) or st.st_uid != os.getuid():
        os.close(fd)
        raise ValueError(f"proposals ledger {path} is not a regular file owned by "
                         "the server user")
    return fd




def _stable_id(ident: dict[str, Any]) -> str:
    """The identity's stable id. Never a name, email or login alone: those
    can change or be claimed by someone else."""
    sid = str(ident.get("id") or "").strip()
    if not sid:
        raise ValueError(
            "the signed-in identity has no stable id (GitHub numeric user id "
            "or OIDC issuer + subject); approvals and the proposals ledger "
            "need one")
    return sid


def _ledger_append(entry: dict[str, Any]) -> None:
    import fcntl
    record = {"v": 1, "time": datetime.now(timezone.utc).isoformat(), **entry}
    line = json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n"
    try:
        _ledger_dir(create=True)
        fd = _ledger_open(os.O_WRONLY | os.O_APPEND | os.O_CREAT)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            os.write(fd, line.encode())
            os.fsync(fd)
        finally:
            os.close(fd)
    except (OSError, ValueError) as exc:
        raise ValueError(f"proposals ledger write failed: {exc}") from exc


def _ledger_entries() -> list[dict[str, Any]]:
    path = _ledger_path()
    if _ledger_dir(create=False) is None or not os.path.lexists(path):
        return []
    out: list[dict[str, Any]] = []
    with os.fdopen(_ledger_open(os.O_RDONLY), encoding="utf-8") as fh:
        for n, line in enumerate(fh, 1):
            try:
                e = json.loads(line)
            except ValueError:
                e = None
            if (not isinstance(e, dict) or e.get("v") != 1
                    or e.get("event") not in ("commit", "merge")
                    or not isinstance(e.get("branch"), str)
                    or not isinstance(e.get("actor"), str) or not e["actor"]
                    or (e["event"] == "commit"
                        and not _SHA_RE.fullmatch(str(e.get("sha") or "")))):
                raise ValueError(f"proposals ledger {path} line {n} is malformed; "
                                 "refusing to trust it")
            out.append(e)
    return out


def _ledger_record_commit(tool: str, branch: str, wt: Path, before: str,
                          ident: dict[str, Any], own: list[str]) -> str:
    """Record the commit the server just made in `wt` (HEAD, whose parent
    must be `before`). The commit must change exactly the tool's own paths
    (`own`: files or directories), nothing else. Raises when no new commit
    exists, it touches other paths, or the write fails."""
    head = _git("rev-parse", "HEAD", worktree=wt).stdout.strip()
    parent = _git("rev-parse", "HEAD^", worktree=wt).stdout.strip()
    if not _SHA_RE.fullmatch(head) or head == before or parent != before:
        raise ValueError("ledger: could not identify the commit the server just made")
    diff = _git("-c", "diff.relative=false", "diff", "--no-renames", "--no-ext-diff",
                "--name-only", "-z", before, head, worktree=wt)
    changed = [f for f in diff.stdout.split("\x00") if f]
    roots = [o.rstrip("/") for o in own]
    stray = [f for f in changed
             if not any(f == r or f.startswith(r + "/") for r in roots)]
    if diff.returncode != 0 or not changed or stray:
        raise ValueError(f"ledger: {tool} commit {head[:12]} changes paths outside "
                         f"its own ({', '.join(stray[:5]) or 'none readable'}); refusing")
    _ledger_append({"event": "commit", "tool": tool, "branch": branch, "sha": head,
                    "actor": _stable_id(ident), "email": ident.get("email") or ""})
    return head


def _ledger_record_merge(tool: str, branch: str, pr: Any, cwd: Path,
                         ident: dict[str, Any]) -> None:
    """Record a merge with the forge's merge commit sha (null when the forge
    does not report it; revert then falls back to the exact branch name)."""
    view = subprocess.run(
        ["gh", "pr", "view", str(pr), "--json", "mergeCommit",
         "--jq", ".mergeCommit.oid"], cwd=str(cwd), capture_output=True, text=True)
    sha = view.stdout.strip() if view.returncode == 0 else ""
    _ledger_append({"event": "merge", "tool": tool, "branch": branch,
                    "pr": pr if isinstance(pr, int) else None,
                    "sha": sha if _SHA_RE.fullmatch(sha) else None,
                    "actor": _stable_id(ident), "email": ident.get("email") or ""})


def _commits(rev_range: str) -> list[tuple[str, str, str, str, str]]:
    """(sha, author name, author email, committer name, committer email) for
    every commit in rev_range. NUL-separated (git identities cannot contain
    NUL), and any malformed record raises: fail closed."""
    log = _git("log", "-z", "--format=%H%x00%an%x00%ae%x00%cn%x00%ce", rev_range)
    if log.returncode != 0:
        raise ValueError(f"cannot read history {rev_range}: {log.stderr.strip()[:200]}")
    raw = log.stdout
    if not raw:
        return []
    fields = raw.split("\x00")
    if fields and fields[-1] == "":
        fields.pop()
    if len(fields) % 5:
        raise ValueError(f"unparseable git history for {rev_range}; refusing")
    out = []
    for i in range(0, len(fields), 5):
        rec = tuple(fields[i:i + 5])
        if not _SHA_RE.fullmatch(rec[0]):
            raise ValueError(f"unparseable git history for {rev_range}; refusing")
        out.append(rec)  # type: ignore[arg-type]
    return out


def _fetch_origin_main() -> None:
    """Fetch origin/main explicitly. Never fall back to a local main: a
    local main that already contains the branch hides its commits."""
    with _flock("repo"):
        _fetch_origin_main_locked()


def _fetch_origin_main_locked() -> None:
    res = _git("fetch", "--quiet", "origin", "+refs/heads/main:refs/remotes/origin/main")
    if res.returncode != 0 or _git("rev-parse", "--verify", "--quiet",
                                   "origin/main").returncode != 0:
        raise ValueError("cannot fetch origin/main; refusing to check authorship "
                         "without it")


def _people_of(commits: list[tuple[str, str, str, str, str]]) -> list[tuple[str, str]]:
    people: list[tuple[str, str]] = []
    for _sha, an, ae, cn, ce in commits:
        for pair in ((an, ae), (cn, ce)):
            if pair not in people:
                people.append(pair)
    return people


def _ledger_actors(commits: list[tuple[str, str, str, str, str]],
                   what: str) -> set[str]:
    """Stable ids of whoever made `commits` through the server. Every commit
    must be in the ledger; a commit made outside the server (pushed
    directly, forged identity) has no trustworthy author, so it refuses."""
    if not commits:
        raise ValueError(f"{what}: no commits to check; refusing")
    by_sha: dict[str, set[str]] = {}
    for e in _ledger_entries():
        if e["event"] == "commit":
            by_sha.setdefault(e["sha"], set()).add(e["actor"])
    missing = [c[0][:12] for c in commits if c[0] not in by_sha]
    if missing:
        raise ValueError(f"{what}: the branch has commits not made through the "
                         f"server ({', '.join(missing[:5])}); refusing")
    actors: set[str] = set()
    for c in commits:
        actors |= by_sha[c[0]]
    if not actors:
        raise ValueError(f"{what}: empty author set; refusing")
    return actors


def _branch_review(branch: str, what: str) -> dict[str, Any]:
    """Everything the distinct-approver check needs for an open proposal:
    the commits on origin/<branch> since its merge base with origin/main,
    their ledger actors (stable ids), their git people (secondary), and the
    head the merge will be pinned to."""
    head, commits = _branch_head_and_commits(branch, what)
    return {"head": head, "actors": _ledger_actors(commits, what),
            "people": _people_of(commits)}


def _branch_head_and_commits(branch: str, what: str
                             ) -> tuple[str, list[tuple[str, str, str, str, str]]]:
    """The head the PR will merge and every commit on it since the merge
    base with origin/main. The one place the commit list comes from, so a
    forge API (which lists the change's commits itself) can replace it."""
    with _flock("repo"):
        _fetch_origin_main_locked()
        _git("fetch", "--quiet", "origin",
             f"+refs/heads/{branch}:refs/remotes/origin/{branch}")
        # Resolve both refs once, under the lock; everything after uses shas.
        head = _git("rev-parse", "--verify", "--quiet",
                    f"origin/{branch}^{{commit}}").stdout.strip()
        main = _git("rev-parse", "--verify", "--quiet", "origin/main^{commit}").stdout.strip()
    if not _SHA_RE.fullmatch(head):
        raise ValueError(f"{what}: branch {branch!r} not found on origin")
    if not _SHA_RE.fullmatch(main):
        raise ValueError(f"{what}: cannot resolve origin/main")
    return head, _commits(f"{main}..{head}")


def _merge_review(merge_sha: str, what: str) -> dict[str, Any]:
    """Same, for the commits a merge brought in (first parent .. second)."""
    commits = _commits(f"{merge_sha}^1..{merge_sha}^2")
    return {"actors": _ledger_actors(commits, what), "people": _people_of(commits)}


def _is_self_approval(author_name: str, author_email: str, ident: dict[str, Any]) -> bool:
    """True when the proposal was authored by the same human who is now
    approving it. Compared against every form the session identity takes
    (login, name, noreply email, real email) — an author cannot launder
    their identity through a different email field."""
    if not author_name and not author_email:
        return False
    candidates = {
        c.lower() for c in (
            ident["login"],
            ident["name"],
            ident["email"],
            f"{ident['login']}@users.noreply.github.com",
        ) if c
    }
    return (author_name.lower() in candidates) or (author_email.lower() in candidates)


# ---- the publish gate ------------------------------------------------------
#
# Every action that lands on main goes through one gate: approve_proposal,
# propose_change / propose_template / propose_plugin with merge=true, and
# revert_proposal. In order:
#   1. _approval_allowed(): MCP_ALLOW_APPROVE (off over HTTP by default).
#   2. approver rights: the caller's stable id is in MCP_APPROVERS. Over
#      HTTP an unset list refuses; on stdio it allows the local user.
#   3. distinct approver (always over HTTP; on stdio with
#      REQUIRE_DISTINCT_APPROVER=1): every commit in the change is in the
#      proposals ledger, and the caller's stable id is not among its actors.
#      Git author and committer names and emails are a secondary check.


def _distinct_required() -> bool:
    """Always over HTTP (many principals). On stdio (one local human; every
    commit carries the same identity) only with REQUIRE_DISTINCT_APPROVER=1."""
    if os.environ.get("PCMS_TRANSPORT") == "http":
        return True
    return os.environ.get("REQUIRE_DISTINCT_APPROVER") == "1"


def _require_approver(ident: dict[str, Any]) -> None:
    """Approver rights, by stable id only (MCP_APPROVERS: comma-separated
    ids such as github:12345 or oidc:https://issuer.example#subject). Over
    HTTP no list means nobody may approve, reject or revert. On stdio an
    unset list allows the local user."""
    allowed = {x.strip() for x in os.environ.get("MCP_APPROVERS", "").split(",")
               if x.strip()}
    if not allowed:
        if os.environ.get("PCMS_TRANSPORT") == "http":
            raise ValueError(
                "no approver is configured (MCP_APPROVERS); over HTTP, approve, "
                "merge, reject and revert are refused until one is")
        return
    if _stable_id(ident) not in allowed:
        raise ValueError(
            f"{ident.get('login') or ident.get('email') or _stable_id(ident)!r} is not "
            "an approver (MCP_APPROVERS lists approver stable ids)")


def _refuse_if_author(ident: dict[str, Any], actors: set[str],
                      people: list[tuple[str, str]], what: str, hint: str) -> None:
    sid = _stable_id(ident)
    if sid in actors:
        raise ValueError(
            f"{what}: {sid} made commits in this change through the server and "
            f"is now publishing it; a change cannot approve itself ({hint})")
    for n, e in people:
        if _is_self_approval(n, e, ident):
            raise ValueError(
                f"{what}: {n or e!r} authored or committed this change and is "
                f"the identity now publishing it; a change cannot approve "
                f"itself ({hint})")


def _merge_now_gate(args: dict[str, Any], tool: str, author_i: dict[str, Any],
                    author_name: str, author_email: str
                    ) -> tuple[str, str, str, dict[str, Any]]:
    """merge=true on a propose_* tool is an approval of a change the same
    session is about to author, so it runs the full publish gate with that
    session as the only actor. Called before any side effect: a refused
    merge leaves no pushed branch or open PR behind. Returns the approver's
    (name, email, identity source, identity)."""
    if os.environ.get("PCMS_TRANSPORT") == "http":
        raise ValueError(
            f"{tool}(merge=true) is refused over HTTP: a proposal is approved "
            "by a different approver with approve_proposal (merge=true is for "
            "a local single user on stdio)")
    apn_i = _check_declared_identity(
        (args.get("approver_name") or "").strip(),
        (args.get("approver_email") or "").strip())
    _approval_allowed()
    _require_approver(apn_i)
    if _distinct_required():
        people = [(author_name, author_email)]
        if author_i.get("login"):
            people.append((author_i["login"], ""))
        _refuse_if_author(apn_i, {_stable_id(author_i)}, people,
                          f"{tool}(merge=true) refused",
                          "another approver must approve it with approve_proposal")
    apn = apn_i["name"] or apn_i["login"]
    ape = apn_i["email"] or f"{apn_i['login']}@users.noreply.github.com"
    return apn, ape, apn_i["source"], apn_i


def _merge_pinned(pr: Any, head: str | None) -> subprocess.CompletedProcess:
    """Merge a PR, pinned to `head` when given (the forge refuses the merge
    if the branch moved after the author check)."""
    cmd = ["gh", "pr", "merge", str(pr), "--merge", "--delete-branch"]
    if head:
        cmd += ["--match-head-commit", head]
    return subprocess.run(cmd, cwd=str(ROOT), capture_output=True, text=True)


def _approve_one(slug: str, approver_name: str, approver_email: str,
                 identity_source: str, head: str | None = None,
                 ident: dict[str, Any] | None = None) -> dict[str, Any]:
    """Merge one proposal PR with the approver's identity on the record.
    `head` pins the merge to the commit the author check read, so a commit
    pushed between the check and the merge makes the merge fail."""
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
    merge = _merge_pinned(pr_number, head)
    if merge.returncode != 0:
        raise ValueError(f"merge failed for {slug}: {merge.stderr.strip()[:300]}")
    out["merged"] = True
    # The approval note is posted only once the pinned merge succeeded.
    comment = subprocess.run(
        ["gh", "pr", "comment", str(pr_number), "--body",
         f"Approved by {approver_name} <{approver_email}> via approve_proposal "
         f"[identity: {identity_source}]"],
        cwd=str(ROOT), capture_output=True, text=True)
    out["approval_recorded"] = comment.returncode == 0
    _ledger_record_merge("approve_proposal", branch, pr_number, ROOT,
                         ident if ident is not None else _gh_identity())
    return out


def tool_approve_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """Approve one proposal (slug) or a batch (slugs): merge the PR(s), then
    build + deploy + purge once, so 'approve' means live. The approver's
    identity is resolved from the authenticated GitHub session (`gh api
    user`) — free-text name/email are accepted only if they MATCH that
    identity. Low-risk copy batches can ride one approval."""
    _approval_allowed()
    declared_name = (args.get("approver_name") or "").strip()
    declared_email = (args.get("approver_email") or "").strip()
    slugs = args.get("slugs")
    if not slugs:
        slugs = [args.get("slug")]
    if not isinstance(slugs, list):
        raise ValueError("slugs must be a list of proposal slugs")
    slugs = [_check_id(s, "slug") for s in slugs]
    ident = _check_declared_identity(declared_name, declared_email)
    approver_name = ident["name"] or ident["login"]
    approver_email = ident["email"] or f"{ident['login']}@users.noreply.github.com"
    identity_source = ident["source"]
    # Same publish gate as merge=true and revert. The author set is every
    # author and committer on the branch since its merge base, so adding
    # one commit to someone else's branch makes you an author of it, and
    # the original author stays one. Distinct approver: on over HTTP, off
    # on stdio (one local human); REQUIRE_DISTINCT_APPROVER wins.
    _require_approver(ident)
    heads: dict[str, str | None] = {}
    for s in slugs:
        if _distinct_required():
            what = f"approve_proposal refused for {s!r}"
            branch = _proposal_branch(s)
            if branch is None:
                raise ValueError(f"no proposal branch for slug {s!r}")
            review = _branch_review(branch, what)
            _refuse_if_author(ident, review["actors"], review["people"], what,
                              "another approver must approve it")
            heads[s] = review["head"]

    merged: list[dict[str, Any]] = []
    result: dict[str, Any] = {}
    try:
        for s in slugs:
            merged.append(_approve_one(s, approver_name, approver_email, identity_source,
                                       heads.get(s), ident))
    finally:
        result["approved"] = merged
        result["count"] = len(merged)
        result["approver"] = {"name": approver_name, "email": approver_email,
                              "source": identity_source}
    if not args.get("deploy", True):
        result["deploy"] = "skipped (deploy=false)"
        return result
    _ship_and_purge(result)
    return result


def tool_reject_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """Reject a proposal: record WHO and WHY on the PR (agent-readable state —
    the proposing agent can read the reason and revise instead of re-proposing
    blind), then close the PR and delete its branch. Reviewer identity comes
    from the authenticated GitHub session; free-text args must match it."""
    slug = (args.get("slug") or "").strip()
    reason = (args.get("reason") or "").strip()
    slug = _check_id(slug, "slug")
    if not reason:
        raise ValueError("reason required — a rejection without a reason teaches nothing")
    ident = _check_declared_identity(
        (args.get("reviewer_name") or "").strip(),
        (args.get("reviewer_email") or "").strip())
    # Rejecting closes someone's proposal: approver rights, by stable id
    # (over HTTP an unset MCP_APPROVERS refuses).
    _require_approver(ident)
    reviewer_name = ident["name"] or ident["login"]
    reviewer_email = ident["email"] or f"{ident['login']}@users.noreply.github.com"
    identity_source = ident["source"]
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
         f"REJECTED: {reason}\n\nBy {reviewer_name} <{reviewer_email}> via "
         f"reject_proposal [identity: {identity_source}]."],
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


def _revert_on_shared_tree(merge_sha: str) -> None:
    """Revert `merge_sha` on main in the shared checkout and push, under the
    shared-checkout lock."""
    with _flock("root"):
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


def _refuse_branch_outside(branch: str, roots: list[str], *, head: str,
                           base: str) -> None:
    """Refuse a proposal branch whose commit `head` changes anything outside
    `roots` since its merge base with `base` (renames count as both sides).
    Both are shas resolved once by the caller, never re-read refs."""
    diff = _git("-c", "diff.relative=false", "diff", "--no-renames", "--no-ext-diff",
                "--name-only", "-z", f"{base}...{head}")
    if diff.returncode != 0:
        raise ValueError(f"cannot diff {branch}: {diff.stderr.strip()[:200]}")
    roots = [r.rstrip("/") for r in roots]
    stray = [f for f in diff.stdout.split("\x00")
             if f and not any(f == r or f.startswith(r + "/") for r in roots)]
    if stray:
        raise ValueError(f"{branch} changes paths outside {', '.join(roots)} "
                         f"({', '.join(stray[:5])}); refusing to run it")


def _find_shipped_merge(slug: str) -> tuple[str, int | None, str]:
    """The exact merge commit that shipped proposal `slug` on origin/main:
    (merge sha, PR number, branch). Branch names match exactly
    (agent/<slug> or agent/<slug>-rN, plus the plugin-/template- kinds),
    never as a substring. The ledger's merge record wins; a merge done in
    the forge UI is found by its merge subject. Either way the commit must
    be a two-parent merge reachable from origin/main."""
    cands = [slug]
    if not slug.startswith(("plugin-", "template-")):
        cands += [f"plugin-{slug}", f"template-{slug}"]
    pat = re.compile(r"agent/(?:%s)(?:-r\d+)?" % "|".join(re.escape(c) for c in cands))

    def shipped(sha: str) -> bool:
        if not _SHA_RE.fullmatch(sha):
            return False
        if _git("merge-base", "--is-ancestor", sha, "origin/main").returncode != 0:
            return False
        parents = _git("rev-list", "--parents", "-n", "1", sha).stdout.split()
        return len(parents) == 3

    for e in reversed(_ledger_entries()):
        if e["event"] == "merge" and pat.fullmatch(e["branch"]) and e.get("sha") \
                and shipped(e["sha"]):
            pr = e.get("pr") if isinstance(e.get("pr"), int) else None
            return e["sha"], pr, e["branch"]
    log = _git("log", "origin/main", "--merges", "-z", "--format=%H%x00%s", "--")
    if log.returncode != 0:
        raise ValueError(f"cannot read origin/main: {log.stderr.strip()[:200]}")
    fields = log.stdout.split("\x00")
    subj = re.compile(r"Merge pull request #(\d+) from [^/\s]+/(\S+)")
    for i in range(0, len(fields) - 1, 2):
        sha, subject = fields[i].strip(), fields[i + 1]
        m = subj.fullmatch(subject.strip())
        if m and pat.fullmatch(m.group(2)) and shipped(sha):
            return sha, int(m.group(1)), m.group(2)
    raise ValueError(f"no merge commit found for proposal {slug!r} on main")


def tool_revert_proposal(args: dict[str, Any]) -> dict[str, Any]:
    """One-action undo: revert the merge commit that shipped a proposal,
    rebuild, redeploy, purge. The revert itself is a commit — the ledger keeps
    both the mistake and the correction. Identity comes from the
    authenticated session; free-text args must match it.

    A revert publishes to main, so it runs the publish gate: approval must
    be allowed (MCP_ALLOW_APPROVE), the caller needs approver rights (stable
    id in MCP_APPROVERS), every reverted commit must be in the proposals
    ledger, and the caller must not be one of their actors (always over
    HTTP; on stdio only with REQUIRE_DISTINCT_APPROVER=1).
    MCP_ALLOW_SELF_REVERT=1 is the operator's break-glass for the author
    check alone; every use is logged to stderr, on the PR and in the result."""
    _approval_allowed()
    ident = _check_declared_identity(
        (args.get("approver_name") or "").strip(),
        (args.get("approver_email") or "").strip())
    approver_name = ident["name"] or ident["login"]
    approver_email = ident["email"] or f"{ident['login']}@users.noreply.github.com"
    identity_source = ident["source"]
    slug = _check_id(args.get("slug"), "slug")
    _require_approver(ident)
    with _flock("repo"):
        _git("fetch", "origin")
        _fetch_origin_main_locked()
    merge_sha, merge_pr, branch = _find_shipped_merge(slug)
    break_glass = False
    if _distinct_required():
        what = "revert_proposal refused"
        # The ledger must account for every commit the merge brought in;
        # the break-glass below never skips that, only the author check.
        review = _merge_review(merge_sha, what)
        try:
            _refuse_if_author(ident, review["actors"], review["people"], what,
                              "a different approver must revert it; the operator "
                              "break-glass is MCP_ALLOW_SELF_REVERT=1")
        except ValueError:
            if os.environ.get("MCP_ALLOW_SELF_REVERT") != "1":
                raise
            break_glass = True
            print(f"post-cms-mcp: BREAK-GLASS MCP_ALLOW_SELF_REVERT=1: "
                  f"{_stable_id(ident)} ({ident.get('login') or ident.get('email')}) "
                  f"reverted their own change {slug!r} ({branch}, merge "
                  f"{merge_sha[:12]})", file=sys.stderr, flush=True)
    out: dict[str, Any] = {"slug": slug, "branch": branch, "merge_commit": merge_sha,
                           "pr": merge_pr}
    if break_glass:
        out["break_glass"] = "MCP_ALLOW_SELF_REVERT"
    if merge_pr is not None:
        note = (" Break-glass: the reverter authored this change "
                "(MCP_ALLOW_SELF_REVERT=1).") if break_glass else ""
        subprocess.run(
            ["gh", "pr", "comment", str(merge_pr), "--body",
             f"Reverted by {approver_name} <{approver_email}> via revert_proposal "
             f"[identity: {identity_source}].{note}"],
            cwd=str(ROOT), capture_output=True, text=True)
    _revert_on_shared_tree(merge_sha)
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
    # Pathspec commit under the shared-checkout lock: only content/ is
    # committed, never whatever else happens to be staged.
    with _flock("root"):
        _git("add", "-A", "--", "content")
        res = _git("commit", "-m", message, "--", "content")
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
    res = _run_build(ROOT)
    if res.returncode != 0:
        raise RuntimeError(f"build failed (exit {res.returncode}): "
                           f"{_build_error_tail(res, lines=8, limit=500)}")
    return {"ok": True, "tail": _build_error_tail(res, lines=5).splitlines()}


def tool_build_preview(args: dict[str, Any]) -> dict[str, Any]:
    """Build every theme (or a comma list) under /preview/<theme>/ without
    changing the live site. `themes`: 'all' or e.g. 'astra,neve'."""
    spec = str(args.get("themes", "all"))
    if not re.fullmatch(r"[A-Za-z0-9_,-]{1,200}", spec):
        raise ValueError("themes must be 'all' or a comma list of theme names")
    res = _run_build(ROOT, SITE_PREVIEW_THEMES=spec)
    if res.returncode != 0:
        raise RuntimeError(f"preview build failed (exit {res.returncode}): "
                           f"{_build_error_tail(res, lines=8, limit=500)}")
    return {"ok": True, "urls": ["/preview/<theme>/", "/preview/"],
            "tail": _build_error_tail(res, lines=5).splitlines()}


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
    "propose_plugin": tool_propose_plugin,
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
    try:
        sweep_stale_worktrees()
    except Exception as exc:  # a failed sweep must not stop the server
        print(f"post-cms-mcp: worktree sweep failed: {exc}", file=sys.stderr)
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

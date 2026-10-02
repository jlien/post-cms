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
import subprocess
import sys
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
        "description": "Update fields of an existing post; returns a diff summary.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "slug": {"type": "string"},
                "title": {"type": "string"},
                "body": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "published": {"type": "boolean"},
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

def _git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)


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
    meta = _read_meta(slug)
    if not _post_dir(slug).is_dir():
        raise ValueError(f"post not found: {slug}")
    changes: list[dict[str, Any]] = []
    for field in ("title", "description", "published", "tags"):
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

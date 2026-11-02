"""Authorization for the post-cms MCP server: who may call which tool.

Standard library only. Four roles, each one including the ones below it:

    reader    read posts, proposals and publish status
    author    + propose and revise changes, build previews
    approver  + approve, reject and revert proposals, merge=true on a propose tool
    admin     + everything, including the direct-commit tools

Roles come from a JSON file (MCP_ROLES_FILE, default mcp/roles.json):

    {
      "allowed_domains": ["example.com"],
      "users": [
        {"email": "alice@example.com", "role": "admin"},
        {"github": "octocat", "role": "author"}
      ]
    }

Identities match by verified email (OIDC, and identity-aware proxies later)
or by forge login (GitHub). Anything else has no role and is refused.

Transports:
  * HTTP (mcp/http_server.py): the roles file is required and its
    allowed_domains list must be non-empty, or the server refuses to start.
    The direct-commit tools (create_draft, update_post, set_theme) are off
    even for admins unless MCP_ALLOW_DIRECT_COMMIT=1.
  * stdio: one local user, who already holds the checkout. Their role is
    MCP_STDIO_ROLE (default admin); no roles file is read.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

ROLES = ("reader", "author", "approver", "admin")
_RANK = {r: i for i, r in enumerate(ROLES)}

# Minimum role per tool. A tool missing from this map is refused for everyone.
TOOL_ROLES: dict[str, str] = {
    "list_posts": "reader",
    "get_post": "reader",
    "list_proposals": "reader",
    "publish_status": "reader",
    "propose_change": "author",
    "propose_template": "author",
    "propose_plugin": "author",
    "revise_proposal": "author",
    "build_preview": "author",
    "approve_proposal": "approver",
    "reject_proposal": "approver",
    "revert_proposal": "approver",
    "create_draft": "admin",
    "update_post": "admin",
    "set_theme": "admin",
    "build_site": "admin",
    "add_media": "admin",
}

# Tools that write and commit straight to the shared tree with no review.
DIRECT_COMMIT_TOOLS = frozenset({"create_draft", "update_post", "set_theme"})

# propose_* with merge=true merges at once: that is an approval.
MERGE_TOOLS = frozenset({"propose_change", "propose_template", "propose_plugin"})

DEFAULT_ROLES_FILE = Path(__file__).resolve().with_name("roles.json")


class ConfigError(Exception):
    """The roles file is missing or invalid; the HTTP server will not start."""


class Forbidden(Exception):
    """The caller's role does not allow this call."""


def roles_file() -> Path:
    raw = os.environ.get("MCP_ROLES_FILE", "").strip()
    return Path(raw) if raw else DEFAULT_ROLES_FILE


def _domain(email: str) -> str:
    return email.rpartition("@")[2].lower() if "@" in email else ""


def parse_config(data: Any, *, require_domains: bool) -> dict[str, Any]:
    """Validate a decoded roles file. Unknown keys, unknown roles, duplicate
    identities and emails outside allowed_domains are errors: a typo must
    never widen access."""
    if not isinstance(data, dict):
        raise ConfigError("roles file must be a JSON object")
    extra = set(data) - {"allowed_domains", "users"}
    if extra:
        raise ConfigError(f"unknown keys in roles file: {sorted(extra)}")
    domains = data.get("allowed_domains", [])
    if not isinstance(domains, list) or not all(isinstance(d, str) and d.strip() for d in domains):
        raise ConfigError("allowed_domains must be a list of domain names")
    domains = [d.strip().lower().lstrip("@") for d in domains]
    if require_domains and not domains:
        raise ConfigError("allowed_domains is missing or empty; over HTTP the "
                          "domain list is required (fail closed)")
    users = data.get("users", [])
    if not isinstance(users, list):
        raise ConfigError("users must be a list")
    by_email: dict[str, str] = {}
    by_github: dict[str, str] = {}
    for i, u in enumerate(users):
        if not isinstance(u, dict):
            raise ConfigError(f"users[{i}] must be an object")
        extra = set(u) - {"email", "github", "role"}
        if extra:
            raise ConfigError(f"users[{i}] has unknown keys: {sorted(extra)}")
        role = u.get("role")
        if role not in _RANK:
            raise ConfigError(f"users[{i}].role must be one of {list(ROLES)}")
        keys = [k for k in ("email", "github") if k in u]
        if len(keys) != 1:
            raise ConfigError(f"users[{i}] needs exactly one of email or github")
        value = u[keys[0]]
        if not isinstance(value, str) or not value.strip():
            raise ConfigError(f"users[{i}].{keys[0]} must be a non-empty string")
        value = value.strip().lower()
        if keys[0] == "email":
            if not _domain(value):
                raise ConfigError(f"users[{i}].email is not an email address")
            if domains and _domain(value) not in domains:
                raise ConfigError(f"users[{i}].email is outside allowed_domains")
            table = by_email
        else:
            table = by_github
        if value in table:
            raise ConfigError(f"users[{i}] repeats {keys[0]} {value!r}")
        table[value] = role
    return {"allowed_domains": domains, "email": by_email, "github": by_github}


def load_config(path: Path | None = None, *, require_domains: bool = True) -> dict[str, Any]:
    path = path or roles_file()
    try:
        data = json.loads(path.read_text())
    except FileNotFoundError:
        raise ConfigError(f"roles file not found: {path} (set MCP_ROLES_FILE)") from None
    except (OSError, ValueError) as exc:
        raise ConfigError(f"roles file unreadable: {path}: {exc}") from None
    return parse_config(data, require_domains=require_domains)


def approvers(cfg: dict[str, Any]) -> list[str]:
    """Identities with approve rights (approver or admin)."""
    return sorted(k for table in ("email", "github") for k, r in cfg[table].items()
                  if _RANK[r] >= _RANK["approver"])


def role_for(ident: dict[str, Any] | None, cfg: dict[str, Any]) -> str | None:
    """Role of a validated identity, or None (refused).

    kind github: matched by login only; a GitHub profile email is not
    verified by us, so it never grants a role.
    kind oidc / iap: matched by email only, and only when the issuer marked
    it verified and its domain is on allowed_domains.
    """
    if not ident:
        return None
    kind = ident.get("kind")
    if kind == "github":
        login = (ident.get("login") or "").strip().lower()
        return cfg["github"].get(login) if login else None
    if kind in ("oidc", "iap"):
        email = (ident.get("email") or "").strip().lower()
        if not email or ident.get("email_verified") is not True:
            return None
        if cfg["allowed_domains"] and _domain(email) not in cfg["allowed_domains"]:
            return None
        return cfg["email"].get(email)
    return None


def direct_commit_enabled() -> bool:
    return os.environ.get("MCP_ALLOW_DIRECT_COMMIT") == "1"


def stdio_role() -> str:
    role = os.environ.get("MCP_STDIO_ROLE", "admin").strip() or "admin"
    if role not in _RANK:
        raise Forbidden(f"MCP_STDIO_ROLE must be one of {list(ROLES)}")
    return role


def allowed(role: str | None, tool: str, *, http: bool,
            args: dict[str, Any] | None = None) -> str | None:
    """None when allowed, else the reason for refusing."""
    if role not in _RANK:
        return "no role: this identity is not in the roles config"
    need = TOOL_ROLES.get(tool)
    if need is None:
        return f"{tool!r} has no role mapping"
    if args and tool in MERGE_TOOLS and args.get("merge"):
        need = max(need, "approver", key=_RANK.__getitem__)
    if _RANK[role] < _RANK[need]:
        return f"{tool!r} needs role {need}; caller has {role}"
    if http and tool in DIRECT_COMMIT_TOOLS and not direct_commit_enabled():
        return (f"{tool!r} commits straight to the shared tree and is off over "
                "HTTP; use propose_change, or set MCP_ALLOW_DIRECT_COMMIT=1")
    return None


def visible_tools(tools: list[dict[str, Any]], role: str | None, *, http: bool) -> list[dict[str, Any]]:
    """tools/list filtered to what the caller may call."""
    return [t for t in tools if allowed(role, t["name"], http=http) is None]

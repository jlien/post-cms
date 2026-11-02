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
        {"id": "https://issuer.example.com|0f3c9a", "email": "alice@example.com", "role": "admin"},
        {"id": "github:583231", "email": "octocat@example.com", "role": "author"},
        {"email": "bob@example.com", "role": "reader"}
      ]
    }

Prefer "id": the stable actor id, "<iss>|<sub>" for OIDC and identity-aware
proxies, "github:<numeric user id>" for GitHub. Logins and emails can be
renamed and re-registered; ids cannot. On an id entry "email" is display
only. An email-only entry matches an OIDC/IAP identity whose email is
verified, ASCII, on allowed_domains and equal to it (ASCII case-insensitive).
Anything else has no role and is refused. The file is read once at startup;
restart the server to apply a change.

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
import re
import stat
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


_GITHUB_ID = re.compile(r"github:[1-9][0-9]{0,19}")
_PRINTABLE_ASCII = re.compile(r"[\x21-\x7e]+")


def ascii_email(value: Any) -> str | None:
    """Lowercased email when it is plain printable ASCII with one @ and a
    non-empty local part and domain; None otherwise. Non-ASCII addresses
    are refused outright so Unicode lookalikes (a KELVIN SIGN for K) can
    never case-map onto a listed address."""
    if not isinstance(value, str) or not value or not value.isascii():
        return None
    if not _PRINTABLE_ASCII.fullmatch(value) or value.count("@") != 1:
        return None
    local, _, domain = value.partition("@")
    if not local or not domain or "." not in domain:
        return None
    return value.lower()


def _valid_id(value: Any) -> bool:
    if not isinstance(value, str) or not _PRINTABLE_ASCII.fullmatch(value):
        return False
    if value.startswith("github:"):
        return bool(_GITHUB_ID.fullmatch(value))
    iss, sep, sub = value.rpartition("|")
    return bool(sep and iss and sub)


def parse_config(data: Any, *, require_domains: bool) -> dict[str, Any]:
    """Validate a decoded roles file. Unknown keys, unknown or non-string
    roles, malformed ids, non-ASCII emails, duplicate identities and emails
    outside allowed_domains are errors: a typo must never widen access."""
    if not isinstance(data, dict):
        raise ConfigError("roles file must be a JSON object")
    extra = set(data) - {"allowed_domains", "users"}
    if extra:
        raise ConfigError(f"unknown keys in roles file: {sorted(extra)}")
    domains = data.get("allowed_domains", [])
    if not isinstance(domains, list) or not all(
            isinstance(d, str) and d.strip() and d.isascii() for d in domains):
        raise ConfigError("allowed_domains must be a list of ASCII domain names")
    domains = [d.strip().lower().lstrip("@") for d in domains]
    if require_domains and not domains:
        raise ConfigError("allowed_domains is missing or empty; over HTTP the "
                          "domain list is required (fail closed)")
    users = data.get("users", [])
    if not isinstance(users, list):
        raise ConfigError("users must be a list")
    by_id: dict[str, str] = {}
    by_email: dict[str, str] = {}
    for i, u in enumerate(users):
        if not isinstance(u, dict):
            raise ConfigError(f"users[{i}] must be an object")
        extra = set(u) - {"id", "email", "role"}
        if extra:
            raise ConfigError(f"users[{i}] has unknown keys: {sorted(extra)} "
                              "(use id, email, role)")
        role = u.get("role")
        if not isinstance(role, str) or role not in _RANK:
            raise ConfigError(f"users[{i}].role must be one of {list(ROLES)}")
        email = None
        if "email" in u:
            email = ascii_email(u["email"])
            if email is None:
                raise ConfigError(f"users[{i}].email must be a plain ASCII email address")
            if domains and _domain(email) not in domains:
                raise ConfigError(f"users[{i}].email is outside allowed_domains")
        if "id" in u:
            ident = u["id"]
            if not _valid_id(ident):
                raise ConfigError(f"users[{i}].id must be \"<iss>|<sub>\" or "
                                  "\"github:<numeric user id>\"")
            if ident in by_id:
                raise ConfigError(f"users[{i}] repeats id {ident!r}")
            by_id[ident] = role
        elif email is not None:
            if email in by_email:
                raise ConfigError(f"users[{i}] repeats email {email!r}")
            by_email[email] = role
        else:
            raise ConfigError(f"users[{i}] needs an id (preferred) or an email")
    return {"allowed_domains": domains, "id": by_id, "email": by_email}


def _no_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    seen: dict[str, Any] = {}
    for k, v in pairs:
        if k in seen:
            raise ConfigError(f"duplicate key {k!r} in roles file")
        seen[k] = v
    return seen


def load_config(path: Path | None = None, *, require_domains: bool = True) -> dict[str, Any]:
    """Read and validate the roles file. Refuses a file that is not a
    regular file or that is world-writable."""
    path = path or roles_file()
    try:
        st = path.stat()
    except FileNotFoundError:
        raise ConfigError(f"roles file not found: {path} (set MCP_ROLES_FILE)") from None
    except OSError as exc:
        raise ConfigError(f"roles file unreadable: {path}: {exc}") from None
    if not stat.S_ISREG(st.st_mode):
        raise ConfigError(f"roles file is not a regular file: {path}")
    if st.st_mode & stat.S_IWOTH:
        raise ConfigError(f"roles file is world-writable: {path} (chmod o-w)")
    try:
        data = json.loads(path.read_text(), object_pairs_hook=_no_duplicate_keys)
    except ConfigError:
        raise
    except (OSError, ValueError) as exc:
        raise ConfigError(f"roles file unreadable: {path}: {exc}") from None
    return parse_config(data, require_domains=require_domains)


def approvers(cfg: dict[str, Any]) -> list[str]:
    """Identities with approve rights (approver or admin)."""
    return sorted(k for table in ("id", "email") for k, r in cfg[table].items()
                  if _RANK[r] >= _RANK["approver"])


def role_for(ident: dict[str, Any] | None, cfg: dict[str, Any]) -> str | None:
    """Role of a validated identity, or None (refused).

    By stable id first ("github:<id>", "<iss>|<sub>"). Then, for OIDC/IAP
    only, an email-only entry when the email is verified, plain ASCII, on
    allowed_domains and equal to it. A GitHub login or profile email never
    grants a role.
    """
    if not ident:
        return None
    kind = ident.get("kind")
    if kind not in ("github", "oidc", "iap"):
        return None
    actor = ident.get("id")
    if isinstance(actor, str) and actor in cfg["id"]:
        return cfg["id"][actor]
    if kind == "github":
        return None
    if ident.get("email_verified") is not True:
        return None
    email = ascii_email(ident.get("email"))
    if email is None:
        return None
    if cfg["allowed_domains"] and _domain(email) not in cfg["allowed_domains"]:
        return None
    return cfg["email"].get(email)


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

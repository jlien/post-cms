"""Hosted HTTP transport for the post-cms MCP server.

Standard OAuth 2.1 per the MCP authorization spec: every /mcp request must
carry `Authorization: Bearer <token>`. Tokens are validated against a
STANDARD connector — no FusionAuth, no bespoke IdP:

    OAUTH_MODE=github   token introspected via api.github.com/user
                        (works with GitHub PATs and OAuth apps)
    OAUTH_MODE=oidc     JWT verified via OIDC discovery
                        (https://<issuer>/.well-known/openid-configuration)
                        + JWKS — works with any standard issuer: Auth0,
                        Okta, Entra ID, WorkOS, Keycloak, Authentik, ...

The validated principal is bound into the request context; approve / reject
/ revert take the approver's identity from that login — the same structural
gate as the local gh session, minus the shared machine. MCP_ALLOW_APPROVE=0
still disables merging on the server as a whole.

Endpoints:
    POST /mcp                                          JSON-RPC (MCP), auth required
    GET  /.well-known/oauth-protected-resource         RFC 9728 metadata, public

Usage:
    python3 http_server.py                 # 127.0.0.1:8791
    HOST=0.0.0.0 PORT=8800 python3 http_server.py
"""
from __future__ import annotations

import importlib.util
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib import request as urlreq
from urllib.error import HTTPError, URLError

ROOT = Path(__file__).resolve().parent.parent

# Load the stdio server module by path (its mcp/ dir must not shadow the
# pip `mcp` package, and this file is executed as a script, not an import).
_spec = importlib.util.spec_from_file_location("pcms_server", Path(__file__).with_name("server.py"))
if _spec is None or _spec.loader is None:
    sys.exit("cannot load server.py next to http_server.py")
srv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(srv)

OAUTH_MODE = os.environ.get("OAUTH_MODE", "").strip().lower()  # github | oidc
OAUTH_ISSUER = os.environ.get("OAUTH_ISSUER", "").rstrip("/")
OAUTH_AUDIENCE = os.environ.get("OAUTH_AUDIENCE", "").strip()
HOST = os.environ.get("HOST", "127.0.0.1")
PORT = int(os.environ.get("PORT", "8791"))

_TOKEN_CACHE: dict[str, tuple[float, dict]] = {}
_CACHE_LOCK = threading.Lock()
_CACHE_TTL = 300.0  # seconds a validated principal is reused


def _cached(token: str) -> dict | None:
    with _CACHE_LOCK:
        hit = _TOKEN_CACHE.get(token)
        if hit and hit[0] > time.monotonic():
            return hit[1]
        if hit:
            del _TOKEN_CACHE[token]
    return None


def _store(token: str, ident: dict) -> dict:
    if len(_TOKEN_CACHE) > 256:
        _TOKEN_CACHE.clear()
    _TOKEN_CACHE[token] = (time.monotonic() + _CACHE_TTL, ident)
    return ident


class AuthError(Exception):
    """Token missing or invalid → 401 with WWW-Authenticate."""


def _http_json(url: str, token: str | None = None) -> dict:
    req = urlreq.Request(url)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urlreq.urlopen(req, timeout=10) as resp:  # noqa: S310 — fixed https URLs
        return json.loads(resp.read().decode())


def _identity_github(token: str) -> dict:
    """Introspect a GitHub token (standard API, no SDK)."""
    u = _http_json("https://api.github.com/user", token)
    login = u.get("login") or ""
    if not login:
        raise AuthError("github token introspection returned no login")
    return {
        "login": login,
        "name": u.get("name") or "",
        "email": u.get("email") or "",
        "source": f"github:{login} via oauth token",
    }


def _identity_oidc(token: str) -> dict:
    """Verify a JWT via OIDC discovery + JWKS (any standard issuer)."""
    import jwt  # PyJWT — resolved in the runtime venv, not bundled

    if not OAUTH_ISSUER:
        raise RuntimeError("OAUTH_MODE=oidc requires OAUTH_ISSUER")
    if not OAUTH_AUDIENCE:
        raise RuntimeError("OAUTH_MODE=oidc requires OAUTH_AUDIENCE")
    jwks_client = _jwks_client(OAUTH_ISSUER)
    try:
        signing_key = jwks_client.get_signing_key_from_jwt(token)
        claims = jwt.decode(token, signing_key.key, algorithms=["RS256", "ES256"],
                            audience=OAUTH_AUDIENCE, issuer=OAUTH_ISSUER)
    except jwt.PyJWTError as exc:
        raise AuthError(f"invalid token: {exc}") from exc
    login = claims.get("preferred_username") or claims.get("sub") or ""
    if not login:
        raise AuthError("token has no principal (preferred_username/sub)")
    return {
        "login": login,
        "name": claims.get("name") or "",
        "email": claims.get("email") or "",
        "source": f"oidc:{login} via oauth token",
    }


_JWKS_CLIENTS: dict[str, tuple[float, object]] = {}


def _jwks_client(issuer: str):
    now = time.monotonic()
    with _CACHE_LOCK:
        hit = _JWKS_CLIENTS.get(issuer)
        if hit and hit[0] > now:
            return hit[1]
    disco = _http_json(f"{issuer}/.well-known/openid-configuration")
    jwks_uri = disco.get("jwks_uri")
    if not jwks_uri:
        raise RuntimeError(f"{issuer} discovery has no jwks_uri")
    import jwt
    client = jwt.PyJWKClient(jwks_uri)
    _JWKS_CLIENTS[issuer] = (now + 3600.0, client)
    return client


def validate_token(token: str) -> dict:
    """Token → principal. Raises AuthError when the connector rejects it."""
    if not token:
        raise AuthError("missing bearer token")
    hit = _cached(token)
    if hit is not None:
        return hit
    if OAUTH_MODE == "github":
        return _store(token, _identity_github(token))
    if OAUTH_MODE == "oidc":
        return _store(token, _identity_oidc(token))
    raise RuntimeError(
        "OAUTH_MODE must be set to a standard connector: github | oidc")


def protected_resource_metadata() -> dict:
    """RFC 9728: tells MCP clients which authorization server(s) to use."""
    servers = [OAUTH_ISSUER] if OAUTH_ISSUER else (
        ["https://github.com"] if OAUTH_MODE == "github" else [])
    return {
        "resource": f"http://{HOST}:{PORT}/mcp",
        "authorization_servers": servers,
        "scopes_supported": ["openid", "profile", "email"],
        "bearer_methods_supported": ["header"],
        "resource_documentation": "https://github.com/jlien/post-cms",
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):  # noqa: A002 — stdlib signature
        sys.stderr.write("%s - %s\n" % (self.address_string(), format % args))

    def _json(self, code: int, payload: dict, headers: dict | None = None) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _auth(self) -> dict:
        header = self.headers.get("Authorization", "")
        if not header.startswith("Bearer "):
            raise AuthError("Authorization: Bearer <token> required")
        return validate_token(header[7:].strip())

    def do_GET(self):  # noqa: N802 (stdlib naming)
        if self.path == "/.well-known/oauth-protected-resource":
            self._json(200, protected_resource_metadata())
            return
        self._json(404, {"error": "not found"})

    def do_POST(self):  # noqa: N802
        if self.path != "/mcp":
            self._json(404, {"error": "not found"})
            return
        try:
            ident = self._auth()
        except AuthError as exc:
            self._json(401, {"error": str(exc)}, {
                "WWW-Authenticate": 'Bearer resource_metadata="/.well-known/oauth-protected-resource"',
            })
            return
        except RuntimeError as exc:
            self._json(500, {"error": str(exc)})
            return

        try:
            length = int(self.headers.get("Content-Length", "0"))
            req = json.loads(self.rfile.read(length).decode() or "null")
        except (ValueError, json.JSONDecodeError):
            self._json(400, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "parse error"}})
            return
        if not isinstance(req, dict):
            self._json(400, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "parse error"}})
            return

        if req.get("method", "").startswith("notifications/"):
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        # Bind the validated principal; tools resolve identity from it
        # (the contextvar is read in this thread by _gh_identity).
        token = ctx_token.set(ident)
        try:
            resp = srv.handle_request(req)
        finally:
            ctx_token.reset(token)
        if resp is None:
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self._json(200, resp)


ctx_token = srv._HTTP_IDENTITY


def main() -> int:
    if not OAUTH_MODE:
        sys.exit("OAUTH_MODE required (github | oidc) — the hosted server "
                 "refuses to run without a standard connector")
    httpd = ThreadingHTTPServer((HOST, PORT), Handler)
    sys.stderr.write(f"post-cms MCP over HTTP on http://{HOST}:{PORT}/mcp "
                     f"(oauth: {OAUTH_MODE})\n")
    httpd.serve_forever()
    return 0


if __name__ == "__main__":
    os.chdir(ROOT)
    sys.exit(main())

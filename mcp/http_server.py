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
    OAUTH_MODE=iap      behind an identity-aware proxy: the proxy's signed
                        `x-goog-iap-jwt-assertion` header is the only
                        identity source (ES256, verified in mcp/iap.py);
                        `Authorization` is ignored. Requires IAP_AUDIENCE.

The validated principal is bound into the request context; approve / reject
/ revert take the approver's identity from that login — the same structural
gate as the local gh session, minus the shared machine. MCP_ALLOW_APPROVE=0
still disables merging on the server as a whole.

Authorization (mcp/authz.py): every validated identity must have a role in
the roles file (MCP_ROLES_FILE, default mcp/roles.json): reader, author,
approver or admin. Identities match by stable id ("github:<numeric id>",
"<iss>|<sub>"); OIDC identities may also match an email-only entry by
verified ASCII email on an allowed domain. The file is read once at startup:
restart to apply a change. No role means 403. The server refuses
to start without a valid roles file with a non-empty allowed_domains list.

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

_iap_spec = importlib.util.spec_from_file_location("pcms_iap", Path(__file__).with_name("iap.py"))
if _iap_spec is None or _iap_spec.loader is None:
    sys.exit("cannot load iap.py next to http_server.py")
iap = importlib.util.module_from_spec(_iap_spec)
_iap_spec.loader.exec_module(iap)

# Load the stdio server module by path (its mcp/ dir must not shadow the
# pip `mcp` package, and this file is executed as a script, not an import).
# The HTTP transport is a multi-principal channel: identity-aware defaults
# (e.g. distinct approver) key off this marker downstream.
os.environ["PCMS_TRANSPORT"] = "http"
_spec = importlib.util.spec_from_file_location("pcms_server", Path(__file__).with_name("server.py"))
if _spec is None or _spec.loader is None:
    sys.exit("cannot load server.py next to http_server.py")
srv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(srv)
_media_spec = importlib.util.spec_from_file_location("pcms_media", Path(__file__).with_name("media.py"))
if _media_spec is None or _media_spec.loader is None:
    sys.exit("cannot load media.py next to http_server.py")
media = importlib.util.module_from_spec(_media_spec)
_media_spec.loader.exec_module(media)

OAUTH_MODE = os.environ.get("OAUTH_MODE", "").strip().lower()  # github | oidc | iap
OAUTH_ISSUER = os.environ.get("OAUTH_ISSUER", "").rstrip("/")
OAUTH_AUDIENCE = os.environ.get("OAUTH_AUDIENCE", "").strip()
HOST = os.environ.get("HOST", "127.0.0.1")
PORT = int(os.environ.get("PORT", "8791"))

IAP_AUDIENCE = os.environ.get("IAP_AUDIENCE", "").strip()
IAP_JWKS_URL = os.environ.get("IAP_JWKS_URL", "").strip() or iap.DEFAULT_JWKS_URL
IAP_JWKS_MAX_STALE_SECS = os.environ.get("IAP_JWKS_MAX_STALE_SECS", "86400").strip()

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
    """Cache a validated principal for min(_CACHE_TTL, exp - now): a token
    is never honored from the cache after it expires."""
    ttl = _CACHE_TTL
    exp = ident.get("exp")
    if isinstance(exp, (int, float)) and not isinstance(exp, bool):
        ttl = min(ttl, exp - time.time())
    if ttl <= 0:
        return ident
    with _CACHE_LOCK:
        if len(_TOKEN_CACHE) > 256:
            _TOKEN_CACHE.clear()
        _TOKEN_CACHE[token] = (time.monotonic() + ttl, ident)
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
    uid = u.get("id")
    if not isinstance(uid, int) or isinstance(uid, bool) or uid <= 0:
        raise AuthError("github token introspection returned no numeric id")
    return {
        # Stable actor id: logins can be renamed and re-registered.
        "id": f"github:{uid}",
        "login": login,
        "name": u.get("name") or "",
        "email": u.get("email") or "",
        "kind": "github",
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
                            audience=OAUTH_AUDIENCE, issuer=OAUTH_ISSUER,
                            options={"require": ["exp", "iss", "aud", "sub"]})
    except jwt.PyJWTError as exc:
        raise AuthError(f"invalid token: {exc}") from exc
    login = claims.get("preferred_username") or claims.get("sub") or ""
    if not login:
        raise AuthError("token has no principal (preferred_username/sub)")
    sub = claims.get("sub")
    if not isinstance(sub, str) or not sub:
        raise AuthError("token has no sub claim")
    return {
        # Stable actor id: issuer plus subject, never the display username.
        "id": f"{OAUTH_ISSUER}|{sub}",
        "login": login,
        "name": claims.get("name") or "",
        "email": claims.get("email") or "",
        # Some issuers send the string "true"; anything else is unverified.
        "email_verified": claims.get("email_verified") in (True, "true"),
        "kind": "oidc",
        "exp": claims.get("exp"),
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
    if OAUTH_MODE == "iap":
        # One identity source: behind the proxy, a bearer token is never
        # an identity, whatever it contains.
        raise AuthError("bearer tokens are not accepted in iap mode")
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


_IAP_VERIFIER = None
_IAP_LOCK = threading.Lock()


def iap_config_error() -> str | None:
    """Why iap mode cannot run with this config, or None."""
    if not IAP_AUDIENCE:
        return "OAUTH_MODE=iap requires IAP_AUDIENCE"
    if not IAP_JWKS_URL.startswith("https://"):
        return "IAP_JWKS_URL must be an https:// URL"
    if not IAP_JWKS_MAX_STALE_SECS.isdigit():
        return "IAP_JWKS_MAX_STALE_SECS must be a whole number of seconds"
    return None


def _iap_verifier():
    global _IAP_VERIFIER
    with _IAP_LOCK:
        if _IAP_VERIFIER is None:
            err = iap_config_error()
            if err:
                raise RuntimeError(err)
            _IAP_VERIFIER = iap.IapVerifier(IAP_AUDIENCE, iap.JwksCache(
                IAP_JWKS_URL, max_stale=float(int(IAP_JWKS_MAX_STALE_SECS))))
        return _IAP_VERIFIER


def validate_iap(assertion: str) -> dict:
    """Proxy assertion → principal (email lowercased, sub kept). Raises
    AuthError; the reason goes to the server log, never to the client."""
    verifier = _iap_verifier()  # config errors (RuntimeError) stay server errors
    try:
        ident = verifier.verify(assertion)
    except iap.IapError as exc:
        raise AuthError(str(exc)) from exc
    except Exception as exc:  # noqa: BLE001 - any verifier fault (RecursionError too) fails closed
        raise AuthError(f"verifier error: {type(exc).__name__}") from exc
    # Roles match on ident["id"] (iss|sub). The email is the account the
    # proxy authenticated and signed for, so it counts as verified for an
    # email-only roles entry.
    return {**ident, "kind": "iap", "email_verified": True}


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


# Request bodies. One rule: MAX_BODY_BYTES (1 MiB) for every request,
# except an admin's tools/call add_media, which may carry a file as base64:
# ceil(MCP_MEDIA_MAX_BYTES * 4 / 3) + 64 KiB. Content-Length is checked
# before a byte is read (413 and close when over); chunked bodies are
# refused; reads time out after BODY_TIMEOUT seconds.
MAX_BODY_BYTES = 1024 * 1024
BODY_TIMEOUT = 30.0


def media_max_body_bytes() -> int:
    return -(-media.max_upload_bytes() * 4 // 3) + 64 * 1024


def max_body_bytes(role: str | None) -> int:
    """Largest body this caller may send. Only admins can call add_media."""
    return max(MAX_BODY_BYTES, media_max_body_bytes()) if role == "admin" else MAX_BODY_BYTES


def _is_add_media(req: object) -> bool:
    return (isinstance(req, dict) and req.get("method") == "tools/call"
            and isinstance(req.get("params"), dict)
            and req["params"].get("name") == "add_media")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = BODY_TIMEOUT  # socket timeout for every read, headers and body

    def log_message(self, format, *args):  # noqa: A002 — stdlib signature
        sys.stderr.write("%s - %s\n" % (self.address_string(), format % args))

    def _json(self, code: int, payload: dict, headers: dict | None = None) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        if code >= 400:
            # Any refusal may leave a body unread: close, so it is never
            # parsed as the next request on a reused connection (a proxy
            # pools connections across clients).
            self.send_header("Connection", "close")
            self.close_connection = True
        self.end_headers()
        self.wfile.write(body)

    def _body_length(self) -> int | None:
        """Declared body length, or None when the framing is unacceptable:
        Transfer-Encoding (chunked is not supported), a Content-Length that
        is not a plain non-negative integer, or conflicting Content-Length
        headers. The size cap is separate (max_body_bytes, 413)."""
        if self.headers.get_all("Transfer-Encoding"):
            return None
        values = self.headers.get_all("Content-Length") or []
        if not values:
            return 0
        if len(set(v.strip() for v in values)) != 1:
            return None
        raw = values[0].strip()
        if not raw.isascii() or not raw.isdigit() or len(raw) > 12:
            return None
        return int(raw)

    def _reject_body(self) -> bool:
        """For methods that take no body (GET): refuse one with 400 and close
        the connection, so it is never parsed as the next request."""
        if self._body_length() == 0:
            return False
        if self.command == "HEAD":  # a HEAD response carries no body
            self.send_response(400)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.close_connection = True
            self.end_headers()
        else:
            self._json(400, {"error": "request body not allowed"})
        return True

    def _auth(self) -> dict:
        if OAUTH_MODE == "iap":
            return validate_iap(self.headers.get(iap.IAP_HEADER, ""))
        header = self.headers.get("Authorization", "")
        if not header.startswith("Bearer "):
            raise AuthError("Authorization: Bearer <token> required")
        return validate_token(header[7:].strip())

    def do_GET(self):  # noqa: N802 (stdlib naming)
        if self._reject_body():
            return
        if self.path == "/.well-known/oauth-protected-resource" and OAUTH_MODE != "iap":
            self._json(200, protected_resource_metadata())
            return
        self._json(404, {"error": "not found"})

    def do_HEAD(self):  # noqa: N802
        if self._reject_body():
            return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):  # noqa: N802
        length = self._body_length()
        if length is None:
            self._json(400, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32600, "message": "invalid request"}})
            return
        if self.path != "/mcp":
            self._json(404, {"error": "not found"})
            return
        try:
            ident = self._auth()
            role = authz.role_for(ident, ROLES_CFG) if ROLES_CFG else None
            if role is None:
                self._json(403, {"error": "forbidden: this identity has no role "
                                          "on this server"})
                return
            ident = {**ident, "role": role}
        except AuthError as exc:
            if OAUTH_MODE == "iap":
                sys.stderr.write(f"iap: 401 ({exc})\n")
                self._json(401, {"error": "unauthorized"})
                return
            self._json(401, {"error": str(exc)}, {
                "WWW-Authenticate": 'Bearer resource_metadata="/.well-known/oauth-protected-resource"',
            })
            return
        except Exception as exc:  # noqa: BLE001 - config or connector fault: no detail to the client
            sys.stderr.write(f"auth: 500 ({type(exc).__name__}: {exc})\n")
            self._json(500, {"error": "server error"})
            return

        if length > max_body_bytes(ident.get("role")):
            self._json(413, {"error": "request body too large"})
            return
        try:
            body = self.rfile.read(length) if length else b""
        except TimeoutError:
            self._json(408, {"error": "request body timed out"})
            return
        if len(body) != length:
            self._json(400, {"error": "short request body"})
            return
        try:
            req = iap.loads_bounded(body.decode() or "null")
        except (ValueError, RecursionError):
            self._json(400, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "parse error"}})
            return
        if length > MAX_BODY_BYTES and not _is_add_media(req):
            self._json(413, {"error": "request body too large"})
            return
        if not isinstance(req, dict):
            self._json(400, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "parse error"}})
            return

        method = req.get("method")
        if isinstance(method, str) and method.startswith("notifications/"):
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        # Bind the validated principal; tools resolve identity from it
        # (the contextvar is read in this thread by _gh_identity).
        token = ctx_token.set(ident)
        try:
            resp = srv.handle_request(req)
        except Exception as exc:  # noqa: BLE001 - no traceback or detail to the client
            sys.stderr.write(f"mcp: 500 ({type(exc).__name__}: {exc})\n")
            self._json(500, {"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32603, "message": "internal error"}})
            return
        finally:
            ctx_token.reset(token)
        if resp is None:
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self._json(200, resp)


ctx_token = srv._HTTP_IDENTITY
authz = srv.authz

# Loaded by startup(); None means every request is refused.
ROLES_CFG: dict | None = None


def startup() -> None:
    """Fail closed before listening: a standard connector, a valid roles
    file with a non-empty allowed_domains list, and one source of approver
    rights (the roles file; MCP_APPROVERS is refused)."""
    global ROLES_CFG
    if OAUTH_MODE not in ("github", "oidc", "iap"):
        sys.exit("OAUTH_MODE required (github | oidc | iap): the hosted server "
                 "refuses to run without a standard connector")
    if OAUTH_MODE == "iap" and iap_config_error():
        sys.exit(f"{iap_config_error()}; refusing to start")
    if os.environ.get("MCP_APPROVERS", "").strip():
        sys.exit("MCP_APPROVERS is not read over HTTP: approver rights come "
                 "from the roles file (role approver or admin). Unset it.")
    try:
        ROLES_CFG = authz.load_config(require_domains=True)
    except authz.ConfigError as exc:
        sys.exit(f"refusing to start: {exc}")
    if not authz.approvers(ROLES_CFG):
        sys.stderr.write("warning: the roles file names no approver or admin; "
                         "approve, reject and revert are refused for everyone\n")
    if authz.direct_commit_enabled():
        sys.stderr.write("warning: MCP_ALLOW_DIRECT_COMMIT=1: admins can commit "
                         "straight to the shared tree with no review\n")


def main() -> int:
    startup()
    httpd = ThreadingHTTPServer((HOST, PORT), Handler)
    sys.stderr.write(f"post-cms MCP over HTTP on http://{HOST}:{PORT}/mcp "
                     f"(oauth: {OAUTH_MODE})\n")
    httpd.serve_forever()
    return 0


if __name__ == "__main__":
    os.chdir(ROOT)
    sys.exit(main())

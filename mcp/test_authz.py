"""Authorization tests for the MCP server: roles, the tool-to-role map, the
allowed domain list, unknown identities, and HTTP startup checks.

Standard library only, no network: tool handlers are stubbed, GitHub and
OIDC token validation is stubbed, and the HTTP server binds 127.0.0.1 on
an OS-assigned port.

    python3 -m unittest discover -s mcp -p 'test_*.py'
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import socket
import time
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest import mock
from urllib import request as urlreq
from urllib.error import HTTPError

HERE = Path(__file__).resolve().parent
_ENV_KEYS = ("PCMS_TRANSPORT", "MCP_ROLES_FILE", "MCP_STDIO_ROLE",
             "MCP_ALLOW_DIRECT_COMMIT", "MCP_APPROVERS", "MCP_MEDIA_MAX_BYTES")


def _load(name: str, file: str):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# http_server.py sets PCMS_TRANSPORT=http on import; keep that out of the
# rest of the process.
with mock.patch.dict(os.environ, {}):
    hs = _load("pcms_http_under_test", "http_server.py")
srv = hs.srv
authz = srv.authz

ROLES = ("reader", "author", "approver", "admin")
READ = {"list_posts", "get_post", "list_proposals", "publish_status"}
AUTHOR = {"propose_change", "propose_template", "propose_plugin",
          "revise_proposal", "build_preview"}
APPROVE = {"approve_proposal", "reject_proposal", "revert_proposal"}
ADMIN = {"create_draft", "update_post", "set_theme", "build_site", "add_media"}
DIRECT = {"create_draft", "update_post", "set_theme"}
EXPECTED = {
    "reader": READ,
    "author": READ | AUTHOR,
    "approver": READ | AUTHOR | APPROVE,
    "admin": READ | AUTHOR | APPROVE | ADMIN,
}

CONFIG = {
    "allowed_domains": ["example.com"],
    "users": [
        {"email": "rita@example.com", "role": "reader"},
        {"email": "arno@example.com", "role": "author"},
        {"email": "pia@example.com", "role": "approver"},
        {"email": "ada@example.com", "role": "admin"},
        {"id": "github:1001", "email": "octo.reader@example.com", "role": "reader"},
        {"id": "github:1002", "role": "admin"},
        {"id": "https://issuer.example.com|sub-7", "role": "approver"},
    ],
}


def _oidc(email: str, verified: bool = True) -> dict:
    return {"login": email.split("@")[0], "name": "", "email": email,
            "email_verified": verified, "kind": "oidc", "source": "test"}


def _call(name: str, args: dict | None = None) -> dict:
    return srv.handle_request({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                               "params": {"name": name, "arguments": args or {}}})


def _forbidden(resp: dict) -> bool:
    r = resp.get("result", {})
    return bool(r.get("isError")) and r["content"][0]["text"].startswith("forbidden:")


class _Env(unittest.TestCase):
    def setUp(self) -> None:
        saved = {k: os.environ.get(k) for k in _ENV_KEYS}
        for k in _ENV_KEYS:
            os.environ.pop(k, None)

        def restore() -> None:
            for k, v in saved.items():
                if v is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = v
        self.addCleanup(restore)
        # Every handler is a stub: these tests exercise the gate, not the tools.
        stubs = {n: (lambda _a, n=n: {"ran": n}) for n in srv.HANDLERS}
        p = mock.patch.dict(srv.HANDLERS, stubs)
        p.start()
        self.addCleanup(p.stop)

    def as_http(self, ident: dict | None):
        os.environ["PCMS_TRANSPORT"] = "http"
        tok = srv._HTTP_IDENTITY.set(ident)
        self.addCleanup(srv._HTTP_IDENTITY.reset, tok)


class ToolRoleMapTest(_Env):
    def test_every_handler_has_a_role(self):
        self.assertEqual(set(authz.TOOL_ROLES), set(srv.HANDLERS))

    def test_each_role_against_each_tool_over_http(self):
        os.environ["MCP_ALLOW_DIRECT_COMMIT"] = "1"
        for role in ROLES:
            for tool in srv.HANDLERS:
                with self.subTest(role=role, tool=tool):
                    tok = srv._HTTP_IDENTITY.set({"login": "x", "role": role})
                    os.environ["PCMS_TRANSPORT"] = "http"
                    try:
                        resp = _call(tool)
                    finally:
                        srv._HTTP_IDENTITY.reset(tok)
                    if tool in EXPECTED[role]:
                        self.assertFalse(resp["result"]["isError"], resp)
                    else:
                        self.assertTrue(_forbidden(resp), resp)

    def test_each_role_against_each_tool_on_stdio(self):
        for role in ROLES:
            os.environ["MCP_STDIO_ROLE"] = role
            for tool in srv.HANDLERS:
                with self.subTest(role=role, tool=tool):
                    resp = _call(tool)
                    self.assertEqual(tool in EXPECTED[role], not _forbidden(resp), resp)

    def test_tools_list_is_filtered_by_role(self):
        self.as_http({"login": "x", "role": "author"})
        r = srv.handle_request({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        self.assertEqual({t["name"] for t in r["result"]["tools"]}, READ | AUTHOR)

    def test_merge_true_on_propose_needs_approver(self):
        for tool in ("propose_change", "propose_template", "propose_plugin"):
            with self.subTest(tool=tool):
                self.assertIsNotNone(authz.allowed("author", tool, http=True,
                                                   args={"merge": True}))
                self.assertIsNone(authz.allowed("author", tool, http=True,
                                                args={"merge": False}))
                self.assertIsNone(authz.allowed("approver", tool, http=True,
                                                args={"merge": True}))
        self.as_http({"login": "x", "role": "author"})
        self.assertTrue(_forbidden(_call("propose_change", {"merge": True})))


class TokenCacheTest(_Env):
    def setUp(self) -> None:
        super().setUp()
        hs._TOKEN_CACHE.clear()
        self.addCleanup(hs._TOKEN_CACHE.clear)

    def test_expired_token_is_not_cached(self):
        hs._store("t1", {"login": "a", "exp": time.time() - 1})
        self.assertIsNone(hs._cached("t1"))

    def test_cache_ends_at_exp(self):
        hs._store("t2", {"login": "a", "exp": time.time() + 10})
        deadline = hs._TOKEN_CACHE["t2"][0]
        self.assertLessEqual(deadline - time.monotonic(), 10.5)
        with mock.patch.object(hs.time, "monotonic", return_value=deadline + 0.01):
            self.assertIsNone(hs._cached("t2"))

    def test_no_exp_uses_five_minutes(self):
        hs._store("t3", {"login": "a"})
        self.assertAlmostEqual(hs._TOKEN_CACHE["t3"][0] - time.monotonic(), 300, delta=2)


class DispatchInputTest(_Env):
    def test_non_object_arguments_are_rejected_before_any_check(self):
        self.as_http({"login": "x", "role": "admin"})
        for args in ([1], "merge", 3, True):
            with self.subTest(args=args):
                r = srv.handle_request({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                        "params": {"name": "propose_change", "arguments": args}})
                self.assertEqual(r["error"]["code"], -32602)

    def test_missing_arguments_mean_empty(self):
        r = srv.handle_request({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                "params": {"name": "list_posts", "arguments": None}})
        self.assertFalse(r["result"]["isError"])


class DirectCommitTest(_Env):
    def test_direct_commit_off_over_http_even_for_admin(self):
        self.as_http({"login": "x", "role": "admin"})
        for tool in DIRECT:
            with self.subTest(tool=tool):
                self.assertTrue(_forbidden(_call(tool)))
        self.assertFalse(_call("build_site")["result"]["isError"])

    def test_direct_commit_on_over_http_when_enabled(self):
        os.environ["MCP_ALLOW_DIRECT_COMMIT"] = "1"
        self.as_http({"login": "x", "role": "admin"})
        for tool in DIRECT:
            self.assertFalse(_call(tool)["result"]["isError"])

    def test_stdio_default_is_one_local_admin(self):
        for tool in srv.HANDLERS:
            self.assertFalse(_forbidden(_call(tool)), tool)
        r = srv.handle_request({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        self.assertEqual({t["name"] for t in r["result"]["tools"]}, set(srv.HANDLERS))

    def test_bad_stdio_role_refuses_everything(self):
        os.environ["MCP_STDIO_ROLE"] = "owner"
        self.assertTrue(_forbidden(_call("list_posts")))


class IdentityTest(_Env):
    def setUp(self) -> None:
        super().setUp()
        self.cfg = authz.parse_config(CONFIG, require_domains=True)

    def test_verified_emails_map_to_roles(self):
        self.assertEqual(authz.role_for(_oidc("rita@example.com"), self.cfg), "reader")
        self.assertEqual(authz.role_for(_oidc("ADA@Example.com"), self.cfg), "admin")

    def test_unverified_email_is_refused(self):
        self.assertIsNone(authz.role_for(_oidc("ada@example.com", verified=False), self.cfg))

    def test_domain_outside_list_is_refused(self):
        cfg = authz.parse_config({"allowed_domains": ["example.com"], "users": []},
                                 require_domains=True)
        cfg["email"]["eve@example.org"] = "admin"  # even if listed somehow
        self.assertIsNone(authz.role_for(_oidc("eve@example.org"), cfg))

    def test_config_email_outside_domains_is_rejected(self):
        with self.assertRaises(authz.ConfigError):
            authz.parse_config({"allowed_domains": ["example.com"],
                                "users": [{"email": "eve@example.org", "role": "admin"}]},
                               require_domains=True)

    def test_unknown_identity_has_no_role(self):
        self.assertIsNone(authz.role_for(_oidc("nobody@example.com"), self.cfg))
        self.assertIsNone(authz.role_for({"login": "x", "kind": "github"}, self.cfg))
        self.assertIsNone(authz.role_for({"login": "ada", "email": "ada@example.com"},
                                         self.cfg))
        self.assertIsNone(authz.role_for(None, self.cfg))

    def test_unknown_identity_is_refused_by_the_dispatcher(self):
        self.as_http({"login": "x"})  # validated but no role bound
        self.assertTrue(_forbidden(_call("list_posts")))

    def test_http_without_identity_is_refused(self):
        os.environ["PCMS_TRANSPORT"] = "http"
        self.assertTrue(_forbidden(_call("list_posts")))

    def test_github_matches_numeric_id_not_login_or_email(self):
        gh = {"id": "github:1001", "login": "renamed", "kind": "github"}
        self.assertEqual(authz.role_for(gh, self.cfg), "reader")
        # A re-registered login or a listed email on another account: nothing.
        gh = {"id": "github:9999", "login": "octo-admin", "kind": "github",
              "email": "ada@example.com", "email_verified": True}
        self.assertIsNone(authz.role_for(gh, self.cfg))

    def test_oidc_matches_iss_sub_id(self):
        ident = {**_oidc("someone@example.org", verified=False),
                 "id": "https://issuer.example.com|sub-7"}
        self.assertEqual(authz.role_for(ident, self.cfg), "approver")
        ident["id"] = "https://other.example.com|sub-7"
        self.assertIsNone(authz.role_for(ident, self.cfg))

    def test_email_on_an_id_entry_is_display_only(self):
        self.assertIsNone(authz.role_for(_oidc("octo.reader@example.com"), self.cfg))

    def test_unicode_lookalike_email_is_refused(self):
        # U+212A KELVIN SIGN lowercases to "k".
        self.assertIsNone(authz.role_for(_oidc("\u212ada@example.com"), {
            **self.cfg, "email": {"kda@example.com": "admin"}}))
        self.assertIsNone(authz.role_for(_oidc("ad\u0430@example.com"), self.cfg))
        self.assertIsNone(authz.role_for(_oidc("ada@ex\u0430mple.com"), self.cfg))
        with self.assertRaises(authz.ConfigError):
            authz.parse_config({"allowed_domains": ["example.com"], "users": [
                {"email": "\u212ate@example.com", "role": "admin"}]}, require_domains=True)
        with self.assertRaises(authz.ConfigError):
            authz.parse_config({"allowed_domains": ["ex\u0430mple.com"], "users": []},
                               require_domains=True)

    def test_invalid_configs(self):
        bad = [
            [],
            {"allowed_domains": ["example.com"], "users": [], "extra": 1},
            {"allowed_domains": ["example.com"], "users": [{"email": "a@example.com"}]},
            {"allowed_domains": ["example.com"], "users": [{"email": "a@example.com", "role": "root"}]},
            {"allowed_domains": ["example.com"],
             "users": [{"github": "a", "role": "reader"}]},
            {"allowed_domains": ["example.com"],
             "users": [{"id": "github:1", "role": "reader"}, {"id": "github:1", "role": "admin"}]},
            {"allowed_domains": ["example.com"],
             "users": [{"email": "a@example.com", "role": "reader"},
                       {"email": "A@example.com", "role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"id": "github:octo", "role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"id": "github:01", "role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"id": "no-separator", "role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"id": 5, "role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"email": "a@example.com", "role": ["admin"]}]},
            {"allowed_domains": ["example.com"], "users": [{"email": "a@example.com", "role": {"a": 1}}]},
            {"allowed_domains": ["example.com"], "users": [{"email": ["a@example.com"], "role": "admin"}]},
            {"allowed_domains": ["example.com"], "users": [{"email": "a b@example.com", "role": "admin"}]},
            {"allowed_domains": "example.com", "users": []},
        ]
        for data in bad:
            with self.subTest(data=data), self.assertRaises(authz.ConfigError):
                authz.parse_config(data, require_domains=True)

    def test_domains_optional_only_when_not_required(self):
        authz.parse_config({"users": []}, require_domains=False)
        with self.assertRaises(authz.ConfigError):
            authz.parse_config({"users": []}, require_domains=True)
        with self.assertRaises(authz.ConfigError):
            authz.parse_config({"allowed_domains": [], "users": []}, require_domains=True)


class StartupTest(_Env):
    def _write(self, data) -> str:
        d = tempfile.mkdtemp(prefix="pcms-authz-test-")
        self.addCleanup(lambda: __import__("shutil").rmtree(d, ignore_errors=True))
        p = Path(d) / "roles.json"
        p.write_text(json.dumps(data) if not isinstance(data, str) else data)
        return str(p)

    def _startup(self, mode: str = "oidc"):
        with mock.patch.object(hs, "OAUTH_MODE", mode), \
                mock.patch.object(hs, "ROLES_CFG", None), \
                contextlib.redirect_stderr(io.StringIO()) as err:
            hs.startup()
            return hs.ROLES_CFG, err.getvalue()

    def test_http_with_no_roles_config_refuses_to_start(self):
        os.environ["MCP_ROLES_FILE"] = str(Path(tempfile.gettempdir()) / "pcms-no-such-roles.json")
        with self.assertRaises(SystemExit):
            self._startup()

    def test_http_with_no_domain_list_refuses_to_start(self):
        os.environ["MCP_ROLES_FILE"] = self._write({"users": [{"id": "github:1", "role": "admin"}]})
        with self.assertRaises(SystemExit):
            self._startup("github")

    def test_http_with_invalid_json_refuses_to_start(self):
        os.environ["MCP_ROLES_FILE"] = self._write("{not json")
        with self.assertRaises(SystemExit):
            self._startup()

    def test_duplicate_json_keys_refuse_to_start(self):
        os.environ["MCP_ROLES_FILE"] = self._write(
            '{"allowed_domains": ["example.com"], "users": ['
            '{"email": "a@example.com", "role": "reader", "role": "admin"}]}')
        with self.assertRaises(SystemExit):
            self._startup()
        with self.assertRaises(authz.ConfigError):
            authz.load_config(Path(os.environ["MCP_ROLES_FILE"]))

    def test_world_writable_roles_file_refuses_to_start(self):
        os.environ["MCP_ROLES_FILE"] = self._write(CONFIG)
        os.chmod(os.environ["MCP_ROLES_FILE"], 0o666)
        with self.assertRaises(SystemExit):
            self._startup()
        os.chmod(os.environ["MCP_ROLES_FILE"], 0o644)
        self._startup()

    def test_non_file_roles_path_refuses_to_start(self):
        os.environ["MCP_ROLES_FILE"] = str(Path(self._write(CONFIG)).parent)
        with self.assertRaises(SystemExit):
            self._startup()

    def test_mcp_approvers_is_refused_over_http(self):
        os.environ["MCP_ROLES_FILE"] = self._write(CONFIG)
        os.environ["MCP_APPROVERS"] = "someone"
        with self.assertRaises(SystemExit):
            self._startup()

    def test_valid_config_starts(self):
        os.environ["MCP_ROLES_FILE"] = self._write(CONFIG)
        cfg, err = self._startup()
        self.assertEqual(cfg["email"]["ada@example.com"], "admin")
        self.assertNotIn("names no approver", err)

    def test_no_approver_warns(self):
        os.environ["MCP_ROLES_FILE"] = self._write(
            {"allowed_domains": ["example.com"],
             "users": [{"email": "a@example.com", "role": "author"}]})
        _, err = self._startup()
        self.assertIn("names no approver", err)


class HttpEndToEndTest(_Env):
    """Real HTTP round trips with token validation stubbed (no network)."""

    def setUp(self) -> None:
        super().setUp()
        os.environ["PCMS_TRANSPORT"] = "http"
        hs._TOKEN_CACHE.clear()
        cfg = authz.parse_config(CONFIG, require_domains=True)
        for target, value in ((hs, "ROLES_CFG"), (hs, "OAUTH_MODE")):
            p = mock.patch.object(target, value, cfg if value == "ROLES_CFG" else "github")
            p.start()
            self.addCleanup(p.stop)
        # The stranger holds the admin's old login on a different account.
        users = {"tok-admin": ("octo-admin", 1002), "tok-reader": ("Octo-Reader", 1001),
                 "tok-stranger": ("octo-admin", 424242)}

        def fake_github(url: str, token: str | None = None) -> dict:
            self.assertEqual(url, "https://api.github.com/user")
            if token not in users:
                raise hs.AuthError("bad token")
            login, uid = users[token]
            return {"login": login, "id": uid, "name": "", "email": "ada@example.com"}
        p = mock.patch.object(hs, "_http_json", fake_github)
        p.start()
        self.addCleanup(p.stop)
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), hs.Handler)
        self.httpd.RequestHandlerClass.log_message = lambda *a, **k: None
        t = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        t.start()
        self.addCleanup(t.join, 5)
        self.addCleanup(self.httpd.server_close)
        self.addCleanup(self.httpd.shutdown)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}/mcp"

    def post(self, token: str, body: dict) -> tuple[int, dict]:
        req = urlreq.Request(self.url, data=json.dumps(body).encode(), method="POST",
                             headers={"Authorization": f"Bearer {token}",
                                      "Content-Type": "application/json"})
        try:
            with urlreq.urlopen(req, timeout=10) as resp:  # noqa: S310 — loopback
                return resp.status, json.loads(resp.read())
        except HTTPError as exc:
            with contextlib.closing(exc):
                return exc.code, json.loads(exc.read() or b"{}")

    def tool(self, token: str, name: str) -> tuple[int, dict]:
        return self.post(token, {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                 "params": {"name": name, "arguments": {}}})

    def test_github_arbitrary_account_is_refused(self):
        code, body = self.tool("tok-stranger", "list_posts")
        self.assertEqual(code, 403)
        self.assertIn("no role", body["error"])
        code, _ = self.post("tok-stranger", {"jsonrpc": "2.0", "id": 1, "method": "initialize"})
        self.assertEqual(code, 403)

    def test_github_profile_email_grants_nothing(self):
        # Same login and profile email as an admin; the numeric id decides.
        code, _ = self.tool("tok-stranger", "build_site")
        self.assertEqual(code, 403)

    def test_github_token_without_numeric_id_is_401(self):
        with mock.patch.object(hs, "_http_json",
                               lambda u, t=None: {"login": "octo-admin", "id": "1002"}):
            code, _ = self.tool("tok-other", "list_posts")
        self.assertEqual(code, 401)

    def test_non_string_tool_name_gets_a_json_rpc_error(self):
        for params in ({"name": 5}, {"name": ["list_posts"]}, {"name": None},
                       {"name": "list_posts", "arguments": [1]},
                       {"name": "list_posts", "arguments": "x"}, ["list_posts"]):
            with self.subTest(params=params):
                code, body = self.post("tok-admin", {"jsonrpc": "2.0", "id": 7,
                                                     "method": "tools/call", "params": params})
                self.assertEqual(code, 200)
                self.assertEqual(body["error"]["code"], -32602)

    # ---- request body limits

    def raw(self, token: str, headers: dict, body: bytes = b"",
            timeout: float = 10) -> tuple[int, str]:
        """Send exact bytes; return (status, Connection header)."""
        port = self.httpd.server_address[1]
        lines = [f"POST /mcp HTTP/1.1", f"Host: 127.0.0.1:{port}",
                 f"Authorization: Bearer {token}"] + [f"{k}: {v}" for k, v in headers.items()]
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as sock:
            sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode() + body)
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                data += chunk
        head = data.split(b"\r\n\r\n", 1)[0].decode()
        status = int(head.split(" ", 2)[1])
        conn = next((ln.split(":", 1)[1].strip() for ln in head.split("\r\n")[1:]
                     if ln.lower().startswith("connection:")), "")
        return status, conn

    def test_oversized_declared_body_is_413_before_reading(self):
        # 300 MB declared, nothing sent: refused from the header alone.
        t0 = time.monotonic()
        status, conn = self.raw("tok-reader", {"Content-Length": str(300 * 1024 * 1024)})
        self.assertEqual((status, conn), (413, "close"))
        self.assertLess(time.monotonic() - t0, 5)

    def test_reader_body_cap_is_one_mib(self):
        status, _ = self.raw("tok-reader", {"Content-Length": str(hs.MAX_BODY_BYTES + 1)})
        self.assertEqual(status, 413)

    def test_admin_may_send_a_large_add_media_call_only(self):
        pad = "A" * (2 * 1024 * 1024)
        code, body = self.post("tok-admin", {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                             "params": {"name": "add_media",
                                                        "arguments": {"content_base64": pad}}})
        self.assertEqual(code, 200)
        self.assertFalse(body["result"]["isError"])
        code, _ = self.post("tok-admin", {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                          "params": {"name": "list_posts",
                                                     "arguments": {"pad": pad}}})
        self.assertEqual(code, 413)

    def test_admin_cap_follows_media_cap(self):
        os.environ["MCP_MEDIA_MAX_BYTES"] = str(3 * 1024 * 1024)
        cap = hs.max_body_bytes("admin")
        self.assertEqual(cap, -(-3 * 1024 * 1024 * 4 // 3) + 64 * 1024)
        status, _ = self.raw("tok-admin", {"Content-Length": str(cap + 1)})
        self.assertEqual(status, 413)
        os.environ["MCP_MEDIA_MAX_BYTES"] = "1000"
        self.assertEqual(hs.max_body_bytes("admin"), hs.MAX_BODY_BYTES)
        for role in ("reader", "author", "approver", None):
            self.assertEqual(hs.max_body_bytes(role), hs.MAX_BODY_BYTES)

    def test_bad_framing_is_400(self):
        for headers in ({"Transfer-Encoding": "chunked"},
                        {"Content-Length": "-1"}, {"Content-Length": "1e3"},
                        {"Content-Length": "abc"}):
            with self.subTest(headers=headers):
                status, conn = self.raw("tok-reader", headers)
                self.assertEqual((status, conn), (400, "close"))

    def test_slow_body_times_out(self):
        with mock.patch.object(hs.Handler, "timeout", 0.5):
            status, conn = self.raw("tok-reader", {"Content-Length": "100"}, b"{", timeout=10)
        self.assertEqual((status, conn), (408, "close"))

    def test_bad_token_is_401(self):
        code, _ = self.tool("nope", "list_posts")
        self.assertEqual(code, 401)

    def test_reader_reads_but_cannot_approve(self):
        code, body = self.tool("tok-reader", "list_posts")
        self.assertEqual(code, 200)
        self.assertFalse(body["result"]["isError"])
        code, body = self.tool("tok-reader", "approve_proposal")
        self.assertEqual(code, 200)
        self.assertTrue(_forbidden(body))

    def test_admin_direct_commit_off_by_default(self):
        _, body = self.tool("tok-admin", "update_post")
        self.assertTrue(_forbidden(body))
        _, body = self.tool("tok-admin", "approve_proposal")
        self.assertFalse(body["result"]["isError"])

    def test_no_roles_loaded_refuses_everyone(self):
        with mock.patch.object(hs, "ROLES_CFG", None):
            code, _ = self.tool("tok-admin", "list_posts")
        self.assertEqual(code, 403)


if __name__ == "__main__":
    unittest.main()

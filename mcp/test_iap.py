"""Tests for OAUTH_MODE=iap (mcp/iap.py and its wiring in http_server.py).

Standard library only, no network: keys are generated locally and the JWKS
fetch is a stub. Run:  python3 -m unittest discover -s mcp -p 'test_*.py'
"""
from __future__ import annotations

import base64
import hashlib
import hmac
import http.client
import importlib.util
import json
import os
import secrets
import subprocess
import sys
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
AUD = "/projects/123/global/backendServices/456"
NOW = 1_800_000_000


def _load(name: str, file: str):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


iap = _load("iap_under_test", "iap.py")


# ---- test-only signing (the module under test has none) ---------------------

def _mul(k: int, pt):
    acc = None
    while k:
        if k & 1:
            acc = iap._add(acc, pt)
        pt = iap._add(pt, pt)
        k >>= 1
    return acc


class Key:
    def __init__(self, kid: str, d: int | None = None):
        self.kid = kid
        self.d = d or (secrets.randbelow(iap.N - 1) + 1)
        self.pub = _mul(self.d, iap.G)

    def sign(self, msg: bytes) -> bytes:
        e = int.from_bytes(hashlib.sha256(msg).digest(), "big")
        while True:
            k = secrets.randbelow(iap.N - 1) + 1
            r = _mul(k, iap.G)[0] % iap.N
            s = pow(k, -1, iap.N) * (e + r * self.d) % iap.N
            if r and s:
                return r.to_bytes(32, "big") + s.to_bytes(32, "big")

    def jwk(self) -> dict:
        return {"kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": self.kid,
                "x": b64(self.pub[0].to_bytes(32, "big")),
                "y": b64(self.pub[1].to_bytes(32, "big"))}


def b64(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def b64j(obj) -> str:
    return b64(json.dumps(obj).encode())


def claims(**over) -> dict:
    c = {"iss": iap.IAP_ISSUER, "aud": AUD, "iat": NOW - 10, "exp": NOW + 590,
         "sub": "accounts.google.com:1234567890", "email": "Alice@Example.com"}
    c.update(over)
    return {k: v for k, v in c.items() if v is not None}


def token(key: Key, payload: dict | None = None, header: dict | None = None) -> str:
    h = {"alg": "ES256", "typ": "JWT", "kid": key.kid}
    h.update(header or {})
    signing = f"{b64j(h)}.{b64j(payload if payload is not None else claims())}"
    return f"{signing}.{b64(key.sign(signing.encode()))}"


class FakeJwks:
    def __init__(self, *keys: Key):
        self.keys = list(keys)
        self.calls = 0
        self.fail = False

    def __call__(self, url: str) -> bytes:
        self.calls += 1
        if self.fail:
            raise OSError("network down")
        return json.dumps({"keys": [k.jwk() for k in self.keys]}).encode()


class Clock:
    def __init__(self, t: float = 1000.0):
        self.t = t

    def __call__(self) -> float:
        return self.t


def verifier(*keys: Key, fetch=None, clock=None, min_refresh=30.0, ttl=3600.0, mod=None):
    mod = mod or iap
    fetch = fetch or FakeJwks(*keys)
    jwks = mod.JwksCache(iap.DEFAULT_JWKS_URL, fetch=fetch, ttl=ttl,
                         min_refresh=min_refresh, clock=clock or Clock())
    return mod.IapVerifier(AUD, jwks, skew=30.0, now=lambda: NOW), fetch


# ---- ECDSA ---------------------------------------------------------------------

class EcdsaTest(unittest.TestCase):
    # RFC 6979 A.2.5 (P-256, SHA-256): an outside known answer, so the verifier
    # is not only checked against the test's own signer.
    UX = 0x60FED4BA255A9D31C961EB74C6356D68C049B8923B61FA6CE669622E60F29FB6
    UY = 0x7903FE1008B8BC99A41AE9E95628BC64F2F1B20C2D7E9F5177A3C294D4462299
    D = 0xC9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721
    R = 0xEFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716
    S = 0xF7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8

    def sig(self, r, s, n=32):
        return r.to_bytes(n, "big") + s.to_bytes(n, "big")

    def test_rfc6979_vector(self):
        q = (self.UX, self.UY)
        self.assertEqual(_mul(self.D, iap.G), q)
        self.assertTrue(iap.ecdsa_p256_verify(q, b"sample", self.sig(self.R, self.S)))
        self.assertFalse(iap.ecdsa_p256_verify(q, b"sample!", self.sig(self.R, self.S)))
        r2 = 0xF1ABB023518351CD71D881567B1EA663ED3EFCF6C5132B354F28D3B0B7D38367
        s2 = 0x019F4113742A2B14BD25926B49C649155F267E60D3814B4C0CC84250E46F0083
        self.assertTrue(iap.ecdsa_p256_verify(q, b"test", self.sig(r2, s2)))

    def test_r_s_out_of_range(self):
        q = (self.UX, self.UY)
        for r, s in [(0, self.S), (self.R, 0), (0, 0), (iap.N, self.S), (self.R, iap.N),
                     (self.R + iap.N, self.S) if self.R + iap.N < 2**256 else (iap.N + 1, self.S),
                     (2**256 - 1, self.S), (self.R, 2**256 - 1)]:
            self.assertFalse(iap.ecdsa_p256_verify(q, b"sample", self.sig(r, s)), (r, s))

    def test_signature_length(self):
        q = (self.UX, self.UY)
        good = self.sig(self.R, self.S)
        for bad in (b"", good[:63], good + b"\x00", b"\x00" + good):
            self.assertFalse(iap.ecdsa_p256_verify(q, b"sample", bad))

    def test_invalid_public_keys(self):
        good = self.sig(self.R, self.S)
        for q in [(0, 0), (self.UX, (self.UY + 1) % iap.P), (self.UX + iap.P, self.UY),
                  (self.UX, self.UY + iap.P), (-1, self.UY), None]:
            self.assertFalse(iap.ecdsa_p256_verify(q, b"sample", good), q)

    def test_point_at_infinity_result(self):
        # Q = G: u1*G + u2*G = (e + r) * w * G is the point at infinity when
        # r = n - e. Must be rejected, not crash.
        msg = b"infinity"
        e = int.from_bytes(hashlib.sha256(msg).digest(), "big") % iap.N
        r = (iap.N - e) % iap.N
        self.assertTrue(1 <= r < iap.N)
        self.assertFalse(iap.ecdsa_p256_verify(iap.G, msg, self.sig(r, 1)))
        self.assertFalse(iap.ecdsa_p256_verify(iap.G, msg, self.sig(r, 12345)))

    def test_own_signer_round_trip(self):
        k = Key("k")
        sig = k.sign(b"m")
        self.assertTrue(iap.ecdsa_p256_verify(k.pub, b"m", sig))
        self.assertFalse(iap.ecdsa_p256_verify(Key("other").pub, b"m", sig))

    def test_jwk_rejects_bad_keys(self):
        good = Key("k").jwk()
        self.assertIsNotNone(iap.jwk_to_point(good))
        for over in ({"crv": "P-384"}, {"kty": "RSA"}, {"alg": "RS256"}, {"use": "enc"},
                     {"x": good["y"]}, {"x": b64(b"\x00" * 31)}, {"y": "!!!"}):
            self.assertIsNone(iap.jwk_to_point({**good, **over}), over)


# ---- token verification --------------------------------------------------------

class VerifyTest(unittest.TestCase):
    def setUp(self):
        self.key = Key("key-1")
        self.v, self.fetch = verifier(self.key)

    def bad(self, tok, why=None):
        with self.assertRaises(iap.IapError) as cm:
            self.v.verify(tok)
        if why:
            self.assertIn(why, str(cm.exception))

    def test_valid(self):
        ident = self.v.verify(token(self.key))
        self.assertEqual(ident["email"], "alice@example.com")
        self.assertEqual(ident["login"], "alice@example.com")
        self.assertEqual(ident["sub"], "accounts.google.com:1234567890")
        self.assertEqual(ident["source"], "iap:alice@example.com")

    def test_wrong_audience(self):
        self.bad(token(self.key, claims(aud="/projects/999/global/backendServices/456")), "audience")
        self.bad(token(self.key, claims(aud=[AUD])), "audience")
        self.bad(token(self.key, claims(aud=None)), "audience")

    def test_wrong_issuer(self):
        self.bad(token(self.key, claims(iss="https://accounts.google.com")), "issuer")
        self.bad(token(self.key, claims(iss=None)), "issuer")

    def test_expired(self):
        self.bad(token(self.key, claims(exp=NOW - 31)), "expired")
        self.v.verify(token(self.key, claims(exp=NOW - 29)))  # inside skew
        self.bad(token(self.key, claims(exp=None)), "exp")
        self.bad(token(self.key, claims(exp="9999999999")), "exp")

    def test_not_yet_valid(self):
        self.bad(token(self.key, claims(nbf=NOW + 31)), "not yet valid")
        self.v.verify(token(self.key, claims(nbf=NOW + 29)))
        self.bad(token(self.key, claims(iat=NOW + 31)), "future")
        self.bad(token(self.key, claims(iat=None)), "iat")

    def test_alg_none(self):
        h, p = b64j({"alg": "none", "kid": "key-1"}), b64j(claims())
        self.bad(f"{h}.{p}.", "alg")
        self.bad(f"{h}.{p}", "compact")

    def test_alg_hs256(self):
        # Key confusion: HMAC with the public key bytes as the secret.
        h, p = b64j({"alg": "HS256", "kid": "key-1"}), b64j(claims())
        secret = json.dumps(self.key.jwk()).encode()
        mac = hmac.new(secret, f"{h}.{p}".encode(), hashlib.sha256).digest()
        self.bad(f"{h}.{p}.{b64(mac)}", "alg")
        for alg in ("es256", "ES384", "RS256", None):
            self.bad(token(self.key, header={"alg": alg}), "alg")

    def test_tampered_payload(self):
        h, _, s = token(self.key).split(".")
        forged = b64j(claims(email="admin@example.com"))
        self.bad(f"{h}.{forged}.{s}", "signature")

    def test_tampered_signature(self):
        h, p, s = token(self.key).split(".")
        raw = bytearray(base64.urlsafe_b64decode(s + "=="))
        raw[10] ^= 1
        self.bad(f"{h}.{p}.{b64(bytes(raw))}", "signature")
        self.bad(f"{h}.{p}.{b64(bytes(64))}", "signature")
        self.bad(f"{h}.{p}.{s}x", None)

    def test_signed_by_unlisted_key(self):
        self.bad(token(Key("key-1")), "signature")  # same kid, different key

    def test_header_and_claims_shape(self):
        self.bad("", "missing")
        self.bad("a.b.c.d", "compact")
        self.bad(token(self.key, header={"crit": ["exp"]}), "crit")
        self.bad(token(self.key, header={"kid": None}), "kid")
        self.bad(token(self.key, header={"kid": "x" * 200}), "kid")
        self.bad(token(self.key, claims(email=None)), "email")
        self.bad(token(self.key, claims(email="")), "email")
        self.bad(token(self.key, claims(sub=None)), "sub")
        self.bad("x" * 9000, "oversized")

    def test_audience_required(self):
        with self.assertRaises(ValueError):
            iap.IapVerifier("", self.v.jwks)


# ---- JWKS cache ------------------------------------------------------------------

class JwksTest(unittest.TestCase):
    def test_unknown_kid_refreshes_once(self):
        old, new = Key("old"), Key("new")
        clock = Clock()
        v, fetch = verifier(old, clock=clock)
        v.verify(token(old))
        self.assertEqual(fetch.calls, 1)
        fetch.keys.append(new)                 # the proxy rotates keys
        clock.t += 31                          # past the rate limit
        v.verify(token(new))
        self.assertEqual(fetch.calls, 2)
        v.verify(token(new))
        v.verify(token(old))
        self.assertEqual(fetch.calls, 2)       # cached after that

    def test_unknown_kid_flood_is_rate_limited(self):
        key = Key("k")
        clock = Clock()
        v, fetch = verifier(key, clock=clock)
        v.verify(token(key))
        for i in range(200):
            with self.assertRaises(iap.IapError):
                v.verify(token(Key(f"made-up-{i}")))
        self.assertEqual(fetch.calls, 1)       # no refresh inside min_refresh
        clock.t += 31
        for i in range(200):
            with self.assertRaises(iap.IapError):
                v.verify(token(Key(f"again-{i}")))
        self.assertEqual(fetch.calls, 2)       # one refresh per window
        v.verify(token(key))                   # known kid still served

    def test_flood_from_threads(self):
        key = Key("k")
        v, fetch = verifier(key)
        v.verify(token(key))
        bogus = [token(Key(f"t{i}")) for i in range(8)]

        def run(t):
            for _ in range(20):
                try:
                    v.verify(t)
                except iap.IapError:
                    pass
        threads = [threading.Thread(target=run, args=(t,)) for t in bogus]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(fetch.calls, 1)

    def test_ttl_refresh_and_failure_keeps_keys(self):
        key = Key("k")
        clock = Clock()
        v, fetch = verifier(key, clock=clock, ttl=100.0)
        v.verify(token(key))
        clock.t += 101
        fetch.fail = True
        v.verify(token(key))                   # stale keys kept on fetch failure
        self.assertEqual(fetch.calls, 2)
        v.verify(token(key))                   # retry rate limited
        self.assertEqual(fetch.calls, 2)
        fetch.fail = False
        clock.t += 31
        v.verify(token(key))
        self.assertEqual(fetch.calls, 3)

    def test_first_fetch_failure_rejects(self):
        key = Key("k")
        fetch = FakeJwks(key)
        fetch.fail = True
        v, _ = verifier(key, fetch=fetch)
        with self.assertRaises(iap.IapError):
            v.verify(token(key))

    def test_fetch_refuses_non_https(self):
        for url in ("http://127.0.0.1:1/jwks", "file:///etc/passwd", "ftp://x/y", ""):
            with self.assertRaises(iap.IapError):
                iap.fetch_https(url)

    def test_invalid_jwks_entries_skipped(self):
        key = Key("good")
        bad = {**Key("bad").jwk(), "y": Key("x").jwk()["y"]}   # off the curve
        fetch = lambda url: json.dumps({"keys": [bad, key.jwk(), "junk"]}).encode()  # noqa: E731
        v, _ = verifier(key, fetch=fetch)
        v.verify(token(key))
        self.assertIsNone(v.jwks.get("bad"))


# ---- HTTP front and approvals ------------------------------------------------------

def _load_http_server():
    os.environ["OAUTH_MODE"] = "iap"
    os.environ["IAP_AUDIENCE"] = AUD
    try:
        return _load("http_server_under_test", "http_server.py")
    finally:
        for k in ("OAUTH_MODE", "IAP_AUDIENCE"):
            os.environ.pop(k, None)


class HttpIapTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.hs = _load_http_server()
        cls.key = Key("key-1")
        cls.hs._IAP_VERIFIER, _ = verifier(cls.key, mod=cls.hs.iap)
        cls.seen: list = []

        def handle(req):
            cls.seen.append(cls.hs.srv._HTTP_IDENTITY.get())
            return {"jsonrpc": "2.0", "id": req.get("id"), "result": {}}
        cls.hs.srv.handle_request = handle
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), cls.hs.Handler)
        cls.port = cls.httpd.server_address[1]
        cls.thread = threading.Thread(target=cls.httpd.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.httpd.server_close()
        cls.thread.join(5)

    def post(self, headers: dict):
        self.seen.clear()
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        conn.request("POST", "/mcp", body, {"Content-Type": "application/json", **headers})
        resp = conn.getresponse()
        data = resp.read()
        conn.close()
        return resp, data

    def assert_401(self, headers: dict):
        resp, data = self.post(headers)
        self.assertEqual(resp.status, 401)
        self.assertEqual(json.loads(data), {"error": "unauthorized"})  # no detail
        self.assertIsNone(resp.getheader("WWW-Authenticate"))
        self.assertEqual(resp.getheader("Connection"), "close")
        self.assertEqual(self.seen, [])

    def test_valid_assertion_binds_identity(self):
        resp, _ = self.post({iap.IAP_HEADER: token(self.key)})
        self.assertEqual(resp.status, 200)
        self.assertEqual(self.seen[0]["email"], "alice@example.com")
        self.assertEqual(self.seen[0]["sub"], "accounts.google.com:1234567890")

    def test_missing_header(self):
        self.assert_401({})

    def test_invalid_assertions_same_401(self):
        h, p, s = token(self.key).split(".")
        for tok in (f"{h}.{b64j(claims(email='bob@example.com'))}.{s}",
                    token(self.key, claims(aud="other")),
                    token(self.key, claims(exp=NOW - 3600)),
                    "garbage"):
            self.assert_401({iap.IAP_HEADER: tok})

    def test_bearer_refused(self):
        self.assert_401({"Authorization": f"Bearer {token(self.key)}"})
        self.assert_401({"Authorization": "Bearer ghp_example"})
        with self.assertRaises(self.hs.AuthError):
            self.hs.validate_token(token(self.key))

    def test_bearer_ignored_next_to_assertion(self):
        resp, _ = self.post({iap.IAP_HEADER: token(self.key, claims(email="carol@example.com")),
                             "Authorization": "Bearer something-else"})
        self.assertEqual(resp.status, 200)
        self.assertEqual(self.seen[0]["email"], "carol@example.com")

    def test_verifier_fault_is_401(self):
        saved = self.hs._IAP_VERIFIER

        class Broken:
            def verify(self, assertion):
                raise KeyError("boom")
        self.hs._IAP_VERIFIER = Broken()
        try:
            self.assert_401({iap.IAP_HEADER: token(self.key)})
        finally:
            self.hs._IAP_VERIFIER = saved

    def test_refused_body_is_not_a_second_request(self):
        # The unread body of a refused request must not be parsed as the
        # next request on the same connection.
        import socket
        smuggled = b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}"
        req = (b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: %d\r\n\r\n" % len(smuggled)
               + smuggled)
        with socket.create_connection(("127.0.0.1", self.port), timeout=10) as sock:
            sock.sendall(req)
            data = b""
            while chunk := sock.recv(65536):
                data += chunk
        self.assertEqual(data.count(b"HTTP/1.1 "), 1)
        self.assertIn(b"401", data.split(b"\r\n", 1)[0])

    def test_metadata_not_served(self):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        conn.request("GET", "/.well-known/oauth-protected-resource")
        self.assertEqual(conn.getresponse().status, 404)
        conn.close()


class StartupTest(unittest.TestCase):
    def run_server(self, env: dict):
        full = {k: v for k, v in os.environ.items()
                if k not in ("OAUTH_MODE", "IAP_AUDIENCE", "IAP_JWKS_URL")}
        full.update(env, HOST="127.0.0.1", PORT="0")
        return subprocess.run([sys.executable, "-I", str(HERE / "http_server.py")], env=full,
                              capture_output=True, text=True, timeout=30)

    def test_refuses_without_audience(self):
        res = self.run_server({"OAUTH_MODE": "iap"})
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("IAP_AUDIENCE", res.stderr)

    def test_refuses_non_https_jwks(self):
        res = self.run_server({"OAUTH_MODE": "iap", "IAP_AUDIENCE": AUD,
                               "IAP_JWKS_URL": "http://127.0.0.1:1/jwks"})
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("https", res.stderr)

    def test_refuses_unknown_mode(self):
        res = self.run_server({"OAUTH_MODE": "iapp"})
        self.assertNotEqual(res.returncode, 0)


class ApproverTest(unittest.TestCase):
    """The author/approver check, unchanged, on top of IAP identities."""

    @classmethod
    def setUpClass(cls):
        cls.hs = _load_http_server()
        cls.srv = cls.hs.srv
        cls.key = Key("key-1")
        cls.v, _ = verifier(cls.key, mod=cls.hs.iap)

    def setUp(self):
        self.approved: list = []
        srv = self.srv
        self.saved = (srv._proposal_branch, srv._author_of, srv._approve_one,
                      os.environ.get("MCP_ALLOW_APPROVE"),
                      os.environ.get("REQUIRE_DISTINCT_APPROVER"))
        srv._proposal_branch = lambda slug: f"agent/{slug}-1"
        srv._author_of = lambda branch: ("Alice", "Alice@Example.com")

        def approve_one(slug, name, email, source):
            self.approved.append((slug, name, email, source))
            return {"slug": slug, "merged": True}
        srv._approve_one = approve_one
        os.environ["MCP_ALLOW_APPROVE"] = "1"
        os.environ.pop("REQUIRE_DISTINCT_APPROVER", None)

    def tearDown(self):
        srv = self.srv
        srv._proposal_branch, srv._author_of, srv._approve_one, allow, distinct = self.saved
        for k, v in (("MCP_ALLOW_APPROVE", allow), ("REQUIRE_DISTINCT_APPROVER", distinct)):
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v

    def as_user(self, email: str, args: dict):
        ident = self.v.verify(token(self.key, claims(email=email)))
        tok = self.srv._HTTP_IDENTITY.set(ident)
        try:
            return self.srv.tool_approve_proposal({"slug": "post", "deploy": False, **args})
        finally:
            self.srv._HTTP_IDENTITY.reset(tok)

    def test_author_cannot_approve_own_proposal(self):
        with self.assertRaises(ValueError) as cm:
            self.as_user("ALICE@example.com", {})
        self.assertIn("distinct approver", str(cm.exception))
        self.assertEqual(self.approved, [])

    def test_other_user_approves_with_iap_identity(self):
        out = self.as_user("Bob@Example.com", {})
        self.assertEqual(self.approved,
                         [("post", "bob@example.com", "bob@example.com", "iap:bob@example.com")])
        self.assertEqual(out["approver"]["source"], "iap:bob@example.com")

    def test_declared_approver_must_match_iap_identity(self):
        with self.assertRaises(ValueError):
            self.as_user("bob@example.com", {"approver_email": "carol@example.com"})
        self.as_user("bob@example.com", {"approver_email": "bob@example.com"})
        self.assertEqual(len(self.approved), 1)

    def test_no_identity_no_approval(self):
        # Outside a request there is no IAP identity; approval must not fall
        # back to free text. (Locally it would ask gh; make that fail.)
        saved = self.srv._GH_IDENTITY_CACHE
        self.srv._GH_IDENTITY_CACHE = None
        real_run = self.srv.subprocess.run
        self.srv.subprocess.run = lambda *a, **k: subprocess.CompletedProcess(a, 1, "", "no gh")
        try:
            with self.assertRaises(ValueError):
                self.srv.tool_approve_proposal({"slug": "post", "deploy": False,
                                                "approver_email": "bob@example.com"})
        finally:
            self.srv.subprocess.run = real_run
            self.srv._GH_IDENTITY_CACHE = saved
        self.assertEqual(self.approved, [])


if __name__ == "__main__":
    unittest.main()

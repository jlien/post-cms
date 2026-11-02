"""Identity-Aware Proxy (IAP) assertion verification, standard library only.

An identity-aware proxy in front of the server signs every request it
forwards with a short-lived ES256 JWT in the `x-goog-iap-jwt-assertion`
header. This module verifies that JWT without third-party packages:

- ECDSA P-256 / SHA-256 verification over the raw r||s signature (JWS form);
- the key comes from a cached JWKS, refreshed on a TTL and on an unknown
  `kid`, with unknown-kid refreshes rate limited so a flood of made-up kids
  cannot turn into a flood of fetches;
- `alg` must be exactly ES256, `iss` the proxy issuer, `aud` the configured
  audience, and `exp` / `nbf` / `iat` hold within a small clock skew.

Verification only touches public data (the token and the proxy's public
keys), so no constant-time arithmetic is needed. There is no signing code
here; tests carry their own.
"""
from __future__ import annotations

import base64
import hashlib
import json
import re
import sys
import threading
import time
from typing import Any, Callable

IAP_HEADER = "x-goog-iap-jwt-assertion"
IAP_ISSUER = "https://cloud.google.com/iap"
DEFAULT_JWKS_URL = "https://www.gstatic.com/iap/verify/public_key-jwk"

# ---- P-256 (secp256r1, FIPS 186-4 D.1.2.3) --------------------------------

P = 0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF
A = P - 3
B = 0x5AC635D8AA3A93E7B3EBBD55769886BC651D06B0CC53B0F63BCE3C3E27D2604B
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551
G = (0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
     0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5)



def on_curve(pt: tuple[int, int]) -> bool:
    x, y = pt
    if not (0 <= x < P and 0 <= y < P):
        return False
    return (y * y - (x * x * x + A * x + B)) % P == 0


def _add(p1, p2):
    if p1 is None:
        return p2
    if p2 is None:
        return p1
    x1, y1 = p1
    x2, y2 = p2
    if x1 == x2:
        if (y1 + y2) % P == 0:
            return None  # P + (-P), including doubling a point with y == 0
        lam = (3 * x1 * x1 + A) * pow(2 * y1, -1, P) % P
    else:
        lam = (y2 - y1) * pow(x2 - x1, -1, P) % P
    x3 = (lam * lam - x1 - x2) % P
    return (x3, (lam * (x1 - x3) - y1) % P)


def _mul2(k1: int, p1, k2: int, p2):
    """k1*p1 + k2*p2 (Shamir's trick); public scalars only."""
    p12 = _add(p1, p2)
    acc = None
    for i in range(max(k1.bit_length(), k2.bit_length()) - 1, -1, -1):
        acc = _add(acc, acc)
        b1, b2 = (k1 >> i) & 1, (k2 >> i) & 1
        if b1 and b2:
            acc = _add(acc, p12)
        elif b1:
            acc = _add(acc, p1)
        elif b2:
            acc = _add(acc, p2)
    return acc


def ecdsa_p256_verify(pub: tuple[int, int], msg: bytes, sig: bytes) -> bool:
    """Verify an ES256 signature (raw 64-byte r||s) over msg. Never raises on
    bad input: anything malformed is simply not a valid signature."""
    if not isinstance(sig, (bytes, bytearray)) or len(sig) != 64:
        return False
    if pub is None or not on_curve(pub):
        return False
    r = int.from_bytes(sig[:32], "big")
    s = int.from_bytes(sig[32:], "big")
    if not (1 <= r < N and 1 <= s < N):
        return False
    e = int.from_bytes(hashlib.sha256(msg).digest(), "big")  # 256 bits = |n|
    w = pow(s, -1, N)
    pt = _mul2(e * w % N, G, r * w % N, pub)
    if pt is None:
        return False
    return pt[0] % N == r


# ---- JWT / JWK parsing -----------------------------------------------------

_B64URL = re.compile(r"[A-Za-z0-9_-]*")


class IapError(Exception):
    """The assertion is missing or invalid. The message is for the server
    log only; clients get a bare 401."""


def b64url_decode(s: str) -> bytes:
    """Strict, canonical base64url without padding: the input must be what
    encoding the decoded bytes gives back, so one value has one spelling."""
    if not isinstance(s, str) or not _B64URL.fullmatch(s) or len(s) % 4 == 1:
        raise IapError("bad base64url")
    raw = base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))
    if base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii") != s:
        raise IapError("non-canonical base64url")
    return raw


MAX_JSON_DEPTH = 32


def json_depth_ok(text: str | bytes, limit: int = MAX_JSON_DEPTH) -> bool:
    """True when the JSON text nests arrays/objects at most `limit` deep.
    Checked before json.loads, so a deep document cannot recurse."""
    if isinstance(text, (bytes, bytearray)):
        text = text.decode("utf-8", "replace")
    depth = 0
    in_str = esc = False
    for ch in text:
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
        elif ch == '"':
            in_str = True
        elif ch in "[{":
            depth += 1
            if depth > limit:
                return False
        elif ch in "]}":
            depth -= 1
    return True


def _reject_constant(name: str):
    raise ValueError(f"{name} is not JSON")


def loads_bounded(text: str | bytes) -> Any:
    """json.loads with a nesting bound and no NaN/Infinity. Raises ValueError."""
    if not json_depth_ok(text):
        raise ValueError("JSON nested too deeply")
    return json.loads(text, parse_constant=_reject_constant)


def jwk_to_point(jwk: Any) -> tuple[int, int] | None:
    """A P-256 signing JWK → validated public point, else None."""
    if not isinstance(jwk, dict):
        return None
    if jwk.get("kty") != "EC" or jwk.get("crv") != "P-256":
        return None
    if jwk.get("alg", "ES256") != "ES256" or jwk.get("use", "sig") != "sig":
        return None
    try:
        x = b64url_decode(jwk.get("x"))
        y = b64url_decode(jwk.get("y"))
    except IapError:
        return None
    if len(x) != 32 or len(y) != 32:
        return None
    pt = (int.from_bytes(x, "big"), int.from_bytes(y, "big"))
    return pt if on_curve(pt) else None


MAX_NUMERIC_DATE = 9_999_999_999


def _numeric_date(v: Any) -> bool:
    """A JWT NumericDate as the proxy sends it: an integer in [1, 9999999999].
    Floats, NaN, Infinity, booleans and strings are refused."""
    return type(v) is int and 1 <= v <= MAX_NUMERIC_DATE


# ---- JWKS cache --------------------------------------------------------------

MAX_JWKS_BYTES = 64 * 1024
MAX_KEYS = 32
MAX_KID_LEN = 128


def fetch_https(url: str, timeout: float = 10.0) -> bytes:
    """GET an https URL, following no redirects (no scheme downgrade)."""
    from urllib import request as urlreq

    if not isinstance(url, str) or not url.startswith("https://"):
        raise IapError("jwks url must be https")

    class _NoRedirect(urlreq.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None

    opener = urlreq.build_opener(_NoRedirect)
    req = urlreq.Request(url, headers={"Accept": "application/json"})
    with opener.open(req, timeout=timeout) as resp:  # noqa: S310 - https checked above
        body = resp.read(MAX_JWKS_BYTES + 1)
    if len(body) > MAX_JWKS_BYTES:
        raise IapError("jwks too large")
    return body


class JwksCache:
    """kid → public point. Refreshes when older than `ttl`, or on an unknown
    kid at most once per `min_refresh` seconds. A failed fetch keeps the keys
    already held. Lookups read a snapshot and never wait on a fetch they do
    not need."""

    def __init__(self, url: str, fetch: Callable[[str], bytes] = fetch_https,
                 ttl: float = 3600.0, min_refresh: float = 30.0,
                 clock: Callable[[], float] = time.monotonic,
                 max_stale: float = 86400.0,
                 log: Callable[[str], None] = lambda m: sys.stderr.write(m + "\n")):
        self.url = url
        self._fetch = fetch
        self.ttl = ttl
        self.max_stale = max_stale  # beyond ttl, while fetches fail
        self._log = log
        self._expired_logged = False
        self.min_refresh = min_refresh
        self._clock = clock
        self._keys: dict[str, tuple[int, int]] = {}
        self._loaded_at: float | None = None   # last successful fetch
        self._tried_at: float | None = None    # last attempt, success or not
        self._lock = threading.Lock()
        self.fetches = 0

    def _refresh_locked(self) -> None:
        self._tried_at = self._clock()
        self.fetches += 1
        try:
            doc = loads_bounded(self._fetch(self.url))
        except Exception:  # noqa: BLE001 - network or parse failure keeps old keys
            return
        keys: dict[str, tuple[int, int]] = {}
        for jwk in (doc.get("keys") if isinstance(doc, dict) else None) or []:
            if len(keys) >= MAX_KEYS:
                break
            kid = jwk.get("kid") if isinstance(jwk, dict) else None
            pt = jwk_to_point(jwk)
            if isinstance(kid, str) and 0 < len(kid) <= MAX_KID_LEN and pt:
                keys[kid] = pt
        if keys:
            self._keys = keys
            self._loaded_at = self._tried_at
            self._expired_logged = False

    def _may_retry(self, now: float) -> bool:
        return self._tried_at is None or now - self._tried_at >= self.min_refresh

    def get(self, kid: str) -> tuple[int, int] | None:
        now = self._clock()
        stale = self._loaded_at is None or now - self._loaded_at >= self.ttl
        pt = self._keys.get(kid)
        if pt is not None and not stale:
            return pt
        with self._lock:
            now = self._clock()
            stale = self._loaded_at is None or now - self._loaded_at >= self.ttl
            pt = self._keys.get(kid)
            if (pt is None or stale) and self._may_retry(now):
                self._refresh_locked()
            if self._loaded_at is not None and \
                    self._clock() - self._loaded_at >= self.ttl + self.max_stale:
                # Fetches have failed for longer than the stale cap: the
                # held keys may have been rotated out. Fail closed.
                if not self._expired_logged:
                    self._expired_logged = True
                    self._log("iap: JWKS keys expired (no successful fetch within "
                              "the TTL plus IAP_JWKS_MAX_STALE_SECS); refusing tokens")
                return None
            return self._keys.get(kid)


# ---- verifier ----------------------------------------------------------------

class IapVerifier:
    def __init__(self, audience: str, jwks: JwksCache, skew: float = 30.0,
                 now: Callable[[], float] = time.time):
        if not audience:
            raise ValueError("IAP_AUDIENCE is required")
        self.audience = audience
        self.jwks = jwks
        self.skew = skew
        self._now = now

    def verify(self, assertion: str) -> dict[str, Any]:
        """Assertion → {login, name, email, sub, source}. Raises IapError."""
        if not assertion or not isinstance(assertion, str) or len(assertion) > 8192:
            raise IapError("missing or oversized assertion")
        parts = assertion.split(".")
        if len(parts) != 3:
            raise IapError("not a compact JWS")
        h64, p64, s64 = parts
        try:
            header = loads_bounded(b64url_decode(h64))
        except ValueError as exc:
            raise IapError("bad header") from exc
        if not isinstance(header, dict):
            raise IapError("bad header")
        if header.get("alg") != "ES256":
            raise IapError("alg must be ES256")
        if "crit" in header:
            raise IapError("crit header not supported")
        kid = header.get("kid")
        if not isinstance(kid, str) or not 0 < len(kid) <= MAX_KID_LEN:
            raise IapError("bad kid")
        sig = b64url_decode(s64)
        pub = self.jwks.get(kid)
        if pub is None:
            raise IapError("unknown kid")
        if not ecdsa_p256_verify(pub, f"{h64}.{p64}".encode("ascii"), sig):
            raise IapError("bad signature")
        # Signature holds: the claims are the proxy's own.
        try:
            claims = loads_bounded(b64url_decode(p64))
        except ValueError as exc:
            raise IapError("bad payload") from exc
        if not isinstance(claims, dict):
            raise IapError("bad payload")
        if claims.get("iss") != IAP_ISSUER:
            raise IapError("wrong issuer")
        if not isinstance(claims.get("aud"), str) or claims["aud"] != self.audience:
            raise IapError("wrong audience")
        now = self._now()
        exp, iat, nbf = claims.get("exp"), claims.get("iat"), claims.get("nbf")
        if not _numeric_date(exp) or not _numeric_date(iat):
            raise IapError("exp and iat must be integers in [1, 9999999999]")
        if now >= exp + self.skew:
            raise IapError("expired")
        if iat > now + self.skew:
            raise IapError("issued in the future")
        if nbf is not None and (not _numeric_date(nbf) or nbf > now + self.skew):
            raise IapError("not yet valid")
        email = claims.get("email")
        sub = claims.get("sub")
        if not isinstance(email, str) or "@" not in email or not email.strip():
            raise IapError("no email claim")
        if not isinstance(sub, str) or not sub:
            raise IapError("no sub claim")
        email = email.strip().lower()
        return {
            # Stable principal for authorization (survives an email change);
            # email is for display and the commit record.
            "id": f"{IAP_ISSUER}|{sub}",
            "login": email,
            "name": "",
            "email": email,
            "sub": sub,
            "source": f"iap:{email}",
        }

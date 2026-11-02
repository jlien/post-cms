//! Identity-Aware Proxy (IAP) token verification for the draft preview.
//!
//! The preview and public hosts can reach the same origin, and any client can
//! send any header to the public host, so neither the Host header nor the mere
//! presence of `x-goog-iap-jwt-assertion` means anything. A request is a
//! verified preview request only when that header holds a JWT that:
//!
//! - is signed ES256 by a key from the IAP key set (JWKS), looked up by `kid`;
//! - has `aud` equal to the configured `IAP_AUDIENCE`;
//! - has `iss` equal to `https://cloud.google.com/iap`;
//! - has not expired (`exp`), and has `iat` and `nbf` (when present) not in
//!   the future, each with [`LEEWAY_SECS`] of clock skew.
//!
//! The key set is fetched over HTTPS with a timeout and a size cap, cached for
//! [`KEYS_TTL`], used up to [`MAX_STALE`] longer while refreshes fail, and
//! refetched early when a token names an unknown `kid`. At most one fetch
//! runs at a time; unknown-`kid` refreshes and stale-set refreshes have
//! separate budgets (see [`IapVerifier::key_for`]), so a flood of forged
//! `kid`s can neither hammer the key endpoint nor delay a real refresh.
//!
//! Signature checks use `ring` (already in the tree through rustls), so this
//! module adds no new crates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
use serde::Deserialize;

/// Header IAP adds to every request it lets through.
pub const ASSERTION_HEADER: &str = "x-goog-iap-jwt-assertion";
/// Required `iss` claim.
pub const ISSUER: &str = "https://cloud.google.com/iap";
/// Google's published IAP signing keys, as a JWK set.
pub const DEFAULT_JWKS_URL: &str = "https://www.gstatic.com/iap/verify/public_key-jwk";
/// Clock skew allowed on `exp`, `iat` and `nbf`.
pub const LEEWAY_SECS: u64 = 30;
/// How long a fetched key set is trusted before it is refetched.
pub const KEYS_TTL: Duration = Duration::from_secs(3600);
/// Minimum spacing between key fetches of one kind: refreshes for an
/// unknown `kid`, and retries after a failed refresh of a stale set.
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// How long past [`KEYS_TTL`] a key set is still used while every refresh
/// fails. After that every token is refused (`IAP_JWKS_MAX_STALE_SECS`).
pub const MAX_STALE: Duration = Duration::from_secs(24 * 3600);
/// How long a `kid` missing from a freshly fetched set is remembered, so
/// repeats of it do not use the refresh budget.
pub const NEGATIVE_KID_TTL: Duration = Duration::from_secs(300);
/// Cap on remembered missing `kid`s.
pub const MAX_NEGATIVE_KIDS: usize = 4096;
/// Largest NumericDate accepted (year 2286); anything larger is malformed.
pub const MAX_NUMERIC_DATE: u64 = 9_999_999_999;
/// Fetch timeout for the key set.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest key set body accepted. Google's is about 2 KB.
pub const MAX_JWKS_BYTES: usize = 64 * 1024;
/// Largest assertion header accepted. IAP tokens are under 1 KB.
pub const MAX_TOKEN_BYTES: usize = 8 * 1024;

/// The identity IAP vouched for, from a verified token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IapIdentity {
    /// `email` claim: the signed-in user's address.
    pub email: String,
    /// `sub` claim: the stable account id (`accounts.google.com:<id>`).
    pub sub: String,
}

/// Why a token was refused. Kept out of responses (they are plain 404s);
/// used only in tests and debug logging.
#[derive(Debug, PartialEq, Eq)]
pub enum IapError {
    /// Preview is off: `IAP_AUDIENCE` is unset.
    NotConfigured,
    MissingHeader,
    Malformed(&'static str),
    BadAlgorithm,
    UnknownKey,
    BadSignature,
    WrongAudience,
    WrongIssuer,
    Expired,
    NotYetValid,
    IssuedInFuture,
    MissingEmail,
}

/// Where the key set comes from.
#[derive(Clone)]
pub enum KeySource {
    /// Fetch this URL. Must be https unless `allow_http` (tests only, loopback only).
    Url { url: String, allow_http: bool },
    /// A fixed fetch function (unit tests): returns the JWKS body.
    #[cfg(test)]
    Fixed(Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>),
}

/// Parsed, validated configuration.
#[derive(Clone)]
pub struct IapConfig {
    pub audience: String,
    pub keys: KeySource,
    pub min_refresh: Duration,
    pub ttl: Duration,
    /// How long past `ttl` the set is still used while refreshes fail.
    pub max_stale: Duration,
}

impl IapConfig {
    /// Read `IAP_AUDIENCE`, `IAP_JWKS_URL` and `IAP_JWKS_ALLOW_HTTP` from
    /// `get`. `Ok(None)` when `IAP_AUDIENCE` is unset or empty (preview off).
    /// A set but malformed value is an error, so a typo refuses to start
    /// instead of silently turning preview off.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Option<IapConfig>, String> {
        let Some(audience) = get("IAP_AUDIENCE").map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        else {
            return Ok(None);
        };
        if !valid_audience(&audience) {
            return Err(format!(
                "IAP_AUDIENCE {audience:?} is not /projects/<number>/global/backendServices/<id>"
            ));
        }
        let allow_http = get("IAP_JWKS_ALLOW_HTTP").as_deref() == Some("1");
        let url = get("IAP_JWKS_URL")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_JWKS_URL.to_string());
        check_jwks_url(&url, allow_http)?;
        let max_stale = match get("IAP_JWKS_MAX_STALE_SECS").map(|v| v.trim().to_string()) {
            None => MAX_STALE,
            Some(v) if v.is_empty() => MAX_STALE,
            Some(v) => Duration::from_secs(v.parse::<u64>().map_err(|_| {
                format!("IAP_JWKS_MAX_STALE_SECS {v:?} is not a whole number of seconds")
            })?),
        };
        Ok(Some(IapConfig {
            audience,
            keys: KeySource::Url { url, allow_http },
            min_refresh: MIN_REFRESH_INTERVAL,
            ttl: KEYS_TTL,
            max_stale,
        }))
    }
}

/// `/projects/<digits>/global/backendServices/<digits>`.
pub fn valid_audience(aud: &str) -> bool {
    let parts: Vec<&str> = aud.split('/').collect();
    let digits = |s: &str| !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit());
    parts.len() == 6
        && parts[0].is_empty()
        && parts[1] == "projects"
        && digits(parts[2])
        && parts[3] == "global"
        && parts[4] == "backendServices"
        && digits(parts[5])
}

/// https only; plain http only with the explicit override and only to a
/// loopback host, so the override can never point at the network.
fn check_jwks_url(url: &str, allow_http: bool) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("IAP_JWKS_URL {url:?}: {e}"))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("IAP_JWKS_URL must not carry credentials".into());
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if allow_http => {
            let host = parsed.host_str().unwrap_or("");
            let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
            let loopback = bare.eq_ignore_ascii_case("localhost")
                || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
            if loopback {
                Ok(())
            } else {
                Err("IAP_JWKS_ALLOW_HTTP=1 only allows a loopback http IAP_JWKS_URL".into())
            }
        }
        _ => Err(format!("IAP_JWKS_URL {url:?} must be https")),
    }
}

struct KeyCache {
    keys: HashMap<String, Vec<u8>>,
    /// Last successful fetch.
    fetched_at: Option<Instant>,
    /// Last refresh for a `kid` the fresh set did not hold. Its own budget, so
    /// forged `kid`s never delay a refresh of a stale set.
    last_kid_refresh: Option<Instant>,
    /// Last refresh because the set was stale or missing.
    last_stale_refresh: Option<Instant>,
    /// `kid`s missing from the set fetched after they were first seen.
    missing: HashMap<String, Instant>,
    /// The past-the-cap warning was logged for the current set.
    expired_logged: bool,
}

/// Verifies assertions against one audience with a cached key set.
pub struct IapVerifier {
    cfg: IapConfig,
    cache: Mutex<KeyCache>,
    /// Held while fetching, so concurrent misses share one fetch.
    fetch_lock: tokio::sync::Mutex<()>,
    client: Option<reqwest::Client>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    iss: Option<String>,
    aud: Option<serde_json::Value>,
    email: Option<String>,
    sub: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<serde_json::Value>,
}

impl IapVerifier {
    pub fn new(cfg: IapConfig) -> Result<IapVerifier, String> {
        let client = match &cfg.keys {
            KeySource::Url { allow_http, .. } => Some(
                reqwest::Client::builder()
                    .timeout(FETCH_TIMEOUT)
                    .connect_timeout(FETCH_TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .https_only(!allow_http)
                    .build()
                    .map_err(|e| format!("IAP key client: {e}"))?,
            ),
            #[cfg(test)]
            KeySource::Fixed(_) => None,
        };
        Ok(IapVerifier {
            cfg,
            cache: Mutex::new(KeyCache {
                keys: HashMap::new(),
                fetched_at: None,
                last_kid_refresh: None,
                last_stale_refresh: None,
                missing: HashMap::new(),
                expired_logged: false,
            }),
            fetch_lock: tokio::sync::Mutex::new(()),
            client,
        })
    }

    /// Verify the assertion header in `headers`.
    pub async fn verify_headers(&self, headers: &HeaderMap) -> Result<IapIdentity, IapError> {
        let mut values = headers.get_all(ASSERTION_HEADER).iter();
        let first = values.next().ok_or(IapError::MissingHeader)?;
        if values.next().is_some() {
            return Err(IapError::Malformed("more than one assertion header"));
        }
        let token = first.to_str().map_err(|_| IapError::Malformed("header is not ASCII"))?;
        self.verify(token).await
    }

    /// Verify a token at the current time.
    pub async fn verify(&self, token: &str) -> Result<IapIdentity, IapError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.verify_at(token, now).await
    }

    /// Verify a token as of `now` (Unix seconds).
    pub async fn verify_at(&self, token: &str, now: u64) -> Result<IapIdentity, IapError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(IapError::MissingHeader);
        }
        if token.len() > MAX_TOKEN_BYTES {
            return Err(IapError::Malformed("token too large"));
        }
        let mut parts = token.split('.');
        let (Some(h64), Some(p64), Some(s64), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(IapError::Malformed("not three segments"));
        };
        let raw_header: serde_json::Value = decode_json(h64)?;
        // Any `crit` (even null or empty) is refused: no extensions are understood.
        if raw_header.get("crit").is_some() {
            return Err(IapError::Malformed("crit header"));
        }
        let header: Header =
            serde_json::from_value(raw_header).map_err(|_| IapError::Malformed("segment json"))?;
        // Exactly ES256. Rejects "none", HS256 (key confusion) and anything else
        // before any key is looked up.
        if header.alg != "ES256" {
            return Err(IapError::BadAlgorithm);
        }
        let kid = header.kid.filter(|k| !k.is_empty() && k.len() <= 256).ok_or(IapError::UnknownKey)?;
        let sig = URL_SAFE_NO_PAD.decode(s64).map_err(|_| IapError::Malformed("signature encoding"))?;
        if sig.len() != 64 {
            return Err(IapError::BadSignature);
        }
        let key = self.key_for(&kid).await.ok_or(IapError::UnknownKey)?;
        let signed = &token[..h64.len() + 1 + p64.len()];
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &key)
            .verify(signed.as_bytes(), &sig)
            .map_err(|_| IapError::BadSignature)?;

        // Claims are read only after the signature holds.
        let raw: serde_json::Value = decode_json(p64)?;
        // Time claims come from the raw object, so an explicit null is
        // malformed rather than absent.
        let time = |name: &str| raw.get(name).cloned();
        let (exp, nbf, iat) = (time("exp"), time("nbf"), time("iat"));
        let c: Claims = serde_json::from_value(raw).map_err(|_| IapError::Malformed("segment json"))?;
        if c.iss.as_deref() != Some(ISSUER) {
            return Err(IapError::WrongIssuer);
        }
        let aud_ok = match &c.aud {
            Some(serde_json::Value::String(a)) => a == &self.cfg.audience,
            Some(serde_json::Value::Array(list)) => {
                list.iter().any(|a| a.as_str() == Some(self.cfg.audience.as_str()))
            }
            _ => false,
        };
        if !aud_ok {
            return Err(IapError::WrongAudience);
        }
        let exp = numeric(&exp)?.ok_or(IapError::Malformed("missing exp"))?;
        if now >= exp.saturating_add(LEEWAY_SECS) {
            return Err(IapError::Expired);
        }
        if let Some(nbf) = numeric(&nbf)? {
            if nbf > now.saturating_add(LEEWAY_SECS) {
                return Err(IapError::NotYetValid);
            }
        }
        if let Some(iat) = numeric(&iat)? {
            if iat > now.saturating_add(LEEWAY_SECS) {
                return Err(IapError::IssuedInFuture);
            }
        }
        let email = c.email.filter(|e| !e.is_empty()).ok_or(IapError::MissingEmail)?;
        Ok(IapIdentity { email, sub: c.sub.unwrap_or_default() })
    }

    /// Public key bytes (uncompressed P-256 point) for `kid`, fetching the
    /// key set when it is stale or does not know `kid`, within the refresh
    /// budgets:
    /// - stale or missing set: one refresh as soon as it goes stale, then
    ///   retries at most once per `min_refresh` while they fail;
    /// - fresh set without `kid`: at most one refresh per `min_refresh`, and
    ///   none for a `kid` the last refresh already showed missing.
    async fn key_for(&self, kid: &str) -> Option<Vec<u8>> {
        if let Some(k) = self.lookup(kid, true) {
            return Some(k);
        }
        let _guard = self.fetch_lock.lock().await;
        // Another request may have fetched while this one waited.
        if let Some(k) = self.lookup(kid, true) {
            return Some(k);
        }
        if !self.claim_refresh(kid) {
            return self.lookup(kid, false);
        }
        match self.fetch().await {
            Ok(keys) => {
                let mut c = self.cache.lock().expect("iap cache poisoned");
                let now = Instant::now();
                c.keys = keys;
                c.fetched_at = Some(now);
                c.expired_logged = false;
                c.missing.retain(|_, seen| seen.elapsed() < NEGATIVE_KID_TTL);
                if !c.keys.contains_key(kid) {
                    if c.missing.len() >= MAX_NEGATIVE_KIDS {
                        c.missing.clear();
                    }
                    c.missing.insert(kid.to_string(), now);
                }
            }
            Err(e) => eprintln!("iap: key fetch failed: {e}"),
        }
        self.lookup(kid, false)
    }

    /// Key for `kid`. `fresh_only`: only while the set is within its TTL;
    /// otherwise also while stale, up to `max_stale` past the TTL. Past that
    /// cap nothing verifies and the server logs it once per set.
    fn lookup(&self, kid: &str, fresh_only: bool) -> Option<Vec<u8>> {
        let mut c = self.cache.lock().expect("iap cache poisoned");
        let age = c.fetched_at?.elapsed();
        if age < self.cfg.ttl {
            return c.keys.get(kid).cloned();
        }
        if fresh_only {
            return None;
        }
        if age >= self.cfg.ttl.saturating_add(self.cfg.max_stale) {
            if !c.expired_logged {
                c.expired_logged = true;
                eprintln!(
                    "error: iap: the IAP key set is {}s old and every refresh failed; \
                     refusing all preview tokens until a refresh succeeds",
                    age.as_secs()
                );
            }
            return None;
        }
        c.keys.get(kid).cloned()
    }

    /// Claim a refresh for `kid` under the budget that applies (see
    /// [`Self::key_for`]); records the attempt when granted.
    fn claim_refresh(&self, kid: &str) -> bool {
        let mut c = self.cache.lock().expect("iap cache poisoned");
        let now = Instant::now();
        let fresh = c.fetched_at.is_some_and(|t| t.elapsed() < self.cfg.ttl);
        let within = |t: Option<Instant>| t.is_some_and(|t| t.elapsed() < self.cfg.min_refresh);
        if fresh {
            if c.missing.get(kid).is_some_and(|seen| seen.elapsed() < NEGATIVE_KID_TTL) {
                return false;
            }
            if within(c.last_kid_refresh) {
                return false;
            }
            c.last_kid_refresh = Some(now);
            return true;
        }
        // Stale or never fetched: the first attempt since the last success
        // always runs; failed retries are spaced by min_refresh.
        let first_since_success = match (c.last_stale_refresh, c.fetched_at) {
            (None, _) => true,
            (Some(a), Some(f)) => a < f,
            (Some(_), None) => false,
        };
        if !first_since_success && within(c.last_stale_refresh) {
            return false;
        }
        c.last_stale_refresh = Some(now);
        true
    }

    async fn fetch(&self) -> Result<HashMap<String, Vec<u8>>, String> {
        let body = match &self.cfg.keys {
            KeySource::Url { url, .. } => {
                let client = self.client.as_ref().ok_or("no client")?;
                fetch_capped(client, url).await?
            }
            #[cfg(test)]
            KeySource::Fixed(f) => f()?,
        };
        parse_jwks(&body)
    }
}

/// GET `url` with the client's timeout, refusing redirects, non-200
/// statuses and bodies over [`MAX_JWKS_BYTES`].
async fn fetch_capped(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let mut resp = client.get(url).send().await.map_err(|e| format!("request: {}", e.without_url()))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("status {}", resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_JWKS_BYTES as u64) {
        return Err("key set too large".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("body: {}", e.without_url()))? {
        if body.len() + chunk.len() > MAX_JWKS_BYTES {
            return Err("key set too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// P-256 keys from a JWK set, by `kid`. Keys of other types or curves, or
/// with an `alg` other than ES256, are skipped.
pub fn parse_jwks(body: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    let set: Jwks = serde_json::from_slice(body).map_err(|e| format!("key set: {e}"))?;
    let mut keys = HashMap::new();
    for k in set.keys {
        let s = |name: &str| k.get(name).and_then(|v| v.as_str());
        if s("kty") != Some("EC") || s("crv") != Some("P-256") {
            continue;
        }
        if s("alg").is_some_and(|a| a != "ES256") || s("use").is_some_and(|u| u != "sig") {
            continue;
        }
        let (Some(kid), Some(x), Some(y)) = (s("kid"), s("x"), s("y")) else { continue };
        let (Ok(x), Ok(y)) = (URL_SAFE_NO_PAD.decode(x), URL_SAFE_NO_PAD.decode(y)) else { continue };
        if x.len() != 32 || y.len() != 32 {
            continue;
        }
        let mut point = Vec::with_capacity(65);
        point.push(0x04);
        point.extend_from_slice(&x);
        point.extend_from_slice(&y);
        keys.insert(kid.to_string(), point);
    }
    if keys.is_empty() {
        return Err("key set has no ES256 P-256 keys".into());
    }
    Ok(keys)
}

fn decode_json<T: serde::de::DeserializeOwned>(seg: &str) -> Result<T, IapError> {
    let raw = URL_SAFE_NO_PAD.decode(seg).map_err(|_| IapError::Malformed("segment encoding"))?;
    serde_json::from_slice(&raw).map_err(|_| IapError::Malformed("segment json"))
}

/// A NumericDate claim: an integer from 1 to [`MAX_NUMERIC_DATE`]. Floats
/// (including `1e300` and `1800000000.0`), strings, negatives and null are
/// malformed. Absent is `None`.
fn numeric(v: &Option<serde_json::Value>) -> Result<Option<u64>, IapError> {
    match v {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .filter(|t| (1..=MAX_NUMERIC_DATE).contains(t))
            .map(Some)
            .ok_or(IapError::Malformed("bad time claim")),
        Some(_) => Err(IapError::Malformed("bad time claim")),
    }
}

// ---- Process-wide verifier -------------------------------------------------

static VERIFIER: OnceLock<Option<Arc<IapVerifier>>> = OnceLock::new();

/// Install the process-wide verifier (`None`: preview off). First call wins.
pub fn install(v: Option<Arc<IapVerifier>>) {
    let _ = VERIFIER.set(v);
}

/// The process-wide verifier, if preview is configured.
pub fn verifier() -> Option<Arc<IapVerifier>> {
    VERIFIER.get().cloned().flatten()
}

/// Verify `headers` against `v`; `NotConfigured` when there is no verifier.
pub async fn verify_with(v: Option<&IapVerifier>, headers: &HeaderMap) -> Result<IapIdentity, IapError> {
    match v {
        Some(v) => v.verify_headers(headers).await,
        None => Err(IapError::NotConfigured),
    }
}

/// Axum extractor for handlers that need the IAP-verified user (for example
/// an approval step that records who approved). Never rejects: holds `None`
/// when preview is off or the assertion is missing or invalid, so the
/// handler decides how to refuse.
///
/// ```ignore
/// async fn approve(VerifiedIdentity(who): VerifiedIdentity) -> Response {
///     let Some(who) = who else { return StatusCode::FORBIDDEN.into_response() };
///     record_approval(&who.email);
///     ...
/// }
/// ```
// Not wired to a route yet: the approval step (a later change) is its first user.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
pub struct VerifiedIdentity(pub Option<IapIdentity>);

impl<S: Send + Sync> FromRequestParts<S> for VerifiedIdentity {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let v = verifier();
        Ok(VerifiedIdentity(identity_from_parts(v.as_deref(), parts).await))
    }
}

/// The verified identity for a request, memoized in its extensions so
/// several extractors on one request verify once.
async fn identity_from_parts(v: Option<&IapVerifier>, parts: &mut Parts) -> Option<IapIdentity> {
    if let Some(id) = parts.extensions.get::<IapIdentity>() {
        return Some(id.clone());
    }
    let id = verify_with(v, &parts.headers).await.ok();
    if let Some(id) = &id {
        parts.extensions.insert(id.clone());
    }
    id
}

#[cfg(test)]
pub(crate) mod test_keys {
    //! Locally generated ES256 keys and token minting for tests.
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    pub struct TestKey {
        pub kid: String,
        pair: EcdsaKeyPair,
    }

    impl TestKey {
        pub fn new(kid: &str) -> TestKey {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
            TestKey { kid: kid.to_string(), pair }
        }

        pub fn jwk(&self) -> serde_json::Value {
            let p = self.pair.public_key().as_ref();
            serde_json::json!({
                "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": self.kid,
                "x": URL_SAFE_NO_PAD.encode(&p[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&p[33..65]),
            })
        }

        pub fn public_point(&self) -> Vec<u8> {
            self.pair.public_key().as_ref().to_vec()
        }

        /// ES256 token over `header` and `claims` as given.
        pub fn sign(&self, header: &serde_json::Value, claims: &serde_json::Value) -> String {
            let input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header.to_string()),
                URL_SAFE_NO_PAD.encode(claims.to_string())
            );
            let sig = self.pair.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
        }

        pub fn token(&self, claims: &serde_json::Value) -> String {
            self.sign(&serde_json::json!({"alg": "ES256", "typ": "JWT", "kid": self.kid}), claims)
        }
    }

    pub fn jwks(keys: &[&TestKey]) -> Vec<u8> {
        serde_json::json!({ "keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>() })
            .to_string()
            .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_keys::*;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const AUD: &str = "/projects/123456789/global/backendServices/987654321";
    const NOW: u64 = 1_800_000_000;

    fn claims() -> serde_json::Value {
        json!({
            "iss": ISSUER, "aud": AUD, "iat": NOW - 10, "exp": NOW + 600,
            "email": "reviewer@example.com", "sub": "accounts.google.com:42",
        })
    }

    /// A verifier whose key set is whatever `body` returns, counting fetches.
    fn verifier_with(
        body: Arc<Mutex<Vec<u8>>>,
        min_refresh: Duration,
    ) -> (IapVerifier, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let fetch = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(body.lock().unwrap().clone())
        });
        let cfg = IapConfig {
            audience: AUD.into(),
            keys: KeySource::Fixed(fetch),
            min_refresh,
            ttl: KEYS_TTL,
            max_stale: MAX_STALE,
        };
        (IapVerifier::new(cfg).unwrap(), count)
    }

    fn one_key() -> (TestKey, IapVerifier, Arc<AtomicUsize>) {
        let k = TestKey::new("k1");
        let (v, n) = verifier_with(Arc::new(Mutex::new(jwks(&[&k]))), Duration::ZERO);
        (k, v, n)
    }

    #[tokio::test]
    async fn valid_token_yields_email() {
        let (k, v, _) = one_key();
        let id = v.verify_at(&k.token(&claims()), NOW).await.unwrap();
        assert_eq!(id.email, "reviewer@example.com");
        assert_eq!(id.sub, "accounts.google.com:42");
    }

    #[tokio::test]
    async fn audience_as_array_is_accepted_only_when_it_contains_ours() {
        let (k, v, _) = one_key();
        let mut c = claims();
        c["aud"] = json!(["/projects/1/global/backendServices/2", AUD]);
        assert!(v.verify_at(&k.token(&c), NOW).await.is_ok());
        c["aud"] = json!(["/projects/1/global/backendServices/2"]);
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::WrongAudience));
    }

    #[tokio::test]
    async fn wrong_or_missing_audience_is_refused() {
        let (k, v, _) = one_key();
        let mut c = claims();
        c["aud"] = json!("/projects/123456789/global/backendServices/111");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::WrongAudience));
        c.as_object_mut().unwrap().remove("aud");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::WrongAudience));
    }

    #[tokio::test]
    async fn wrong_or_missing_issuer_is_refused() {
        let (k, v, _) = one_key();
        let mut c = claims();
        c["iss"] = json!("https://accounts.google.com");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::WrongIssuer));
        c.as_object_mut().unwrap().remove("iss");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::WrongIssuer));
    }

    #[tokio::test]
    async fn expiry_allows_only_the_leeway() {
        let (k, v, _) = one_key();
        let mut c = claims();
        c["exp"] = json!(NOW - LEEWAY_SECS + 1);
        assert!(v.verify_at(&k.token(&c), NOW).await.is_ok());
        c["exp"] = json!(NOW - LEEWAY_SECS);
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::Expired));
        c["exp"] = json!(NOW - 3600);
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::Expired));
        c.as_object_mut().unwrap().remove("exp");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::Malformed("missing exp")));
    }

    #[tokio::test]
    async fn not_yet_valid_and_future_iat_are_refused() {
        let (k, v, _) = one_key();
        let mut c = claims();
        c["nbf"] = json!(NOW + LEEWAY_SECS);
        assert!(v.verify_at(&k.token(&c), NOW).await.is_ok());
        c["nbf"] = json!(NOW + LEEWAY_SECS + 1);
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::NotYetValid));
        let mut c = claims();
        c["iat"] = json!(NOW + 300);
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::IssuedInFuture));
        c["iat"] = json!("soon");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::Malformed("bad time claim")));
    }

    #[tokio::test]
    async fn alg_none_is_refused() {
        let (k, v, n) = one_key();
        let h = URL_SAFE_NO_PAD.encode(json!({"alg": "none", "kid": "k1"}).to_string());
        let p = URL_SAFE_NO_PAD.encode(claims().to_string());
        for t in [format!("{h}.{p}."), format!("{h}.{p}")] {
            assert!(v.verify_at(&t, NOW).await.is_err(), "{t}");
        }
        // Signed by the right key but labelled none: still refused.
        let t = k.sign(&json!({"alg": "none", "kid": "k1"}), &claims());
        assert_eq!(v.verify_at(&t, NOW).await, Err(IapError::BadAlgorithm));
        assert_eq!(n.load(Ordering::SeqCst), 0, "no key fetch for a refused alg");
    }

    #[tokio::test]
    async fn hs256_with_the_public_key_as_secret_is_refused() {
        let (k, v, _) = one_key();
        let h = URL_SAFE_NO_PAD.encode(json!({"alg": "HS256", "kid": "k1"}).to_string());
        let p = URL_SAFE_NO_PAD.encode(claims().to_string());
        let input = format!("{h}.{p}");
        for secret in [k.public_point(), k.jwk().to_string().into_bytes()] {
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &secret);
            let mac = ring::hmac::sign(&key, input.as_bytes());
            let t = format!("{input}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref()));
            assert_eq!(v.verify_at(&t, NOW).await, Err(IapError::BadAlgorithm));
        }
        // Other ES-looking or RSA algs too.
        for alg in ["ES384", "RS256", "es256", "ES256 "] {
            let t = k.sign(&json!({"alg": alg, "kid": "k1"}), &claims());
            assert_eq!(v.verify_at(&t, NOW).await, Err(IapError::BadAlgorithm), "{alg}");
        }
    }

    #[tokio::test]
    async fn tampered_payload_or_signature_is_refused() {
        let (k, v, _) = one_key();
        let t = k.token(&claims());
        let parts: Vec<&str> = t.split('.').collect();
        let mut c = claims();
        c["email"] = json!("someone-else@example.com");
        let forged = format!("{}.{}.{}", parts[0], URL_SAFE_NO_PAD.encode(c.to_string()), parts[2]);
        assert_eq!(v.verify_at(&forged, NOW).await, Err(IapError::BadSignature));
        let mut sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        sig[10] ^= 1;
        let flipped = format!("{}.{}.{}", parts[0], parts[1], URL_SAFE_NO_PAD.encode(&sig));
        assert_eq!(v.verify_at(&flipped, NOW).await, Err(IapError::BadSignature));
        // Signed by a different key under the same kid.
        let other = TestKey::new("k1");
        assert_eq!(v.verify_at(&other.token(&claims()), NOW).await, Err(IapError::BadSignature));
    }

    #[tokio::test]
    async fn malformed_tokens_are_refused() {
        let (_, v, _) = one_key();
        for t in ["", "a", "a.b", "a.b.c.d", "!!.!!.!!", &"a".repeat(MAX_TOKEN_BYTES + 1)] {
            assert!(v.verify_at(t, NOW).await.is_err(), "{t:.20}");
        }
    }

    #[tokio::test]
    async fn unknown_kid_refreshes_the_key_set_once() {
        let k1 = TestKey::new("k1");
        let k2 = TestKey::new("k2");
        let body = Arc::new(Mutex::new(jwks(&[&k1])));
        let (v, n) = verifier_with(body.clone(), Duration::ZERO);
        v.verify_at(&k1.token(&claims()), NOW).await.unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // Cached: no fetch for a known kid.
        v.verify_at(&k1.token(&claims()), NOW).await.unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // Google rotates in k2: the unknown kid triggers one refetch.
        *body.lock().unwrap() = jwks(&[&k1, &k2]);
        let id = v.verify_at(&k2.token(&claims()), NOW).await.unwrap();
        assert_eq!(id.email, "reviewer@example.com");
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn forged_kid_flood_fetches_at_most_once_per_interval() {
        let k1 = TestKey::new("k1");
        let (v, n) = verifier_with(Arc::new(Mutex::new(jwks(&[&k1]))), Duration::from_secs(3600));
        v.verify_at(&k1.token(&claims()), NOW).await.unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        let v = Arc::new(v);
        let mut tasks = Vec::new();
        for i in 0..50 {
            let v = v.clone();
            let forged = TestKey::new(&format!("forged-{i}"));
            let t = forged.token(&claims());
            tasks.push(tokio::spawn(async move { v.verify_at(&t, NOW).await }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), Err(IapError::UnknownKey));
        }
        // The first forged kid spends the one unknown-kid refresh; the rest wait.
        assert_eq!(n.load(Ordering::SeqCst), 2, "rate limit blocks refetches");
        // The known key still verifies from cache.
        assert!(v.verify_at(&k1.token(&claims()), NOW).await.is_ok());
    }

    #[tokio::test]
    async fn failed_fetch_refuses_and_is_rate_limited() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let cfg = IapConfig {
            audience: AUD.into(),
            keys: KeySource::Fixed(Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
                Err("offline".into())
            })),
            min_refresh: Duration::from_secs(3600),
            ttl: KEYS_TTL,
            max_stale: MAX_STALE,
        };
        let v = IapVerifier::new(cfg).unwrap();
        let k = TestKey::new("k1");
        for _ in 0..5 {
            assert_eq!(v.verify_at(&k.token(&claims()), NOW).await, Err(IapError::UnknownKey));
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// A verifier with explicit timings over a fetch that can be switched off.
    fn timed_verifier(
        body: Arc<Mutex<Vec<u8>>>,
        failing: Arc<std::sync::atomic::AtomicBool>,
        min_refresh: Duration,
        ttl: Duration,
        max_stale: Duration,
    ) -> (IapVerifier, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let fetch = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            if failing.load(Ordering::SeqCst) {
                return Err("offline".to_string());
            }
            Ok(body.lock().unwrap().clone())
        });
        let cfg = IapConfig { audience: AUD.into(), keys: KeySource::Fixed(fetch), min_refresh, ttl, max_stale };
        (IapVerifier::new(cfg).unwrap(), count)
    }

    /// Stale keys are used only up to max_stale past the TTL while refreshes
    /// fail; then every token is refused until a refresh succeeds.
    #[tokio::test]
    async fn stale_keys_have_an_upper_bound() {
        let k = TestKey::new("k1");
        let failing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (v, n) = timed_verifier(
            Arc::new(Mutex::new(jwks(&[&k]))),
            failing.clone(),
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );
        v.verify_at(&k.token(&claims()), NOW).await.unwrap();
        failing.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Stale, refresh fails, still inside the cap: the cached key serves.
        assert!(v.verify_at(&k.token(&claims()), NOW).await.is_ok());
        assert_eq!(n.load(Ordering::SeqCst), 2);
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Past ttl + max_stale: refused.
        assert_eq!(v.verify_at(&k.token(&claims()), NOW).await, Err(IapError::UnknownKey));
        // A successful refresh brings it back.
        failing.store(false, Ordering::SeqCst);
        assert!(v.verify_at(&k.token(&claims()), NOW).await.is_ok());
    }

    /// Forged kids spend only the unknown-kid budget: once the set goes
    /// stale, a rotated-in key still refreshes at once.
    #[tokio::test]
    async fn forged_kids_cannot_block_the_stale_refresh() {
        let k1 = TestKey::new("k1");
        let k2 = TestKey::new("k2");
        let body = Arc::new(Mutex::new(jwks(&[&k1])));
        let (v, n) = timed_verifier(
            body.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Duration::from_secs(3600),
            Duration::from_millis(150),
            MAX_STALE,
        );
        v.verify_at(&k1.token(&claims()), NOW).await.unwrap();
        for i in 0..20 {
            let t = TestKey::new(&format!("forged-{i}")).token(&claims());
            assert_eq!(v.verify_at(&t, NOW).await, Err(IapError::UnknownKey));
        }
        assert_eq!(n.load(Ordering::SeqCst), 2);
        *body.lock().unwrap() = jwks(&[&k1, &k2]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(v.verify_at(&k2.token(&claims()), NOW).await.is_ok());
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }

    /// A kid the last refresh showed missing does not spend the budget again.
    #[tokio::test]
    async fn missing_kid_is_remembered() {
        let k1 = TestKey::new("k1");
        let (v, n) = verifier_with(Arc::new(Mutex::new(jwks(&[&k1]))), Duration::ZERO);
        v.verify_at(&k1.token(&claims()), NOW).await.unwrap();
        let forged = TestKey::new("forged").token(&claims());
        for _ in 0..10 {
            assert_eq!(v.verify_at(&forged, NOW).await, Err(IapError::UnknownKey));
        }
        assert_eq!(n.load(Ordering::SeqCst), 2, "one refresh for the forged kid, then remembered");
    }

    #[tokio::test]
    async fn crit_header_is_refused() {
        let (k, v, _) = one_key();
        for crit in [json!(["exp"]), json!([]), json!("b64"), json!(null)] {
            let t = k.sign(&json!({"alg": "ES256", "kid": "k1", "crit": crit}), &claims());
            assert_eq!(v.verify_at(&t, NOW).await, Err(IapError::Malformed("crit header")), "{crit}");
        }
    }

    #[tokio::test]
    async fn time_claims_must_be_sane_integers() {
        let (k, v, _) = one_key();
        let bad = [json!(1e300), json!(1_800_000_600.0), json!(1.5), json!("1800000600"), json!(-1),
            json!(0), json!(MAX_NUMERIC_DATE + 1), json!(u64::MAX), json!(null), json!(true)];
        for claim in ["exp", "nbf", "iat"] {
            for value in &bad {
                let mut c = claims();
                c[claim] = value.clone();
                assert_eq!(
                    v.verify_at(&k.token(&c), NOW).await,
                    Err(IapError::Malformed("bad time claim")),
                    "{claim}={value}"
                );
            }
        }
    }

    #[tokio::test]
    async fn missing_email_is_refused() {
        let (k, v, _) = one_key();
        let mut c = claims();
        c.as_object_mut().unwrap().remove("email");
        assert_eq!(v.verify_at(&k.token(&c), NOW).await, Err(IapError::MissingEmail));
    }

    #[tokio::test]
    async fn header_rules() {
        let (k, v, _) = one_key();
        let mut h = HeaderMap::new();
        assert_eq!(v.verify_headers(&h).await, Err(IapError::MissingHeader));
        // verify_headers uses the real clock: mint a token valid now.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut c = claims();
        c["iat"] = json!(now);
        c["exp"] = json!(now + 600);
        let t = k.token(&c);
        h.insert(ASSERTION_HEADER, t.parse().unwrap());
        assert!(v.verify_headers(&h).await.is_ok());
        h.append(ASSERTION_HEADER, t.parse().unwrap());
        assert!(matches!(v.verify_headers(&h).await, Err(IapError::Malformed(_))));
        assert_eq!(verify_with(None, &HeaderMap::new()).await, Err(IapError::NotConfigured));
    }

    #[tokio::test]
    async fn extractor_exposes_the_verified_email_once() {
        let (k, v, n) = one_key();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut c = claims();
        c["iat"] = json!(now);
        c["exp"] = json!(now + 600);
        let req = axum::http::Request::builder()
            .header(ASSERTION_HEADER, k.token(&c))
            .body(())
            .unwrap();
        let (mut parts, ()) = req.into_parts();
        let id = identity_from_parts(Some(&v), &mut parts).await.unwrap();
        assert_eq!(id.email, "reviewer@example.com");
        // Memoized: a second extractor on the same request does not re-verify.
        parts.headers.remove(ASSERTION_HEADER);
        assert_eq!(identity_from_parts(Some(&v), &mut parts).await, Some(id));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // No header, no identity; preview off, no identity.
        let (mut bare, ()) = axum::http::Request::builder().body(()).unwrap().into_parts();
        assert_eq!(identity_from_parts(Some(&v), &mut bare).await, None);
        let (mut bare, ()) = axum::http::Request::builder().body(()).unwrap().into_parts();
        assert_eq!(identity_from_parts(None, &mut bare).await, None);
        // The extractor itself, with no process-wide verifier installed in tests.
        let (mut bare, ()) = axum::http::Request::builder().body(()).unwrap().into_parts();
        let VerifiedIdentity(got) = VerifiedIdentity::from_request_parts(&mut bare, &()).await.unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn jwks_parsing_keeps_only_es256_p256_keys() {
        let k = TestKey::new("good");
        let mut rsa = k.jwk();
        rsa["kty"] = json!("RSA");
        rsa["kid"] = json!("rsa");
        let mut p384 = k.jwk();
        p384["crv"] = json!("P-384");
        p384["kid"] = json!("p384");
        let mut short = k.jwk();
        short["x"] = json!("AAAA");
        short["kid"] = json!("short");
        let body = json!({"keys": [k.jwk(), rsa, p384, short]}).to_string();
        let keys = parse_jwks(body.as_bytes()).unwrap();
        assert_eq!(keys.keys().collect::<Vec<_>>(), vec!["good"]);
        assert!(parse_jwks(b"{\"keys\":[]}").is_err());
        assert!(parse_jwks(b"not json").is_err());
    }

    #[test]
    fn config_from_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert!(IapConfig::from_env(env(&[])).unwrap().is_none());
        assert!(IapConfig::from_env(env(&[("IAP_AUDIENCE", "  ")])).unwrap().is_none());
        let c = IapConfig::from_env(env(&[("IAP_AUDIENCE", AUD)])).unwrap().unwrap();
        assert!(matches!(c.keys, KeySource::Url { ref url, allow_http: false } if url == DEFAULT_JWKS_URL));
        for bad in ["projects/1/global/backendServices/2", "/projects/x/global/backendServices/2",
            "/projects/1/global/backendServices/", "/projects/1/apps/2", "/projects/1/global/backendServices/2/x"]
        {
            assert!(!valid_audience(bad), "{bad}");
        }
        assert!(IapConfig::from_env(env(&[("IAP_AUDIENCE", "nope")])).is_err());
        assert!(IapConfig::from_env(env(&[("IAP_AUDIENCE", AUD), ("IAP_JWKS_URL", "http://127.0.0.1:9/k")]))
            .is_err());
        assert!(IapConfig::from_env(env(&[
            ("IAP_AUDIENCE", AUD), ("IAP_JWKS_URL", "http://127.0.0.1:9/k"), ("IAP_JWKS_ALLOW_HTTP", "1"),
        ]))
        .unwrap()
        .is_some());
        // The override never reaches past loopback.
        assert!(IapConfig::from_env(env(&[
            ("IAP_AUDIENCE", AUD), ("IAP_JWKS_URL", "http://example.com/k"), ("IAP_JWKS_ALLOW_HTTP", "1"),
        ]))
        .is_err());
        assert!(IapConfig::from_env(env(&[("IAP_AUDIENCE", AUD), ("IAP_JWKS_URL", "https://u:p@example.com/k")]))
            .is_err());
        assert!(IapConfig::from_env(env(&[("IAP_AUDIENCE", AUD), ("IAP_JWKS_URL", "file:///etc/passwd")]))
            .is_err());
        assert_eq!(c.max_stale, MAX_STALE);
        let c = IapConfig::from_env(env(&[("IAP_AUDIENCE", AUD), ("IAP_JWKS_MAX_STALE_SECS", "600")]))
            .unwrap()
            .unwrap();
        assert_eq!(c.max_stale, Duration::from_secs(600));
        assert!(IapConfig::from_env(env(&[("IAP_AUDIENCE", AUD), ("IAP_JWKS_MAX_STALE_SECS", "1d")])).is_err());
    }
}

//! Per-path response headers from site.yaml `headers:`.
//!
//! ```yaml
//! headers:
//!   default:                      # every response the static fallback serves
//!     X-Frame-Options: SAMEORIGIN
//!   rules:                        # applied in order; later rules win per header
//!     - path: /signup             # exact path (slash form and case do not matter)
//!       headers:
//!         Content-Security-Policy: "default-src 'self'"
//!     - path: [/docs/*, /guides/*] # `*` matches any run of characters, `/` included
//!       headers:
//!         X-Robots-Tag: noindex
//! ```
//!
//! Loaded once at startup. Anything malformed (bad header name, CR/LF in a
//! value, a header the server owns) refuses to start, the same contract as
//! redirects.yaml.

use std::collections::BTreeMap;

use axum::http::{HeaderName, HeaderValue};

/// One rule: path patterns plus the headers they set.
#[derive(Clone, Debug)]
pub struct Rule {
    pub patterns: Vec<String>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

/// The parsed `headers:` block.
#[derive(Clone, Debug, Default)]
pub struct SiteHeaders {
    pub default: Vec<(HeaderName, HeaderValue)>,
    pub rules: Vec<Rule>,
}

/// Headers a site may not set: hop-by-hop headers (RFC 9110 section 7.6.1),
/// framing and routing headers the server computes per response, and
/// `X-Content-Type-Options`, which the server always sends as `nosniff`.
const RESERVED: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "content-type",
    "content-encoding",
    "content-range",
    "accept-ranges",
    "location",
    "date",
    "server",
    "set-cookie",
    "vary",
    "x-content-type-options",
];

impl SiteHeaders {
    /// Parse site.yaml text. A missing `headers:` key is an empty config.
    pub fn from_site_yaml(raw: &str) -> anyhow::Result<SiteHeaders> {
        let doc: serde_yaml::Value =
            serde_yaml::from_str(raw).map_err(|e| anyhow::anyhow!("site.yaml: {e}"))?;
        match doc.get("headers") {
            None | Some(serde_yaml::Value::Null) => Ok(SiteHeaders::default()),
            Some(v) => SiteHeaders::from_value(v),
        }
    }

    pub fn from_value(v: &serde_yaml::Value) -> anyhow::Result<SiteHeaders> {
        let map = v
            .as_mapping()
            .ok_or_else(|| anyhow::anyhow!("headers: must be a mapping with `default` and/or `rules`"))?;
        let mut out = SiteHeaders::default();
        for (k, val) in map {
            match k.as_str() {
                Some("default") => out.default = header_map(val, "headers.default")?,
                Some("rules") => {
                    let list = val
                        .as_sequence()
                        .ok_or_else(|| anyhow::anyhow!("headers.rules: must be a list"))?;
                    for (i, r) in list.iter().enumerate() {
                        out.rules.push(rule(r, i)?);
                    }
                }
                _ => anyhow::bail!(
                    "headers: unknown key {:?} (expected `default` or `rules`)",
                    k.as_str().unwrap_or("?")
                ),
            }
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.default.is_empty() && self.rules.is_empty()
    }

    /// The headers for a request path: `default` first, then every matching
    /// rule in file order, a later value replacing an earlier one per header.
    pub fn resolve(&self, path: &str) -> Vec<(HeaderName, HeaderValue)> {
        let key = normalize(path);
        let mut out: Vec<(HeaderName, HeaderValue)> = Vec::new();
        let mut set = |name: &HeaderName, value: &HeaderValue| {
            match out.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = value.clone(),
                None => out.push((name.clone(), value.clone())),
            }
        };
        for (n, v) in &self.default {
            set(n, v);
        }
        for r in &self.rules {
            if r.patterns.iter().any(|p| glob_match(p, &key)) {
                for (n, v) in &r.headers {
                    set(n, v);
                }
            }
        }
        out
    }
}

fn rule(v: &serde_yaml::Value, i: usize) -> anyhow::Result<Rule> {
    let at = format!("headers.rules[{i}]");
    let m = v
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("{at}: must be a mapping with `path` and `headers`"))?;
    for k in m.keys() {
        match k.as_str() {
            Some("path") | Some("headers") => {}
            other => anyhow::bail!("{at}: unknown key {:?}", other.unwrap_or("?")),
        }
    }
    let raw_paths: Vec<String> = match m.get("path") {
        Some(serde_yaml::Value::String(s)) => vec![s.clone()],
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("{at}.path: every entry must be a string"))
            })
            .collect::<anyhow::Result<_>>()?,
        _ => anyhow::bail!("{at}: needs `path`, a path pattern or a list of them"),
    };
    if raw_paths.is_empty() {
        anyhow::bail!("{at}.path: list is empty");
    }
    let mut patterns = Vec::new();
    for p in raw_paths {
        validate_pattern(&p).map_err(|e| anyhow::anyhow!("{at}.path {p:?}: {e}"))?;
        patterns.push(normalize(&p));
    }
    let headers = header_map(
        m.get("headers")
            .ok_or_else(|| anyhow::anyhow!("{at}: needs `headers`"))?,
        &format!("{at}.headers"),
    )?;
    if headers.is_empty() {
        anyhow::bail!("{at}.headers: is empty");
    }
    Ok(Rule { patterns, headers })
}

fn header_map(v: &serde_yaml::Value, at: &str) -> anyhow::Result<Vec<(HeaderName, HeaderValue)>> {
    let m = v
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("{at}: must be a mapping of header name to value"))?;
    let mut out: Vec<(HeaderName, HeaderValue)> = Vec::new();
    for (k, val) in m {
        let name = k
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("{at}: header names must be strings"))?;
        let value = val
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("{at}.{name}: value must be a string (quote it)"))?;
        let (n, v) = validate_header(name, value).map_err(|e| anyhow::anyhow!("{at}.{name}: {e}"))?;
        if out.iter().any(|(seen, _)| *seen == n) {
            anyhow::bail!("{at}: {name} is listed twice");
        }
        out.push((n, v));
    }
    Ok(out)
}

/// RFC 9110 token characters.
fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// Check one header: token name, not reserved, value with no control
/// characters (CR, LF, NUL and friends; tab is allowed) and no surrounding
/// whitespace.
pub fn validate_header(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), String> {
    if name.is_empty() || !name.chars().all(is_tchar) {
        return Err("header name must be a token (letters, digits, and !#$%&'*+-.^_`|~)".into());
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED.contains(&lower.as_str()) {
        return Err(format!("{name} is set by the server and cannot be configured"));
    }
    if value.trim().is_empty() {
        return Err("value is empty".into());
    }
    if value.chars().any(|c| (c.is_control() && c != '\t') || c == '\u{7f}') {
        return Err("value contains a control character (CR, LF, or similar)".into());
    }
    if value != value.trim() {
        return Err("value has leading or trailing whitespace".into());
    }
    let n = HeaderName::from_bytes(lower.as_bytes()).map_err(|e| e.to_string())?;
    let v = HeaderValue::from_str(value).map_err(|e| e.to_string())?;
    Ok((n, v))
}

fn validate_pattern(p: &str) -> Result<(), String> {
    if !p.starts_with('/') {
        return Err("must start with /".into());
    }
    if p.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("must not contain whitespace or control characters".into());
    }
    if p.contains('?') || p.contains('#') {
        return Err("matches the path only; drop the query or fragment".into());
    }
    Ok(())
}

/// Canonical form for matching, the same file resolution `serve` uses:
/// leading and trailing slashes trimmed, and a trailing `index.html` dropped,
/// so `/signup`, `/signup/`, `//signup` and `/signup/index.html` all compare
/// as `signup`. Without this a page could be fetched by an alias path that
/// skips its headers.
pub fn normalize(path: &str) -> String {
    let mut p = path.trim_matches('/');
    if p == "index.html" {
        p = "";
    } else if let Some(stripped) = p.strip_suffix("/index.html") {
        p = stripped;
    }
    p.trim_end_matches('/').to_string()
}

/// `*` matches any run of characters, including `/` and the empty string.
/// Everything else is literal, compared ASCII case-insensitively: on a
/// case-insensitive filesystem `/Signup` serves the same file as `/signup`,
/// and must not lose its headers.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p = pattern.to_ascii_lowercase().into_bytes();
    let s = path.to_ascii_lowercase().into_bytes();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] != b'*' && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// For logging: header names set per rule.
pub fn summary(h: &SiteHeaders) -> String {
    let mut names: BTreeMap<String, ()> = BTreeMap::new();
    for (n, _) in h.default.iter().chain(h.rules.iter().flat_map(|r| r.headers.iter())) {
        names.insert(n.as_str().to_string(), ());
    }
    format!(
        "{} default, {} rules ({})",
        h.default.len(),
        h.rules.len(),
        names.into_keys().collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(y: &str) -> anyhow::Result<SiteHeaders> {
        SiteHeaders::from_site_yaml(y)
    }

    fn get<'a>(h: &'a [(HeaderName, HeaderValue)], name: &str) -> Option<&'a str> {
        h.iter().find(|(n, _)| n == name).map(|(_, v)| v.to_str().unwrap())
    }

    const SAMPLE: &str = r#"
title: x
headers:
  default:
    X-Frame-Options: SAMEORIGIN
    Permissions-Policy: "camera=()"
  rules:
    - path: /signup
      headers:
        Content-Security-Policy: "default-src 'self'"
    - path: [/docs/*, /guides/*]
      headers:
        X-Robots-Tag: noindex
    - path: /docs/public/*
      headers:
        X-Robots-Tag: all
        X-Frame-Options: DENY
"#;

    #[test]
    fn missing_block_is_empty() {
        assert!(parse("title: x\n").unwrap().is_empty());
        assert!(parse("headers:\n").unwrap().is_empty());
    }

    #[test]
    fn default_applies_everywhere() {
        let h = parse(SAMPLE).unwrap();
        let r = h.resolve("/anything/at/all");
        assert_eq!(get(&r, "x-frame-options"), Some("SAMEORIGIN"));
        assert_eq!(get(&r, "permissions-policy"), Some("camera=()"));
        assert_eq!(get(&r, "content-security-policy"), None);
        assert_eq!(get(&h.resolve("/"), "x-frame-options"), Some("SAMEORIGIN"));
    }

    #[test]
    fn exact_path_matches_every_slash_form_and_index_alias() {
        let h = parse(SAMPLE).unwrap();
        for p in ["/signup", "/signup/", "//signup", "/signup/index.html", "signup", "/SignUp"] {
            assert_eq!(
                get(&h.resolve(p), "content-security-policy"),
                Some("default-src 'self'"),
                "{p}"
            );
        }
        for p in ["/signups", "/signup/next", "/x/signup", "/signup%2F"] {
            assert_eq!(get(&h.resolve(p), "content-security-policy"), None, "{p}");
        }
    }

    #[test]
    fn glob_matches_children_not_the_parent() {
        let h = parse(SAMPLE).unwrap();
        assert_eq!(get(&h.resolve("/docs/a"), "x-robots-tag"), Some("noindex"));
        assert_eq!(get(&h.resolve("/docs/a/b/"), "x-robots-tag"), Some("noindex"));
        assert_eq!(get(&h.resolve("/guides/x"), "x-robots-tag"), Some("noindex"));
        assert_eq!(get(&h.resolve("/docs"), "x-robots-tag"), None);
        assert_eq!(get(&h.resolve("/docsx"), "x-robots-tag"), None);
        assert!(glob_match("events/*/speakers", "events/2026/speakers"));
        assert!(!glob_match("events/*/speakers", "events/2026/talks"));
        assert!(glob_match("*", "anything/here"));
    }

    #[test]
    fn later_rules_override_earlier_per_header() {
        let h = parse(SAMPLE).unwrap();
        let r = h.resolve("/docs/public/page");
        assert_eq!(get(&r, "x-robots-tag"), Some("all"));
        assert_eq!(get(&r, "x-frame-options"), Some("DENY"));
        // Untouched default survives the override.
        assert_eq!(get(&r, "permissions-policy"), Some("camera=()"));
        assert_eq!(r.iter().filter(|(n, _)| n == "x-frame-options").count(), 1);
    }

    #[test]
    fn rejects_bad_names() {
        for name in ["Bad Name", "Bad:Name", "", "Café", "X(1)"] {
            let y = format!("headers:\n  default:\n    \"{name}\": v\n");
            assert!(parse(&y).is_err(), "{name:?} should be rejected");
        }
    }

    #[test]
    fn rejects_crlf_and_control_chars_in_values() {
        for v in ["a\\r\\nSet-Cookie: x=1", "a\\nb", "a\\u0000b", "a\\u007fb"] {
            let y = format!("headers:\n  default:\n    X-Test: \"{v}\"\n");
            let err = parse(&y).unwrap_err().to_string();
            assert!(err.contains("control character"), "{v}: {err}");
        }
        assert!(validate_header("X-Test", "a\tb").is_ok());
        assert!(validate_header("X-Test", "").is_err());
        assert!(validate_header("X-Test", " padded").is_err());
    }

    #[test]
    fn rejects_hop_by_hop_and_server_owned_headers() {
        for name in [
            "Connection",
            "Transfer-Encoding",
            "Content-Length",
            "content-type",
            "Content-Encoding",
            "Location",
            "Set-Cookie",
            "Vary",
            "X-Content-Type-Options",
            "Upgrade",
        ] {
            let err = validate_header(name, "x").unwrap_err();
            assert!(err.contains("set by the server"), "{name}: {err}");
        }
        assert!(validate_header("Referrer-Policy", "no-referrer").is_ok());
        assert!(validate_header("Cache-Control", "no-store").is_ok());
    }

    #[test]
    fn rejects_malformed_structure() {
        assert!(parse("headers: [a]\n").is_err());
        assert!(parse("headers:\n  defaults: {}\n").is_err());
        assert!(parse("headers:\n  rules:\n    - headers: {X-A: b}\n").is_err());
        assert!(parse("headers:\n  rules:\n    - path: signup\n      headers: {X-A: b}\n").is_err());
        assert!(parse("headers:\n  rules:\n    - path: /a?b=1\n      headers: {X-A: b}\n").is_err());
        assert!(parse("headers:\n  rules:\n    - path: /a\n      headers: {}\n").is_err());
        assert!(parse("headers:\n  rules:\n    - path: /a\n      header: {X-A: b}\n").is_err());
        assert!(parse("headers:\n  default:\n    X-A: 1\n").is_err());
        assert!(parse("headers:\n  default:\n    X-A: a\n    x-a: b\n").is_err());
    }
}

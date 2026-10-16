//! Build-time image pipeline, opt-in per site with `images:` in site.yaml.
//!
//! Templates call `image(src=…, alt=…)` (an `<picture>`/`<img>` with
//! `srcset`, `sizes`, `width` and `height`) and `src | image_url` (one local
//! URL, for CSS backgrounds and `og:image`). Portable Text image blocks
//! without a `pt/image.html` override render through the same path.
//!
//! Rendering only records each call and prints a marker. After every page is
//! rendered, [`Pipeline::finish`] fetches each distinct source once (in
//! parallel), reads its size from the header, writes the variants under
//! `dist/<output_dir>/` (in parallel), and swaps the markers in the HTML for
//! the final markup. So only images a template actually uses are fetched and
//! written.
//!
//! Remote sources: https only, from `allowed_hosts`, every redirect hop
//! re-checked, and the resolver refuses non-public addresses (SSRF). Bodies
//! are capped at `max_bytes`, images at `max_pixels` before any pixel is
//! decoded. Fetched bytes go to a content-addressed cache (`cache_dir`)
//! reused across builds; `IMAGES_OFFLINE=1` builds from the cache alone.
//! Output names are `<hash>-<width>.<ext>` built from hex digests and numbers
//! only, so no source name reaches the file system.
//!
//! Raster images (JPEG, PNG, static WebP) are resized to the configured
//! widths (never upscaled), encoded as AVIF and/or WebP plus a fallback in
//! the source format, with metadata dropped (EXIF orientation is applied
//! first). SVG, GIF and animated PNG/WebP are copied as they are; an SVG with
//! script, event handlers, `foreignObject` or external references is refused
//! and keeps its original URL. Sources the pipeline does not handle (another
//! host, plain http, a failed fetch) render with their original URL and a
//! warning, so a build never breaks on one bad image.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Cursor;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Widths generated when `images.widths` is not set.
pub const DEFAULT_WIDTHS: [u32; 8] = [320, 480, 640, 768, 1024, 1280, 1600, 1920];
const MAX_REDIRECTS: usize = 3;
/// WebP's format limit on either side.
const WEBP_MAX_SIDE: u32 = 16383;
/// Upper bound for any configured width.
const MAX_WIDTH: u32 = 8192;
const ACCEPT: &str = "image/webp,image/png,image/jpeg,image/gif,image/svg+xml;q=0.9,*/*;q=0.1";

/// A modern format generated next to the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modern {
    Avif,
    Webp,
}

/// site.yaml `images:`. Every key is optional; the block's presence turns
/// the pipeline on (`enabled: false` turns it off again).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagesCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Remote hosts images may be fetched from (exact names, or `*.example.com`
    /// for subdomains). Anything else keeps its original URL.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default = "default_widths")]
    pub widths: Vec<u32>,
    /// Modern formats, in `<source>` order (default `[webp]`).
    #[serde(default = "default_formats")]
    pub formats: Vec<Modern>,
    #[serde(default = "default_webp_quality")]
    pub webp_quality: u8,
    #[serde(default = "default_avif_quality")]
    pub avif_quality: u8,
    /// rav1e speed, 1 (slow, smaller) to 10 (fast).
    #[serde(default = "default_avif_speed")]
    pub avif_speed: u8,
    #[serde(default = "default_jpeg_quality")]
    pub jpeg_quality: u8,
    /// Width of the single fallback `<img src>` when modern formats are on.
    #[serde(default = "default_fallback_width")]
    pub fallback_width: u32,
    /// Largest response body accepted, in bytes.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Largest image accepted (width x height), checked from the header.
    #[serde(default = "default_max_pixels")]
    pub max_pixels: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Cache directory, relative to the site root unless absolute
    /// (`IMAGES_CACHE_DIR` overrides it).
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Output directory under dist/ (letters, digits, `-`, `_`, `/`).
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
}

fn yes() -> bool {
    true
}
fn default_widths() -> Vec<u32> {
    DEFAULT_WIDTHS.to_vec()
}
fn default_formats() -> Vec<Modern> {
    vec![Modern::Webp]
}
fn default_webp_quality() -> u8 {
    75
}
fn default_avif_quality() -> u8 {
    55
}
fn default_avif_speed() -> u8 {
    8
}
fn default_jpeg_quality() -> u8 {
    80
}
fn default_fallback_width() -> u32 {
    1280
}
fn default_max_bytes() -> u64 {
    25 * 1024 * 1024
}
fn default_max_pixels() -> u64 {
    50_000_000
}
fn default_timeout() -> u64 {
    30
}
fn default_cache_dir() -> String {
    ".cache/images".into()
}
fn default_output_dir() -> String {
    "assets/img".into()
}

impl ImagesCfg {
    /// The `images:` block of a site.yaml, validated. `Ok(None)` when it is
    /// absent or `enabled: false`.
    pub fn from_site_yaml(raw: &str) -> Result<Option<ImagesCfg>, String> {
        let doc: serde_yaml::Value = serde_yaml::from_str(raw).map_err(|e| format!("site.yaml: {e}"))?;
        let Some(block) = doc.get("images").filter(|v| !v.is_null()) else { return Ok(None) };
        let mut cfg: ImagesCfg =
            serde_yaml::from_value(block.clone()).map_err(|e| format!("site.yaml images: {e}"))?;
        if !cfg.enabled {
            return Ok(None);
        }
        cfg.validate().map_err(|e| format!("site.yaml images: {e}"))?;
        Ok(Some(cfg))
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.widths.is_empty() || self.widths.iter().any(|w| *w == 0 || *w > MAX_WIDTH) {
            return Err(format!("widths must be 1 to {MAX_WIDTH} and not empty"));
        }
        self.widths.sort_unstable();
        self.widths.dedup();
        let mut seen = BTreeSet::new();
        self.formats.retain(|f| seen.insert(*f));
        for (name, q) in [("webp_quality", self.webp_quality), ("avif_quality", self.avif_quality), ("jpeg_quality", self.jpeg_quality)] {
            if !(1..=100).contains(&q) {
                return Err(format!("{name} must be 1 to 100"));
            }
        }
        if !(1..=10).contains(&self.avif_speed) {
            return Err("avif_speed must be 1 to 10".into());
        }
        if self.fallback_width == 0 || self.max_bytes == 0 || self.max_pixels == 0 || self.timeout_secs == 0 {
            return Err("fallback_width, max_bytes, max_pixels and timeout_secs must be positive".into());
        }
        let out_ok = !self.output_dir.is_empty()
            && self.output_dir.split('/').all(|s| {
                !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            });
        if !out_ok {
            return Err(format!("output_dir {:?} must be a relative path of letters, digits, - and _", self.output_dir));
        }
        for h in &mut self.allowed_hosts {
            *h = h.to_ascii_lowercase();
            let name = h.strip_prefix("*.").unwrap_or(h);
            let ok = !name.is_empty()
                && name.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            if !ok {
                return Err(format!("allowed_hosts entry {h:?} must be a host name (no scheme, port or path)"));
            }
        }
        Ok(())
    }

    /// Fingerprint of everything that changes output bytes; part of every
    /// output name, so a settings change never reuses a cached file name.
    fn settings(&self) -> String {
        format!(
            "v1|webp{}|avif{}s{}|jpeg{}|png",
            self.webp_quality, self.avif_quality, self.avif_speed, self.jpeg_quality
        )
    }

    fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allowed_hosts.iter().any(|h| match h.strip_prefix("*.") {
            Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
            None => *h == host,
        })
    }
}

// ---- fetching --------------------------------------------------------------

/// What a transport returns for one request (no redirect following).
#[derive(Debug, Clone, Default)]
pub struct RawResponse {
    pub status: u16,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// One HTTP GET: `(url, max_bytes, timeout_secs)`. Must not follow redirects.
pub type Transport = dyn Fn(&str, u64, u64) -> Result<RawResponse, String> + Send + Sync;

/// Why a URL is not fetched. `Foreign` sources are not the pipeline's
/// (plain http, another host) and render untouched without a warning.
#[derive(Debug, PartialEq)]
enum UrlCheck {
    Foreign(String),
    Bad(String),
}

/// An https URL on an allowed host, default port, no user info.
fn check_url(url: &str, cfg: &ImagesCfg) -> Result<(), UrlCheck> {
    let uri: ureq::http::Uri = url.parse().map_err(|_| UrlCheck::Bad(format!("unparseable URL {url:?}")))?;
    if uri.scheme_str().map(|s| s.to_ascii_lowercase()) != Some("https".into()) {
        return Err(UrlCheck::Foreign("not https".into()));
    }
    let auth = uri.authority().ok_or_else(|| UrlCheck::Bad("no host".into()))?;
    if auth.as_str().contains('@') {
        return Err(UrlCheck::Bad("user info in URL".into()));
    }
    if auth.port_u16().is_some_and(|p| p != 443) {
        return Err(UrlCheck::Bad("non-default port".into()));
    }
    if !cfg.host_allowed(auth.host()) {
        return Err(UrlCheck::Foreign(format!("host {} not in allowed_hosts", auth.host())));
    }
    Ok(())
}

/// Absolute URL of a redirect `Location` relative to `base`.
fn join_location(base: &str, loc: &str) -> Result<String, String> {
    let loc = loc.trim();
    if loc.contains("://") {
        return Ok(loc.to_string());
    }
    if let Some(rest) = loc.strip_prefix("//") {
        return Ok(format!("https://{rest}"));
    }
    let uri: ureq::http::Uri = base.parse().map_err(|_| "bad base URL".to_string())?;
    let origin = format!("https://{}", uri.authority().ok_or("no host")?.as_str());
    if loc.starts_with('/') {
        return Ok(format!("{origin}{loc}"));
    }
    let path = uri.path();
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    Ok(format!("{origin}{dir}{loc}"))
}

/// GET `url`, following at most [`MAX_REDIRECTS`] redirects, each target
/// checked against https and the allowlist before it is requested.
fn fetch_checked(url: &str, cfg: &ImagesCfg, transport: &Transport) -> Result<Vec<u8>, String> {
    let mut cur = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        check_url(&cur, cfg).map_err(|e| match e {
            UrlCheck::Foreign(m) | UrlCheck::Bad(m) => format!("refused {cur}: {m}"),
        })?;
        let r = transport(&cur, cfg.max_bytes, cfg.timeout_secs)?;
        match r.status {
            200 => {
                if r.body.len() as u64 > cfg.max_bytes {
                    return Err(format!("body over max_bytes ({})", cfg.max_bytes));
                }
                return Ok(r.body);
            }
            301 | 302 | 303 | 307 | 308 => {
                let loc = r.location.ok_or("redirect without Location")?;
                cur = join_location(&cur, &loc)?;
            }
            s => return Err(format!("HTTP {s}")),
        }
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// True for addresses on the public internet: not loopback, private,
/// link-local, CGNAT, multicast, documentation, or other reserved ranges
/// (IPv4-mapped and NAT64 IPv6 are checked as their IPv4 address).
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                let v4 = std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
                return is_public_ip(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || s[0..6] == [0, 0, 0, 0, 0, 0])
        }
    }
}

/// The system resolver, keeping only public addresses; ureq connects to
/// exactly what this returns, so a host cannot re-resolve to a private one.
struct PublicResolver;

impl std::fmt::Debug for PublicResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublicResolver")
    }
}

impl ureq::unversioned::resolver::Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let all = ureq::unversioned::resolver::DefaultResolver::default().resolve(uri, config, timeout)?;
        let mut out = self.empty();
        for a in all.iter().filter(|a| is_public_ip(a.ip())) {
            out.push(*a);
        }
        if out.is_empty() {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} resolves only to non-public addresses", uri.host().unwrap_or("")),
            )));
        }
        Ok(out)
    }
}

/// The real transport: https only, no redirects, no proxy, public addresses
/// only, a global timeout and a body cap.
pub fn ureq_transport(timeout_secs: u64) -> Box<Transport> {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
        .user_agent("post-cms-images/1")
        .build();
    let agent = ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::default(),
        PublicResolver,
    );
    Box::new(move |url: &str, max_bytes: u64, _timeout: u64| {
        let mut resp = agent.get(url).header("Accept", ACCEPT).call().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = if status == 200 {
            resp.body_mut()
                .with_config()
                .limit(max_bytes)
                .read_to_vec()
                .map_err(|e| format!("body: {e}"))?
        } else {
            Vec::new()
        };
        Ok(RawResponse { status, location, body })
    })
}

// ---- sources ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Enc {
    Avif,
    Webp,
    Jpeg,
    Png,
}

impl Enc {
    fn ext(self) -> &'static str {
        match self {
            Enc::Avif => "avif",
            Enc::Webp => "webp",
            Enc::Jpeg => "jpg",
            Enc::Png => "png",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Enc::Avif => "image/avif",
            Enc::Webp => "image/webp",
            Enc::Jpeg => "image/jpeg",
            Enc::Png => "image/png",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Resized and re-encoded; `fallback` is the source format.
    Raster { fallback: Enc },
    /// Copied byte for byte (SVG, GIF, animated PNG/WebP).
    Passthrough { ext: &'static str },
}

#[derive(Debug)]
struct Source {
    /// sha256 of the bytes: the cache key.
    content: String,
    /// 16 hex digits of sha256(content | settings): the output name prefix.
    id: String,
    /// Display size (EXIF orientation applied); 0 when unknown (SVG without size).
    width: u32,
    height: u32,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Out {
    width: u32,
    enc: Enc,
}

fn sha_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    let t = head.trim_start_matches('\u{feff}').trim_start();
    (t.starts_with("<svg") || t.starts_with("<?xml") || t.starts_with("<!--") || t.starts_with("<!doctype svg"))
        && String::from_utf8_lossy(bytes).to_ascii_lowercase().contains("<svg")
}

/// Refuse an SVG that could run script or load anything when opened directly:
/// script, event handler attributes, foreignObject, embedded documents,
/// entity declarations, `javascript:`, animations that target links, and
/// references other than `#fragment` or an inline raster `data:image/…`.
pub fn svg_problem(bytes: &[u8]) -> Option<String> {
    let Ok(text) = std::str::from_utf8(bytes) else { return Some("not UTF-8".into()) };
    let t = text.to_ascii_lowercase();
    for bad in ["<script", "<foreignobject", "<iframe", "<embed", "<object", "<!entity", "javascript:", "data:text", "@import", "<handler", "<listener", ] {
        if t.contains(bad) {
            return Some(format!("contains {bad:?}"));
        }
    }
    let b = t.as_bytes();
    for i in 1..b.len().saturating_sub(2) {
        // an attribute named on*: whitespace or '/' or quote before "on", letters, then '='
        if (b[i - 1].is_ascii_whitespace() || b[i - 1] == b'/' || b[i - 1] == b'"' || b[i - 1] == b'\'') && b[i] == b'o' && b[i + 1] == b'n' {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_alphabetic() {
                j += 1;
            }
            let mut k = j;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if j > i + 2 && k < b.len() && b[k] == b'=' {
                return Some("event handler attribute".into());
            }
        }
    }
    // href / xlink:href / src / url() must point inside the document
    // (whitespace around `=` and inside `url(` dropped first).
    let mut norm = String::with_capacity(t.len());
    for c in t.chars() {
        if c.is_whitespace() && (norm.ends_with('=') || norm.ends_with('(')) {
            continue;
        }
        if c == '=' || c == '(' {
            while norm.ends_with(char::is_whitespace) {
                norm.pop();
            }
        }
        norm.push(c);
    }
    let t = norm;
    for target in ["attributename=\"href\"", "attributename=\"xlink:href\"", "attributename='href'", "attributename='xlink:href'"] {
        if t.contains(target) {
            return Some("animation of a link".into());
        }
    }
    let inline = |v: &str| {
        v.starts_with('#') || ["png", "jpeg", "jpg", "gif", "webp"].iter().any(|f| v.starts_with(&format!("data:image/{f};")))
    };
    for key in ["href=", "src="] {
        let mut from = 0;
        while let Some(p) = t[from..].find(key) {
            let v = t[from + p + key.len()..].trim_start_matches(['"', '\'', ' ']);
            if !inline(v) {
                return Some(format!("external reference ({key})"));
            }
            from += p + key.len();
        }
    }
    let mut from = 0;
    while let Some(p) = t[from..].find("url(") {
        let v = t[from + p + 4..].trim_start_matches(['"', '\'', ' ']);
        if !inline(v) {
            return Some("external reference (url())".into());
        }
        from += p + 4;
    }
    None
}

/// `width`/`height` of the root `<svg>` in px, else from `viewBox`.
fn svg_dims(bytes: &[u8]) -> (u32, u32) {
    let t = String::from_utf8_lossy(bytes);
    let Some(start) = t.find("<svg") else { return (0, 0) };
    let tag = &t[start..start + t[start..].find('>').unwrap_or(t.len() - start)];
    let attr = |name: &str| -> Option<String> {
        let mut from = 0;
        while let Some(p) = tag[from..].find(name) {
            let at = from + p;
            let before = tag[..at].chars().last();
            let rest = tag[at + name.len()..].trim_start();
            if before.is_some_and(char::is_whitespace) && rest.starts_with('=') {
                let rest = rest[1..].trim_start();
                let q = rest.chars().next()?;
                if q == '"' || q == '\'' {
                    let end = rest[1..].find(q)?;
                    return Some(rest[1..1 + end].to_string());
                }
            }
            from = at + name.len();
        }
        None
    };
    let px = |v: Option<String>| -> Option<u32> {
        let v = v?;
        let v = v.trim().trim_end_matches("px");
        let n: f64 = v.parse().ok()?;
        (n > 0.0 && n < 100_000.0).then(|| n.round() as u32)
    };
    if let (Some(w), Some(h)) = (px(attr("width")), px(attr("height"))) {
        return (w, h);
    }
    if let Some(vb) = attr("viewBox") {
        let n: Vec<f64> = vb.split([' ', ',']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        if n.len() == 4 && n[2] > 0.0 && n[3] > 0.0 && n[2] < 100_000.0 && n[3] < 100_000.0 {
            return (n[2].round() as u32, n[3].round() as u32);
        }
    }
    (0, 0)
}

/// An APNG has an `acTL` chunk before its first `IDAT`.
fn is_apng(b: &[u8]) -> bool {
    let mut i = 8;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let ty = &b[i + 4..i + 8];
        if ty == b"acTL" {
            return true;
        }
        if ty == b"IDAT" {
            return false;
        }
        i = i.saturating_add(12).saturating_add(len);
    }
    false
}

/// A WebP with the VP8X animation flag or an `ANIM` chunk.
fn is_animated_webp(b: &[u8]) -> bool {
    b.len() > 21 && &b[12..16] == b"VP8X" && (b[20] & 0x02) != 0
}

fn reader(bytes: &[u8], fmt: ImageFormat, max_pixels: u64) -> ImageReader<Cursor<&[u8]>> {
    let mut r = ImageReader::new(Cursor::new(bytes));
    r.set_format(fmt);
    let mut limits = image::Limits::default();
    let side = max_pixels.min(u32::MAX as u64) as u32;
    limits.max_image_width = Some(side);
    limits.max_image_height = Some(side);
    r.limits(limits);
    r
}

/// Format, display size and handling of `bytes`, from the header only; the
/// pixel limit is checked here, before anything is decoded.
fn probe(bytes: &[u8], cfg: &ImagesCfg) -> Result<(u32, u32, Kind), String> {
    if looks_like_svg(bytes) {
        if let Some(p) = svg_problem(bytes) {
            return Err(format!("unsafe SVG: {p}"));
        }
        let (w, h) = svg_dims(bytes);
        return Ok((w, h, Kind::Passthrough { ext: "svg" }));
    }
    let fmt = image::guess_format(bytes).map_err(|_| "unknown image format".to_string())?;
    let kind = match fmt {
        ImageFormat::Gif => Kind::Passthrough { ext: "gif" },
        ImageFormat::Png if is_apng(bytes) => Kind::Passthrough { ext: "png" },
        ImageFormat::WebP if is_animated_webp(bytes) => Kind::Passthrough { ext: "webp" },
        ImageFormat::Png => Kind::Raster { fallback: Enc::Png },
        ImageFormat::Jpeg => Kind::Raster { fallback: Enc::Jpeg },
        ImageFormat::WebP => Kind::Raster { fallback: Enc::Webp },
        other => return Err(format!("unsupported format {other:?}")),
    };
    let mut dec = reader(bytes, fmt, cfg.max_pixels)
        .into_decoder()
        .map_err(|e| format!("header: {e}"))?;
    let (w, h) = dec.dimensions();
    if w == 0 || h == 0 {
        return Err("zero size".into());
    }
    if w as u64 * h as u64 > cfg.max_pixels {
        return Err(format!("{w}x{h} is over max_pixels ({})", cfg.max_pixels));
    }
    use image::metadata::Orientation::*;
    let swap = matches!(dec.orientation().unwrap_or(NoTransforms), Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH);
    Ok(if swap { (h, w, kind) } else { (w, h, kind) })
}

/// Decode (applying EXIF orientation) under the same limits as [`probe`].
fn decode(bytes: &[u8], cfg: &ImagesCfg) -> Result<DynamicImage, String> {
    let fmt = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let mut dec = reader(bytes, fmt, cfg.max_pixels).into_decoder().map_err(|e| e.to_string())?;
    let o = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| e.to_string())?;
    img.apply_orientation(o);
    Ok(img)
}

fn encode(img: &DynamicImage, enc: Enc, cfg: &ImagesCfg) -> Result<Vec<u8>, String> {
    let alpha = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] != 255);
    let mut buf = Cursor::new(Vec::new());
    match enc {
        Enc::Webp => {
            let (w, h) = (img.width(), img.height());
            let mem = if alpha {
                let px = img.to_rgba8();
                webp::Encoder::from_rgba(&px, w, h).encode_simple(false, cfg.webp_quality as f32)
            } else {
                let px = img.to_rgb8();
                webp::Encoder::from_rgb(&px, w, h).encode_simple(false, cfg.webp_quality as f32)
            }
            .map_err(|e| format!("webp: {e:?}"))?;
            return Ok(mem.to_vec());
        }
        Enc::Avif => {
            let e = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut buf, cfg.avif_speed, cfg.avif_quality)
                .with_num_threads(Some(1));
            if alpha {
                img.to_rgba8().write_with_encoder(e)
            } else {
                DynamicImage::ImageRgb8(img.to_rgb8()).write_with_encoder(e)
            }
            .map_err(|e| format!("avif: {e}"))?;
        }
        Enc::Jpeg => {
            let e = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, cfg.jpeg_quality);
            img.to_rgb8().write_with_encoder(e).map_err(|e| format!("jpeg: {e}"))?;
        }
        Enc::Png => {
            let e = image::codecs::png::PngEncoder::new_with_quality(
                &mut buf,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            if alpha { img.to_rgba8().write_with_encoder(e) } else { img.to_rgb8().write_with_encoder(e) }
                .map_err(|e| format!("png: {e}"))?;
        }
    }
    Ok(buf.into_inner())
}

/// Percent-decode a site path; `None` on bad escapes or a decoded NUL.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    (!s.contains('\0')).then_some(s)
}

/// The file under `dist` a site path names, refusing `..`, absolute
/// segments, and anything (symlinks included) that lands outside `dist`.
fn local_file(dist: &Path, src: &str) -> Result<PathBuf, String> {
    let path = src.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(path).ok_or("bad escape in path")?;
    let rel = decoded.trim_start_matches('/');
    let rel_path = Path::new(rel);
    if rel.contains('\\') || !rel_path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("path escapes the site".into());
    }
    let root = dist.canonicalize().map_err(|e| format!("dist: {e}"))?;
    let file = root.join(rel_path).canonicalize().map_err(|_| "no such file in dist".to_string())?;
    if !file.starts_with(&root) || !file.is_file() {
        return Err("path escapes the site".into());
    }
    Ok(file)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

// ---- requests and markers --------------------------------------------------

/// One `image()` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ImgArgs {
    pub src: String,
    pub alt: String,
    pub sizes: Option<String>,
    pub class: Option<String>,
    pub loading: Option<String>,
    pub fetchpriority: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Request {
    Img(ImgArgs),
    Url { src: String, width: Option<u32>, absolute: bool },
}

impl Request {
    fn src(&self) -> &str {
        match self {
            Request::Img(a) => &a.src,
            Request::Url { src, .. } => src,
        }
    }
}

#[derive(Default)]
struct State {
    requests: Vec<Request>,
    index: HashMap<Request, usize>,
    /// Requests already turned into markup, by index.
    done: Vec<Option<String>>,
    resolved: HashMap<String, Result<Arc<Source>, Option<String>>>,
    outputs: BTreeMap<String, (Arc<Source>, BTreeSet<Out>)>,
    written: BTreeSet<(String, Out)>,
    passthrough_written: BTreeSet<String>,
}

/// Totals from [`Pipeline::finish`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub sources: usize,
    pub files: usize,
    pub bytes: u64,
    pub fallbacks: usize,
}

pub struct Pipeline {
    cfg: ImagesCfg,
    dist: PathBuf,
    cache: PathBuf,
    base_url: String,
    offline: bool,
    nonce: String,
    transport: Box<Transport>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline").field("output_dir", &self.cfg.output_dir).field("cache", &self.cache).finish()
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#x27;"),
            _ => o.push(c),
        }
    }
    o
}

/// `<img>` attributes after `src`/`srcset`, in a fixed order.
fn img_tail(a: &ImgArgs, w: Option<u32>, h: Option<u32>) -> String {
    let mut s = format!(" alt=\"{}\"", esc(&a.alt));
    if let Some(c) = a.class.as_deref().filter(|c| !c.is_empty()) {
        s.push_str(&format!(" class=\"{}\"", esc(c)));
    }
    if let (Some(w), Some(h)) = (a.width.or(w).filter(|v| *v > 0), a.height.or(h).filter(|v| *v > 0)) {
        s.push_str(&format!(" width=\"{w}\" height=\"{h}\""));
    }
    s.push_str(&format!(" loading=\"{}\" decoding=\"async\"", loading(a)));
    if let Some(p) = a.fetchpriority.as_deref().filter(|p| matches!(*p, "high" | "low" | "auto")) {
        s.push_str(&format!(" fetchpriority=\"{p}\""));
    }
    s.push('>');
    s
}

fn loading(a: &ImgArgs) -> &'static str {
    if a.loading.as_deref() == Some("eager") { "eager" } else { "lazy" }
}

fn sizes(a: &ImgArgs) -> String {
    match a.sizes.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) => esc(s),
        None if loading(a) == "lazy" => "auto, 100vw".into(),
        None => "100vw".into(),
    }
}

/// The markup for an image the pipeline does not handle: the original URL
/// (empty when it is not http(s) or a site path).
pub fn plain_img(a: &ImgArgs) -> String {
    match crate::pt::safe_src(&a.src) {
        Some(src) => format!("<img src=\"{}\"{}", esc(src), img_tail(a, None, None)),
        None => String::new(),
    }
}

impl Pipeline {
    pub fn new(cfg: ImagesCfg, site_root: &Path, dist: &Path, base_url: &str, offline: bool, transport: Box<Transport>) -> Pipeline {
        let cache = std::env::var("IMAGES_CACHE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| site_root.join(&cfg.cache_dir));
        let seed = format!("{:?}{}{:?}", std::time::SystemTime::now(), std::process::id(), dist);
        let nonce = sha_hex(seed.as_bytes())[..12].to_string();
        Pipeline {
            cfg,
            dist: dist.to_path_buf(),
            cache,
            base_url: base_url.trim_end_matches('/').to_string(),
            offline,
            nonce,
            transport,
            state: Mutex::new(State::default()),
        }
    }

    pub fn cfg(&self) -> &ImagesCfg {
        &self.cfg
    }

    fn marker(&self, req: Request) -> String {
        let mut st = self.state.lock().unwrap();
        let n = match st.index.get(&req) {
            Some(n) => *n,
            None => {
                let n = st.requests.len();
                st.requests.push(req.clone());
                st.done.push(None);
                st.index.insert(req, n);
                n
            }
        };
        format!("__postcms_img_{}_{}__", self.nonce, n)
    }

    /// Record an `image()` call; returns a marker that [`Pipeline::finish`]
    /// replaces with the markup.
    pub fn request_img(&self, a: ImgArgs) -> String {
        if crate::pt::safe_src(&a.src).is_none() {
            return String::new();
        }
        self.marker(Request::Img(a))
    }

    /// Record an `image_url` filter call; see [`Pipeline::request_img`].
    pub fn request_url(&self, src: &str, width: Option<u32>, absolute: bool) -> String {
        if crate::pt::safe_src(src).is_none() {
            return String::new();
        }
        self.marker(Request::Url { src: src.to_string(), width, absolute })
    }

    fn url_index(&self, url: &str) -> PathBuf {
        self.cache.join("urls").join(sha_hex(url.as_bytes()))
    }
    fn src_file(&self, content: &str) -> PathBuf {
        self.cache.join("src").join(content)
    }
    fn out_name(&self, s: &Source, o: Out) -> String {
        format!("{}-{}.{}", s.id, o.width, o.enc.ext())
    }

    /// Source bytes: a dist file, the cache, or the network (then cached).
    /// `Err(None)` means not ours (foreign host, plain http): no warning.
    fn load(&self, src: &str) -> Result<Vec<u8>, Option<String>> {
        if src.starts_with('/') {
            let f = local_file(&self.dist, src).map_err(Some)?;
            let meta = std::fs::metadata(&f).map_err(|e| Some(e.to_string()))?;
            if meta.len() > self.cfg.max_bytes {
                return Err(Some(format!("over max_bytes ({})", self.cfg.max_bytes)));
            }
            return std::fs::read(&f).map_err(|e| Some(e.to_string()));
        }
        match check_url(src, &self.cfg) {
            Err(UrlCheck::Foreign(_)) => return Err(None),
            Err(UrlCheck::Bad(m)) => return Err(Some(m)),
            Ok(()) => {}
        }
        let idx = self.url_index(src);
        if let Ok(content) = std::fs::read_to_string(&idx) {
            let content = content.trim();
            if content.len() == 64 && content.bytes().all(|c| c.is_ascii_hexdigit()) {
                if let Ok(b) = std::fs::read(self.src_file(content)) {
                    if sha_hex(&b) == content {
                        return Ok(b);
                    }
                }
            }
        }
        if self.offline {
            return Err(Some("not in the image cache (IMAGES_OFFLINE=1)".into()));
        }
        let bytes = fetch_checked(src, &self.cfg, &*self.transport).map_err(Some)?;
        let content = sha_hex(&bytes);
        write_atomic(&self.src_file(&content), &bytes).map_err(|e| Some(format!("cache: {e}")))?;
        write_atomic(&idx, content.as_bytes()).map_err(|e| Some(format!("cache: {e}")))?;
        Ok(bytes)
    }

    fn resolve_one(&self, src: &str) -> Result<Arc<Source>, Option<String>> {
        let bytes = self.load(src)?;
        let (width, height, kind) = probe(&bytes, &self.cfg).map_err(Some)?;
        let content = sha_hex(&bytes);
        if src.starts_with('/') {
            // local files go through the same cache, so finish() reads one place
            let f = self.src_file(&content);
            if !f.exists() {
                write_atomic(&f, &bytes).map_err(|e| Some(format!("cache: {e}")))?;
            }
        }
        let id = sha_hex(format!("{content}|{}", self.cfg.settings()).as_bytes())[..16].to_string();
        Ok(Arc::new(Source { content, id, width, height, kind }))
    }

    /// Widths for a source `w` px wide: the configured ones below it, plus
    /// `min(w, largest)`. Never larger than the source.
    fn widths_for(&self, w: u32) -> Vec<u32> {
        let top = w.min(*self.cfg.widths.last().unwrap());
        let mut v: Vec<u32> = self.cfg.widths.iter().copied().filter(|x| *x < top).collect();
        v.push(top);
        v
    }

    fn height_for(s: &Source, w: u32) -> u32 {
        ((s.height as f64 * w as f64 / s.width as f64).round() as u32).max(1)
    }

    fn url_path(&self, name: &str) -> String {
        format!("/{}/{}", self.cfg.output_dir, name)
    }

    /// Markup or URL for one request, registering the outputs it needs.
    fn materialize(&self, st: &mut State, req: &Request) -> String {
        let res = st.resolved.get(req.src()).cloned();
        let src = match res {
            Some(Ok(s)) => s,
            _ => {
                return match req {
                    Request::Img(a) => plain_img(a),
                    Request::Url { src, absolute, .. } => {
                        let s = crate::pt::safe_src(src).unwrap_or("");
                        if *absolute && s.starts_with('/') { esc(&format!("{}{s}", self.base_url)) } else { esc(s) }
                    }
                };
            }
        };
        let mut need = |o: Out| {
            st.outputs.entry(src.id.clone()).or_insert_with(|| (src.clone(), BTreeSet::new())).1.insert(o);
        };
        match (req, src.kind) {
            (Request::Img(a), Kind::Passthrough { ext }) => {
                need(Out { width: 0, enc: Enc::Png });
                let url = self.url_path(&format!("{}.{ext}", src.id));
                let (w, h) = (src.width > 0).then_some((src.width, src.height)).unzip();
                format!("<img src=\"{}\"{}", esc(&url), img_tail(a, w, h))
            }
            (Request::Url { absolute, .. }, Kind::Passthrough { ext }) => {
                need(Out { width: 0, enc: Enc::Png });
                let url = self.url_path(&format!("{}.{ext}", src.id));
                esc(&if *absolute { format!("{}{url}", self.base_url) } else { url })
            }
            (Request::Url { width, absolute, .. }, Kind::Raster { fallback }) => {
                let top = *self.widths_for(src.width).last().unwrap();
                let w = width.map_or(top, |w| w.clamp(1, src.width));
                let o = Out { width: w, enc: fallback };
                need(o);
                let url = self.url_path(&self.out_name(&src, o));
                esc(&if *absolute { format!("{}{url}", self.base_url) } else { url })
            }
            (Request::Img(a), Kind::Raster { fallback }) => {
                let widths = self.widths_for(src.width);
                let top = *widths.last().unwrap();
                let tall = Self::height_for(&src, top) > WEBP_MAX_SIDE || top > WEBP_MAX_SIDE;
                let modern: Vec<Enc> = self
                    .cfg
                    .formats
                    .iter()
                    .map(|m| match m {
                        Modern::Avif => Enc::Avif,
                        Modern::Webp => Enc::Webp,
                    })
                    .filter(|e| !(tall && *e == Enc::Webp))
                    .collect();
                let srcset = |enc: Enc, need: &mut dyn FnMut(Out)| -> String {
                    widths
                        .iter()
                        .map(|w| {
                            let o = Out { width: *w, enc };
                            need(o);
                            format!("{} {w}w", self.url_path(&self.out_name(&src, o)))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let sizes = sizes(a);
                let dims = (Some(top), Some(Self::height_for(&src, top)));
                if modern.iter().all(|e| *e == fallback) {
                    // no separate modern format: a plain <img> with srcset
                    let set = srcset(fallback, &mut need);
                    let fb = Out { width: top, enc: fallback };
                    return format!(
                        "<img src=\"{}\" srcset=\"{}\" sizes=\"{}\"{}",
                        self.url_path(&self.out_name(&src, fb)),
                        set,
                        sizes,
                        img_tail(a, dims.0, dims.1)
                    );
                }
                let mut html = String::from("<picture>");
                for enc in modern.iter().copied().filter(|e| *e != fallback) {
                    let set = srcset(enc, &mut need);
                    html.push_str(&format!("<source type=\"{}\" srcset=\"{set}\" sizes=\"{sizes}\">", enc.mime()));
                }
                let fb = Out { width: top.min(self.cfg.fallback_width), enc: fallback };
                need(fb);
                if modern.contains(&fallback) {
                    // a WebP source with webp listed: its srcset is the img's
                    let set = srcset(fallback, &mut need);
                    html.push_str(&format!(
                        "<img src=\"{}\" srcset=\"{set}\" sizes=\"{sizes}\"{}</picture>",
                        self.url_path(&self.out_name(&src, fb)),
                        img_tail(a, dims.0, dims.1)
                    ));
                } else {
                    html.push_str(&format!(
                        "<img src=\"{}\"{}</picture>",
                        self.url_path(&self.out_name(&src, fb)),
                        img_tail(a, dims.0, dims.1)
                    ));
                }
                html
            }
        }
    }

    /// Fetch and probe every pending source, write every variant, and replace
    /// the markers in dist's HTML. Safe to call again after more rendering.
    pub fn finish(&self, jobs: usize) -> Result<Stats, String> {
        let jobs = jobs.max(1);
        // 1. resolve new sources in parallel (network bound)
        let pending: Vec<String> = {
            let st = self.state.lock().unwrap();
            let set: BTreeSet<String> = st
                .requests
                .iter()
                .map(|r| r.src().to_string())
                .filter(|s| !st.resolved.contains_key(s))
                .collect();
            set.into_iter().collect()
        };
        let results: Vec<Mutex<Option<Result<Arc<Source>, Option<String>>>>> =
            pending.iter().map(|_| Mutex::new(None)).collect();
        run_parallel(pending.len(), jobs.max(8).min(32), |i| {
            *results[i].lock().unwrap() = Some(self.resolve_one(&pending[i]));
            Ok(())
        })?;
        let mut stats = Stats::default();
        let replacements: HashMap<usize, String>;
        let work: Vec<(Arc<Source>, Vec<Out>)>;
        {
            let mut st = self.state.lock().unwrap();
            for (src, r) in pending.iter().zip(results) {
                let r = r.into_inner().unwrap().unwrap();
                if let Err(Some(w)) = &r {
                    eprintln!("warning: image {src}: {w}; kept the original URL");
                }
                st.resolved.insert(src.clone(), r);
            }
            // 2. markup for every request not done yet
            let todo: Vec<usize> = (0..st.requests.len()).filter(|i| st.done[*i].is_none()).collect();
            for i in todo {
                let req = st.requests[i].clone();
                let html = self.materialize(&mut st, &req);
                st.done[i] = Some(html);
            }
            stats.fallbacks = st.resolved.values().filter(|r| r.is_err()).count();
            stats.sources = st.outputs.len();
            replacements = st.done.iter().enumerate().map(|(i, h)| (i, h.clone().unwrap_or_default())).collect();
            work = st
                .outputs
                .values()
                .map(|(s, outs)| {
                    let new: Vec<Out> = outs
                        .iter()
                        .copied()
                        .filter(|o| match s.kind {
                            Kind::Passthrough { .. } => !st.passthrough_written.contains(&s.id),
                            Kind::Raster { .. } => !st.written.contains(&(s.id.clone(), *o)),
                        })
                        .collect();
                    (s.clone(), new)
                })
                .filter(|(_, o)| !o.is_empty())
                .collect();
        }
        // 3. variants in parallel (CPU bound)
        let out_dir = self.dist.join(&self.cfg.output_dir);
        std::fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;
        let files = AtomicUsize::new(0);
        let bytes = std::sync::atomic::AtomicU64::new(0);
        run_parallel(work.len(), jobs, |i| {
            let (s, outs) = &work[i];
            let (n, b) = self.write_outputs(s, outs, &out_dir)?;
            files.fetch_add(n, Ordering::Relaxed);
            bytes.fetch_add(b, Ordering::Relaxed);
            Ok(())
        })?;
        {
            let mut st = self.state.lock().unwrap();
            for (s, outs) in &work {
                match s.kind {
                    Kind::Passthrough { .. } => {
                        st.passthrough_written.insert(s.id.clone());
                    }
                    Kind::Raster { .. } => {
                        for o in outs {
                            st.written.insert((s.id.clone(), *o));
                        }
                    }
                }
            }
        }
        stats.files = files.into_inner();
        stats.bytes = bytes.into_inner();
        // 4. swap markers in every HTML/XML file
        replace_markers(&self.dist, &self.nonce, &replacements)?;
        Ok(stats)
    }

    /// Write one source's outputs to dist, from the cache when present.
    fn write_outputs(&self, s: &Source, outs: &[Out], out_dir: &Path) -> Result<(usize, u64), String> {
        let ctx = |e: std::io::Error| format!("image {}: {e}", s.id);
        if let Kind::Passthrough { ext } = s.kind {
            let name = format!("{}.{ext}", s.id);
            let bytes = std::fs::read(self.src_file(&s.content)).map_err(ctx)?;
            std::fs::write(out_dir.join(&name), &bytes).map_err(ctx)?;
            return Ok((1, bytes.len() as u64));
        }
        let cached = |o: &Out| self.cache.join("out").join(self.out_name(s, *o));
        let missing: Vec<Out> = outs.iter().copied().filter(|o| !cached(o).is_file()).collect();
        if !missing.is_empty() {
            let raw = std::fs::read(self.src_file(&s.content)).map_err(ctx)?;
            let img = decode(&raw, &self.cfg).map_err(|e| format!("image {}: {e}", s.id))?;
            drop(raw);
            // largest first, each resize from the previous step (fewer pixels to filter)
            let mut by_width: BTreeMap<std::cmp::Reverse<u32>, Vec<Enc>> = BTreeMap::new();
            for o in &missing {
                by_width.entry(std::cmp::Reverse(o.width)).or_default().push(o.enc);
            }
            let mut cur = img;
            for (std::cmp::Reverse(w), encs) in by_width {
                let h = Self::height_for(s, w);
                if cur.width() != w || cur.height() != h {
                    cur = cur.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
                }
                for enc in encs {
                    let bytes = encode(&cur, enc, &self.cfg).map_err(|e| format!("image {} {w}w: {e}", s.id))?;
                    write_atomic(&cached(&Out { width: w, enc }), &bytes).map_err(ctx)?;
                }
            }
        }
        let mut total = 0;
        for o in outs {
            total += std::fs::copy(cached(o), out_dir.join(self.out_name(s, *o))).map_err(ctx)?;
        }
        Ok((outs.len(), total))
    }
}

/// Run `f(0..n)` on up to `jobs` threads; the first error wins.
fn run_parallel(n: usize, jobs: usize, f: impl Fn(usize) -> Result<(), String> + Sync) -> Result<(), String> {
    let next = AtomicUsize::new(0);
    let err: Mutex<Option<String>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(n) {
            scope.spawn(|| loop {
                if err.lock().unwrap().is_some() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    return;
                }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(i)))
                    .unwrap_or_else(|_| Err("image worker panicked".into()));
                if let Err(e) = r {
                    err.lock().unwrap().get_or_insert(e);
                    return;
                }
            });
        }
    });
    match err.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Replace `__postcms_img_<nonce>_<n>__` in every .html/.xml file under
/// `dist`. A marker with no replacement fails the build.
fn replace_markers(dist: &Path, nonce: &str, repl: &HashMap<usize, String>) -> Result<(), String> {
    let prefix = format!("__postcms_img_{nonce}_");
    let mut stack = vec![dist.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if !matches!(p.extension().and_then(|x| x.to_str()), Some("html" | "xml")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            if !text.contains(&prefix) {
                continue;
            }
            let mut out = String::with_capacity(text.len());
            let mut rest = text.as_str();
            while let Some(at) = rest.find(&prefix) {
                out.push_str(&rest[..at]);
                let after = &rest[at + prefix.len()..];
                let digits = after.bytes().take_while(u8::is_ascii_digit).count();
                let n: Option<usize> = after[..digits].parse().ok();
                match (n, after[digits..].starts_with("__")) {
                    (Some(n), true) if repl.contains_key(&n) => {
                        out.push_str(&repl[&n]);
                        rest = &after[digits + 2..];
                    }
                    _ => return Err(format!("{}: unresolved image marker", p.display())),
                }
            }
            out.push_str(rest);
            std::fs::write(&p, out).map_err(|e| format!("write {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

// ---- Tera ------------------------------------------------------------------

fn arg_str(args: &HashMap<String, Value>, k: &str) -> Option<String> {
    match args.get(k)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn arg_u32(args: &HashMap<String, Value>, k: &str) -> Option<u32> {
    match args.get(k)? {
        Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

struct ImageFn(Option<Arc<Pipeline>>);

impl tera::Function for ImageFn {
    fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let a = ImgArgs {
            src: arg_str(args, "src").unwrap_or_default(),
            alt: arg_str(args, "alt").unwrap_or_default(),
            sizes: arg_str(args, "sizes"),
            class: arg_str(args, "class"),
            loading: arg_str(args, "loading"),
            fetchpriority: arg_str(args, "fetchpriority"),
            width: arg_u32(args, "width"),
            height: arg_u32(args, "height"),
        };
        Ok(Value::String(match &self.0 {
            Some(p) => p.request_img(a),
            None => plain_img(&a),
        }))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

struct ImageUrlFilter(Option<Arc<Pipeline>>, String);

impl tera::Filter for ImageUrlFilter {
    fn filter(&self, value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let src = value.as_str().unwrap_or("");
        let absolute = args.get("absolute").and_then(Value::as_bool).unwrap_or(false);
        let width = arg_u32(args, "width");
        Ok(Value::String(match &self.0 {
            Some(p) => p.request_url(src, width, absolute),
            None => {
                let s = crate::pt::safe_src(src).unwrap_or("");
                esc(&if absolute && s.starts_with('/') { format!("{}{s}", self.1.trim_end_matches('/')) } else { s.to_string() })
            }
        }))
    }
    fn is_safe(&self) -> bool {
        true
    }
}

/// Register `image()` and `image_url` on a template set. Without a pipeline
/// (images off), `image()` prints a plain `<img>` with the original URL and
/// `image_url` returns its input, so templates work either way.
pub fn register(tera: &mut tera::Tera, pipeline: Option<Arc<Pipeline>>, base_url: &str) {
    tera.register_function("image", ImageFn(pipeline.clone()));
    tera.register_filter("image_url", ImageUrlFilter(pipeline, base_url.to_string()));
}

#[cfg(test)]
mod tests;

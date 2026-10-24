//! Tiny static-file origin for dist/, built on axum.
//!
//! Everything under dist/ is served verbatim: pages (/about/, /blog/, ...),
//! posts (/posts/<slug>/), and feed/meta files. Cache-Control headers follow
//! the CDN guidance baked into the build.

use anyhow::Context;
use std::collections::HashMap;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path as StdPath, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

mod site_headers;
use site_headers::SiteHeaders;

const HTML_CACHE_CONTROL: &str =
    "public, max-age=0, s-maxage=86400, stale-while-revalidate=604800";
const XML_CACHE_CONTROL: &str = "public, s-maxage=3600";

#[derive(Clone)]
struct StaticDir(PathBuf);

/// Map a dist file extension to its content type + cache policy.
fn content_type_for(path: &StdPath) -> (&'static str, &'static str) {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => ("text/html; charset=utf-8", HTML_CACHE_CONTROL),
        Some("xml") => ("application/xml", XML_CACHE_CONTROL),
        Some("txt") => ("text/plain; charset=utf-8", XML_CACHE_CONTROL),
        Some("md") => ("text/markdown; charset=utf-8", XML_CACHE_CONTROL),
        Some("css") => ("text/css; charset=utf-8", HTML_CACHE_CONTROL),
        Some("ttf") | Some("otf") => ("font/ttf", "public, max-age=31536000, immutable"),
        Some("woff") => ("font/woff", "public, max-age=31536000, immutable"),
        Some("woff2") => ("font/woff2", "public, max-age=31536000, immutable"),
        Some("js") => ("application/javascript", HTML_CACHE_CONTROL),
        Some("json") => ("application/json", XML_CACHE_CONTROL),
        Some("svg") => ("image/svg+xml", HTML_CACHE_CONTROL),
        Some("png") => ("image/png", "public, max-age=31536000, immutable"),
        Some("jpg") | Some("jpeg") => ("image/jpeg", "public, max-age=31536000, immutable"),
        Some("webp") => ("image/webp", "public, max-age=31536000, immutable"),
        Some("avif") => ("image/avif", "public, max-age=31536000, immutable"),
        Some("gif") => ("image/gif", "public, max-age=31536000, immutable"),
        Some("mp4") => ("video/mp4", "public, max-age=31536000, immutable"),
        Some("webm") => ("video/webm", "public, max-age=31536000, immutable"),
        Some("mp3") => ("audio/mpeg", "public, max-age=31536000, immutable"),
        Some("ogg") => ("audio/ogg", "public, max-age=31536000, immutable"),
        Some("wav") => ("audio/wav", "public, max-age=31536000, immutable"),
        Some("pdf") => ("application/pdf", "public, max-age=31536000, immutable"),
        _ => ("application/octet-stream", HTML_CACHE_CONTROL),
    }
}

const USAGE: &str = "\
Usage: server

Serves the dist/ of the site at SITE_ROOT (else the current directory) on
PORT (default 8787). Configuration comes from the environment only.

Options:
  -h, --help  Print this help.
";

/// Build marker the engine writes in dist/ (see build-site). Never served.
const DIST_MARKER: &str = ".post-cms-build";

/// `Ok(true)` for help; any other argument is an error, so a flag this
/// binary does not know never starts a server.
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<bool, String> {
    let mut help = false;
    for a in args {
        match a.as_str() {
            "-h" | "--help" => help = true,
            _ if a.starts_with('-') => return Err(format!("unknown option {a:?}")),
            _ => return Err(format!("unexpected argument {a:?} (set SITE_ROOT to choose the site)")),
        }
    }
    Ok(help)
}

fn main() -> anyhow::Result<()> {
    match parse_args(env::args().skip(1)) {
        Ok(true) => {
            print!("{USAGE}");
            return Ok(());
        }
        Ok(false) => {}
        Err(e) => {
            eprint!("server: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    let root = repo_root()?;
    let dist = root.join("dist");
    if !dist.is_dir() {
        anyhow::bail!(
            "dist/ not found at {} — run `cargo run -p build` first",
            dist.display()
        );
    }
    // Redirect rules are startup config: a malformed map refuses to start
    // (loud, like the build failing on a broken schema) instead of silently
    // 404-ing migrated URLs.
    let redirects = load_redirects(&root)?;
    let _ = REDIRECTS.set(redirects);
    let _ = TRAILING_SLASH.set(load_trailing_slash(&root));
    // site.yaml `headers:` is startup config too: a bad header name, a CR/LF
    // in a value, or a server-owned header refuses to start.
    let site_headers = load_site_headers(&root)?;
    let _ = SITE_HEADERS.set(site_headers);

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);

    let state = StaticDir(dist.clone());
    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/ask", axum::routing::post(ask))
        .route("/mcp", axum::routing::post(mcp_endpoint))
        // Everything else (pages, posts, feeds) is a file under dist/.
        .fallback(get(serve_path))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        eprintln!("origin serving dist/ on http://{addr}");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

// ---- Rate limiting / abuse protection (built into the origin) -----------
//
// /api/ask is the expensive path (embedding + LLM call), so it is gated by:
//   1. a per-client sliding-window rate limit (RATE_LIMIT_PER_MIN, default 10)
//   2. a global concurrency cap (ASK_MAX_CONCURRENT, default 4) so many
//      distinct IPs can't stampede the upstream model at once
// Static routes stay unlimited — they're cheap, immutable-cached files and
// the CDN absorbs traffic before it reaches this binary anyway.

const RATE_WINDOW_SECS: u64 = 60;

static RATE_BUCKETS: OnceLock<
    std::sync::Mutex<HashMap<IpAddr, (u32, Instant)>>,
> = OnceLock::new();

/// Requests per minute per client for /api/ask.
fn rate_limit_per_min() -> u32 {
    env::var("RATE_LIMIT_PER_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
        .max(1)
}

/// Global in-flight cap for /api/ask (protects the upstream model).
fn ask_max_concurrent() -> usize {
    env::var("ASK_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
        .max(1)
}

static ASK_SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();

// ---- URL redirect map (migration / restructure) ---------------------------
//
// redirects.yaml at the repo root, committed like any content change:
//
//   /old-page/: {to: /new-page/, status: 301}
//   /old-post: /posts/moved/
//
// A bare string target means 301 (permanent). Loaded once at startup — a
// malformed map fails startup loudly, the same contract as the build failing
// on a broken schema.
#[derive(Clone, Debug)]
struct Redirect {
    from: String,
    to: String,
    status: u16,
}

static REDIRECTS: OnceLock<Vec<Redirect>> = OnceLock::new();

fn load_redirects(root: &StdPath) -> anyhow::Result<Vec<Redirect>> {
    let path = root.join("redirects.yaml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed: serde_yaml::Value = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    let map = parsed
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("{}: must be a mapping of from → to", path.display()))?;
    let mut out = Vec::new();
    for (k, v) in map {
        let from = k
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("{}: keys must be paths", path.display()))?
            .to_string();
        let (to, status) = match v {
            serde_yaml::Value::String(to) => (to.clone(), 301),
            serde_yaml::Value::Mapping(m) => {
                let to = m
                    .get(serde_yaml::Value::from("to"))
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("{}: `{from}` needs a `to` target", path.display())
                    })?
                    .to_string();
                let status = m
                    .get(serde_yaml::Value::from("status"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(301) as u16;
                if status != 301 && status != 302 && status != 307 && status != 308 {
                    return Err(anyhow::anyhow!(
                        "{}: `{from}` status {status} must be 301, 302, 307 or 308",
                        path.display()
                    ));
                }
                (to, status)
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "{}: `{from}` value must be a target path or a {{to, status}} mapping",
                    path.display()
                ))
            }
        };
        if !valid_redirect_target(&to) {
            return Err(anyhow::anyhow!(
                "{}: `{from}` target `{to}` must be an absolute path or an http(s) URL",
                path.display()
            ));
        }
        out.push(Redirect { from: from.trim_matches('/').to_string(), to, status });
    }
    if out.is_empty() {
        eprintln!("redirects: {} is empty", path.display());
    } else {
        eprintln!("redirects: {} rules loaded from {}", out.len(), path.display());
    }
    Ok(out)
}

/// Exact-match lookup against the loaded map. Paths are compared
/// slash-stripped, so /old-page, /old-page/ and old-page all match.
/// A redirect target is a site path (`/x`) or a full http(s) URL for moves to
/// another domain. Protocol-relative `//host` and `/\host` (which browsers
/// read as `//host`) are rejected, as is any control character.
fn valid_redirect_target(to: &str) -> bool {
    if to.chars().any(|c| c.is_ascii_control()) {
        return false;
    }
    is_local_path(to) || to.starts_with("https://") || to.starts_with("http://")
}

/// A same-site absolute path: one leading `/`, not followed by `/` or `\`.
fn is_local_path(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\")
}

/// Rewrite a request path into a same-site Location: a leading run of `/`
/// and `\` collapses to a single `/`, so the result can never be read as a
/// protocol-relative URL. `None` when the path holds control characters.
fn local_location(path: &str) -> Option<String> {
    if path.chars().any(|c| c.is_ascii_control()) {
        return None;
    }
    let rest = path.trim_start_matches(['/', '\\']);
    Some(format!("/{rest}"))
}

fn redirect_for(rel: &str) -> Option<&'static Redirect> {
    let rel = rel.trim_matches('/');
    REDIRECTS.get().and_then(|map| {
        map.iter()
            .find(|r| !r.from.is_empty() && r.from == rel)
    })
}

fn redirect_response(r: &Redirect) -> Response {
    // Last line of defense: never emit a Location a browser could read as
    // another host, whatever built it.
    if !valid_redirect_target(&r.to) {
        return (StatusCode::NOT_FOUND, "404 Not Found").into_response();
    }
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
    let mut resp = (status, String::new()).into_response();
    if let Ok(v) = HeaderValue::from_str(&r.to) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    // Redirects are permanent routing facts, not content: let the CDN cache
    // them but revalidate daily.
    let v = HeaderValue::from_static("public, max-age=86400");
    resp.headers_mut().insert(header::CACHE_CONTROL, v);
    resp
}

fn ask_semaphore() -> &'static tokio::sync::Semaphore {
    ASK_SEM.get_or_init(|| tokio::sync::Semaphore::new(ask_max_concurrent()))
}

/// Best-effort client identity. Behind a CDN/proxy (the normal deploy for this
/// stack) X-Forwarded-For carries the real client; a direct client that spoofs
/// XFF only wins itself a fresh bucket — the global semaphore still caps total
/// load, so spoofing gains nothing.
fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
    const XFF: HeaderName = HeaderName::from_static("x-forwarded-for");
    if let Some(xff) = headers.get(XFF).and_then(|v| v.to_str().ok()) {
        if let Some(ip) = xff.split(',').next().and_then(|s| s.trim().parse().ok()) {
            return ip;
        }
    }
    addr.ip()
}

/// Fixed-window counter per client IP. Returns (allowed, remaining, retry_after_secs).
fn rate_limit(ip: IpAddr) -> (bool, u32, u64) {
    let limit = rate_limit_per_min();
    let map = RATE_BUCKETS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = map.lock().expect("rate map poisoned");
    let now = Instant::now();
    // Opportunistic pruning so the map can't grow unbounded under attack.
    if map.len() > 10_000 {
        map.retain(|_, (_, window_start)| now.duration_since(*window_start).as_secs() < RATE_WINDOW_SECS);
    }
    let entry = map.entry(ip).and_modify(|(count, start)| {
        if now.duration_since(*start).as_secs() >= RATE_WINDOW_SECS {
            *count = 0;
            *start = now;
        }
    });
    let entry = match entry {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => v.insert((0, now)),
    };
    if entry.0 >= limit {
        let retry = RATE_WINDOW_SECS - now.duration_since(entry.1).as_secs().min(RATE_WINDOW_SECS - 1);
        (false, 0, retry.max(1))
    } else {
        entry.0 += 1;
        (true, limit - entry.0, 0)
    }
}

/// Site root, resolved the same way as the build: SITE_ROOT when set, else
/// the current directory.
fn repo_root() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = env::var("SITE_ROOT") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    env::current_dir().context("cannot read the current directory")
}

/// site.yaml `trailing_slash` (default true). When false, `/x/` 301s to `/x`.
/// When true the server never redirects for slash form: `/x` and `/x/` both
/// serve the same file, so there is no Location to build in that direction.
static TRAILING_SLASH: OnceLock<bool> = OnceLock::new();

fn load_trailing_slash(root: &StdPath) -> bool {
    std::fs::read_to_string(root.join("site.yaml"))
        .ok()
        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
        .and_then(|v| v.get("trailing_slash").and_then(|b| b.as_bool()))
        .unwrap_or(true)
}

static SITE_HEADERS: OnceLock<SiteHeaders> = OnceLock::new();

fn load_site_headers(root: &StdPath) -> anyhow::Result<SiteHeaders> {
    let path = root.join("site.yaml");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(SiteHeaders::default());
    };
    let h = SiteHeaders::from_site_yaml(&raw).with_context(|| format!("{}", path.display()))?;
    if !h.is_empty() {
        eprintln!("headers: {}", site_headers::summary(&h));
    }
    Ok(h)
}

/// Add site.yaml `headers:` for `path` to a response from the static
/// fallback (pages, assets, redirects, and 404s alike). Applied last, so a
/// site value replaces the server's own Cache-Control or Referrer-Policy;
/// headers the server must own are rejected at startup.
fn with_site_headers(cfg: &SiteHeaders, path: &str, mut resp: Response) -> Response {
    for (name, value) in cfg.resolve(path) {
        resp.headers_mut().insert(name, value);
    }
    resp
}

fn site_headers_for(path: &str, resp: Response) -> Response {
    match SITE_HEADERS.get() {
        Some(cfg) if !cfg.is_empty() => with_site_headers(cfg, path, resp),
        _ => resp,
    }
}

/// The slash-free URL a request should 301 to, when the site uses
/// `trailing_slash: false` and the path ends in `/` (the root excepted).
fn slash_redirect(path: &str, query: Option<&str>) -> Option<String> {
    slash_redirect_with(*TRAILING_SLASH.get().unwrap_or(&true), path, query)
}

fn slash_redirect_with(trailing_slash: bool, path: &str, query: Option<&str>) -> Option<String> {
    if trailing_slash || path == "/" || !path.ends_with('/') {
        return None;
    }
    let bare = local_location(path.trim_end_matches('/'))?;
    Some(match query {
        Some(q) if !q.is_empty() => format!("{bare}?{q}"),
        _ => bare,
    })
}

fn not_found(root: PathBuf) -> Response {
    match std::fs::read_to_string(root.join("404.html")) {
        Ok(html) => {
            let mut resp = Html(html).into_response();
            *resp.status_mut() = StatusCode::NOT_FOUND;
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(HTML_CACHE_CONTROL),
            );
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}

// ---- Case-sensitive path resolution ---------------------------------------
//
// URLs are case-sensitive, and so is a Linux origin: `/Pricing` is not
// `/pricing/`. A case-insensitive filesystem (macOS APFS, Windows NTFS) would
// happily open `pricing/` for `/Pricing`, so local runs disagreed with
// production. Every component of a resolved path must therefore match an
// on-disk name byte-for-byte (which also rules out Unicode-normalization
// aliases on APFS). Directory listings are cached and revalidated against
// the directory's mtime, so a request costs one stat per component and no
// directory scan unless a rebuild changed that directory.

type DirListing = (std::time::SystemTime, std::sync::Arc<std::collections::HashSet<std::ffi::OsString>>);

static DIR_NAMES: OnceLock<std::sync::RwLock<HashMap<PathBuf, DirListing>>> = OnceLock::new();

/// Cap on cached directories; a rebuild that drops directories leaves stale
/// keys behind, so the cache is cleared rather than allowed to grow.
const DIR_CACHE_MAX: usize = 4096;

/// Does `dir` hold an entry named exactly `name`?
fn dir_has_entry(dir: &StdPath, name: &str) -> bool {
    let Ok(mtime) = std::fs::metadata(dir).and_then(|m| m.modified()) else {
        return false;
    };
    let cache = DIR_NAMES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    if let Ok(map) = cache.read() {
        if let Some((cached, names)) = map.get(dir) {
            if *cached == mtime {
                return names.contains(std::ffi::OsStr::new(name));
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let names: std::collections::HashSet<_> = entries.flatten().map(|e| e.file_name()).collect();
    let found = names.contains(std::ffi::OsStr::new(name));
    if let Ok(mut map) = cache.write() {
        if map.len() >= DIR_CACHE_MAX {
            map.clear();
        }
        map.insert(dir.to_path_buf(), (mtime, std::sync::Arc::new(names)));
    }
    found
}

/// True when every `/`-separated component of `rel` (already checked for
/// traversal and empty segments) names an entry under `root` with exactly
/// that spelling.
fn exact_case(root: &StdPath, rel: &str) -> bool {
    let mut dir = root.to_path_buf();
    for seg in rel.split('/') {
        if !dir_has_entry(&dir, seg) {
            return false;
        }
        dir.push(seg);
    }
    true
}

/// Security + CDN headers applied to every successful file response.
fn harden(resp: &mut Response) {
    let headers = resp.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    // An SVG opened directly is a document: no script, no outside loads.
    if headers.get(header::CONTENT_TYPE).is_some_and(|v| v == "image/svg+xml") {
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(SVG_CSP));
    }
}

const SVG_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox";

/// Serve a file under dist/. A bare path that resolves to a directory is
/// treated as ".../index.html" (so /about/ and /about both work). Path
/// traversal is rejected on every segment. Precompressed .gz siblings are
/// served when the client sends Accept-Encoding: gzip.
fn serve(root: &PathBuf, rel: &str, accept_gzip: bool) -> Response {
    // Strip surrounding slashes: "/blog/" → "blog".
    let rel = rel.trim_matches('/').to_string();
    if rel.is_empty() {
        return not_found(root.clone());
    }
    // Redirects win before the filesystem: a migrated URL must never depend
    // on the old file still existing.
    if let Some(r) = redirect_for(&rel) {
        return redirect_response(r);
    }
    // Reject traversal and empty segments, and the build marker.
    if rel == DIST_MARKER || rel.split('/').any(|seg| {
        seg.is_empty() || seg == "." || seg == ".." || seg.contains('\0')
    }) {
        return not_found(root.clone());
    }

    let mut file = root.join(&rel);
    let mut file_rel = rel.clone();
    if file.is_dir() {
        file = file.join("index.html");
        file_rel.push_str("/index.html");
    }
    // Case-insensitive filesystems would open `Pricing/` for `pricing/`;
    // only an exact on-disk spelling serves.
    if !exact_case(root, &file_rel) {
        return not_found(root.clone());
    }
    let (content_type, cache_control) = content_type_for(&file);

    // Prefer the precompressed sibling when the client can decode gzip.
    let gz_path = PathBuf::from(format!("{}.gz", file.display()));
    let compressible = file
        .extension()
        .and_then(|e| e.to_str())
        .map_or(false, |e| {
            ["html", "htm", "css", "js", "json", "xml", "txt", "svg", "md"].contains(&e)
        });
    if accept_gzip && compressible && gz_path.exists() && exact_case(root, &format!("{file_rel}.gz")) {
        match std::fs::read(&gz_path) {
            Ok(bytes) => {
                let mut resp = (StatusCode::OK, bytes).into_response();
                let headers = resp.headers_mut();
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
                if let Ok(v) = HeaderValue::from_str(cache_control) {
                    headers.insert(header::CACHE_CONTROL, v);
                }
                headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                harden(&mut resp);
                return resp;
            }
            Err(_) => return not_found(root.clone()),
        }
    }

    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            let headers = resp.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            if let Ok(v) = HeaderValue::from_str(cache_control) {
                headers.insert(header::CACHE_CONTROL, v);
            }
            harden(&mut resp);
            resp
        }
        Err(_) => not_found(root.clone()),
    }
}

fn wants_gzip(req: &Request) -> bool {
    req.headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v.to_ascii_lowercase().contains("gzip"))
}

// ---- Public read-only MCP endpoint --------------------------------------
//
// Streamable-HTTP-style JSON-RPC at POST /mcp, unauthenticated by design:
// every tool is read-only over dist/ output, so nothing here can mutate
// content or trigger builds. Discovery paths (per the SEP-1649/1960
// proposal + llms.txt convention, all emitted by the build):
//   /.well-known/mcp/server-card.json, llms.txt "MCP Servers" section.
// Registry/directory listings (official MCP Registry, connector dirs) are
// deployment-side and not automated here.

static SEARCH_INDEX: OnceLock<Option<serde_json::Value>> = OnceLock::new();

/// dist/search-index.json as a vector of entry objects (title/url/description/
/// date/tags/body per published post).
fn search_index(state: &StaticDir) -> Vec<serde_json::Value> {
    SEARCH_INDEX
        .get_or_init(|| {
            std::fs::read_to_string(state.0.join("search-index.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
        })
        .clone()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn mcp_tools_manifest() -> serde_json::Value {
    let schema = |props: serde_json::Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": props,
            "required": required,
        })
    };
    json!({
        "tools": [
            {
                "name": "list_posts",
                "description": "List all published posts with title, URL, description, date, and tags.",
                "inputSchema": schema(json!({}), &[]),
            },
            {
                "name": "get_post",
                "description": "Fetch one published post as markdown by slug.",
                "inputSchema": schema(
                    json!({ "slug": { "type": "string", "description": "Post slug, e.g. caching-strategy" } }),
                    &["slug"],
                ),
            },
            {
                "name": "search",
                "description": "Keyword search over published posts (weighted: title > tags > body).",
                "inputSchema": schema(
                    json!({ "query": { "type": "string", "description": "Space-separated keywords" } }),
                    &["query"],
                ),
            },
        ]
    })
}

/// Slug safety: lowercase alnum + dashes only, no traversal possible.
fn mcp_slug_ok(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn mcp_call_tool(state: &StaticDir, req: &serde_json::Value) -> Result<serde_json::Value, (i64, String)> {
    let name = req
        .pointer("/params/name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let args = req
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text_of = |v: serde_json::Value| -> Result<serde_json::Value, (i64, String)> {
        Ok(json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }))
    };
    match name {
        "list_posts" => {
            let posts: Vec<serde_json::Value> = search_index(state)
                .iter()
                .map(|e| {
                    json!({
                        "title": e.get("title"),
                        "url": e.get("url"),
                        "description": e.get("description"),
                        "date": e.get("date"),
                        "tags": e.get("tags"),
                    })
                })
                .collect();
            text_of(json!({ "count": posts.len(), "posts": posts }))
        }
        "get_post" => {
            let slug = args.get("slug").and_then(|v| v.as_str()).unwrap_or("");
            if !mcp_slug_ok(slug) {
                return Err((-32000, "invalid slug".into()));
            }
            let path = state.0.join("posts").join(slug).join("index.md");
            let body = std::fs::read_to_string(&path)
                .map_err(|_| (-32000, format!("post not found: {slug}")))?;
            Ok(json!({ "content": [{ "type": "text", "text": body }] }))
        }
        "search" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let tokens: Vec<String> = q
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .collect();
            if tokens.is_empty() {
                return Err((-32000, "empty query".into()));
            }
            let index = search_index(state);
            let mut scored: Vec<(u32, &serde_json::Value)> = index
                .iter()
                .map(|e| {
                    let title = e.get("title").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    let body = e.get("body").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    let tags: Vec<String> = e
                        .get("tags")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(|s| s.to_lowercase()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut score = 0u32;
                    for t in &tokens {
                        if title.contains(t.as_str()) {
                            score += 3;
                        }
                        if tags.iter().any(|tag| tag.contains(t.as_str())) {
                            score += 2;
                        }
                        if body.contains(t.as_str()) {
                            score += 1;
                        }
                    }
                    (score, e)
                })
                .filter(|(s, _)| *s > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            let hits: Vec<serde_json::Value> = scored
                .iter()
                .take(8)
                .map(|(score, e)| {
                    json!({
                        "title": e.get("title"),
                        "url": e.get("url"),
                        "description": e.get("description"),
                        "date": e.get("date"),
                        "score": score,
                    })
                })
                .collect();
            text_of(json!({ "query": q, "hits": hits }))
        }
        _ => Err((-32601, format!("unknown tool: {name}"))),
    }
}

/// POST /mcp — JSON-RPC 2.0 request in, JSON-RPC response out.
async fn mcp_endpoint(
    State(s): State<StaticDir>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    // Notifications take no response body (HTTP 202 per streamable-http).
    if method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "post-cms", "version": env!("CARGO_PKG_VERSION") },
        })),
        "tools/list" => Ok(mcp_tools_manifest()),
        "tools/call" => mcp_call_tool(&s, &req),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    match result {
        Ok(v) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": v })).into_response(),
        Err((code, message)) => {
            Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
                .into_response()
        }
    }
}

async fn serve_path(State(s): State<StaticDir>, req: Request) -> Response {
    let gzip = wants_gzip(&req);
    let path = req.uri().path();
    // A redirect-map hit wins, so a migrated `/old/` goes straight to its
    // target in one hop. Otherwise normalize the slash form.
    if redirect_for(path).is_none() {
        if let Some(to) = slash_redirect(path, req.uri().query()) {
            let resp = redirect_response(&Redirect { from: String::new(), to, status: 301 });
            return site_headers_for(path, resp);
        }
    }
    site_headers_for(path, serve(&s.0, path, gzip))
}

async fn index(State(s): State<StaticDir>, req: Request) -> Response {
    let gzip = wants_gzip(&req);
    site_headers_for("/", serve(&s.0, "index.html", gzip))
}

async fn health() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

// ---- Tier 3 retrieval helpers (used when dist/embeddings.json exists) ----

/// Lazily-initialized shared embedding model (one ~100MB ONNX load per process).
static EMBED: std::sync::OnceLock<std::sync::Mutex<Option<fastembed::TextEmbedding>>> =
    std::sync::OnceLock::new();

fn embed_model() -> Option<&'static std::sync::Mutex<Option<fastembed::TextEmbedding>>> {
    let m = EMBED
        .get_or_init(|| {
            let opts = fastembed::InitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
                .with_cache_dir(embed_cache_dir());
            std::sync::Mutex::new(match fastembed::TextEmbedding::try_new(opts) {
                Ok(m) => {
                    Some(m)
                }
                Err(e) => {
                    eprintln!("embeddings: model unavailable ({e}); /api/ask falls back to inline mode");
                    None
                }
            })
        });
    Some(m)
}

/// Where fastembed caches downloaded ONNX weights (same default as the build crate).
fn embed_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".cache").join("post-cms-fastembed")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Embed the question, score all chunks, return the top-k as (context, sources).
fn retrieve(dist: &StdPath, question: &str) -> anyhow::Result<(String, Vec<Source>)> {
    let model = embed_model().ok_or_else(|| anyhow::anyhow!("no embedding model"))?;
    let raw = std::fs::read_to_string(dist.join("embeddings.json"))?;
    let chunks: Vec<serde_json::Value> = serde_json::from_str(&raw)?;

    let mut guard = model.lock().map_err(|_| anyhow::anyhow!("embed mutex poisoned"))?;
    let model = guard.as_mut().ok_or_else(|| anyhow::anyhow!("no embedding model"))?;
    let qvec = model
        .embed(vec![question.to_string()], None)
        .map_err(|e| anyhow::anyhow!("embed query: {e}"))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty query embedding"))?;

    let mut scored: Vec<(f32, &serde_json::Value)> = chunks
        .iter()
        .filter_map(|c| {
            let vec = c["vec"].as_array()?;
            let v: Vec<f32> = vec.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
            Some((cosine(&qvec, &v), c))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut context = String::from("## RETRIEVED SITE EXCERPTS\n\n");
    let mut sources: Vec<Source> = Vec::new();
    for (score, c) in scored.iter().take(6) {
        let title = c["meta"]["title"].as_str().unwrap_or("");
        let url = c["meta"]["url"].as_str().unwrap_or("");
        let text = c["text"].as_str().unwrap_or("");
        context.push_str(&format!("--- {title} (relevance {score:.3}) ---\n{text}\n\n"));
        if !sources.iter().any(|s| s.url == url) {
            sources.push(Source { title: title.to_string(), url: url.to_string() });
        }
    }
    Ok((context, sources))
}

// ---- Tier 2: /api/ask — a thin agent endpoint over the static corpus. ----
//
// It reads the per-post markdown the build already emitted (posts/<slug>/index.md),
// inlines it (tiny corpora), and asks an OpenAI-compatible chat model to answer a
// user question using ONLY that content. Config via env: LLM_API_KEY (required),
// LLM_BASE_URL (default api.openai.com/v1), LLM_MODEL, LLM_MAX_RAG_CHARS.

#[derive(serde::Deserialize)]
struct AskBody {
    question: String,
}

#[derive(serde::Serialize)]
struct Source {
    title: String,
    url: String,
}

#[derive(serde::Serialize)]
struct Answer {
    answer: String,
    sources: Vec<Source>,
}

fn load_corpus(root: &PathBuf) -> Vec<(String, String, String)> {
    let max_total: usize = env::var("LLM_MAX_RAG_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120_000);
    let dir = root.join("posts");
    let mut out = Vec::new();
    let mut used = 0usize;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let slug = e.file_name().to_string_lossy().into_owned();
        let md = e.path().join("index.md");
        let Ok(body) = std::fs::read_to_string(&md) else {
            continue;
        };
        // Title = first `# ` line; url derived from slug.
        let title = body
            .lines()
            .find(|l| l.starts_with("# "))
            .map(|l| l[2..].trim().to_string())
            .unwrap_or_else(|| slug.clone());
        let url = format!("/posts/{slug}/");
        if used + body.len() > max_total {
            let keep = max_total.saturating_sub(used);
            let cut: String = body.chars().take(keep).collect();
            used += keep;
            out.push((title, url, cut));
        } else {
            used += body.len();
            out.push((title, url, body));
        }
    }
    out
}

/// Tier 2 fallback: inline the whole corpus with all posts as sources.
fn inline_corpus(corpus: &[(String, String, String)]) -> (String, Vec<Source>) {
    let mut content = String::from(
        "You are the find-it-on-this-site agent. Answer the user's question using ONLY the site \
         content below. Cite each claim's post title. If the answer is not in the content, say so \
         plainly and do not invent.\n\n## SITE CONTENT\n\n",
    );
    let sources: Vec<Source> = corpus
        .iter()
        .map(|(t, u, _)| Source { title: t.clone(), url: u.clone() })
        .collect();
    for (i, (title, _url, body)) in corpus.iter().enumerate() {
        content.push_str(&format!("--- POST {i}: {title} ---\n{body}\n\n"));
    }
    (content, sources)
}

async fn ask(
    State(s): State<StaticDir>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Json<AskBody>,
) -> Response {
    let question = body.question.trim();
    if question.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "question is required" }))).into_response();
    }

    // Abuse protection: per-IP fixed window + global in-flight cap.
    let ip = client_ip(&headers, addr);
    let (allowed, _remaining, retry_after) = rate_limit(ip);
    if !allowed {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (header::RETRY_AFTER, HeaderValue::from(retry_after)),
                (
                    header::HeaderName::from_static("x-ratelimit-limit"),
                    HeaderValue::from(rate_limit_per_min()),
                ),
            ],
            Json(json!({ "error": "rate limit exceeded", "retry_after_seconds": retry_after })),
        )
            .into_response();
    }
    let _permit = match ask_semaphore().try_acquire() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "server busy, try again shortly" })),
            )
                .into_response()
        }
    };
    // Keyless endpoints (e.g. a private GLM server on the tailnet) are fine: no auth header.
    // Only a present-but-empty key is an error.
    let key = env::var("LLM_API_KEY").ok().filter(|k| !k.trim().is_empty());
    if env::var("LLM_API_KEY").map_or(false, |k| k.trim().is_empty()) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "LLM_API_KEY is empty" }))).into_response();
    }

    let base = env::var("LLM_BASE_URL").ok().filter(|b| !b.trim().is_empty());
    let model = env::var("LLM_MODEL").ok().filter(|m| !m.trim().is_empty());
    // No hardcoded fallback: an unconfigured origin must refuse /api/ask
    // rather than silently proxying to whatever host was compiled in.
    let (base, model) = match (base, model) {
        (Some(b), Some(m)) => (b, m),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "LLM not configured: set LLM_BASE_URL and LLM_MODEL to enable /api/ask" })),
            )
                .into_response()
        }
    };
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));

    let corpus = load_corpus(&s.0);
    if corpus.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "dist/posts has no markdown — rebuild with `cargo run -p build`" }))).into_response();
    }

    // Tier 3 when embeddings exist (retrieve top-k chunks, cite only those
    // posts); otherwise Tier 2 (inline the whole corpus).
    let emb_path = s.0.join("embeddings.json");
    let (content, sources) = if emb_path.is_file() {
        match retrieve(&s.0, question) {
            Ok((ctx, srcs)) => {
                let mut content = String::from(
                    "You are the find-it-on-this-site agent. Answer the user's question using ONLY \
                     the retrieved site excerpts below. Cite each claim's post title. If the answer \
                     is not in the excerpts, say so plainly and do not invent.\n\n",
                );
                content.push_str(&ctx);
                (content, srcs)
            }
            Err(e) => {
                eprintln!("retrieval failed ({e}); falling back to inline corpus");
                inline_corpus(&corpus)
            }
        }
    } else {
        inline_corpus(&corpus)
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();
    let payload = serde_json::json!({
        "model": model,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": content.clone() },
            { "role": "user", "content": question },
        ],
    });
    let mut req = client.post(&url).json(&payload);
    if let Some(k) = &key {
        req = req.bearer_auth(k);
    }
    let req = req.send().await;
    match req {
        Ok(resp) if resp.status().is_success() => {
            let j: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let answer = j["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            if answer.is_empty() {
                return (StatusCode::BAD_GATEWAY, Json(json!({ "error": "model returned no content" }))).into_response();
            }
            Json(Answer { answer, sources }).into_response()
        }
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": "upstream LLM error", "status": status.as_u16(), "detail": text.chars().take(300).collect::<String>() }))).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("upstream unreachable: {e}") }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_types_and_svg_policy() {
        assert_eq!(content_type_for(StdPath::new("a/b.avif")).0, "image/avif");
        let root = std::env::temp_dir().join(format!("postcms-svg-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("x.svg"), "<svg/>").unwrap();
        std::fs::write(root.join("x.png"), "png").unwrap();
        let svg = serve(&root, "x.svg", false);
        assert_eq!(svg.headers()[header::CONTENT_SECURITY_POLICY], SVG_CSP);
        let png = serve(&root, "x.png", false);
        assert!(png.headers().get(header::CONTENT_SECURITY_POLICY).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn redirect_targets_allow_paths_and_urls() {
        assert!(valid_redirect_target("/platform/ops"));
        assert!(valid_redirect_target("https://www.example.org/"));
        assert!(!valid_redirect_target("//evil.example"));
        assert!(!valid_redirect_target("platform/ops"));
    }

    #[test]
    fn slash_form_redirects_only_when_disabled() {
        assert_eq!(slash_redirect_with(true, "/pricing/", None), None);
        assert_eq!(slash_redirect_with(false, "/pricing/", None), Some("/pricing".into()));
        assert_eq!(
            slash_redirect_with(false, "/a/b/", Some("utm=1")),
            Some("/a/b?utm=1".into())
        );
        assert_eq!(slash_redirect_with(false, "/", None), None);
        assert_eq!(slash_redirect_with(false, "/pricing", None), None);
    }

    #[test]
    fn slash_redirect_never_emits_protocol_relative_location() {
        for (path, want) in [
            ("//evil.example/", "/evil.example"),
            ("///evil.example/", "/evil.example"),
            ("/\\evil.example/", "/evil.example"),
            ("/\\/evil.example/", "/evil.example"),
            ("\\\\evil.example/", "/evil.example"),
            ("//", "/"),
        ] {
            let to = slash_redirect_with(false, path, None).expect(path);
            assert_eq!(to, want, "{path}");
            assert!(valid_redirect_target(&to), "{path} -> {to}");
        }
        assert_eq!(
            slash_redirect_with(false, "//evil.example/", Some("a=1")),
            Some("/evil.example?a=1".into())
        );
        assert_eq!(slash_redirect_with(false, "/a\t/", None), None);
    }

    #[test]
    fn redirect_targets_reject_backslash_and_control_chars() {
        assert!(!valid_redirect_target("/\\evil.example"));
        assert!(!valid_redirect_target("///evil.example"));
        assert!(!valid_redirect_target("/ok\r\nSet-Cookie: x=1"));
        assert!(valid_redirect_target("/a\\b"));
    }

    #[test]
    fn redirect_response_refuses_unsafe_location() {
        let r = Redirect { from: String::new(), to: "//evil.example".into(), status: 301 };
        let resp = redirect_response(&r);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(resp.headers().get(header::LOCATION).is_none());
        let ok = Redirect { from: String::new(), to: "/fine".into(), status: 301 };
        assert_eq!(redirect_response(&ok).headers()[header::LOCATION], "/fine");
    }

    fn temp_dist(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("postcms-headers-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("signup")).unwrap();
        std::fs::write(dir.join("signup/index.html"), "<p>signup</p>").unwrap();
        std::fs::write(dir.join("signup/index.html.gz"), "gz").unwrap();
        std::fs::write(dir.join("about.html"), "<p>about</p>").unwrap();
        std::fs::write(dir.join("app.js"), "1").unwrap();
        std::fs::write(dir.join("404.html"), "<p>nope</p>").unwrap();
        dir
    }

    const HEADERS_YAML: &str = r#"
headers:
  default:
    X-Frame-Options: SAMEORIGIN
  rules:
    - path: [/signup, /*.js]
      headers:
        Content-Security-Policy: "default-src 'self'"
    - path: /signup
      headers:
        Referrer-Policy: no-referrer
        Cache-Control: no-store
"#;

    #[test]
    fn page_gets_rule_headers_and_keeps_security_headers() {
        let dist = temp_dist("page");
        let cfg = SiteHeaders::from_site_yaml(HEADERS_YAML).unwrap();
        let resp = with_site_headers(&cfg, "/signup/", serve(&dist, "/signup/", false));
        let h = resp.headers();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(h["content-security-policy"], "default-src 'self'");
        assert_eq!(h["x-frame-options"], "SAMEORIGIN");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        // A rule replaces the server's own value, once.
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(h.get_all(header::REFERRER_POLICY).iter().count(), 1);
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
    }

    #[test]
    fn other_pages_get_only_the_default() {
        let dist = temp_dist("other");
        let cfg = SiteHeaders::from_site_yaml(HEADERS_YAML).unwrap();
        let resp = with_site_headers(&cfg, "/about.html", serve(&dist, "/about.html", false));
        let h = resp.headers();
        assert!(h.get("content-security-policy").is_none());
        assert_eq!(h["x-frame-options"], "SAMEORIGIN");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::REFERRER_POLICY], "strict-origin-when-cross-origin");
        assert_eq!(h[header::CACHE_CONTROL], HTML_CACHE_CONTROL);
    }

    #[test]
    fn assets_404s_and_redirects_get_matching_headers() {
        let dist = temp_dist("misc");
        let cfg = SiteHeaders::from_site_yaml(HEADERS_YAML).unwrap();
        let js = with_site_headers(&cfg, "/app.js", serve(&dist, "/app.js", false));
        assert_eq!(js.headers()["content-security-policy"], "default-src 'self'");
        let missing = with_site_headers(&cfg, "/missing", serve(&dist, "/missing", false));
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()["x-frame-options"], "SAMEORIGIN");
        assert!(missing.headers().get("content-security-policy").is_none());
        let redirect = Redirect { from: String::new(), to: "/signup".into(), status: 301 };
        let r = with_site_headers(&cfg, "/signup/", redirect_response(&redirect));
        assert_eq!(r.headers()[header::LOCATION], "/signup");
        assert_eq!(r.headers()["content-security-policy"], "default-src 'self'");
    }

    #[test]
    fn index_alias_cannot_skip_a_page_rule() {
        let dist = temp_dist("alias");
        let cfg = SiteHeaders::from_site_yaml(HEADERS_YAML).unwrap();
        for p in ["/signup", "/signup/index.html", "//signup/", "/signup/index.html.gz"] {
            let resp = with_site_headers(&cfg, p, serve(&dist, p, false));
            assert_eq!(resp.status(), StatusCode::OK, "{p}");
            assert_eq!(resp.headers()["content-security-policy"], "default-src 'self'", "{p}");
        }
        // Mixed case is a 404 on every filesystem, and the 404 still carries
        // the rule headers (header matching ignores case).
        for p in ["/SignUp", "/SIGNUP/Index.html", "/SignUp/index.html.GZ"] {
            let resp = with_site_headers(&cfg, p, serve(&dist, p, false));
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{p}");
            assert_eq!(resp.headers()["content-security-policy"], "default-src 'self'", "{p}");
            assert_eq!(resp.headers()["x-frame-options"], "SAMEORIGIN", "{p}");
        }
    }

    fn case_dist(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("postcms-case-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pricing")).unwrap();
        std::fs::create_dir_all(dir.join("events/speakers")).unwrap();
        std::fs::write(dir.join("pricing/index.html"), "<p>pricing</p>").unwrap();
        std::fs::write(dir.join("pricing/index.html.gz"), "gz").unwrap();
        std::fs::write(dir.join("events/speakers/index.html"), "<p>speakers</p>").unwrap();
        std::fs::write(dir.join("Report.pdf"), "pdf").unwrap();
        std::fs::write(dir.join("404.html"), "<p>nope</p>").unwrap();
        dir
    }

    #[test]
    fn mixed_case_request_is_not_found() {
        let dist = case_dist("mixed");
        for p in [
            "/Pricing",
            "/PRICING/",
            "/pricing/Index.html",
            "/pricing/index.HTML",
            "/Events/speakers/",
            "/events/Speakers",
            "/report.pdf",
            "/REPORT.PDF",
        ] {
            let resp = serve(&dist, p, false);
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{p}");
            assert_eq!(resp.headers()[header::CACHE_CONTROL], HTML_CACHE_CONTROL, "{p}");
        }
        // gzip negotiation cannot reach a file through a case alias either.
        assert_eq!(serve(&dist, "/Pricing/", true).status(), StatusCode::NOT_FOUND);
        assert_eq!(serve(&dist, "/pricing/index.html.GZ", false).status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn exact_case_and_index_aliases_still_serve() {
        let dist = case_dist("exact");
        for p in [
            "/pricing",
            "/pricing/",
            "//pricing/",
            "/pricing/index.html",
            "/events/speakers",
            "/events/speakers/",
            "/events/speakers/index.html",
            "/Report.pdf",
        ] {
            let resp = serve(&dist, p, false);
            assert_eq!(resp.status(), StatusCode::OK, "{p}");
        }
        let gz = serve(&dist, "/pricing/", true);
        assert_eq!(gz.status(), StatusCode::OK);
        assert_eq!(gz.headers()[header::CONTENT_ENCODING], "gzip");
    }

    #[test]
    fn case_check_follows_rebuilds() {
        let dist = case_dist("rebuild");
        assert_eq!(serve(&dist, "/pricing/", false).status(), StatusCode::OK);
        // A rebuild renames the directory: the old spelling stops serving and
        // the new one starts, without a restart.
        std::fs::rename(dist.join("pricing"), dist.join("plans")).unwrap();
        assert_eq!(serve(&dist, "/pricing/", false).status(), StatusCode::NOT_FOUND);
        assert_eq!(serve(&dist, "/plans/", false).status(), StatusCode::OK);
        std::fs::create_dir_all(dist.join("new-page")).unwrap();
        std::fs::write(dist.join("new-page/index.html"), "<p>new</p>").unwrap();
        assert_eq!(serve(&dist, "/new-page", false).status(), StatusCode::OK);
        assert_eq!(serve(&dist, "/New-Page", false).status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn traversal_and_encoding_unchanged() {
        let dist = case_dist("traversal");
        for p in ["/../etc/passwd", "/pricing/../pricing/", "/pricing/./", "/%2e%2e/etc/passwd", "/pric%69ng/"] {
            assert_eq!(serve(&dist, p, false).status(), StatusCode::NOT_FOUND, "{p}");
        }
    }

    #[test]
    fn redirects_match_case_sensitively_before_files() {
        let _ = REDIRECTS.set(vec![Redirect {
            from: "old-page".into(),
            to: "/events/speakers/".into(),
            status: 301,
        }]);
        let dist = case_dist("redirects");
        let resp = serve(&dist, "/old-page/", false);
        assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(resp.headers()[header::LOCATION], "/events/speakers/");
        // Redirect keys were always exact-match; a case variant is not a hit.
        assert_eq!(serve(&dist, "/Old-Page", false).status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn startup_refuses_invalid_headers() {
        let dir = env::temp_dir().join(format!("postcms-headers-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("site.yaml"), "headers:\n  default:\n    Content-Length: \"1\"\n").unwrap();
        let err = load_site_headers(&dir).unwrap_err();
        assert!(format!("{err:#}").contains("set by the server"), "{err:#}");
        std::fs::write(dir.join("site.yaml"), "title: x\n").unwrap();
        assert!(load_site_headers(&dir).unwrap().is_empty());
    }

    #[test]
    fn build_marker_is_never_served() {
        let dist = temp_dist("marker");
        std::fs::write(dist.join(DIST_MARKER), "marker").unwrap();
        for p in ["/.post-cms-build", ".post-cms-build", "/.post-cms-build/"] {
            assert_eq!(serve(&dist, p, false).status(), StatusCode::NOT_FOUND, "{p}");
        }
        assert_eq!(serve(&dist, "/app.js", false).status(), StatusCode::OK);
    }

    #[test]
    fn unknown_arguments_are_usage_errors() {
        let args = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert_eq!(args(&[]), Ok(false));
        assert_eq!(args(&["--help"]), Ok(true));
        assert_eq!(args(&["-h"]), Ok(true));
        assert!(args(&["--version"]).unwrap_err().contains("unknown option \"--version\""));
        assert!(args(&["--port", "9000"]).is_err());
        assert!(args(&["site"]).unwrap_err().contains("unexpected argument"));
    }
}

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

fn main() -> anyhow::Result<()> {
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
        if !to.starts_with('/') {
            return Err(anyhow::anyhow!(
                "{}: `{from}` target `{to}` must be an absolute path",
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
fn redirect_for(rel: &str) -> Option<&'static Redirect> {
    let rel = rel.trim_matches('/');
    REDIRECTS.get().and_then(|map| {
        map.iter()
            .find(|r| !r.from.is_empty() && r.from == rel)
    })
}

fn redirect_response(r: &Redirect) -> Response {
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

/// Site root, resolved the same way as the build: SITE_ROOT, then the git
/// toplevel of the current directory (so a separate site repo serves its own
/// dist/), then the compile-time manifest dir.
fn repo_root() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = env::var("SITE_ROOT") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Ok(PathBuf::from(s));
            }
        }
    }
    let manifest = env!("CARGO_MANIFEST_DIR");
    StdPath::new(manifest)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("cannot locate repo root"))
}

/// site.yaml `trailing_slash` (default true). When false, `/x/` 301s to `/x`.
static TRAILING_SLASH: OnceLock<bool> = OnceLock::new();

fn load_trailing_slash(root: &StdPath) -> bool {
    std::fs::read_to_string(root.join("site.yaml"))
        .ok()
        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
        .and_then(|v| v.get("trailing_slash").and_then(|b| b.as_bool()))
        .unwrap_or(true)
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
    let bare = path.trim_end_matches('/');
    let bare = if bare.is_empty() { "/" } else { bare };
    Some(match query {
        Some(q) if !q.is_empty() => format!("{bare}?{q}"),
        _ => bare.to_string(),
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

/// Security + CDN headers applied to every successful file response.
fn harden(resp: &mut Response) {
    let headers = resp.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
}

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
    // Reject traversal and empty segments.
    if rel.split('/').any(|seg| {
        seg.is_empty() || seg == "." || seg == ".." || seg.contains('\0')
    }) {
        return not_found(root.clone());
    }

    let mut file = root.join(&rel);
    if file.is_dir() {
        file = file.join("index.html");
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
    if accept_gzip && compressible && gz_path.exists() {
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
            return redirect_response(&Redirect { from: String::new(), to, status: 301 });
        }
    }
    serve(&s.0, path, gzip)
}

async fn index(State(s): State<StaticDir>, req: Request) -> Response {
    let gzip = wants_gzip(&req);
    serve(&s.0, "index.html", gzip)
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
}

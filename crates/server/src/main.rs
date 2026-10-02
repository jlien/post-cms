//! Tiny static-file origin for dist/, built on axum.
//!
//! Everything under dist/ is served verbatim: pages (/about/, /blog/, ...),
//! posts (/posts/<slug>/), and feed/meta files. Cache-Control headers follow
//! the CDN guidance baked into the build.

use std::env;
use std::path::{Path as StdPath, PathBuf};

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
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
    let dist = repo_root()?.join("dist");
    if !dist.is_dir() {
        anyhow::bail!(
            "dist/ not found at {} — run `cargo run -p build` first",
            dist.display()
        );
    }

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);

    let state = StaticDir(dist.clone());
    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
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
        axum::serve(listener, app).await?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn repo_root() -> anyhow::Result<PathBuf> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    StdPath::new(manifest)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("cannot locate repo root"))
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

/// Serve a file under dist/. A bare path that resolves to a directory is
/// treated as ".../index.html" (so /about/ and /about both work). Path
/// traversal is rejected on every segment.
fn serve(root: &PathBuf, rel: &str) -> Response {
    // Strip surrounding slashes: "/blog/" → "blog".
    let rel = rel.trim_matches('/').to_string();
    if rel.is_empty() {
        return not_found(root.clone());
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
    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            let headers = resp.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            if let Ok(v) = HeaderValue::from_str(cache_control) {
                headers.insert(header::CACHE_CONTROL, v);
            }
            resp
        }
        Err(_) => not_found(root.clone()),
    }
}

async fn serve_path(State(s): State<StaticDir>, req: Request) -> Response {
    serve(&s.0, req.uri().path())
}

async fn index(State(s): State<StaticDir>) -> Response {
    serve(&s.0, "index.html")
}

async fn health() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}
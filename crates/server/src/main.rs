//! Tiny static-file origin for dist/, built on axum.
//!
//! Routes mirror the build output: /, /posts/<slug>, /rss.xml, /sitemap.xml,
//! /robots.txt, /health, and a 404 fallback. Cache-Control headers follow the
//! CDN guidance baked into the build.

use std::env;
use std::path::{Path, PathBuf};

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
        .route("/rss.xml", get(rss))
        .route("/sitemap.xml", get(sitemap))
        .route("/robots.txt", get(robots))
        .route("/posts/{slug}", get(post_page))
        .fallback(get(not_found_handler))
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
    Path::new(manifest)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("cannot locate repo root"))
}

fn file_response(path: &Path, content_type: &'static str, cache_control: &str) -> Response {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            let headers = resp.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            if let Ok(v) = HeaderValue::from_str(cache_control) {
                headers.insert(header::CACHE_CONTROL, v);
            }
            resp
        }
        Err(_) => not_found(),
    }
}

fn html_response(path: &Path) -> Response {
    match std::fs::read_to_string(path) {
        Ok(html) => {
            let mut resp = Html(html).into_response();
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(HTML_CACHE_CONTROL),
            );
            resp
        }
        Err(_) => not_found(),
    }
}

fn not_found() -> Response {
    let path = dist_path("404.html");
    match std::fs::read_to_string(path) {
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

async fn not_found_handler() -> Response {
    not_found()
}

fn dist_path(rel: &str) -> PathBuf {
    // Root is crates/server/.. — same as repo_root(); cached lazily.
    repo_root().expect("repo root").join("dist").join(rel)
}

async fn index(axum::extract::State(StaticDir(root)): axum::extract::State<StaticDir>) -> Response {
    html_response(&root.join("index.html"))
}

async fn health() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

async fn rss(axum::extract::State(StaticDir(root)): axum::extract::State<StaticDir>) -> Response {
    file_response(
        &root.join("rss.xml"),
        "application/rss+xml",
        XML_CACHE_CONTROL,
    )
}

async fn sitemap(
    axum::extract::State(StaticDir(root)): axum::extract::State<StaticDir>,
) -> Response {
    file_response(
        &root.join("sitemap.xml"),
        "application/xml",
        XML_CACHE_CONTROL,
    )
}

async fn robots(
    axum::extract::State(StaticDir(root)): axum::extract::State<StaticDir>,
) -> Response {
    file_response(&root.join("robots.txt"), "text/plain", XML_CACHE_CONTROL)
}

async fn post_page(
    axum::extract::State(StaticDir(root)): axum::extract::State<StaticDir>,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response {
    // Reject path traversal: only [a-z0-9-] slugs are ever produced by build.
    if slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return not_found();
    }
    html_response(&root.join("posts").join(slug).join("index.html"))
}

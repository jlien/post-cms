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
        .route("/api/ask", axum::routing::post(ask))
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

async fn serve_path(State(s): State<StaticDir>, req: Request) -> Response {
    let gzip = wants_gzip(&req);
    serve(&s.0, req.uri().path(), gzip)
}

async fn index(State(s): State<StaticDir>, req: Request) -> Response {
    let gzip = wants_gzip(&req);
    serve(&s.0, "index.html", gzip)
}

async fn health() -> Response {
    Json(json!({ "status": "ok" })).into_response()
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

async fn ask(State(s): State<StaticDir>, body: Json<AskBody>) -> Response {
    let question = body.question.trim();
    if question.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "question is required" }))).into_response();
    }
    let Ok(key) = env::var("LLM_API_KEY") else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
            "error": "LLM_API_KEY not set — export LLM_API_KEY (and optionally LLM_BASE_URL, LLM_MODEL) on the origin"
        }))).into_response();
    };
    if key.trim().is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "LLM_API_KEY is empty" }))).into_response();
    }

    let base = env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));

    let corpus = load_corpus(&s.0);
    if corpus.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "dist/posts has no markdown — rebuild with `cargo run -p build`" }))).into_response();
    }

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
    let req = client
        .post(&url)
        .bearer_auth(&key)
        .json(&payload)
        .send()
        .await;
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
//! Sanity source for typed collections.
//!
//! A collection whose schema.yaml declares `source: {type: sanity, ...}` gets
//! its items from a GROQ query instead of item folders. Each build:
//!
//! 1. Fetches `GET https://<project_id>.api.sanity.io/v<api_version>/data/query/<dataset>?query=...`
//!    and records the raw response at `<cache>/response.json`
//!    (skipped with `SANITY_OFFLINE=1`, which reads the recorded response).
//! 2. Materializes every document in `result` into `<cache>/items/<slug>/`
//!    as `meta.yaml` (scalar and list fields) plus `index.pt.json` (the
//!    Portable Text body), the same layout as file-based items.
//!
//! The collection loader then reads `<cache>/items/` exactly as it reads a
//! file-based collection, so Sanity items pass the same schema validation
//! and render through the same Markdown/Portable Text path.
//!
//! The read token comes from the env var named by `token_env` (default
//! `SANITY_READ_TOKEN`) and is sent as a Bearer header, never in the URL or
//! the cache. With the var unset the request is anonymous (public datasets).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

/// `source:` block of a collection schema.yaml with `type: sanity`.
#[derive(Debug, Clone, Deserialize)]
pub struct SanitySource {
    /// Falls back to site.yaml `sanity.project_id`.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Falls back to site.yaml `sanity.dataset`, then `production`.
    #[serde(default)]
    pub dataset: Option<String>,
    /// Sanity API version date, with or without the leading `v`.
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// GROQ query returning an array of documents.
    pub query: String,
    /// GROQ parameters (`$name`), sent JSON-encoded.
    #[serde(default)]
    pub params: Map<String, Value>,
    /// Env var holding the read token.
    #[serde(default = "default_token_env")]
    pub token_env: String,
    /// Document field holding the slug: a string or Sanity's `{current: ...}`.
    #[serde(default = "default_slug_field")]
    pub slug_field: String,
    /// Document field holding the Portable Text body (written to index.pt.json).
    #[serde(default = "default_body_field")]
    pub body_field: String,
}

fn default_api_version() -> String {
    "2025-02-19".to_string()
}
fn default_token_env() -> String {
    "SANITY_READ_TOKEN".to_string()
}
fn default_slug_field() -> String {
    "slug".to_string()
}
fn default_body_field() -> String {
    "body".to_string()
}

/// Project and dataset defaults from site.yaml `sanity:`.
#[derive(Debug, Clone, Default)]
pub struct SiteDefaults {
    pub project_id: Option<String>,
    pub dataset: Option<String>,
}

/// Performs the HTTP GET: (url, bearer token) → response body.
pub type Fetch<'a> = &'a dyn Fn(&str, Option<&str>) -> Result<String>;

impl SanitySource {
    /// Parse a schema.yaml `source:` value. `None` (or `files`) means the
    /// collection reads item folders.
    pub fn from_schema(source: Option<&serde_yaml::Value>) -> Result<Option<SanitySource>> {
        let Some(v) = source else { return Ok(None) };
        if v.as_str() == Some("files") {
            return Ok(None);
        }
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "files" => Ok(None),
            "sanity" => Ok(Some(
                serde_yaml::from_value(v.clone()).context("parse source: {type: sanity}")?,
            )),
            other => bail!("unknown source type `{other}` (files|sanity)"),
        }
    }

    /// The query URL. Never carries the token.
    pub fn query_url(&self, site: &SiteDefaults) -> Result<String> {
        let project = self
            .project_id
            .clone()
            .or_else(|| site.project_id.clone())
            .filter(|s| !s.is_empty())
            .context("source: sanity needs project_id (schema.yaml source or site.yaml sanity)")?;
        let dataset = self
            .dataset
            .clone()
            .or_else(|| site.dataset.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "production".to_string());
        // These land in the host name and path, so only Sanity's own
        // identifier alphabets are accepted.
        if !valid_project_id(&project) {
            bail!("source: sanity project_id `{project}` must match ^[a-z0-9]+$");
        }
        if !valid_dataset(&dataset) {
            bail!("source: sanity dataset `{dataset}` must match ^[a-z0-9_-]+$");
        }
        let version = self.api_version.trim_start_matches('v');
        if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("source: sanity api_version `{}` must look like v2025-02-19", self.api_version);
        }
        let mut url = format!(
            "https://{project}.api.sanity.io/v{version}/data/query/{dataset}?query={}",
            encode(&self.query)
        );
        for (name, value) in &self.params {
            url.push_str(&format!("&{}={}", encode(&format!("${name}")), encode(&value.to_string())));
        }
        Ok(url)
    }

    fn token(&self) -> Option<String> {
        std::env::var(&self.token_env).ok().filter(|t| !t.is_empty())
    }
}

/// Fetch (unless offline) and materialize a Sanity collection into
/// `cache_dir`. Returns the items directory to load like a file collection.
pub fn sync(
    src: &SanitySource,
    site: &SiteDefaults,
    cache_dir: &Path,
    offline: bool,
    fetch: Fetch,
) -> Result<PathBuf> {
    let response_path = cache_dir.join("response.json");
    let raw = if offline {
        fs::read_to_string(&response_path).with_context(|| {
            format!(
                "SANITY_OFFLINE=1 but no recorded response at {} (run one online build first)",
                response_path.display()
            )
        })?
    } else {
        let url = src.query_url(site)?;
        let body = fetch(&url, src.token().as_deref())?;
        // Validate before overwriting a good cache with an error page.
        parse_response(&body).context("Sanity query response")?;
        fs::create_dir_all(cache_dir).with_context(|| format!("mkdir {}", cache_dir.display()))?;
        fs::write(&response_path, &body)
            .with_context(|| format!("write {}", response_path.display()))?;
        body
    };
    let docs = parse_response(&raw).with_context(|| format!("parse {}", response_path.display()))?;
    let items_dir = cache_dir.join("items");
    materialize(&docs, src, &items_dir)?;
    Ok(items_dir)
}

/// The `result` array of a GROQ query response.
pub fn parse_response(raw: &str) -> Result<Vec<Value>> {
    let json: Value = serde_json::from_str(raw).context("response is not JSON")?;
    if let Some(err) = json.get("error") {
        bail!("Sanity returned an error: {err}");
    }
    match json.get("result") {
        Some(Value::Array(docs)) => Ok(docs.clone()),
        Some(_) => bail!("`result` is not an array (the query must return a list of documents)"),
        None => bail!("response has no `result`"),
    }
}

/// Write each document as `<items_dir>/<slug>/meta.yaml` (+ `index.pt.json`).
/// Replaces whatever was there, so deleted documents disappear.
pub fn materialize(docs: &[Value], src: &SanitySource, items_dir: &Path) -> Result<usize> {
    if items_dir.exists() {
        fs::remove_dir_all(items_dir).with_context(|| format!("clear {}", items_dir.display()))?;
    }
    fs::create_dir_all(items_dir).with_context(|| format!("mkdir {}", items_dir.display()))?;
    let mut seen = std::collections::HashSet::new();
    for (i, doc) in docs.iter().enumerate() {
        let id = doc
            .get("_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("result[{i}]"));
        let Some(obj) = doc.as_object() else {
            bail!("Sanity document {id} is not an object");
        };
        let slug = match obj.get(&src.slug_field) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(o)) => o
                .get("current")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        if slug.is_empty() || slug.contains('/') || slug.contains('\\') || slug.starts_with('.') {
            bail!(
                "Sanity document {id}: slug field `{}` is missing or not a safe path segment ({slug:?})",
                src.slug_field
            );
        }
        if !seen.insert(slug.clone()) {
            bail!("Sanity document {id}: duplicate slug `{slug}`");
        }
        let (mut meta, body) = split_document(obj, src);
        // File items carry `slug` in meta.yaml (templates list `item.slug`).
        meta.insert("slug".to_string(), Value::String(slug.clone()));
        let dir = items_dir.join(&slug);
        fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let yaml = serde_yaml::to_string(&Value::Object(meta))
            .with_context(|| format!("serialize meta for {id}"))?;
        fs::write(dir.join("meta.yaml"), yaml)
            .with_context(|| format!("write {}/meta.yaml", dir.display()))?;
        if let Some(blocks) = body {
            fs::write(dir.join("index.pt.json"), serde_json::to_string_pretty(&blocks)?)
                .with_context(|| format!("write {}/index.pt.json", dir.display()))?;
        }
    }
    Ok(docs.len())
}

/// Split a document into meta fields and the Portable Text body. Drops
/// Sanity system keys (`_id`, `_type`, `_rev`, ...), nulls (GROQ projections
/// of unset fields), and the raw slug field (re-added as a plain `slug`).
/// Documents without `published` are published: the default perspective
/// returns only published documents.
fn split_document(obj: &Map<String, Value>, src: &SanitySource) -> (Map<String, Value>, Option<Value>) {
    let mut meta = Map::new();
    let mut body = None;
    for (k, v) in obj {
        if k == &src.body_field {
            if !v.is_null() {
                body = Some(v.clone());
            }
            continue;
        }
        if k.starts_with('_') || k == &src.slug_field || v.is_null() {
            continue;
        }
        meta.insert(k.clone(), v.clone());
    }
    meta.entry("published").or_insert(Value::Bool(true));
    (meta, body)
}

/// Sanity project IDs: lowercase letters and digits.
pub fn valid_project_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Sanity dataset names: lowercase letters, digits, `_` and `-`.
pub fn valid_dataset(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Whole-request limit for a Sanity fetch, so a stalled API cannot hang a build.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Blocking HTTPS GET with an optional Bearer token.
pub fn http_fetch(url: &str, token: Option<&str>) -> Result<String> {
    http_fetch_with_timeout(url, token, FETCH_TIMEOUT)
}

fn http_fetch_with_timeout(url: &str, token: Option<&str>, timeout: Duration) -> Result<String> {
    let mut req = ureq::get(url).header("Accept", "application/json");
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut resp = req
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .call()
        .with_context(|| format!("GET {}", redact(url)))?;
    let status = resp.status();
    let body = resp
        .body_mut()
        .read_to_string()
        .with_context(|| format!("read body of {}", redact(url)))?;
    if !status.is_success() {
        bail!("GET {} returned {status}: {}", redact(url), body.chars().take(300).collect::<String>());
    }
    Ok(body)
}

/// URL without its query string, for error messages.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Percent-encode everything outside RFC 3986 unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const RECORDED: &str = include_str!("../fixtures/sanity/articles/response.json");

    fn source() -> SanitySource {
        let schema: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../fixtures/sanity/articles/schema.yaml"
        ))
        .unwrap();
        SanitySource::from_schema(schema.get("source")).unwrap().unwrap()
    }

    #[test]
    fn source_parses_with_defaults() {
        assert!(SanitySource::from_schema(None).unwrap().is_none());
        let files: serde_yaml::Value = serde_yaml::from_str("files").unwrap();
        assert!(SanitySource::from_schema(Some(&files)).unwrap().is_none());
        let bad: serde_yaml::Value = serde_yaml::from_str("type: ftp").unwrap();
        assert!(SanitySource::from_schema(Some(&bad)).is_err());
        let min: serde_yaml::Value = serde_yaml::from_str("type: sanity\nquery: '*'").unwrap();
        let s = SanitySource::from_schema(Some(&min)).unwrap().unwrap();
        assert_eq!(s.token_env, "SANITY_READ_TOKEN");
        assert_eq!(s.slug_field, "slug");
        assert_eq!(s.body_field, "body");
    }

    #[test]
    fn query_url_encodes_query_and_params() {
        let mut s = source();
        s.params.insert("kind".into(), Value::String("guide".into()));
        let url = s.query_url(&SiteDefaults::default()).unwrap();
        assert!(url.starts_with(
            "https://exmpl123.api.sanity.io/v2025-02-19/data/query/production?query=%2A%5B_type%20%3D%3D%20%22article%22"
        ), "{url}");
        assert!(url.ends_with("&%24kind=%22guide%22"), "{url}");
    }

    #[test]
    fn query_url_falls_back_to_site_config() {
        let raw: serde_yaml::Value =
            serde_yaml::from_str("type: sanity\nquery: '*'\napi_version: v2024-01-01").unwrap();
        let s = SanitySource::from_schema(Some(&raw)).unwrap().unwrap();
        assert!(s.query_url(&SiteDefaults::default()).is_err());
        let site = SiteDefaults { project_id: Some("p1".into()), dataset: Some("staging".into()) };
        assert_eq!(
            s.query_url(&site).unwrap(),
            "https://p1.api.sanity.io/v2024-01-01/data/query/staging?query=%2A"
        );
    }

    #[test]
    fn query_url_rejects_unsafe_project_and_dataset() {
        let raw: serde_yaml::Value = serde_yaml::from_str("type: sanity\nquery: '*'").unwrap();
        let s = SanitySource::from_schema(Some(&raw)).unwrap().unwrap();
        for (project, dataset) in [
            ("evil.example/x", "production"),
            ("p1@evil.example", "production"),
            ("P1", "production"),
            ("p1", "../other"),
            ("p1", "prod?x=1"),
            ("p1", "Prod"),
        ] {
            let site = SiteDefaults { project_id: Some(project.into()), dataset: Some(dataset.into()) };
            assert!(s.query_url(&site).is_err(), "{project} {dataset}");
        }
        let site = SiteDefaults { project_id: Some("abc123".into()), dataset: Some("my_data-set".into()) };
        assert!(s.query_url(&site).is_ok());
        let mut bad_version = s.clone();
        bad_version.api_version = "v1/../x".into();
        assert!(bad_version.query_url(&site).is_err());
    }

    #[test]
    fn http_fetch_times_out_on_stalled_server() {
        // A listener that accepts and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = std::thread::spawn(move || {
            let conn = listener.accept();
            std::thread::sleep(Duration::from_secs(1));
            drop(conn);
        });
        let start = std::time::Instant::now();
        let err = http_fetch_with_timeout(&format!("http://{addr}/q"), None, Duration::from_millis(300));
        assert!(err.is_err());
        assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
        assert_eq!(FETCH_TIMEOUT, Duration::from_secs(30));
        hold.join().unwrap();
    }

    #[test]
    fn parse_response_requires_result_array() {
        assert_eq!(parse_response(RECORDED).unwrap().len(), 3);
        assert!(parse_response("{\"ms\":1,\"result\":{}}").is_err());
        assert!(parse_response("{\"error\":{\"description\":\"bad query\"}}").is_err());
        assert!(parse_response("<html>").is_err());
    }

    #[test]
    fn materialize_writes_file_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = parse_response(RECORDED).unwrap();
        let n = materialize(&docs, &source(), tmp.path()).unwrap();
        assert_eq!(n, 3);
        let meta: serde_yaml::Value = serde_yaml::from_str(
            &fs::read_to_string(tmp.path().join("caching-guide/meta.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["title"].as_str(), Some("A Caching Guide"));
        assert_eq!(meta["published"].as_bool(), Some(true));
        assert!(meta.get("_id").is_none() && meta.get("_rev").is_none());
        assert_eq!(meta["slug"].as_str(), Some("caching-guide"));
        // Null projections are dropped, matching a file item that omits the key.
        assert!(meta.get("hero_url").is_none());
        let pt: Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join("caching-guide/index.pt.json")).unwrap(),
        )
        .unwrap();
        assert!(pt.as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn materialize_rejects_bad_and_duplicate_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        let s = source();
        let bad = vec![serde_json::json!({"_id": "a", "title": "x", "slug": {"current": "../up"}})];
        let err = materialize(&bad, &s, tmp.path()).unwrap_err().to_string();
        assert!(err.contains("Sanity document a"), "{err}");
        let missing = vec![serde_json::json!({"_id": "b", "title": "x"})];
        assert!(materialize(&missing, &s, tmp.path()).is_err());
        let dup = vec![
            serde_json::json!({"_id": "c", "slug": {"current": "same"}}),
            serde_json::json!({"_id": "d", "slug": "same"}),
        ];
        let err = materialize(&dup, &s, tmp.path()).unwrap_err().to_string();
        assert!(err.contains("duplicate slug"), "{err}");
    }

    #[test]
    fn online_sync_records_response_and_sends_token_as_header() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = source();
        s.token_env = "POSTCMS_TEST_SANITY_TOKEN_ONLINE".into();
        std::env::set_var(&s.token_env, "sk-test");
        let calls: RefCell<Vec<(String, Option<String>)>> = RefCell::new(Vec::new());
        let fake = |url: &str, token: Option<&str>| -> Result<String> {
            calls.borrow_mut().push((url.to_string(), token.map(str::to_string)));
            Ok(RECORDED.to_string())
        };
        let items = sync(&s, &SiteDefaults::default(), tmp.path(), false, &fake).unwrap();
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].0.contains("sk-test"));
        assert_eq!(calls[0].1.as_deref(), Some("sk-test"));
        assert_eq!(fs::read_to_string(tmp.path().join("response.json")).unwrap(), RECORDED);
        assert!(items.join("edge-basics/meta.yaml").exists());
    }

    #[test]
    fn online_sync_keeps_cache_when_response_is_bad() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("response.json"), RECORDED).unwrap();
        let fake = |_: &str, _: Option<&str>| -> Result<String> { Ok("{\"error\":\"nope\"}".into()) };
        assert!(sync(&source(), &SiteDefaults::default(), tmp.path(), false, &fake).is_err());
        assert_eq!(fs::read_to_string(tmp.path().join("response.json")).unwrap(), RECORDED);
    }

    #[test]
    fn offline_sync_reads_cache_without_fetching() {
        let tmp = tempfile::tempdir().unwrap();
        let no_net = |_: &str, _: Option<&str>| -> Result<String> { panic!("offline build fetched") };
        let err = sync(&source(), &SiteDefaults::default(), tmp.path(), true, &no_net)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SANITY_OFFLINE=1"), "{err}");
        fs::write(tmp.path().join("response.json"), RECORDED).unwrap();
        let items = sync(&source(), &SiteDefaults::default(), tmp.path(), true, &no_net).unwrap();
        assert_eq!(fs::read_dir(items).unwrap().count(), 3);
    }
}

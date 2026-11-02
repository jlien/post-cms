//! Cache-Control per response class, from site.yaml `cache:`.
//!
//! ```yaml
//! cache:                     # every key optional; these are the defaults
//!   html: "public, max-age=0, s-maxage=300, stale-while-revalidate=86400"
//!   hashed: "public, max-age=31536000, immutable"
//!   static: "public, max-age=3600, s-maxage=300, stale-while-revalidate=86400"
//!   feeds: "public, max-age=0, s-maxage=300, stale-while-revalidate=3600"
//!   not_found: "public, max-age=0, s-maxage=60"
//!   redirects: "public, max-age=3600, s-maxage=300"
//! ```
//!
//! - `html`: pages, plus unhashed CSS, JS, SVG and unknown file types.
//! - `hashed`: files the image pipeline wrote under its content-addressed
//!   names (`<16 hex>.<ext>` or `<16 hex>-<width>.<ext>` directly in
//!   `images.output_dir`). Nothing else is treated as hashed.
//! - `static`: other images, fonts, media and PDFs (theme and plugin assets
//!   are copied under their own names, so they can change in place).
//! - `feeds`: XML, JSON, TXT and Markdown (sitemap, RSS, robots, llms.txt).
//! - `not_found`: every 404 from the static fallback.
//! - `redirects`: `redirects.yaml` hits and slash redirects.
//!
//! Responses that depend on who asks are never cacheable and are not
//! configurable: `/preview` (IAP-gated drafts), `/mcp` and `/api/ask` get
//! `private, no-store` from a router layer that runs after site.yaml.
//!
//! Values are checked at startup: known response directives only, each at
//! most once, delta-seconds as plain integers, and not both `public` and
//! `private`. Anything else refuses to start.

use axum::http::HeaderValue;

pub const DEFAULT_HTML: &str = "public, max-age=0, s-maxage=300, stale-while-revalidate=86400";
pub const DEFAULT_HASHED: &str = "public, max-age=31536000, immutable";
pub const DEFAULT_STATIC: &str = "public, max-age=3600, s-maxage=300, stale-while-revalidate=86400";
pub const DEFAULT_FEEDS: &str = "public, max-age=0, s-maxage=300, stale-while-revalidate=3600";
pub const DEFAULT_NOT_FOUND: &str = "public, max-age=0, s-maxage=60";
pub const DEFAULT_REDIRECTS: &str = "public, max-age=3600, s-maxage=300";

/// What every response that varies by caller gets. Never configurable.
pub const PRIVATE: &str = "private, no-store";

/// The image pipeline's default `images.output_dir` (crates/build).
const IMAGES_DEFAULT_OUTPUT_DIR: &str = "assets/img";

/// Extensions the image pipeline writes: encoded variants (avif, webp, jpg,
/// png) and byte-for-byte copies (gif, png, webp, svg).
const HASHED_EXTS: &[&str] = &["avif", "webp", "jpg", "png", "gif", "svg"];

/// Length of the pipeline's hex id (the first 16 hex digits of a sha256).
const HASH_LEN: usize = 16;

/// Response classes with a configurable policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Html,
    Hashed,
    Static,
    Feeds,
    NotFound,
    Redirects,
}

#[derive(Clone, Debug)]
pub struct CachePolicy {
    pub html: HeaderValue,
    pub hashed: HeaderValue,
    pub static_files: HeaderValue,
    pub feeds: HeaderValue,
    pub not_found: HeaderValue,
    pub redirects: HeaderValue,
    /// `images.output_dir` when the image pipeline is on, else `None` (then
    /// no file counts as hashed).
    pub hashed_dir: Option<String>,
}

impl Default for CachePolicy {
    fn default() -> Self {
        CachePolicy {
            html: HeaderValue::from_static(DEFAULT_HTML),
            hashed: HeaderValue::from_static(DEFAULT_HASHED),
            static_files: HeaderValue::from_static(DEFAULT_STATIC),
            feeds: HeaderValue::from_static(DEFAULT_FEEDS),
            not_found: HeaderValue::from_static(DEFAULT_NOT_FOUND),
            redirects: HeaderValue::from_static(DEFAULT_REDIRECTS),
            hashed_dir: None,
        }
    }
}

impl CachePolicy {
    /// Parse site.yaml text: the `cache:` block and `images.output_dir`.
    pub fn from_site_yaml(raw: &str) -> anyhow::Result<CachePolicy> {
        let doc: serde_yaml::Value =
            serde_yaml::from_str(raw).map_err(|e| anyhow::anyhow!("site.yaml: {e}"))?;
        let mut p = CachePolicy { hashed_dir: hashed_dir(&doc), ..CachePolicy::default() };
        let block = match doc.get("cache") {
            None | Some(serde_yaml::Value::Null) => return Ok(p),
            Some(v) => v,
        };
        let map = block.as_mapping().ok_or_else(|| {
            anyhow::anyhow!("cache: must be a mapping of class (html, hashed, static, feeds, not_found, redirects) to a Cache-Control value")
        })?;
        for (k, v) in map {
            let key = k.as_str().ok_or_else(|| anyhow::anyhow!("cache: keys must be strings"))?;
            let slot = match key {
                "html" => &mut p.html,
                "hashed" => &mut p.hashed,
                "static" => &mut p.static_files,
                "feeds" => &mut p.feeds,
                "not_found" => &mut p.not_found,
                "redirects" => &mut p.redirects,
                other => anyhow::bail!(
                    "cache: unknown key {other:?} (expected html, hashed, static, feeds, not_found or redirects)"
                ),
            };
            let value = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("cache.{key}: value must be a string (quote it)"))?;
            *slot = validate(value).map_err(|e| anyhow::anyhow!("cache.{key}: {e}"))?;
        }
        Ok(p)
    }

    pub fn get(&self, class: Class) -> HeaderValue {
        match class {
            Class::Html => self.html.clone(),
            Class::Hashed => self.hashed.clone(),
            Class::Static => self.static_files.clone(),
            Class::Feeds => self.feeds.clone(),
            Class::NotFound => self.not_found.clone(),
            Class::Redirects => self.redirects.clone(),
        }
    }

    /// Class of a file under dist/, by its dist-relative path (no leading
    /// slash, index.html already resolved).
    pub fn class_for_file(&self, rel: &str) -> Class {
        let ext = rel.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('/'));
        match ext {
            Some("html" | "htm") => Class::Html,
            Some("xml" | "txt" | "md" | "json") => Class::Feeds,
            _ if self.is_hashed(rel) => Class::Hashed,
            Some("css" | "js" | "svg") => Class::Html,
            Some(
                "png" | "jpg" | "jpeg" | "webp" | "avif" | "gif" | "ttf" | "otf" | "woff" | "woff2"
                | "mp4" | "webm" | "mp3" | "ogg" | "wav" | "pdf",
            ) => Class::Static,
            _ => Class::Html,
        }
    }

    /// True only for a file the image pipeline wrote: directly in
    /// `images.output_dir`, named `<16 lowercase hex>.<ext>` or
    /// `<16 lowercase hex>-<width>.<ext>` with one of the pipeline's
    /// extensions. Draft-only images (under /preview) are private anyway.
    pub fn is_hashed(&self, rel: &str) -> bool {
        let Some(dir) = &self.hashed_dir else { return false };
        let Some((parent, name)) = rel.rsplit_once('/') else { return false };
        parent == dir && is_hashed_name(name)
    }
}

/// The pipeline's output file name: `<id>.<ext>` or `<id>-<width>.<ext>`.
pub fn is_hashed_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    if !HASHED_EXTS.contains(&ext) {
        return false;
    }
    let (id, width) = match stem.split_once('-') {
        Some((id, w)) => (id, Some(w)),
        None => (stem, None),
    };
    let hex = id.len() == HASH_LEN && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let width_ok = width.map_or(true, |w| {
        !w.is_empty() && w.len() <= 5 && !w.starts_with('0') && w.bytes().all(|b| b.is_ascii_digit())
    });
    hex && width_ok
}

/// `images.output_dir` when the `images:` block turns the pipeline on.
fn hashed_dir(doc: &serde_yaml::Value) -> Option<String> {
    let images = doc.get("images").filter(|v| !v.is_null())?;
    if images.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let dir = images
        .get("output_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(IMAGES_DEFAULT_OUTPUT_DIR)
        .trim_matches('/');
    (!dir.is_empty()).then(|| dir.to_string())
}

/// Response directives that take no value.
const FLAGS: &[&str] = &[
    "public",
    "private",
    "no-cache",
    "no-store",
    "no-transform",
    "must-revalidate",
    "proxy-revalidate",
    "must-understand",
    "immutable",
];

/// Response directives that take delta-seconds.
const SECONDS: &[&str] = &["max-age", "s-maxage", "stale-while-revalidate", "stale-if-error"];

/// Largest delta-seconds accepted (RFC 9111 section 1.2.2 caps at 2^31).
const MAX_SECONDS: u64 = 2_147_483_648;

/// Check a Cache-Control value: a comma-separated list of known response
/// directives, each at most once, `name=<integer>` for the delta-seconds
/// ones, and not `public` together with `private`.
pub fn validate(value: &str) -> Result<HeaderValue, String> {
    if value.trim().is_empty() {
        return Err("Cache-Control is empty".into());
    }
    if value != value.trim() {
        return Err("Cache-Control has leading or trailing whitespace".into());
    }
    if value.chars().any(|c| c.is_control() || !c.is_ascii()) {
        return Err("Cache-Control contains a control or non-ASCII character".into());
    }
    let mut seen: Vec<String> = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("Cache-Control {value:?} has an empty directive"));
        }
        let (name, arg) = match part.split_once('=') {
            Some((n, a)) => (n.trim(), Some(a.trim())),
            None => (part, None),
        };
        let lower = name.to_ascii_lowercase();
        if FLAGS.contains(&lower.as_str()) {
            if arg.is_some() {
                return Err(format!("Cache-Control directive {name:?} takes no value"));
            }
        } else if SECONDS.contains(&lower.as_str()) {
            let Some(a) = arg else {
                return Err(format!("Cache-Control directive {name:?} needs a number of seconds ({name}=300)"));
            };
            let ok = !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()) && a.parse::<u64>().is_ok_and(|n| n <= MAX_SECONDS);
            if !ok {
                return Err(format!("Cache-Control {name}={a}: seconds must be a whole number from 0 to {MAX_SECONDS}"));
            }
        } else {
            return Err(format!(
                "Cache-Control directive {name:?} is not a known response directive ({}, {})",
                FLAGS.join(", "),
                SECONDS.join(", ")
            ));
        }
        if seen.contains(&lower) {
            return Err(format!("Cache-Control directive {name:?} is listed twice"));
        }
        seen.push(lower);
    }
    if seen.iter().any(|d| d == "public") && seen.iter().any(|d| d == "private") {
        return Err("Cache-Control cannot be both public and private".into());
    }
    HeaderValue::from_str(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(yaml: &str) -> CachePolicy {
        CachePolicy::from_site_yaml(yaml).unwrap()
    }

    #[test]
    fn defaults_without_a_cache_block() {
        let p = pol("title: x\n");
        assert_eq!(p.html, DEFAULT_HTML);
        assert_eq!(p.hashed, DEFAULT_HASHED);
        assert_eq!(p.not_found, DEFAULT_NOT_FOUND);
        assert_eq!(p.redirects, DEFAULT_REDIRECTS);
        assert_eq!(p.hashed_dir, None);
        // Every default is itself a valid value.
        for v in [DEFAULT_HTML, DEFAULT_HASHED, DEFAULT_STATIC, DEFAULT_FEEDS, DEFAULT_NOT_FOUND, DEFAULT_REDIRECTS, PRIVATE] {
            assert!(validate(v).is_ok(), "{v}");
        }
        assert_eq!(DEFAULT_HTML, "public, max-age=0, s-maxage=300, stale-while-revalidate=86400");
    }

    #[test]
    fn site_yaml_overrides_each_class() {
        let p = pol(
            "cache:\n  html: \"public, max-age=0, s-maxage=120\"\n  hashed: \"public, max-age=600\"\n  \
             static: no-cache\n  feeds: \"public, max-age=60\"\n  not_found: no-store\n  redirects: \"public, max-age=30\"\n",
        );
        assert_eq!(p.get(Class::Html), "public, max-age=0, s-maxage=120");
        assert_eq!(p.get(Class::Hashed), "public, max-age=600");
        assert_eq!(p.get(Class::Static), "no-cache");
        assert_eq!(p.get(Class::Feeds), "public, max-age=60");
        assert_eq!(p.get(Class::NotFound), "no-store");
        assert_eq!(p.get(Class::Redirects), "public, max-age=30");
        // A partial block keeps the other defaults.
        let p = pol("cache:\n  html: no-cache\n");
        assert_eq!(p.html, "no-cache");
        assert_eq!(p.hashed, DEFAULT_HASHED);
    }

    #[test]
    fn rejects_invalid_values_and_keys() {
        for (yaml, want) in [
            ("cache:\n  html: \"public, max-age=abc\"\n", "whole number"),
            ("cache:\n  html: \"public, max-age=-1\"\n", "whole number"),
            ("cache:\n  html: \"public, max-age=99999999999\"\n", "whole number"),
            ("cache:\n  html: \"public, max-age\"\n", "needs a number"),
            ("cache:\n  html: \"public=1\"\n", "takes no value"),
            ("cache:\n  html: \"public, maxage=60\"\n", "not a known"),
            ("cache:\n  html: \"public,, max-age=60\"\n", "empty directive"),
            ("cache:\n  html: \"public, private\"\n", "both public and private"),
            ("cache:\n  html: \"max-age=1, max-age=2\"\n", "listed twice"),
            ("cache:\n  html: \"\"\n", "empty"),
            ("cache:\n  html: \"public\\r\\nSet-Cookie: a=1\"\n", "control"),
            ("cache:\n  html: 300\n", "must be a string"),
            ("cache:\n  pages: no-store\n", "unknown key"),
            ("cache: [a]\n", "must be a mapping"),
            ("cache:\n  html: \"private=\\\"set-cookie\\\"\"\n", "takes no value"),
        ] {
            let err = format!("{:#}", CachePolicy::from_site_yaml(yaml).unwrap_err());
            assert!(err.contains(want), "{yaml:?}: {err}");
        }
        // Case-insensitive names, any spacing after commas.
        assert!(validate("Public,Max-Age=60,  immutable").is_ok());
    }

    #[test]
    fn hashed_names_follow_the_image_pipeline() {
        for n in ["0123456789abcdef.png", "0123456789abcdef-640.webp", "deadbeefdeadbeef-1920.avif", "0123456789abcdef.svg", "0123456789abcdef-320.jpg", "0123456789abcdef.gif"] {
            assert!(is_hashed_name(n), "{n}");
        }
        for n in [
            "logo.png",
            "0123456789ABCDEF.png",
            "0123456789abcde.png",
            "0123456789abcdef0.png",
            "0123456789abcdef-.png",
            "0123456789abcdef-064.png",
            "0123456789abcdef-x.png",
            "0123456789abcdef.css",
            "0123456789abcdef.jpeg",
            "0123456789abcdef-640-2.png",
        ] {
            assert!(!is_hashed_name(n), "{n}");
        }
    }

    #[test]
    fn hashed_only_in_the_pipeline_output_dir() {
        let on = pol("images: {}\n");
        assert_eq!(on.hashed_dir.as_deref(), Some("assets/img"));
        assert!(on.is_hashed("assets/img/0123456789abcdef-640.webp"));
        assert!(!on.is_hashed("assets/img/sub/0123456789abcdef-640.webp"));
        assert!(!on.is_hashed("assets/theme/0123456789abcdef-640.webp"));
        assert!(!on.is_hashed("0123456789abcdef-640.webp"));
        let custom = pol("images:\n  output_dir: media/out\n");
        assert!(custom.is_hashed("media/out/0123456789abcdef.png"));
        assert!(!custom.is_hashed("assets/img/0123456789abcdef.png"));
        // Pipeline off: nothing is hashed.
        assert!(!pol("title: x\n").is_hashed("assets/img/0123456789abcdef.png"));
        assert!(!pol("images:\n  enabled: false\n").is_hashed("assets/img/0123456789abcdef.png"));
    }

    #[test]
    fn file_classes() {
        let p = pol("images: {}\n");
        let c = |r: &str| p.class_for_file(r);
        assert_eq!(c("index.html"), Class::Html);
        assert_eq!(c("about/index.html"), Class::Html);
        assert_eq!(c("assets/site/app.css"), Class::Html);
        assert_eq!(c("assets/site/app.js"), Class::Html);
        assert_eq!(c("assets/site/logo.svg"), Class::Html);
        assert_eq!(c("assets/img/0123456789abcdef.svg"), Class::Hashed);
        assert_eq!(c("assets/img/0123456789abcdef-640.webp"), Class::Hashed);
        assert_eq!(c("assets/site/logo.png"), Class::Static);
        assert_eq!(c("assets/site/font.woff2"), Class::Static);
        assert_eq!(c("files/report.pdf"), Class::Static);
        assert_eq!(c("sitemap.xml"), Class::Feeds);
        assert_eq!(c("robots.txt"), Class::Feeds);
        assert_eq!(c("search-index.json"), Class::Feeds);
        assert_eq!(c("posts/a/index.md"), Class::Feeds);
        assert_eq!(c("data.bin"), Class::Html);
        assert_eq!(c("a.dir/noext"), Class::Html);
    }
}

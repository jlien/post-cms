//! Static build pipeline: content/posts/* → dist/.
//!
//! Reads every post directory (meta.yaml + index.mdx), renders markdown to
//! HTML, and emits post pages, an index, 404, RSS 2.0, sitemap.xml, and
//! robots.txt into dist/.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, Utc};
use pulldown_cmark::{html, Options, Parser};
use serde::Deserialize;
use tera::{Context as TeraContext, Tera};

#[derive(Debug, Deserialize)]
struct Meta {
    title: String,
    slug: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_lang")]
    lang: String,
    #[serde(default)]
    published: bool,
    date: NaiveDate,
    #[serde(default)]
    description: String,
    /// Agent commentary carried on drafts and rendered in the preview
    /// (why the draft is written this way, open questions) — never on the
    /// live site, where drafts are skipped entirely.
    #[serde(default)]
    agent_notes: Option<String>,
}

/// One item of a typed collection (content/<collection>/<slug>/).
/// Metadata is kept generic: every key in meta.yaml is passed through to
/// the template, and the collection's schema.yaml decides what is required.
#[derive(Debug)]
struct CollectionItem {
    slug: String,
    title: String,
    description: String,
    date: Option<NaiveDate>,
    published: bool,
    fields: serde_json::Map<String, serde_json::Value>,
    dir: PathBuf,
}

/// A typed collection: a folder under content/ with a schema.yaml.
#[derive(Debug)]
struct Collection {
    name: String,
    title: String,
    required: Vec<String>,
    types: std::collections::HashMap<String, String>,
    /// Template used for item pages (schema.yaml `item_template`, default collection-item.html).
    item_template: String,
    /// Template used for the collection index (schema.yaml `index_template`, default collection.html).
    index_template: String,
    /// URL prefix for the index and items (schema.yaml `url_prefix`, default `/<name>`).
    url_prefix: String,
    /// Whether to render the index page (schema.yaml `index`, default true).
    index: bool,
    items: Vec<CollectionItem>,
}

impl Collection {
    fn item_path(&self, slug: &str) -> String {
        format!("{}/{}", self.url_prefix.trim_end_matches('/'), slug)
    }
}

/// URL style for every emitted link (sitemap, search index, llms.txt, RSS,
/// canonical). site.yaml `trailing_slash` (default true keeps `/x/`); false
/// emits `/x` and the server 301s the slash form.
static TRAILING_SLASH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Normalize a site path ("/a/b", "a/b/", "/") to the configured URL style.
fn page_url(path: &str) -> String {
    page_url_with(path, *TRAILING_SLASH.get().unwrap_or(&true))
}

fn page_url_with(path: &str, trailing_slash: bool) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    if trailing_slash {
        format!("/{trimmed}/")
    } else {
        format!("/{trimmed}")
    }
}

/// Output directory under dist/ for a site path ("/" is dist itself).
fn out_dir_for(dist: &Path, path: &str) -> PathBuf {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        dist.to_path_buf()
    } else {
        dist.join(trimmed)
    }
}

fn default_lang() -> String {
    "en".to_string()
}

impl Meta {
    fn to_context(&self, body: &str) -> TeraContext {
        let mut ctx = TeraContext::new();
        ctx.insert("title", &self.title);
        ctx.insert("slug", &self.slug);
        ctx.insert("tags", &self.tags);
        ctx.insert("lang", &self.lang);
        ctx.insert("published", &self.published);
        ctx.insert("date", &self.date.to_string());
        ctx.insert("description", &self.description);
        if let Some(notes) = &self.agent_notes {
            ctx.insert("agent_notes", notes);
        }
        ctx.insert("body", body);
        ctx
    }
}

#[derive(Debug, Deserialize)]
struct PageMeta {
    title: String,
    slug: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_lang")]
    lang: String,
    #[serde(default = "default_true")]
    published: bool,
    /// Site path (e.g. `/platform/operations`, `/` for the home page).
    /// Default `/<slug>`.
    #[serde(default)]
    path: Option<String>,
    /// Template to render with (default page.html).
    #[serde(default)]
    template: Option<String>,
}

impl PageMeta {
    fn site_path(&self) -> String {
        self.path.clone().unwrap_or_else(|| format!("/{}", self.slug))
    }
}

fn default_true() -> bool {
    true
}

impl PageMeta {
    fn to_context(&self, body: &str) -> TeraContext {
        let mut ctx = TeraContext::new();
        ctx.insert("title", &self.title);
        ctx.insert("slug", &self.slug);
        ctx.insert("lang", &self.lang);
        ctx.insert("description", &self.description);
        ctx.insert("url", &page_url(&self.site_path()));
        ctx.insert("body", body);
        ctx
    }
}

#[derive(Debug)]
struct Page {
    meta: PageMeta,
    dir: PathBuf,
}

/// site.yaml — theme plus the copy every default page needs.
#[derive(Debug, Deserialize)]
struct SiteConfig {
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_title")]
    title: String,
    #[serde(default)]
    tagline: String,
    #[serde(default = "default_contact_email")]
    contact_email: String,
    /// Emit `/x/` (true, default) or `/x` (false) URLs.
    #[serde(default = "default_true")]
    trailing_slash: bool,
    /// Render the built-in blog home (`/`), `/blog/`, and `/contact/` and
    /// require posts (true, default). Sites with their own home page set false.
    #[serde(default = "default_true")]
    builtin_pages: bool,
}

fn default_theme() -> String {
    "default".to_string()
}
fn default_title() -> String {
    "Post-CMS".to_string()
}
fn default_contact_email() -> String {
    "hello@example.com".to_string()
}

impl SiteConfig {
    fn load(root: &Path) -> SiteConfig {
        let raw = fs::read_to_string(root.join("site.yaml")).ok();
        raw.and_then(|raw| serde_yaml::from_str(&raw).ok())
            .unwrap_or_else(SiteConfig::default_theme_config)
    }
    fn default_theme_config() -> SiteConfig {
        SiteConfig {
            theme: default_theme(),
            title: default_title(),
            tagline: String::new(),
            contact_email: default_contact_email(),
            trailing_slash: true,
            builtin_pages: true,
        }
    }
    fn base_ctx(&self, style: &Styling, root: &str) -> TeraContext {
        let mut ctx = TeraContext::new();
        ctx.insert("css_hash", &style.hash);
        ctx.insert("style", &style.css);
        ctx.insert("site_title", &self.title);
        // Available to every template (headers/footers), not just home.html.
        ctx.insert("site_tagline", &self.tagline);
        ctx.insert("theme", &style.theme);
        ctx.insert("root", root);
        ctx.insert("lang", "en");
        ctx
    }
}

#[derive(Debug)]
struct Post {
    meta: Meta,
    dir: PathBuf,
}

impl Post {
    /// Path used in the sitemap, RSS, and llms.txt. With the default
    /// trailing_slash: true this stays the historical `/posts/<slug>`, so
    /// existing sites emit exactly what they did before.
    fn url_path(&self) -> String {
        let raw = format!("/posts/{}", self.meta.slug);
        if *TRAILING_SLASH.get().unwrap_or(&true) { raw } else { page_url(&raw) }
    }

    /// Path used in the search and embeddings indexes (slash form by default,
    /// as before).
    fn index_url(&self) -> String {
        page_url(&format!("/posts/{}", self.meta.slug))
    }

    fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "slug": self.meta.slug,
            "title": self.meta.title,
            "date": self.meta.date.to_string(),
            "published": self.meta.published,
            "tags": self.meta.tags,
            "description": self.meta.description,
        })
    }
}

fn main() -> Result<()> {
    let root = repo_root()?;
    let dist = root.join("dist");
    if dist.exists() {
        fs::remove_dir_all(&dist)
            .with_context(|| format!("failed to clear {}", dist.display()))?;
    }
    fs::create_dir_all(&dist).with_context(|| format!("mkdir {}", dist.display()))?;

    // ---- theme plugin system -------------------------------------------
    // Priority: SITE_THEME env > site.yaml `theme:` > "default".
    let theme_name = std::env::var("SITE_THEME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| read_site_theme(&root))
        .unwrap_or_else(|| "default".to_string());
    let theme_dir = root.join("themes").join(&theme_name);
    if !theme_dir.is_dir() {
        bail!(
            "theme {:?} not found under themes/ (available: {:?})",
            theme_name,
            available_themes(&root)
        );
    }
    let templates_dir = root.join("templates");
    let tera = load_tera(&root, &theme_dir, &templates_dir)?;
    let style = compile_theme_css(&root, &theme_dir, &theme_name)?;
    let css_hash = sha256_hex(style.as_bytes());
    let styling = Styling {
        css: style,
        hash: css_hash,
        theme: theme_name.clone(),
    };
    // Theme asset kit (fonts, icons, images) → dist/assets/<theme>/
    copy_theme_assets(&theme_dir, &dist, &theme_name)?;

    let cfg = SiteConfig::load(&root);
    let _ = TRAILING_SLASH.set(cfg.trailing_slash);
    let posts_dir = root.join("content/posts");
    let posts = if posts_dir.is_dir() { load_posts(&posts_dir)? } else { Vec::new() };
    if posts.is_empty() && cfg.builtin_pages {
        bail!("no posts found under content/posts");
    }
    // Default pages (about, privacy, terms, ...) — optional directory.
    let pages = load_pages(&root.join("content/pages")).unwrap_or_default();

    // Live site renders at the site root.
    render_posts(&tera, &dist, &posts, &styling, &cfg, "")?;
    render_pages(&tera, &dist, &pages, &styling, &cfg, "")?;
    let (collections, _ref_checks) = load_collections(&root.join("content"))?;
    render_collections(&tera, &dist, &collections, &styling, &cfg, "")?;
    if cfg.builtin_pages {
        render_home(&tera, &dist, &posts, &styling, &cfg, "")?;
        render_blog(&tera, &dist, &posts, &styling, &cfg, "")?;
        render_contact(&tera, &dist, &styling, &cfg, "")?;
    }
    render_404(&tera, &dist, &styling, &cfg, "")?;
    if !posts.is_empty() {
        render_rss(&root, &dist, &posts)?;
    }
    render_sitemap(&root, &dist, &posts, &pages, &collections, cfg.builtin_pages)?;
    render_robots(&dist, &site_base_url(&root))?;
    render_llms_txt(&root, &dist, &posts)?;
    render_mcp_card(&root, &dist)?;
    render_search_index(&root, &dist, &posts, &collections)?;
    render_posts_markdown(&dist, &posts)?;
    render_embeddings(&root, &dist, &posts, &collections)?;
    // ---- draft previews --------------------------------------------------
    // DRAFTS=1 renders unpublished posts + collection items under
    // dist/preview/drafts/ with a draft banner and embedded agent_notes, so
    // a proposal's reviewer sees the rendered page without touching git.
    // The plain build never produces preview/, so deploys stay clean.
    let drafts_on = std::env::var("DRAFTS")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if drafts_on {
        let draft_root = dist.join("preview").join("drafts");
        copy_theme_assets(&theme_dir, &draft_root, &theme_name)?;
        let mut draft_links = render_draft_posts(
            &tera, &draft_root, &posts, &styling, &cfg, "/preview/drafts",
        )?;
        draft_links.extend(render_draft_collections(
            &tera, &draft_root, &collections, &styling, &cfg, "/preview/drafts",
        )?);
        let mut idx = String::from(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>Draft previews</title></head><body \
             style=\"font-family:system-ui,-apple-system,sans-serif;max-width:40rem;\
             margin:3rem auto;padding:0 1rem\"><h1>Draft previews</h1>\
             <p>Unpublished items rendered from the current branch. Not for production.</p><ul>",
        );
        for (path, title) in &draft_links {
            idx.push_str(&format!("<li><a href=\"{}\">{}</a></li>", path, title));
        }
        idx.push_str("</ul></body></html>");
        fs::create_dir_all(&draft_root)?;
        fs::write(draft_root.join("index.html"), idx)?;
        eprintln!(
            "draft previews built: {} items -> /preview/drafts/",
            draft_links.len()
        );
    }

    // ---- client-side plugins (analytics pixels, ad tags) ------------------
    // Declared in site.yaml `plugins:`; snippets live in plugins/<name>/.
    // Snippets are injected into the rendered HTML at BUILD time — the
    // origin never serves or executes anything. Skipped in DRAFTS=1 builds:
    // review previews must not fire tracking.
    if !drafts_on {
        let injected = render_plugins(&root, &dist)?;
        if injected > 0 {
            println!("plugins injected into {} html files", injected);
        }
    }

    let images_saved = optimize_images(&dist)?;
    if images_saved > 0 {
        println!("optimized images: {} bytes saved", images_saved);
    }
    lint_contrast(&fs::read_to_string(theme_dir.join("theme.css")).unwrap_or_default())?;
    precompress(&dist)?;

    // ---- theme previews ------------------------------------------------
    // SITE_PREVIEW_THEMES=all (or a comma list) builds the whole site once per
    // theme under dist/preview/<theme>/, so every theme is browsable without
    // flipping the live site. Interactive links are root-prefixed via {{ root }}.
    if let Some(spec) = std::env::var("SITE_PREVIEW_THEMES")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        let wanted: Vec<String> = if spec.trim() == "all" {
            available_themes(&root)
        } else {
            spec.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let preview_dir = dist.join("preview");
        let mut built: Vec<String> = Vec::new();
        for name in &wanted {
            let tdir = root.join("themes").join(name);
            if !tdir.is_dir() {
                eprintln!("preview: skip unknown theme {name:?}");
                continue;
            }
            let root_prefix = format!("/preview/{name}");
            let out = preview_dir.join(name);
            let ttera = load_tera(&root, &tdir, &templates_dir)?;
            let tcss = compile_theme_css(&root, &tdir, name)?;
            let tcss_hash = sha256_hex(tcss.as_bytes());
            let tstyle = Styling {
                css: tcss,
                hash: tcss_hash,
                theme: name.clone(),
            };
            copy_theme_assets(&tdir, &out, name)?;
            render_posts(&ttera, &out, &posts, &tstyle, &cfg, &root_prefix)?;
            render_pages(&ttera, &out, &pages, &tstyle, &cfg, &root_prefix)?;
            render_collections(&ttera, &out, &collections, &tstyle, &cfg, &root_prefix)?;
            render_home(&ttera, &out, &posts, &tstyle, &cfg, &root_prefix)?;
            render_blog(&ttera, &out, &posts, &tstyle, &cfg, &root_prefix)?;
            render_contact(&ttera, &out, &tstyle, &cfg, &root_prefix)?;
            render_404(&ttera, &out, &tstyle, &cfg, &root_prefix)?;
            built.push(name.clone());
        }
        // A lightweight picker page listing the previews.
        let mut idx = String::from(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Theme previews</title></head><body style=\"font-family:system-ui,-apple-system,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem\"><h1>Theme previews</h1><ul>",
        );
        for n in &built {
            idx.push_str(&format!("<li><a href=\"/preview/{n}/\">{n}</a></li>"));
        }
        idx.push_str("</ul><p><a href=\"/\">Back to the live site</a></p></body></html>");
        fs::create_dir_all(&preview_dir)?;
        fs::write(preview_dir.join("index.html"), idx)?;
        eprintln!(
            "theme previews built: {} -> /preview/<theme>/",
            built.join(", ")
        );
    }

    let drafts = posts.iter().filter(|p| !p.meta.published).count();
    eprintln!(
        "built dist/: {} posts ({} drafts skipped), {} pages, theme={:?}, home, blog, contact, 404, rss.xml, sitemap.xml, robots.txt, llms.txt",
        posts.len() - drafts,
        drafts,
        pages.iter().filter(|p| p.meta.published).count(),
        theme_name
    );
    Ok(())
}

/// Compile the template set: repo `templates/` plus any `<theme>/templates/*.html`
/// overrides (which win on name collision).
fn load_tera(root: &Path, theme_dir: &Path, templates_dir: &Path) -> Result<Tera> {
    let mut tera = Tera::new(
        templates_dir
            .join("**/*.html")
            .to_str()
            .context("templates path is not valid UTF-8")?,
    )
    .context("failed to compile templates")?;
    let theme_templates = theme_dir.join("templates");
    if theme_templates.is_dir() {
        for entry in fs::read_dir(&theme_templates)
            .with_context(|| format!("read_dir {}", theme_templates.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("html") {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .context("theme template file name is not valid UTF-8")?
                    .to_string();
                tera.add_template_file(&path, Some(&name))
                    .with_context(|| format!("load theme template {}", path.display()))?;
            }
        }
    }
    Ok(tera)
}

/// Compiled CSS + its content hash, threaded through every render step.
struct Styling {
    css: String,
    hash: String,
    theme: String,
}

/// Recursively copy themes/<name>/assets/ → dist/assets/<name>/.
/// Missing assets dir is fine (themes are not required to ship assets).
fn copy_theme_assets(theme_dir: &Path, dist: &Path, theme: &str) -> Result<()> {
    let src = theme_dir.join("assets");
    if !src.is_dir() {
        return Ok(());
    }
    let dst_root = dist.join("assets").join(theme);
    fn walk(src: &Path, dst: &Path) -> Result<()> {
        fs::create_dir_all(dst).with_context(|| format!("mkdir {}", dst.display()))?;
        for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                walk(&from, &to)?;
            } else {
                fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
            }
        }
        Ok(())
    }
    walk(&src, &dst_root)
}

fn read_site_theme(root: &Path) -> Option<String> {
    let raw = fs::read_to_string(root.join("site.yaml")).ok()?;
    let val: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    val.get("theme")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ---- client-side plugins (analytics pixels, ad tags) -----------------------
//
// A plugin is declared in site.yaml `plugins:` and its snippet lives in
// plugins/<name>/snippet.html. The build injects snippets into every
// rendered HTML page (before </head> or </body>) and computes a CSP meta
// tag from the domains each plugin declares — a plugin cannot phone home
// to a domain it did not declare. Assets in plugins/<name>/assets/ are
// copied to dist/assets/<name>/ like theme assets.

#[derive(Debug, Deserialize)]
struct PluginCfg {
    name: String,
    /// Where the snippet mounts: "head" (before </head>) and/or
    /// "body_end" (before </body>).
    #[serde(default)]
    mounts: Vec<String>,
    /// Extra script-src hosts this plugin needs (feeds the CSP meta tag).
    #[serde(default)]
    csp_script_src: Vec<String>,
    /// Wrap the snippet in a consent gate: it only runs after the visitor
    /// grants consent (cookie pcms_consent=granted, or a pcms:grant event).
    #[serde(default)]
    require_consent: bool,
    /// In-content mount: the template places `<div data-pcms-slot="<name>">`
    /// and the snippet is injected inside it instead of at the head/body_end
    /// mount (a form below the footer is a bug, not a feature). Falls back
    /// to the declared mount when the slot is absent from the page.
    #[serde(default)]
    anchor: Option<String>,
    /// Restrict injection to pages whose dist path contains this component
    /// (e.g. `pages: [contact]` matches dist/contact/index.html). Empty =
    /// every page. Forms and page-scoped widgets use this; consent-gated
    /// tracking stays site-wide.
    #[serde(default)]
    pages: Vec<String>,
}

fn load_plugins(root: &Path) -> Result<Vec<PluginCfg>> {
    let raw = fs::read_to_string(root.join("site.yaml"))
        .with_context(|| "read site.yaml".to_string())?;
    let val: serde_yaml::Value =
        serde_yaml::from_str(&raw).with_context(|| "parse site.yaml".to_string())?;
    let Some(list) = val.get("plugins").and_then(|v| v.as_sequence()) else {
        return Ok(Vec::new());
    };
    let mut plugins = Vec::new();
    for item in list {
        let cfg: PluginCfg = serde_yaml::from_value(item.clone())
            .with_context(|| "malformed entry in site.yaml plugins:".to_string())?;
        if cfg.name.is_empty() {
            bail!("plugins: entry with empty name");
        }
        if !cfg.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("plugin name {:?} must be [A-Za-z0-9-]+", cfg.name);
        }
        if cfg.mounts.is_empty() {
            bail!("plugin {:?} declares no mounts (head / body_end)", cfg.name);
        }
        for m in &cfg.mounts {
            if m != "head" && m != "body_end" {
                bail!("plugin {:?} has unknown mount {:?} (head / body_end)", cfg.name, m);
            }
        }
        if cfg.pages.iter().any(|p| p.trim().is_empty()) {
            bail!("plugin {:?} has an empty entry in pages:", cfg.name);
        }
        plugins.push(cfg);
    }
    Ok(plugins)
}

/// The plugin's snippet, consent-gated when require_consent is set. The gate
/// re-creates <script> nodes from the stored template (scripts inserted via
/// innerHTML never execute) and only runs once consent exists. Fixes over
/// the first gate: non-script nodes mount at the DECLARED mount (head or
/// body — a form div appended to <head> is invisible), and scripts execute
/// in document order with external src awaited (an inline `hbspt.forms...`
/// can no longer run before v2.js loads).
fn plugin_snippet(root: &Path, p: &PluginCfg, mount: &str) -> Result<String> {
    let path = root.join("plugins").join(&p.name).join("snippet.html");
    let raw = fs::read_to_string(path)
        .with_context(|| format!("plugin {:?} declared but plugins/{}/snippet.html missing", p.name, p.name))?;
    if !p.require_consent {
        return Ok(raw);
    }
    let anchor_js = match &p.anchor {
        // Mount into the template's slot element when present; else the
        // declared mount. The slot lookup runs at grant time.
        Some(a) => format!(
            "var slot=document.querySelector('[data-pcms-slot=\"{a}\"]');var host=slot||(MOUNT==='head'?document.head:document.body);",
            a = a
        ),
        None => "var host=MOUNT==='head'?document.head:document.body;".to_string(),
    };
    let esc = raw
        .replace('\\', "\\\\")
        .replace('$', "\\$") // ${...} inside a template literal would EVALUATE
        .replace('`', "\\`")
        .replace("</", "<\\/");
    Ok(format!(
        r#"<script>(function(){{
var MOUNT="{mount}";
{anchor_js}
var granted=function(){{return document.cookie.indexOf('pcms_consent=granted')!==-1}};
var go=function(){{
if(window.__pcms_{name})return;window.__pcms_{name}=1;
var t=document.createElement('template');t.innerHTML=`{esc}`;
var root=document.createElement('div');root.setAttribute('data-pcms-plugin','{name}');
Array.from(t.content.childNodes).forEach(function(n){{
if(n.nodeType===1&&n.tagName==='SCRIPT')return;
root.appendChild(n);
}});
if(root.childNodes.length)host.appendChild(root);
var scripts=Array.from(t.content.querySelectorAll('script'));var i=0;
var next=function(){{
if(i>=scripts.length)return;
var old=scripts[i++];var s=document.createElement('script');
for(var a=0;a<old.attributes.length;a++){{var at=old.attributes[a];s.setAttribute(at.name,at.value)}}
if(old.src){{s.onload=next;s.onerror=next;host.appendChild(s);}}
else{{s.text=old.text;host.appendChild(s);next();}}
}};
next();
}};
var check=function(){{if(granted())go()}};
check();window.addEventListener('pcms:grant',check);
}})();</script>
"#,
        name = p.name,
        mount = mount,
        anchor_js = anchor_js,
        esc = esc
    ))
}

/// One consent banner per build: shown only while pcms_consent is unset,
/// Accept sets the cookie for a year, dispatches pcms:grant, removes itself.
/// Without it, require_consent plugins wait forever — nothing in a static
/// site ever fires pcms:grant.
fn consent_banner() -> String {
    r#"<div id="pcms-consent-bar" style="display:none;position:fixed;left:0;right:0;bottom:0;z-index:9999;background:#111827;color:#f9fafb;font:14px system-ui,sans-serif;padding:12px 16px;justify-content:space-between;align-items:center;gap:16px">
<span>This site uses cookies for analytics and forms. Accept to enable them.</span>
<button id="pcms-consent-accept" type="button" style="background:#2563eb;color:#fff;border:0;border-radius:6px;padding:8px 16px;cursor:pointer;font:inherit">Accept</button>
</div>
<script>(function(){
var bar=document.getElementById('pcms-consent-bar');
if(!bar)return;
if(document.cookie.indexOf('pcms_consent=granted')===-1){bar.style.display='flex';}
document.getElementById('pcms-consent-accept').addEventListener('click',function(){
var d=new Date();d.setFullYear(d.getFullYear()+1);
document.cookie='pcms_consent=granted; expires='+d.toUTCString()+'; path=/';
bar.style.display='none';
window.dispatchEvent(new Event('pcms:grant'));
});
})();</script>
"#
    .to_string()
}

/// Copy plugins/<name>/assets/* to dist/assets/<name>/ (plain copies — the
/// server serves /assets with immutable caching like theme assets).
fn copy_plugin_assets(root: &Path, dist: &Path, name: &str) -> Result<()> {
    let src = root.join("plugins").join(name).join("assets");
    if !src.is_dir() {
        return Ok(());
    }
    let dst_root = dist.join("assets").join(name);
    fn walk(src: &Path, dst: &Path) -> Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                walk(&from, &to)?;
            } else {
                fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }
    walk(&src, &dst_root)
}

/// Inject all declared plugins into rendered HTML files in dist/. Plugins
/// with a `pages:` list only inject on matching pages (path component match);
/// the consent banner ships once per page when any active plugin requires
/// consent. Returns the number of files touched.
fn render_plugins(root: &Path, dist: &Path) -> Result<usize> {
    let plugins = load_plugins(root)?;
    if plugins.is_empty() {
        return Ok(0);
    }
    for p in &plugins {
        copy_plugin_assets(root, dist, &p.name)?;
    }
    let any_consent = plugins.iter().any(|p| p.require_consent);

    fn plugin_applies(p: &PluginCfg, rel: &Path) -> bool {
        if p.pages.is_empty() {
            return true;
        }
        let comps: Vec<String> = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
            .collect();
        p.pages.iter().any(|pg| {
            comps.iter().any(|c| c == pg)
                // dist/index.html is the home page
                || (pg == "home" && comps.len() == 1 && comps[0] == "index.html")
        })
    }

    let mut touched = 0usize;
    fn inject_one(
        root: &Path,
        path: &Path,
        rel: &Path,
        plugins: &[PluginCfg],
        any_consent: bool,
        touched: &mut usize,
    ) -> Result<()> {
        let applicable: Vec<&PluginCfg> =
            plugins.iter().filter(|p| plugin_applies(p, rel)).collect();
        let mut head_parts: Vec<String> = Vec::new();
        let mut end_parts: Vec<String> = Vec::new();
        // (anchor name, declared mount, snippet) for UNGATED plugins — they
        // are string-inserted inside the template's slot element.
        let mut anchored_parts: Vec<(String, String, String)> = Vec::new();
        let mut domains: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for p in &applicable {
            for m in &p.mounts {
                let snippet = plugin_snippet(root, p, m)?;
                match (&p.anchor, p.require_consent) {
                    // Gated plugins keep the mount position: their gate JS
                    // looks the slot up in the DOM at grant time.
                    (Some(a), false) => anchored_parts.push((a.clone(), m.clone(), snippet)),
                    _ => match m.as_str() {
                        "head" => head_parts.push(snippet),
                        "body_end" => end_parts.push(snippet),
                        _ => unreachable!("mounts validated in load_plugins"),
                    },
                }
            }
            domains.extend(p.csp_script_src.iter().cloned());
        }
        let consent = applicable.iter().any(|p| p.require_consent) && any_consent;
        if head_parts.is_empty()
            && end_parts.is_empty()
            && anchored_parts.is_empty()
            && domains.is_empty()
            && !consent
        {
            return Ok(());
        }
        let html = fs::read_to_string(path)?;
        let mut out = html.clone();
        // Ungated anchored plugins: inject inside the template's slot
        // element (after its opening tag). Missing slot → declared mount.
        for (a, m, snip) in &anchored_parts {
            let needle = format!("data-pcms-slot=\"{}\"", a);
            let at = out
                .find(&needle)
                .and_then(|i| out[i..].find('>').map(|g| i + g + 1));
            match at {
                Some(ins) => out.insert_str(ins, snip),
                None => {
                    if m == "head" {
                        head_parts.push(snip.clone());
                    } else {
                        end_parts.push(snip.clone());
                    }
                }
            }
        }
        if !domains.is_empty() {
            let mut list: Vec<&str> = domains.iter().map(|s| s.as_str()).collect();
            list.sort();
            let csp = format!(
                "<meta http-equiv=\"Content-Security-Policy\" content=\"script-src 'self' 'unsafe-inline' {}\">\n",
                list.join(" ")
            );
            if let Some(i) = out.find("<head>") {
                out.insert_str(i + 6, &csp);
            }
        }
        let banner = if consent { consent_banner() } else { String::new() };
        for (parts, anchor) in [
            (&head_parts, "</head>"),
            (&end_parts, "</body>"),
        ] {
            if parts.is_empty() {
                continue;
            }
            let mut block =
                String::from("\n<!-- plugins: build-time injection (site.yaml plugins:) -->\n");
            for p in parts {
                block.push_str(p);
                block.push('\n');
            }
            if !banner.is_empty() {
                block.push_str(&banner);
            }
            if let Some(i) = out.find(anchor) {
                out.insert_str(i, &block);
            }
        }
        if out != html {
            fs::write(path, out)?;
            *touched += 1;
        }
        Ok(())
    }

    fn walk(root: &Path, dir: &Path, base: &Path, plugins: &[PluginCfg], any_consent: bool,
            touched: &mut usize) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, base, plugins, any_consent, touched)?;
            } else if path.extension().and_then(|e| e.to_str()) == Some("html") {
                let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                inject_one(root, &path, &rel, plugins, any_consent, touched)?;
            }
        }
        Ok(())
    }

    walk(root, dist, dist, &plugins, any_consent, &mut touched)?;
    Ok(touched)
}

fn available_themes(root: &Path) -> Vec<String> {
    let dir = root.join("themes");
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// Compile the theme CSS with the Tailwind CLI (`@import "tailwindcss"` +
/// @theme tokens + component layers). Falls back to inlining the raw CSS if
/// the CLI is missing or fails, so a build never hard-fails on styling.
fn compile_theme_css(root: &Path, theme_dir: &Path, theme: &str) -> Result<String> {
    let input = theme_dir.join("theme.css");
    let raw = fs::read_to_string(&input)
        .with_context(|| format!("read {}", input.display()))?;
    let output = std::env::temp_dir().join(format!("postcms-theme-{}.css", theme));
    let res = std::process::Command::new("tailwindcss")
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--minify")
        .current_dir(root) // v4 auto-detects template content from cwd
        .output();
    match res {
        Ok(o) if o.status.success() && output.exists() => {
            let compiled = fs::read_to_string(&output)
                .with_context(|| format!("read {}", output.display()))?;
            if compiled.trim().is_empty() {
                eprintln!("warning: tailwindcss produced empty CSS; inlining raw theme.css");
                return Ok(raw);
            }
            eprintln!(
                "theme {:?}: tailwindcss minified {} → {} bytes",
                theme,
                raw.len(),
                compiled.len()
            );
            Ok(compiled)
        }
        _ => {
            let why = match res {
                Ok(o) => format!("exit {}", o.status),
                Err(e) => format!("{e}"),
            };
            eprintln!(
                "warning: tailwindcss CLI unavailable ({why}); inlining raw theme.css"
            );
            Ok(raw)
        }
    }
}

fn repo_root() -> Result<PathBuf> {
    // SITE_ROOT wins (containers and CI without a .git). Otherwise the
    // runtime repo root (git toplevel of the current directory), so builds in
    // tenant worktrees, or a separate site repo, use that repo's content.
    // Falls back to the compile-time manifest dir only outside a git repo.
    if let Ok(dir) = std::env::var("SITE_ROOT") {
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
    // crates/build → repo root
    let root = Path::new(manifest)
        .parent()
        .and_then(|p| p.parent())
        .context("cannot locate repo root from CARGO_MANIFEST_DIR")?;
    Ok(root.to_path_buf())
}

fn load_posts(dir: &Path) -> Result<Vec<Post>> {
    let mut posts = Vec::new();
    let entries =
        fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("entry in {}", dir.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.yaml");
        let md_path = path.join("index.mdx");
        if !meta_path.exists() || !md_path.exists() {
            eprintln!("skipping {} (missing meta.yaml or index.mdx)", path.display());
            continue;
        }
        let raw = fs::read_to_string(&meta_path)
            .with_context(|| format!("read {}", meta_path.display()))?;
        let meta: Meta = serde_yaml::from_str(&raw)
            .with_context(|| format!("parse {}", meta_path.display()))?;
        posts.push(Post { meta, dir: path });
    }
    // date desc; tie-break by slug for determinism
    posts.sort_by(|a, b| {
        b.meta
            .date
            .cmp(&a.meta.date)
            .then_with(|| a.meta.slug.cmp(&b.meta.slug))
    });
    Ok(posts)
}

/// A cross-item reference to validate after all collections load:
/// `<at>` names a meta.yaml whose `<field>` must be an item of `<target>`.
struct RefCheck {
    at: String,
    field: String,
    target: String,
    value: String,
}

/// Load typed collections: every directory under content/ other than posts/
/// pages/ is a collection if it has a schema.yaml. Each item is
/// content/<collection>/<slug>/ with meta.yaml + index.mdx. Required fields
/// (and optional type checks) from schema.yaml are enforced at build time —
/// a missing or wrongly-typed field fails the build naming the exact file.
/// Returns (collections, deferred reference checks).
fn load_collections(content_dir: &Path) -> Result<(Vec<Collection>, Vec<RefCheck>)> {
    let mut ref_checks: Vec<RefCheck> = Vec::new();
    let mut collections = Vec::new();
    let entries = fs::read_dir(content_dir)
        .with_context(|| format!("read_dir {}", content_dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("entry in {}", content_dir.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_dir() || name.starts_with('.') || name == "posts" || name == "pages" {
            continue;
        }
        let schema_path = path.join("schema.yaml");
        if !schema_path.exists() {
            continue; // plain folder, not a typed collection
        }
        let schema_raw = fs::read_to_string(&schema_path)
            .with_context(|| format!("read {}", schema_path.display()))?;
        let schema: serde_yaml::Value = serde_yaml::from_str(&schema_raw)
            .with_context(|| format!("parse {}", schema_path.display()))?;
        let title = schema
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(name)
            .to_string();
        let required: Vec<String> = schema
            .get("required")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let types: std::collections::HashMap<String, String> = schema
            .get("types")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        Some((k.as_str()?.to_string(), v.as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // enums: {field: [allowed values]} — the value (or each list item)
        // must be one of the declared set.
        let enums: std::collections::HashMap<String, Vec<String>> = schema
            .get("enums")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let field = k.as_str()?.to_string();
                        let allowed: Vec<String> = v
                            .as_sequence()?
                            .iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect();
                        Some((field, allowed))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // formats: {field: url|date|email} — shape checks beyond types.
        let formats: std::collections::HashMap<String, String> = schema
            .get("formats")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        Some((k.as_str()?.to_string(), v.as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // references: {field: collection} — the value names an item in
        // another typed collection (cross-item integrity). Validated after
        // all collections load, since the target may be defined later.
        let references: std::collections::HashMap<String, String> = schema
            .get("references")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        Some((k.as_str()?.to_string(), v.as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut items = Vec::new();
        let item_template = schema
            .get("item_template")
            .and_then(|v| v.as_str())
            .unwrap_or("collection-item.html")
            .to_string();
        let index_template = schema
            .get("index_template")
            .and_then(|v| v.as_str())
            .unwrap_or("collection.html")
            .to_string();
        let url_prefix = schema
            .get("url_prefix")
            .and_then(|v| v.as_str())
            .map(|p| format!("/{}", p.trim_matches('/')))
            .unwrap_or_else(|| format!("/{name}"));
        let index = schema.get("index").and_then(|v| v.as_bool()).unwrap_or(true);
        for item_entry in fs::read_dir(&path)
            .with_context(|| format!("read_dir {}", path.display()))?
        {
            let item_path = item_entry
                .with_context(|| format!("entry in {}", path.display()))?
                .path();
            if !item_path.is_dir() {
                continue;
            }
            let meta_path = item_path.join("meta.yaml");
            if !meta_path.exists() {
                continue; // non-item dirs inside a collection are ignored
            }
            let raw = fs::read_to_string(&meta_path)
                .with_context(|| format!("read {}", meta_path.display()))?;
            let meta: serde_yaml::Value = serde_yaml::from_str(&raw)
                .with_context(|| format!("parse {}", meta_path.display()))?;
            let map = meta
                .as_mapping()
                .ok_or_else(|| anyhow::anyhow!("{}: meta.yaml must be a mapping", meta_path.display()))?
                .clone();

            // ---- schema validation (the "typed" part) ----
            for field in &required {
                let val = map
                    .get(serde_yaml::Value::String(field.clone()))
                    .unwrap_or(&serde_yaml::Value::Null);
                let missing = match val {
                    serde_yaml::Value::Null => true,
                    serde_yaml::Value::String(s) => s.trim().is_empty(),
                    serde_yaml::Value::Sequence(s) => s.is_empty(),
                    _ => false,
                };
                if missing {
                    bail!(
                        "{}: required field `{}` is missing or empty (schema: {})",
                        meta_path.display(),
                        field,
                        schema_path.display()
                    );
                }
            }
            if let Some(ty) = types.get("published") {
                let _ = ty; // types map only covers custom fields below
            }
            for (field, ty) in &types {
                let val = map.get(serde_yaml::Value::String(field.clone()));
                if let Some(v) = val {
                    let ok = match ty.as_str() {
                        "string" => matches!(v, serde_yaml::Value::String(_)),
                        "number" => matches!(v, serde_yaml::Value::Number(_)),
                        "boolean" => matches!(v, serde_yaml::Value::Bool(_)),
                        "list" => matches!(v, serde_yaml::Value::Sequence(_)),
                        other => bail!(
                            "{}: schema type `{other}` for field `{field}` is not one of string|number|boolean|list",
                            schema_path.display()
                        ),
                    };
                    if !ok {
                        bail!(
                            "{}: field `{}` must be of type `{}`",
                            meta_path.display(),
                            field,
                            ty
                        );
                    }
                }
            }
            // ---- enums: value (or every list item) must be declared ----
            for (field, allowed) in &enums {
                let val = map.get(serde_yaml::Value::String(field.clone()));
                let Some(v) = val else { continue };
                let check_one = |item: &serde_yaml::Value, at: &str| -> Result<()> {
                    let s = item.as_str().unwrap_or_default();
                    if allowed.iter().any(|a| a == s) {
                        Ok(())
                    } else {
                        bail!(
                            "{}: field `{}` value `{}` is not one of {:?} (schema: {})",
                            at,
                            field,
                            s,
                            allowed,
                            schema_path.display()
                        );
                    }
                };
                match v {
                    serde_yaml::Value::String(_) => check_one(v, &meta_path.display().to_string())?,
                    serde_yaml::Value::Sequence(seq) => {
                        for item in seq {
                            check_one(item, &meta_path.display().to_string())?;
                        }
                    }
                    _ => bail!(
                        "{}: field `{}` must be a string or list for enum checking",
                        meta_path.display(),
                        field
                    ),
                }
            }
            // ---- formats: url | date | email ----
            for (field, format) in &formats {
                let val = map.get(serde_yaml::Value::String(field.clone()));
                let Some(serde_yaml::Value::String(s)) = val else {
                    continue;
                };
                let ok = match format.as_str() {
                    "url" => {
                        let after_scheme = s
                            .strip_prefix("https://")
                            .or_else(|| s.strip_prefix("http://"));
                        match after_scheme {
                            Some(rest) => {
                                let host = rest.split(['/', '?', '#']).next().unwrap_or("");
                                host.contains('.') && !host.starts_with('.') && !host.ends_with('.')
                            }
                            None => false,
                        }
                    }
                    "date" => s.parse::<NaiveDate>().is_ok(),
                    "email" => {
                        let (local, domain) = match s.split_once('@') {
                            Some(x) => x,
                            None => ("", ""),
                        };
                        !local.is_empty()
                            && domain.contains('.')
                            && !domain.starts_with('.')
                            && !domain.ends_with('.')
                            && !domain.contains('@')
                    }
                    other => bail!(
                        "{}: unknown format `{other}` for field `{}` (url|date|email)",
                        schema_path.display(),
                        field
                    ),
                };
                if !ok {
                    bail!(
                        "{}: field `{}` value `{}` is not a valid {format} (schema: {})",
                        meta_path.display(),
                        field,
                        s,
                        schema_path.display()
                    );
                }
            }
            // ---- references: defer to the cross-collection pass ----
            for (field, target) in &references {
                let val = map.get(serde_yaml::Value::String(field.clone()));
                let Some(v) = val else { continue };
                let mut push = |value: &str| {
                    ref_checks.push(RefCheck {
                        at: meta_path.display().to_string(),
                        field: field.clone(),
                        target: target.clone(),
                        value: value.to_string(),
                    });
                };
                match v {
                    serde_yaml::Value::String(s) => push(s),
                    serde_yaml::Value::Sequence(seq) => {
                        for item in seq {
                            if let Some(s) = item.as_str() {
                                push(s);
                            }
                        }
                    }
                    _ => bail!(
                        "{}: field `{}` must be a string or list for reference checking",
                        meta_path.display(),
                        field
                    ),
                }
            }

            let slug = item_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let get_str = |k: &str| -> String {
                map.get(serde_yaml::Value::String(k.to_string()))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let date = map
                .get(serde_yaml::Value::String("date".to_string()))
                .and_then(|v| v.as_str())
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
            // Every meta key flows to the template as `fields.<key>`.
            let mut fields = serde_json::Map::new();
            for (k, v) in &map {
                let key = k.as_str().unwrap_or_default().to_string();
                let json_v = match v {
                    serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
                    serde_yaml::Value::Number(n) => {
                        serde_json::Value::Number(serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0)).unwrap_or(serde_json::Number::from(0)))
                    }
                    serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
                    serde_yaml::Value::Sequence(seq) => serde_json::Value::Array(
                        seq.iter()
                            .filter_map(|x| x.as_str().map(|s| serde_json::Value::String(s.to_string())))
                            .collect(),
                    ),
                    _ => serde_json::Value::Null,
                };
                fields.insert(key, json_v);
            }
            items.push(CollectionItem {
                slug,
                title: get_str("title"),
                description: get_str("description"),
                date,
                published: map
                    .get(serde_yaml::Value::String("published".to_string()))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                fields,
                dir: item_path,
            });
        }
        items.sort_by(|a, b| a.slug.cmp(&b.slug));
        collections.push(Collection {
            name: name.to_string(),
            title,
            required,
            types,
            item_template,
            index_template,
            url_prefix,
            index,
            items,
        });
    }
    collections.sort_by(|a, b| a.name.cmp(&b.name));
    // ---- cross-collection reference validation ----
    // Every reference must name an item that actually exists in the target
    // collection. A dangling reference is a broken page waiting to happen,
    // so it fails the build naming the exact file.
    if !ref_checks.is_empty() {
        let slugs_by_collection: std::collections::HashMap<&str, Vec<&str>> = collections
            .iter()
            .map(|c| (c.name.as_str(), c.items.iter().map(|i| i.slug.as_str()).collect()))
            .collect();
        for r in &ref_checks {
            let known = slugs_by_collection.get(r.target.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: field `{}` references collection `{}` which does not exist",
                    r.at,
                    r.field,
                    r.target
                )
            })?;
            if !known.contains(&r.value.as_str()) {
                bail!(
                    "{}: field `{}` references `{}` — no such item in collection `{}` (known: {:?})",
                    r.at,
                    r.field,
                    r.value,
                    r.target,
                    known
                );
            }
        }
    }
    Ok((collections, ref_checks))
}

/// Render preview copies under dist/preview/drafts/ so a reviewer can see
/// the page as it will appear, with the proposal's agent_notes embedded.
/// All items are rendered (not just unpublished) so a publish-on-approve
/// proposal is reviewable too. Only runs when DRAFTS=1 — deploys always
/// build without it, and ci/deploy.sh excludes preview/ from the rsync.
fn render_draft_posts(
    tera: &Tera,
    out_root: &Path,
    posts: &[Post],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<Vec<(String, String)>> {
    let mut listed = Vec::new();
    for post in posts.iter() {
        let md_path = post.dir.join("index.mdx");
        let body = if md_path.exists() {
            render_markdown(&fs::read_to_string(&md_path).with_context(|| {
                format!("read {}", md_path.display())
            })?)?
        } else {
            String::new()
        };
        let mut ctx = post.meta.to_context(&body);
        ctx.insert("draft", &true);
        ctx.extend(cfg.base_ctx(style, root));
        let html = tera
            .render("post.html", &ctx)
            .with_context(|| format!("render draft post {}", post.meta.slug))?;
        let out_dir = out_root.join("posts").join(&post.meta.slug);
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;
        fs::write(out_dir.join("index.html"), html)
            .with_context(|| format!("write draft {}/index.html", out_dir.display()))?;
        let title = post.meta.title.clone();
        listed.push((format!("/preview/drafts/posts/{}/", post.meta.slug), title));
    }
    Ok(listed)
}

/// Draft items of typed collections, at the collection's schema-selected
/// template. agent_notes travels in the item's fields map (pass-through), so
/// surface it explicitly and keep it out of the field list via the template.
fn render_draft_collections(
    tera: &Tera,
    out_root: &Path,
    collections: &[Collection],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<Vec<(String, String)>> {
    let mut listed = Vec::new();
    for coll in collections {
        for item in coll.items.iter() {
            let md_path = item.dir.join("index.mdx");
            let body = if md_path.exists() {
                render_markdown(&fs::read_to_string(&md_path).with_context(|| {
                    format!("read {}", md_path.display())
                })?)?
            } else {
                String::new()
            };
            let mut ctx = TeraContext::new();
            ctx.insert("title", &item.title);
            ctx.insert("slug", &item.slug);
            ctx.insert("description", &item.description);
            if let Some(d) = item.date {
                ctx.insert("date", &d.to_string());
            }
            ctx.insert("fields", &item.fields);
            if let Some(serde_json::Value::String(notes)) =
                item.fields.get("agent_notes").cloned()
            {
                ctx.insert("agent_notes", &notes);
            }
            ctx.insert("body", &body);
            ctx.insert("draft", &true);
            ctx.extend(cfg.base_ctx(style, root));
            let html = tera
                .render(&coll.item_template, &ctx)
                .with_context(|| format!("render draft {} item {}", coll.name, item.slug))?;
            let out_dir = out_root.join(&coll.name).join(&item.slug);
            fs::create_dir_all(&out_dir)
                .with_context(|| format!("mkdir {}", out_dir.display()))?;
            fs::write(out_dir.join("index.html"), html)
                .with_context(|| format!("write draft {}/index.html", out_dir.display()))?;
            listed.push((
                format!("/preview/drafts/{}/{}/", coll.name, item.slug),
                item.title.clone(),
            ));
        }
    }
    Ok(listed)
}

/// Render typed collections: per-item pages at /<collection>/<slug>/ and a
/// collection index at /<collection>/. Draft items (published: false, the
/// same convention as posts) are invisible on the site.
fn render_collections(
    tera: &Tera,
    dist: &Path,
    collections: &[Collection],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    for coll in collections {
        // Item pages first; the index lists the published ones.
        let mut listed: Vec<serde_json::Value> = Vec::new();
        // Fail fast with a clear name when a schema points at a missing template.
        for (tpl, kind) in [(&coll.item_template, "item"), (&coll.index_template, "index")] {
            if kind == "index" && !coll.index {
                continue;
            }
            if !tera.templates.contains_key(tpl) {
                bail!(
                    "collection `{}`: {} template `{}` not found (templates/ or theme overrides)",
                    coll.name,
                    kind,
                    tpl
                );
            }
        }
        for item in coll.items.iter().filter(|i| i.published) {
            let md_path = item.dir.join("index.mdx");
            let body = if md_path.exists() {
                render_markdown(&fs::read_to_string(&md_path).with_context(|| {
                    format!("read {}", md_path.display())
                })?)?
            } else {
                String::new()
            };
            let mut ctx = TeraContext::new();
            ctx.insert("title", &item.title);
            ctx.insert("slug", &item.slug);
            ctx.insert("description", &item.description);
            if let Some(d) = item.date {
                ctx.insert("date", &d.to_string());
            }
            ctx.insert("fields", &item.fields);
            ctx.insert("body", &body);
            ctx.insert("url", &page_url(&coll.item_path(&item.slug)));
            ctx.extend(cfg.base_ctx(style, root));
            let html = tera
                .render(&coll.item_template, &ctx)
                .with_context(|| format!("render {} for {}", coll.item_template, item.slug))?;
            let out_dir = out_dir_for(dist, &coll.item_path(&item.slug));
            fs::create_dir_all(&out_dir)
                .with_context(|| format!("mkdir {}", out_dir.display()))?;
            fs::write(out_dir.join("index.html"), html)
                .with_context(|| format!("write {}/index.html", out_dir.display()))?;

            let mut row = serde_json::Map::new();
            for (k, v) in &item.fields {
                row.insert(k.clone(), v.clone());
            }
            listed.push(serde_json::Value::Object(row));
        }
        if !coll.index {
            continue;
        }
        let mut ctx = TeraContext::new();
        ctx.insert("coll_name", &coll.name);
        ctx.insert("coll_title", &coll.title);
        ctx.insert("items", &listed);
        ctx.insert("url", &page_url(&coll.url_prefix));
        ctx.extend(cfg.base_ctx(style, root));
        let html = tera
            .render(&coll.index_template, &ctx)
            .with_context(|| format!("render {} for {}", coll.index_template, coll.name))?;
        let out_dir = out_dir_for(dist, &coll.url_prefix);
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;
        fs::write(out_dir.join("index.html"), html)
            .with_context(|| format!("write {}/index.html", out_dir.display()))?;
    }
    Ok(())
}

fn render_markdown(input: &str) -> Result<String> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(input, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    Ok(out)
}

/// Strip markdown to plain text for the search index: keep text and code
/// spans, break on paragraph/heading ends.
fn markdown_to_text(input: &str) -> String {
    use pulldown_cmark::{Event, TagEnd};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut out = String::new();
    for ev in Parser::new_ext(input, opts) {
        match ev {
            Event::Text(t) => out.push_str(&t),
            Event::Code(c) => out.push_str(&c),
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::Heading(_)) => out.push('\n'),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out.chars().take(6_000).collect()
}

/// Emit search-index.json: one row per published post with title, description,
/// url, date, tags and plain-text body — consumed lazily by the client search
/// widget (Tier 1) and by /api/ask (Tier 2).
fn render_search_index(
    root: &Path,
    dist: &Path,
    posts: &[Post],
    collections: &[Collection],
) -> Result<()> {
    let site_base = site_base_url(root);
    let mut idx = Vec::new();
    for post in posts {
        if !post.meta.published {
            continue;
        }
        let md = fs::read_to_string(post.dir.join("index.mdx"))
            .with_context(|| format!("read {}", post.dir.join("index.mdx").display()))?;
        idx.push(serde_json::json!({
            "title": post.meta.title,
            "description": post.meta.description,
            "url": format!("{}{}", site_base, post.index_url()),
            "date": post.meta.date.to_string(),
            "tags": post.meta.tags,
            "body": markdown_to_text(&md),
        }));
    }
    // Typed-collection items are searchable too — /api/ask must be able to
    // cite them like any other page.
    for coll in collections {
        for item in coll.items.iter().filter(|i| i.published) {
            let md = fs::read_to_string(item.dir.join("index.mdx")).unwrap_or_default();
            idx.push(serde_json::json!({
                "title": item.title,
                "description": item.description,
                "url": format!("{}{}", site_base, page_url(&coll.item_path(&item.slug))),
                "date": item.date.map(|d| d.to_string()).unwrap_or_default(),
                "tags": [],
                "body": markdown_to_text(&md),
            }));
        }
    }
    fs::write(dist.join("search-index.json"), serde_json::to_vec_pretty(&idx)?)
        .with_context(|| "write search-index.json".to_string())?;
    Ok(())
}

/// Tier 3: embed per-post chunks at build time into dist/embeddings.json.
/// Uses fastembed (ONNX on CPU, weights cached in ~/.cache) — no API key.
/// Skips gracefully when the model can't be fetched (offline), leaving
/// /api/ask in Tier-2 inline mode.
fn render_embeddings(
    root: &Path,
    dist: &Path,
    posts: &[Post],
    collections: &[Collection],
) -> Result<()> {
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let live_items: Vec<(&Collection, &CollectionItem)> = collections
        .iter()
        .flat_map(|c| c.items.iter().filter(|i| i.published).map(move |i| (c, i)))
        .collect();
    if published.is_empty() && live_items.is_empty() {
        return Ok(());
    }

    let opts = InitOptions::new(EmbeddingModel::BGESmallENV15)
        .with_cache_dir(embed_cache_dir())
        .with_show_download_progress(true);
    let mut model = match TextEmbedding::try_new(opts) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "embeddings: model unavailable ({e}); skipping dist/embeddings.json (Tier 2 inline mode still works)"
            );
            return Ok(());
        }
    };

    let site_base = site_base_url(root);
    let mut rows: Vec<(serde_json::Value, String)> = Vec::new();
    for post in &published {
        let md = fs::read_to_string(post.dir.join("index.mdx"))
            .with_context(|| format!("read {}", post.dir.join("index.mdx").display()))?;
        let text = markdown_to_text(&md);
        let title = &post.meta.title;
        let desc = &post.meta.description;
        for chunk in chunk_text(&text, 700) {
            if chunk.chars().count() < 80 {
                continue;
            }
            rows.push((
                serde_json::json!({
                    "title": title,
                    "url": format!("{}{}", site_base, post.index_url()),
                }),
                format!("# {title} — {desc}\n\n{chunk}"),
            ));
        }
    }
    // Collection items get the same treatment (published only).
    for (coll, item) in &live_items {
        let md = fs::read_to_string(item.dir.join("index.mdx")).unwrap_or_default();
        let text = markdown_to_text(&md);
        let title = &item.title;
        let desc = &item.description;
        for chunk in chunk_text(&text, 700) {
            if chunk.chars().count() < 80 {
                continue;
            }
            rows.push((
                serde_json::json!({
                    "title": title,
                    "url": format!("{}{}", site_base, page_url(&coll.item_path(&item.slug))),
                }),
                format!("# {title} — {desc}\n\n{chunk}"),
            ));
        }
    }
    if rows.is_empty() {
        return Ok(());
    }

    let docs: Vec<String> = rows.iter().map(|(_, c)| c.clone()).collect();
    let vecs = model
        .embed(docs, None)
        .map_err(|e| anyhow::anyhow!("embed chunks: {e}"))?;

    let mut out = Vec::with_capacity(rows.len());
    for ((meta, text), vec) in rows.into_iter().zip(vecs) {
        out.push(serde_json::json!({ "meta": meta, "text": text, "vec": vec }));
    }
    fs::write(dist.join("embeddings.json"), serde_json::to_vec(&out)?)
        .with_context(|| "write embeddings.json".to_string())?;
    println!("embedded {} chunks -> embeddings.json", out.len());
    Ok(())
}

/// Split plain text into word-boundary chunks of at most `max_chars`.
fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.chars().count() + word.chars().count() + 1 > max_chars {
            if !cur.trim().is_empty() {
                chunks.push(cur.trim().to_string());
            }
            cur.clear();
        }
        cur.push_str(word);
        cur.push(' ');
    }
    if !cur.trim().is_empty() {
        chunks.push(cur.trim().to_string());
    }
    chunks
}

/// Where fastembed caches downloaded ONNX weights (shared by build + server).
fn embed_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".cache").join("post-cms-fastembed")
}

/// Emit per-post markdown next to each rendered post (posts/<slug>/index.md),
/// so agents fetching a page get text, not DOM (Tier 2).
fn render_posts_markdown(dist: &Path, posts: &[Post]) -> Result<()> {
    for post in posts {
        if !post.meta.published {
            continue;
        }
        let md = fs::read_to_string(post.dir.join("index.mdx"))
            .with_context(|| format!("read {}", post.dir.join("index.mdx").display()))?;
        let head = format!("# {}\n\n> {}\n\n", post.meta.title, post.meta.description);
        fs::write(dist.join("posts").join(&post.meta.slug).join("index.md"), head + &md)
            .with_context(|| format!("write {}/index.md", post.meta.slug))?;
    }
    Ok(())
}

fn render_posts(
    tera: &Tera,
    dist: &Path,
    posts: &[Post],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    for post in posts {
        // Drafts are invisible on the site: no HTML page is emitted until
        // published flips to true (see agents/CONVENTIONS.md).
        if !post.meta.published {
            continue;
        }
        let md = fs::read_to_string(post.dir.join("index.mdx"))
            .with_context(|| format!("read {}", post.dir.join("index.mdx").display()))?;
        let body = render_markdown(&md)?;
        let mut ctx = post.meta.to_context(&body);
        ctx.extend(cfg.base_ctx(style, root));
        let html = tera.render("post.html", &ctx).context("render post.html")?;
        let out_dir = dist.join("posts").join(&post.meta.slug);
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;
        fs::write(out_dir.join("index.html"), html)
            .with_context(|| format!("write {}/index.html", out_dir.display()))?;
    }
    Ok(())
}

/// Generic content pages from content/pages/<slug>/ → /<slug>/.
fn render_pages(
    tera: &Tera,
    dist: &Path,
    pages: &[Page],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    for page in pages {
        if !page.meta.published {
            continue;
        }
        let md = fs::read_to_string(page.dir.join("index.mdx"))
            .with_context(|| format!("read {}/index.mdx", page.dir.display()))?;
        let body = render_markdown(&md)?;
        let mut ctx = page.meta.to_context(&body);
        ctx.extend(cfg.base_ctx(style, root));
        let tpl = page.meta.template.as_deref().unwrap_or("page.html");
        let html = tera.render(tpl, &ctx).with_context(|| format!("render {tpl}"))?;
        let out_dir = out_dir_for(dist, &page.meta.site_path());
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;
        fs::write(out_dir.join("index.html"), html)
            .with_context(|| format!("write {}/index.html", out_dir.display()))?;
    }
    Ok(())
}

/// Landing page at / — hero (site title + tagline) plus the 3 newest posts.
fn render_home(
    tera: &Tera,
    dist: &Path,
    posts: &[Post],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    let recent: Vec<serde_json::Value> = posts
        .iter()
        .filter(|p| p.meta.published)
        .take(3)
        .map(Post::summary)
        .collect();
    let mut ctx = cfg.base_ctx(style, root);
    ctx.insert("posts", &recent);
    ctx.insert("tagline", &cfg.tagline);
    let html = tera.render("home.html", &ctx).context("render home.html")?;
    fs::write(dist.join("index.html"), html).context("write index.html")?;
    Ok(())
}

/// Post list at /blog/ — the old index, relocated.
fn render_blog(
    tera: &Tera,
    dist: &Path,
    posts: &[Post],
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    let listed: Vec<serde_json::Value> = posts
        .iter()
        .filter(|p| p.meta.published)
        .map(Post::summary)
        .collect();
    let mut ctx = cfg.base_ctx(style, root);
    ctx.insert("posts", &listed);
    let html = tera.render("blog.html", &ctx).context("render blog.html")?;
    let out_dir = dist.join("blog");
    fs::create_dir_all(&out_dir).context("mkdir blog/")?;
    fs::write(out_dir.join("index.html"), html).context("write blog/index.html")?;
    Ok(())
}

/// Contact page — template-driven from site.yaml `contact_email` (static-first:
/// a mailto link, no backend form to run).
fn render_contact(
    tera: &Tera,
    dist: &Path,
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    let mut ctx = cfg.base_ctx(style, root);
    ctx.insert("contact_email", &cfg.contact_email);
    let html = tera
        .render("contact.html", &ctx)
        .context("render contact.html")?;
    let out_dir = dist.join("contact");
    fs::create_dir_all(&out_dir).context("mkdir contact/")?;
    fs::write(out_dir.join("index.html"), html).context("write contact/index.html")?;
    Ok(())
}

/// Load content/pages/<slug>/{meta.yaml,index.mdx}. Missing dir → empty vec.
fn load_pages(dir: &Path) -> Result<Vec<Page>> {
    let mut pages = Vec::new();
    if !dir.is_dir() {
        return Ok(pages);
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    for path in entries {
        let meta_path = path.join("meta.yaml");
        let md_path = path.join("index.mdx");
        if !meta_path.exists() || !md_path.exists() {
            eprintln!("skipping page {} (missing meta.yaml or index.mdx)", path.display());
            continue;
        }
        let raw = fs::read_to_string(&meta_path)
            .with_context(|| format!("read {}", meta_path.display()))?;
        let meta: PageMeta =
            serde_yaml::from_str(&raw).with_context(|| format!("parse {}", meta_path.display()))?;
        pages.push(Page { meta, dir: path });
    }
    Ok(pages)
}

fn render_404(
    tera: &Tera,
    dist: &Path,
    style: &Styling,
    cfg: &SiteConfig,
    root: &str,
) -> Result<()> {
    let ctx = cfg.base_ctx(style, root);
    let html = tera.render("404.html", &ctx).context("render 404.html")?;
    fs::write(dist.join("404.html"), html).context("write 404.html")?;
    Ok(())
}

fn render_rss(root: &Path, dist: &Path, posts: &[Post]) -> Result<()> {
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let site_base = site_base_url(root);
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<rss version=\"2.0\">\n<channel>\n");
    xml.push_str(&format!(
        "<title>Post-CMS</title>\n<link>{}</link>\n<description>Git-based publishing stack</description>\n",
        site_base
    ));
    // RFC 822 date of last build
    let now = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    xml.push_str(&format!("<lastBuildDate>{}</lastBuildDate>\n", now));
    for post in published {
        xml.push_str("<item>\n");
        xml.push_str(&format!("<title>{}</title>\n", escape_xml(&post.meta.title)));
        xml.push_str(&format!("<link>{}</link>\n", format!("{}{}", site_base, post.url_path())));
        xml.push_str(&format!(
            "<guid>{}</guid>\n",
            format!("{}{}", site_base, post.url_path())
        ));
        xml.push_str(&format!(
            "<pubDate>{}</pubDate>\n",
            rfc2822_date(&post.meta.date)
        ));
        xml.push_str(&format!(
            "<description>{}</description>\n",
            escape_xml(&post.meta.description)
        ));
        xml.push_str("</item>\n");
    }
    xml.push_str("</channel>\n</rss>\n");
    fs::write(dist.join("rss.xml"), xml).context("write rss.xml")?;
    Ok(())
}

fn render_sitemap(
    root: &Path,
    dist: &Path,
    posts: &[Post],
    pages: &[Page],
    collections: &[Collection],
    builtin_pages: bool,
) -> Result<()> {
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let live_pages: Vec<&Page> = pages.iter().filter(|p| p.meta.published).collect();
    let site_base = site_base_url(root);
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    if builtin_pages {
        xml.push_str(&format!("<url><loc>{}/</loc></url>\n", site_base));
        xml.push_str(&format!("<url><loc>{}{}</loc></url>\n", site_base, page_url("/blog")));
        xml.push_str(&format!("<url><loc>{}{}</loc></url>\n", site_base, page_url("/contact")));
    }
    for page in &live_pages {
        xml.push_str(&format!(
            "<url><loc>{}{}</loc></url>\n",
            site_base,
            page_url(&page.meta.site_path())
        ));
    }
    for post in published.iter().copied() {
        xml.push_str(&format!(
            "<url><loc>{}</loc><lastmod>{}</lastmod></url>\n",
            format!("{}{}", site_base, post.url_path()),
            post.meta.date
        ));
    }
    // Typed-collection items are first-class pages; they belong in the
    // sitemap too.
    for coll in collections {
        let live_items: Vec<&CollectionItem> =
            coll.items.iter().filter(|i| i.published).collect();
        if live_items.is_empty() {
            continue;
        }
        if coll.index {
            xml.push_str(&format!(
                "<url><loc>{}{}</loc></url>\n",
                site_base,
                page_url(&coll.url_prefix)
            ));
        }
        for item in live_items {
            xml.push_str(&format!(
                "<url><loc>{}{}</loc></url>\n",
                site_base,
                page_url(&coll.item_path(&item.slug))
            ));
        }
    }
    xml.push_str("</urlset>\n");
    fs::write(dist.join("sitemap.xml"), xml).context("write sitemap.xml")?;
    Ok(())
}

fn render_robots(dist: &Path, site_base: &str) -> Result<()> {
    fs::write(
        dist.join("robots.txt"),
        format!("User-agent: *\nAllow: /\nSitemap: {site_base}/sitemap.xml\n"),
    )
    .context("write robots.txt")?;
    Ok(())
}

/// llms.txt (https://llmstxt.org): a markdown index so LLM clients can
/// discover the site's content without crawling HTML. Published posts only.
fn render_llms_txt(root: &Path, dist: &Path, posts: &[Post]) -> Result<()> {
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let site_base = site_base_url(root);
    let mut out = String::new();
    out.push_str("# Post-CMS\n\n");
    out.push_str(
        "> A git-based publishing stack: content as files, static Rust build, \
         no CMS. This file is a machine-readable index of all published posts.\n\n",
    );
    out.push_str("## Posts\n\n");
    for post in published {
        out.push_str(&format!(
            "- [{title}]({base}{path}): {desc} ({date})\n",
            title = post.meta.title,
            base = site_base,
            path = post.url_path(),
            desc = post.meta.description,
            date = post.meta.date,
        ));
    }
    // MCP discovery section: agents reading llms.txt learn there is a
    // queryable tool endpoint (convention adopted by several MCP toolkits;
    // no standard makes clients auto-connect from here, but it costs nothing).
    out.push_str("\n## MCP Servers\n\n");
    out.push_str(&format!(
        "- [Post-CMS MCP]({base}/mcp): read-only tools — list_posts, get_post, \
         search. Query published content without auth.\n",
        base = site_base,
    ));
    fs::write(dist.join("llms.txt"), out).context("write llms.txt")?;
    Ok(())
}

/// MCP discovery card (proposed SEP-1649/SEP-1960 convention):
/// dist/.well-known/mcp/server-card.json describes the origin's public
/// read-only MCP endpoint so tooling that probes /.well-known can find it.
fn render_mcp_card(root: &Path, dist: &Path) -> Result<()> {
    use serde_json::json;
    let base = site_base_url(root).trim_end_matches('/').to_string();
    let card = json!({
        "name": "Post-CMS MCP",
        "description": "Read-only MCP server over this site's published content: list posts, fetch a post as markdown, keyword search.",
        "endpoint": format!("{base}/mcp"),
        "protocolVersion": "2024-11-05",
        "transports": ["streamable-http"],
        "auth": "none",
        "readOnly": true,
        "tools": ["list_posts", "get_post", "search"],
    });
    let dir = dist.join(".well-known/mcp");
    fs::create_dir_all(&dir).context("create .well-known/mcp")?;
    fs::write(
        dir.join("server-card.json"),
        serde_json::to_string_pretty(&card)?,
    )
    .context("write server-card.json")?;
    Ok(())
}

fn site_base_url(_root: &Path) -> String {
    std::env::var("SITE_BASE_URL").unwrap_or_else(|_| "http://localhost:8787".to_string())
}

/// Build-time image optimization (replaces hand-running `sips`): walks dist/,
/// downscales any raster image larger than MAX_DIM on its long edge, and
/// re-encodes JPEG/PNG in place when the re-encode is actually smaller.
/// Returns total bytes saved. Only ever overwrites when it wins bytes.
fn optimize_images(dist: &Path) -> Result<u64> {
    use image::ImageReader;
    use std::io::Cursor;

    const MAX_DIM: u32 = 1600;
    const JPEG_QUALITY: u8 = 80;
    let mut saved: u64 = 0;
    for entry in walk_files(dist) {
        let ext = entry
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let Some(ext) = ext else { continue };
        if !matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
            continue;
        }
        let original = fs::read(&entry)?;
        let Ok(img) = ImageReader::new(Cursor::new(&original))
            .with_guessed_format()
            .map_err(|_| anyhow::anyhow!("format probe"))?
            .decode()
        else {
            continue; // not a decodable raster image — leave untouched
        };
        let (w, h) = (img.width(), img.height());
        let img = if w.max(h) > MAX_DIM {
            let (nw, nh) = if w >= h {
                (MAX_DIM, (h as f64 * MAX_DIM as f64 / w as f64).round() as u32)
            } else {
                ((w as f64 * MAX_DIM as f64 / h as f64).round() as u32, MAX_DIM)
            };
            img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let encoded: Vec<u8> = match ext.as_str() {
            "png" => {
                let mut buf = Cursor::new(Vec::new());
                img.write_to(&mut buf, image::ImageFormat::Png)
                    .map_err(|e| anyhow::anyhow!("png encode: {e}"))?;
                buf.into_inner()
            }
            _ => {
                let mut buf = Cursor::new(Vec::new());
                let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
                img.write_with_encoder(enc)
                    .map_err(|e| anyhow::anyhow!("jpeg encode: {e}"))?;
                buf.into_inner()
            }
        };
        // Only overwrite when the optimized bytes are actually smaller.
        if encoded.len() < original.len() {
            let delta = (original.len() - encoded.len()) as u64;
            if delta > 0 {
                fs::write(&entry, &encoded)
                    .with_context(|| format!("rewrite {}", entry.display()))?;
                saved += delta;
            }
        }
    }
    Ok(saved)
}

/// Recursive file walker under `dir` (skips dotfiles).
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// WCAG 2.1 contrast lint over the theme's color tokens.
/// Any text pair below 4.5:1 (AA normal text) prints a warning; below 3.0:1
/// (unreadable — fails even the large-text AA bar) fails the build.
/// A production-site exercise shipped a real contrast bug that only Lighthouse
/// caught after the fact; this moves that check to build time.
fn lint_contrast(css: &str) -> Result<()> {
    // Pull `--color-*: #hex` custom properties from the file.
    let mut tokens: Vec<(String, [f64; 3])> = Vec::new();
    for line in css.lines() {
        let line = line.trim();
        let Some((name, val)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !name.starts_with("--color-") {
            continue;
        }
        let val = val.trim().trim_end_matches(';').trim();
        if let Some(rgb) = parse_hex_color(val) {
            tokens.push((name.trim_start_matches("--color-").to_string(), rgb));
        }
    }
    if tokens.is_empty() {
        return Ok(()); // no tokens — nothing to lint (e.g. utility-only css)
    }

    let by_name = |n: &str| tokens.iter().find(|(t, _)| t == n).map(|(_, c)| *c);
    // Canonical text/background pairs; presence-optional so themes with fewer
    // tokens lint only the pairs they define.
    let pairs: [(&str, &str); 7] = [
        ("fg", "bg"),
        ("heading", "bg"),
        ("accent", "bg"),
        ("muted", "bg"),
        ("muted", "card"),
        ("faint", "bg"),
        ("faint", "card"),
    ];

    let mut failures = Vec::new();
    for (fg, bg) in pairs {
        let (Some(f), Some(b)) = (by_name(fg), by_name(bg)) else {
            continue;
        };
        let ratio = contrast_ratio(f, b);
        let msg = format!(
            "contrast: --color-{fg} on --color-{bg} = {ratio:.2}:1 ({} threshold 4.5:1)",
            if ratio < 3.0 { "FAIL" } else { "warn" }
        );
        if ratio < 3.0 {
            failures.push(msg.clone());
        }
        if ratio < 4.5 {
            eprintln!("warning: {msg}");
        }
    }
    if !failures.is_empty() {
        bail!(
            "contrast lint failed (below 3.0:1 — unreadable text):\n{}",
            failures.join("\n")
        );
    }
    Ok(())
}

/// `#rrggbb` / `#rgb` → linear sRGB components (0..1). Returns None for non-hex.
fn parse_hex_color(s: &str) -> Option<[f64; 3]> {
    let s = s.strip_prefix('#')?;
    let (r, g, b) = match s.len() {
        3 => (
            u8::from_str_radix(&s[0..1].repeat(2), 16).ok()?,
            u8::from_str_radix(&s[1..2].repeat(2), 16).ok()?,
            u8::from_str_radix(&s[2..3].repeat(2), 16).ok()?,
        ),
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
        ),
        _ => return None,
    };
    // sRGB → linear channel per WCAG definition.
    let lin = |c: u8| -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Some([lin(r), lin(g), lin(b)])
}

/// WCAG contrast ratio between two linear-RGB colors.
fn contrast_ratio(a: [f64; 3], b: [f64; 3]) -> f64 {
    let lum = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let (la, lb) = (lum(a), lum(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Write a .gz sibling next to every compressible file in dist, so the server
/// can serve precompressed bytes at zero runtime CPU cost.
const PRECOMPRESS_EXTS: [&str; 9] = ["html", "htm", "css", "js", "json", "xml", "txt", "svg", "md"];

fn precompress(dist: &Path) -> Result<()> {
    fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                out.extend(walk(&p)?);
            } else {
                out.push(p);
            }
        }
        Ok(out)
    }
    for f in walk(dist)? {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !PRECOMPRESS_EXTS.contains(&ext) {
            continue;
        }
        let gz = {
            let mut s = f.as_os_str().to_os_string();
            s.push(".gz");
            PathBuf::from(s)
        };
        if gz.exists() {
            continue;
        }
        let raw = fs::read(&f)?;
        use std::io::Write as _;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw)?;
        fs::write(gz, enc.finish()?)?;
    }
    Ok(())
}

/// RFC 2822 date, e.g. `Mon, 20 Sep 2026 00:00:00 +0000`
fn rfc2822_date(date: &NaiveDate) -> String {
    date.format("%a, %d %b %Y 00:00:00 +0000").to_string()
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn sha256_hex(bytes: &[u8]) -> String {
    // Minimal SHA-256 implementation (FIPS 180-4) to avoid pulling an extra
    // crate; input is the template CSS we control, so this is fine.
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
        0x1f83d9ab, 0x5be0cd19,
    ];
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
        0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
        0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut msg = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::new();
    for word in h {
        out.push_str(&format!("{:08x}", word));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_url_follows_slash_setting() {
        assert_eq!(page_url_with("/platform/ops", true), "/platform/ops/");
        assert_eq!(page_url_with("platform/ops/", false), "/platform/ops");
        assert_eq!(page_url_with("/", false), "/");
        assert_eq!(page_url_with("", true), "/");
    }

    #[test]
    fn out_dir_maps_root_and_nested_paths() {
        let dist = Path::new("/tmp/dist");
        assert_eq!(out_dir_for(dist, "/"), PathBuf::from("/tmp/dist"));
        assert_eq!(
            out_dir_for(dist, "/community/hs-sponsors/acme/"),
            PathBuf::from("/tmp/dist/community/hs-sponsors/acme")
        );
    }

    #[test]
    fn page_site_path_defaults_to_slug() {
        let meta: PageMeta = serde_yaml::from_str("title: T\nslug: about\n").unwrap();
        assert_eq!(meta.site_path(), "/about");
        let meta: PageMeta =
            serde_yaml::from_str("title: Home\nslug: home\npath: /\n").unwrap();
        assert_eq!(meta.site_path(), "/");
    }

    #[test]
    fn collection_item_path_uses_prefix() {
        let c = Collection {
            name: "hs-sponsors".into(),
            title: "Sponsors".into(),
            required: vec![],
            types: Default::default(),
            item_template: "collection-item.html".into(),
            index_template: "collection.html".into(),
            url_prefix: "/community/hs-sponsors/".into(),
            index: false,
            items: vec![],
        };
        assert_eq!(c.item_path("acme"), "/community/hs-sponsors/acme");
    }

    #[test]
    fn site_config_defaults_keep_blog_behavior() {
        let cfg: SiteConfig = serde_yaml::from_str("title: X\n").unwrap();
        assert!(cfg.trailing_slash);
        assert!(cfg.builtin_pages);
        let cfg: SiteConfig =
            serde_yaml::from_str("trailing_slash: false\nbuiltin_pages: false\n").unwrap();
        assert!(!cfg.trailing_slash);
        assert!(!cfg.builtin_pages);
    }
}

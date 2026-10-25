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
    /// schema.yaml `references: {field: collection}`: fields whose values
    /// are item slugs in another collection, resolved for templates as `refs`.
    references: std::collections::BTreeMap<String, String>,
    /// schema.yaml `expose_to_pages` (default false): page templates see every
    /// item in `collections.<name>`, drafts included. For data collections
    /// (people, logos) whose items never render pages of their own.
    expose_to_pages: bool,
    items: Vec<CollectionItem>,
}

impl Collection {
    fn item_path(&self, slug: &str) -> String {
        format!("{}/{}", self.url_prefix.trim_end_matches('/'), slug)
    }

    /// `collection` in item and index contexts: name, title, and index URL.
    fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "title": self.title,
            "url": page_url(&self.url_prefix),
        })
    }

    /// Whether page templates may see this item: published items always,
    /// drafts only in an `expose_to_pages` collection.
    fn exposes(&self, item: &CollectionItem) -> bool {
        item.published || self.expose_to_pages
    }
}

/// `collections` in every template: `{<name>: {name, title, url, items}}`,
/// built once per build. `url` is the index path, or null when the schema
/// sets `index: false`. `items` holds the exposed items (see
/// `Collection::exposes`), newest first by `date`, undated items last in slug
/// order. Each row is `{slug, title, description, url, date, published,
/// fields, refs}`: `url` and `date` are null when the item has no page (a
/// draft) or no date, and `refs` is present only when the schema declares
/// references. `resolve_refs` keeps exposed items from referencing hidden
/// drafts, so a page never shows one through a reference.
fn page_collections(collections: &[Collection]) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    for coll in collections {
        let mut exposed: Vec<&CollectionItem> = coll.items.iter().filter(|i| coll.exposes(i)).collect();
        // Items arrive in slug order; the stable sort keeps it for ties.
        exposed.sort_by(|a, b| match (a.date, b.date) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        let mut rows = Vec::with_capacity(exposed.len());
        for item in exposed {
            let mut row = serde_json::json!({
                "slug": item.slug,
                "title": item.title,
                "description": item.description,
                "url": if item.published { serde_json::Value::String(page_url(&coll.item_path(&item.slug))) } else { serde_json::Value::Null },
                "date": item.date.map(|d| d.to_string()),
                "published": item.published,
                "fields": item.fields,
            });
            if let Some(refs) = resolve_refs(coll, item, collections)? {
                row["refs"] = serde_json::Value::Object(refs);
            }
            rows.push(row);
        }
        out.insert(
            coll.name.clone(),
            serde_json::json!({
                "name": coll.name,
                "title": coll.title,
                "url": if coll.index { serde_json::Value::String(page_url(&coll.url_prefix)) } else { serde_json::Value::Null },
                "items": rows,
            }),
        );
    }
    Ok(out)
}

/// meta.yaml `no_index: true` keeps a page or item out of every
/// machine-readable index (see `Page::indexable`, `CollectionItem::indexable`).
fn is_no_index(fields: &serde_json::Map<String, serde_json::Value>) -> bool {
    matches!(fields.get("no_index"), Some(serde_json::Value::Bool(true)))
}

impl Page {
    /// The one rule for listing a page in sitemap.xml and llms.txt:
    /// published and not `no_index: true`.
    fn indexable(&self) -> bool {
        self.meta.published && !is_no_index(&self.meta.fields)
    }
}

impl CollectionItem {
    /// The one rule for listing an item in sitemap.xml, llms.txt,
    /// search-index.json, and embeddings.json: published and not
    /// `no_index: true`.
    fn indexable(&self) -> bool {
        self.published && !is_no_index(&self.fields)
    }
}

/// A collection's items that the machine-readable indexes list, in order.
fn indexable_items(coll: &Collection) -> impl Iterator<Item = &CollectionItem> {
    coll.items.iter().filter(|i| i.indexable())
}

/// Every meta.yaml key as template data (`fields.<key>` on collection items,
/// `page.fields.<key>` on pages). Lists keep their string entries; nested
/// mappings become null.
fn yaml_to_fields(map: &serde_yaml::Mapping) -> serde_json::Map<String, serde_json::Value> {
    let mut fields = serde_json::Map::new();
    for (k, v) in map {
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
    fields
}

/// Resolve an item's reference fields (schema.yaml `references:`) into
/// `{field: {slug, title, description, url, published, fields}}`, or a list of
/// those for list fields. A published item, or a draft in an
/// `expose_to_pages` collection, may only reference published items or items
/// of `expose_to_pages` collections (the build fails by name otherwise; `url`
/// is null for such a draft target); other drafts may reference drafts.
/// One level deep: the referenced item's own
/// references stay slugs in its `fields`. `None` when the collection declares
/// no references, so templates of other collections see no `refs` at all.
fn resolve_refs(
    coll: &Collection,
    item: &CollectionItem,
    collections: &[Collection],
) -> Result<Option<serde_json::Map<String, serde_json::Value>>> {
    if coll.references.is_empty() {
        return Ok(None);
    }
    let mut refs = serde_json::Map::new();
    for (field, target) in &coll.references {
        let Some(value) = item.fields.get(field) else { continue };
        let target_coll = collections.iter().find(|c| &c.name == target).ok_or_else(|| {
            anyhow::anyhow!(
                "collection `{}` item `{}`: field `{}` references collection `{}`, which does not exist",
                coll.name,
                item.slug,
                field,
                target
            )
        })?;
        let one = |slug: &str| -> Result<serde_json::Value> {
            let found = target_coll.items.iter().find(|i| i.slug == slug).ok_or_else(|| {
                anyhow::anyhow!(
                    "collection `{}` item `{}`: field `{}` references `{}`, no such item in collection `{}`",
                    coll.name,
                    item.slug,
                    field,
                    slug,
                    target
                )
            })?;
            // A published page never exposes a draft's title, URL, or fields,
            // unless the draft's collection opts in with `expose_to_pages`.
            // Items pages can see (published, or exposed drafts) follow the
            // same rule, so `collections` never leaks a hidden draft either.
            let visible = coll.exposes(item);
            if visible && !target_coll.exposes(found) {
                bail!(
                    "collection `{}` item `{}`: field `{}` references `{}`, which is unpublished in collection `{}` (publish it, remove the reference, or set `expose_to_pages: true` on `{}`)",
                    coll.name,
                    item.slug,
                    field,
                    slug,
                    target,
                    target
                );
            }
            // An exposed draft has no page, so a visible item gets no link to it.
            let url = if visible && !found.published {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(page_url(&target_coll.item_path(&found.slug)))
            };
            Ok(serde_json::json!({
                "slug": found.slug,
                "title": found.title,
                "description": found.description,
                "url": url,
                "published": found.published,
                "fields": found.fields,
            }))
        };
        let resolved = match value {
            serde_json::Value::String(s) => one(s)?,
            serde_json::Value::Array(list) => serde_json::Value::Array(
                list.iter()
                    .filter_map(|v| v.as_str())
                    .map(one)
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => continue,
        };
        refs.insert(field.clone(), resolved);
    }
    Ok(Some(refs))
}

/// Context shared by item pages (live and draft): title, slug, description,
/// date, fields, collection, and refs when the schema declares references.
fn item_ctx(
    coll: &Collection,
    item: &CollectionItem,
    collections: &[Collection],
    body: &str,
) -> Result<TeraContext> {
    let mut ctx = TeraContext::new();
    ctx.insert("title", &item.title);
    ctx.insert("slug", &item.slug);
    ctx.insert("description", &item.description);
    if let Some(d) = item.date {
        ctx.insert("date", &d.to_string());
    }
    ctx.insert("fields", &item.fields);
    ctx.insert("collection", &coll.summary());
    if let Some(refs) = resolve_refs(coll, item, collections)? {
        ctx.insert("refs", &refs);
    }
    ctx.insert("body", body);
    Ok(ctx)
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
    /// Every meta.yaml key (known and extra, e.g. `no_index`, `seo_title`,
    /// `og_image`), exposed to templates as `page.fields`.
    #[serde(skip)]
    fields: serde_json::Map<String, serde_json::Value>,
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
        ctx.insert("page", &serde_json::json!({ "fields": self.fields }));
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
    /// Sanity project for image URLs in Portable Text (`sanity: {project_id, dataset}`).
    #[serde(default)]
    sanity: Option<SanityCfg>,
    /// Render the built-in blog home (`/`), `/blog/`, and `/contact/` and
    /// require posts (true, default). Sites with their own home page set false.
    #[serde(default = "default_true")]
    builtin_pages: bool,
    /// llms.txt header and sections (`llms: {title, summary, details, mcp}`).
    #[serde(default)]
    llms: LlmsCfg,
    /// robots.txt rules (`robots: {allow, disallow, sitemap, extra}`).
    #[serde(default)]
    robots: RobotsCfg,
    /// Site origin with no trailing slash, passed to every template as
    /// `base_url`. main() sets it from `site_base_url` (SITE_BASE_URL env,
    /// then site.yaml `base_url`, then the local origin).
    #[serde(default)]
    base_url: String,
    /// `collections` in every template (see `page_collections`); main() fills
    /// it once after loading collections.
    #[serde(skip)]
    page_collections: serde_json::Map<String, serde_json::Value>,
    /// The image pipeline when site.yaml has `images:` (main() sets it;
    /// see `postcms_build::images`).
    #[serde(skip)]
    images: Option<std::sync::Arc<postcms_build::images::Pipeline>>,
}

/// site.yaml `llms:`. Every key is optional; unset keys fall back to the
/// site title and tagline.
#[derive(Debug, Deserialize, Default, Clone)]
struct LlmsCfg {
    /// H1 of llms.txt (default: site `title`).
    #[serde(default)]
    title: Option<String>,
    /// Blockquote summary under the H1 (default: site `tagline`, else a
    /// generic line).
    #[serde(default)]
    summary: Option<String>,
    /// Optional free-form markdown placed after the summary.
    #[serde(default)]
    details: Option<String>,
    /// Emit the "MCP Servers" section pointing at `/mcp` (default true).
    #[serde(default)]
    mcp: Option<bool>,
}

/// site.yaml `robots:`. The default renders `User-agent: *`, `Allow: /`,
/// and the sitemap line, as before.
#[derive(Debug, Deserialize, Clone)]
struct RobotsCfg {
    #[serde(default = "default_robots_allow")]
    allow: Vec<String>,
    #[serde(default)]
    disallow: Vec<String>,
    /// Emit `Sitemap: <base_url>/sitemap.xml` (default true).
    #[serde(default = "default_true")]
    sitemap: bool,
    /// Raw lines appended at the end (extra user-agent groups, comments).
    #[serde(default)]
    extra: Vec<String>,
}

impl Default for RobotsCfg {
    fn default() -> Self {
        RobotsCfg { allow: default_robots_allow(), disallow: Vec::new(), sitemap: true, extra: Vec::new() }
    }
}

fn default_robots_allow() -> Vec<String> {
    vec!["/".to_string()]
}

#[derive(Debug, Deserialize, Clone)]
struct SanityCfg {
    project_id: String,
    #[serde(default = "default_dataset")]
    dataset: String,
}

fn default_dataset() -> String {
    "production".to_string()
}

impl SiteConfig {
    fn pt_options(&self) -> postcms_build::pt::PtOptions {
        let (project, dataset) = self
            .sanity
            .as_ref()
            .map(|c| (c.project_id.clone(), c.dataset.clone()))
            .unwrap_or_default();
        postcms_build::pt::PtOptions {
            sanity_project_id: project,
            sanity_dataset: dataset,
            image_width: Some(1600),
            images: self.images.clone(),
        }
    }
}

/// Body of a collection item: `index.mdx` (Markdown) or `index.pt.json`
/// (Portable Text). Unrenderable Portable Text blocks fail the build by name;
/// dropped links and images print one warning line for the item.
fn item_body(item: &CollectionItem, tera: &Tera, cfg: &SiteConfig) -> Result<String> {
    let md_path = item.dir.join("index.mdx");
    if md_path.exists() {
        return render_markdown(
            &fs::read_to_string(&md_path).with_context(|| format!("read {}", md_path.display()))?,
        );
    }
    let pt_path = item.dir.join("index.pt.json");
    if pt_path.exists() {
        let (html, warning) = pt_item_body(&pt_path, tera, cfg)?;
        if let Some(w) = warning {
            eprintln!("{w}");
        }
        return Ok(html);
    }
    Ok(String::new())
}

/// Plain text of an item body for the search and embeddings indexes.
fn item_text(item: &CollectionItem) -> String {
    if let Ok(md) = fs::read_to_string(item.dir.join("index.mdx")) {
        return markdown_to_text(&md);
    }
    fs::read_to_string(item.dir.join("index.pt.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|j| postcms_build::pt::plain_text(j.get("blocks").unwrap_or(&j)))
        .unwrap_or_default()
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
            sanity: None,
            llms: LlmsCfg::default(),
            robots: RobotsCfg::default(),
            base_url: String::new(),
            page_collections: serde_json::Map::new(),
            images: None,
        }
    }
    fn base_ctx(&self, style: &Styling, root: &str) -> TeraContext {
        let mut ctx = TeraContext::new();
        ctx.insert("css_hash", &style.hash);
        ctx.insert("style", &style.css);
        ctx.insert("site_title", &self.title);
        // Available to every template (headers/footers), not just home.html.
        ctx.insert("site_tagline", &self.tagline);
        ctx.insert("base_url", &self.base_url);
        ctx.insert("theme", &style.theme);
        ctx.insert("root", root);
        ctx.insert("lang", "en");
        ctx.insert("collections", &self.page_collections);
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
    // AVIF decodes run in a copy of this binary (see images::decoder_child)
    if let Some(code) = postcms_build::images::decoder_child() {
        std::process::exit(code);
    }
    let opts = match parse_args(std::env::args().skip(1)) {
        Ok(Args::Help) => {
            print!("{USAGE}");
            return Ok(());
        }
        Ok(Args::Version) => {
            println!("build-site {} (libwebp {})", env!("CARGO_PKG_VERSION"), postcms_build::images::webp::libwebp_version());
            return Ok(());
        }
        Ok(Args::Build(o)) => o,
        Err(e) => {
            eprint!("build-site: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let root = repo_root()?;
    require_site_config(&root)?;
    // Held until exit: one build-site at a time per site, so two builds
    // never write (or clear) the same dist/.
    let _site_lock = lock_site(&root)?;
    let dist = prepare_dist(&root, opts.force_clean)?;

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
    let mut tera = load_tera(&root, &theme_dir, &templates_dir)?;
    let style = compile_theme_css(&root, &theme_dir, &theme_name)?;
    let css_hash = sha256_hex(style.as_bytes());
    let styling = Styling {
        css: style,
        hash: css_hash,
        theme: theme_name.clone(),
    };
    // Theme asset kit (fonts, icons, images) → dist/assets/<theme>/
    copy_theme_assets(&theme_dir, &dist, &theme_name)?;

    let mut cfg = SiteConfig::load(&root);
    cfg.base_url = validate_base_url(&site_base_url(&root))?;
    let _ = TRAILING_SLASH.set(cfg.trailing_slash);
    // ---- image pipeline (site.yaml `images:`, opt-in) -----------------------
    let images_cfg = postcms_build::images::ImagesCfg::from_site_yaml(
        &fs::read_to_string(root.join("site.yaml")).unwrap_or_default(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    cfg.images = images_cfg.map(|ic| {
        let offline = std::env::var("IMAGES_OFFLINE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let transport = postcms_build::images::ureq_transport(ic.timeout_secs);
        std::sync::Arc::new(postcms_build::images::Pipeline::new(ic, &root, &dist, &cfg.base_url, offline, transport))
    });
    postcms_build::images::register(&mut tera, cfg.images.clone(), &cfg.base_url);
    let posts_dir = root.join("content/posts");
    let posts = if posts_dir.is_dir() { load_posts(&posts_dir)? } else { Vec::new() };
    if posts.is_empty() && cfg.builtin_pages {
        bail!("no posts found under content/posts");
    }
    // Default pages (about, privacy, terms, ...) — optional directory.
    let pages = load_pages(&root.join("content/pages")).unwrap_or_default();

    let sources = SourceCtx::from_env(&root, &cfg, &postcms_build::sanity::http_fetch);
    let (collections, _ref_checks) = load_collections(&root.join("content"), &sources)?;
    // Built once; every template reads it as `collections`.
    cfg.page_collections = page_collections(&collections)?;

    // Live site renders at the site root.
    render_posts(&tera, &dist, &posts, &styling, &cfg, "")?;
    render_pages(&tera, &dist, &pages, &styling, &cfg, "")?;
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
    let redirect_sources = load_redirect_sources(&root)?;
    render_sitemap(&root, &dist, &posts, &pages, &collections, cfg.builtin_pages, &redirect_sources)?;
    let site_base = site_base_url(&root);
    fs::write(dist.join("robots.txt"), robots_txt(&cfg.robots, &site_base))
        .context("write robots.txt")?;
    fs::write(
        dist.join("llms.txt"),
        llms_txt(&cfg, &site_base, &posts, &pages, &collections),
    )
    .context("write llms.txt")?;
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

    if let Some(p) = &cfg.images {
        finish_images(p).map_err(|e| discard_dist(&dist, e))?;
    }
    let skip_dir = cfg.images.as_ref().map(|p| dist.join(&p.cfg().output_dir));
    let images_saved = optimize_images(&dist, skip_dir.as_deref())?;
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
            let mut ttera = load_tera(&root, &tdir, &templates_dir)?;
            postcms_build::images::register(&mut ttera, cfg.images.clone(), &cfg.base_url);
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
        if let Some(p) = &cfg.images {
            finish_images(p).map_err(|e| discard_dist(&dist, e))?;
        }
        eprintln!(
            "theme previews built: {} -> /preview/<theme>/",
            built.join(", ")
        );
    }

    // Every image placeholder must be gone from every page and its .gz copy.
    postcms_build::images::verify_dist(&dist, cfg.images.as_deref())
        .map_err(|e| discard_dist(&dist, anyhow::anyhow!(e)))?;

    let drafts = posts.iter().filter(|p| !p.meta.published).count();
    eprintln!(
        "built dist/: {} posts ({} drafts skipped), {} pages, {} collections, theme={:?}, {}404, {}sitemap.xml, robots.txt, llms.txt",
        posts.len() - drafts,
        drafts,
        pages.iter().filter(|p| p.meta.published).count(),
        collections.len(),
        theme_name,
        if cfg.builtin_pages { "home, blog, contact, " } else { "" },
        if posts.is_empty() { "" } else { "rss.xml, " },
    );
    Ok(())
}

/// Run the image pipeline over everything rendered so far and print totals.
fn finish_images(p: &postcms_build::images::Pipeline) -> Result<()> {
    let t = std::time::Instant::now();
    let jobs = std::env::var("IMAGES_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let s = p.finish(jobs).map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "images: {} sources, {} files written ({} bytes), {} kept their original URL, {:.1}s",
        s.sources,
        s.files,
        s.bytes,
        s.kept.len(),
        t.elapsed().as_secs_f64()
    );
    for (url, why) in &s.kept {
        println!("  kept original URL: {url} ({why})");
    }
    Ok(())
}

/// Compile the template set: repo `templates/` plus any `<theme>/templates/*.html`
/// overrides (which win on name collision).
/// Render one `index.pt.json`: the HTML plus, when the renderer dropped
/// links or images, a single warning line naming the file.
fn pt_item_body(pt_path: &Path, tera: &Tera, cfg: &SiteConfig) -> Result<(String, Option<String>)> {
    let raw = fs::read_to_string(pt_path).with_context(|| format!("read {}", pt_path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", pt_path.display()))?;
    let blocks = json.get("blocks").cloned().unwrap_or(json);
    let out = postcms_build::pt::render(&blocks, &cfg.pt_options(), Some(tera));
    if !out.unhandled.is_empty() {
        bail!(
            "{}: Portable Text blocks with no renderer: {:?} (add templates/pt/<type>.html)",
            pt_path.display(),
            out.unhandled
        );
    }
    let warning = (!out.warnings.is_empty())
        .then(|| format!("warning: {}: {}", pt_path.display(), out.warnings.join("; ")));
    Ok((out.html, warning))
}

fn load_tera(root: &Path, theme_dir: &Path, templates_dir: &Path) -> Result<Tera> {
    let mut tera = Tera::new(
        templates_dir
            .join("**/*.html")
            .to_str()
            .context("templates path is not valid UTF-8")?,
    )
    .context("failed to compile templates")?;
    postcms_build::pt::register_filters(&mut tera);
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

const USAGE: &str = "\
Usage: build-site [--force-clean]

Renders the site at SITE_ROOT (else the current directory) into its dist/.
The site root must contain site.yaml.

Options:
  --force-clean  Replace an existing dist/ that has no build marker
                 (dist/.post-cms-build), such as one written by an older
                 build-site. Use it once; later builds find the marker.
  --version      Print the engine and linked libwebp versions.
  -h, --help     Print this help.
";

/// File every build writes in dist/. An existing dist/ is cleared only when
/// it carries this marker, so the build never deletes a directory it did
/// not create.
const DIST_MARKER: &str = ".post-cms-build";
const DIST_MARKER_BODY: &str = "Written by post-cms build-site. This directory is build output: the next build deletes and recreates it.\n";

#[derive(Debug, Default, PartialEq)]
struct BuildOpts {
    force_clean: bool,
}

#[derive(Debug, PartialEq)]
enum Args {
    Build(BuildOpts),
    Help,
    Version,
}

/// Command line after the program name. Anything not listed in [`USAGE`]
/// is an error, so a flag this binary does not know never starts a build.
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> std::result::Result<Args, String> {
    let mut opts = BuildOpts::default();
    for a in args {
        match a.as_str() {
            "-h" | "--help" => return Ok(Args::Help),
            "--version" => return Ok(Args::Version),
            "--force-clean" => opts.force_clean = true,
            _ if a.starts_with('-') => return Err(format!("unknown option {a:?}")),
            _ => return Err(format!("unexpected argument {a:?} (set SITE_ROOT to choose the site)")),
        }
    }
    Ok(Args::Build(opts))
}

/// The site root: SITE_ROOT when set, else the current directory.
fn repo_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SITE_ROOT") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    std::env::current_dir().context("cannot read the current directory")
}

/// A directory is only a site when it has site.yaml: anything else (an
/// unrelated repo, a home directory) is refused before dist/ is touched.
fn require_site_config(root: &Path) -> Result<()> {
    if root.join("site.yaml").is_file() {
        return Ok(());
    }
    bail!(
        "{} is not a post-cms site (no site.yaml); set SITE_ROOT to the site directory. Nothing was built or deleted.",
        root.display()
    )
}

/// Lock the site root for this build (an advisory lock on the directory,
/// released when the process exits, even on a crash). A second build-site
/// on the same site exits before touching dist/: two builds writing one
/// dist/ mix their pages, and one build's precompressed copies can keep the
/// other's image placeholders.
fn lock_site(root: &Path) -> Result<fs::File> {
    let dir = fs::File::open(root).with_context(|| format!("open {}", root.display()))?;
    match dir.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => bail!(
            "another build-site is already building {}; wait for it to finish, or build a separate copy of the site. Nothing was built or deleted.",
            root.display()
        ),
        Err(fs::TryLockError::Error(e)) => {
            eprintln!("warning: cannot lock {} ({e}); concurrent builds of this site are not prevented", root.display());
        }
    }
    Ok(dir)
}

/// A build that failed after rendering removes the dist/ it wrote, so no
/// half-finished site (pages with image placeholders) is ever served.
fn discard_dist(dist: &Path, e: anyhow::Error) -> anyhow::Error {
    match fs::remove_dir_all(dist) {
        Ok(()) => e.context(format!("build failed; removed {} so no partial site is served", dist.display())),
        Err(re) => e.context(format!("build failed; could not remove {} ({re}): do not serve it", dist.display())),
    }
}

/// Make `<root>/dist` ready for a fresh build and write the build marker.
/// A missing or empty dist/ is used as is. An existing one is deleted only
/// when it carries [`DIST_MARKER`] (or with `force_clean`); a symlink or a
/// file named dist is always refused.
fn prepare_dist(root: &Path, force_clean: bool) -> Result<PathBuf> {
    let dist = root.join("dist");
    match fs::symlink_metadata(&dist) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", dist.display())),
        Ok(m) if !m.is_dir() => bail!(
            "{} is not a directory (a symlink or file); refusing to replace it. Remove it and build again.",
            dist.display()
        ),
        Ok(_) => {
            let marked = fs::symlink_metadata(dist.join(DIST_MARKER)).is_ok_and(|m| m.is_file());
            let empty = fs::read_dir(&dist)
                .with_context(|| format!("read_dir {}", dist.display()))?
                .next()
                .is_none();
            if !marked && !empty && !force_clean {
                bail!(
                    "{} exists but has no {DIST_MARKER} marker, so build-site did not create it; refusing to delete it. \
                     If it is output of an older build-site, run once with --force-clean.",
                    dist.display()
                );
            }
            if !empty {
                fs::remove_dir_all(&dist)
                    .with_context(|| format!("failed to clear {}", dist.display()))?;
            }
        }
    }
    fs::create_dir_all(&dist).with_context(|| format!("mkdir {}", dist.display()))?;
    fs::write(dist.join(DIST_MARKER), DIST_MARKER_BODY)
        .with_context(|| format!("write {}", dist.join(DIST_MARKER).display()))?;
    Ok(dist)
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
#[derive(Debug)]
struct RefCheck {
    at: String,
    field: String,
    target: String,
    value: String,
}

/// How non-file collection sources (schema.yaml `source:`) are synced.
struct SourceCtx<'a> {
    /// `<root>/.cache/sanity`; each Sanity collection caches under `<name>/`.
    cache_root: PathBuf,
    /// SANITY_OFFLINE=1: build from the recorded responses, never fetch.
    offline: bool,
    /// site.yaml `sanity:` project and dataset, the defaults for every source.
    sanity: postcms_build::sanity::SiteDefaults,
    fetch: postcms_build::sanity::Fetch<'a>,
}

impl<'a> SourceCtx<'a> {
    fn from_env(root: &Path, cfg: &SiteConfig, fetch: postcms_build::sanity::Fetch<'a>) -> Self {
        SourceCtx {
            cache_root: root.join(".cache").join("sanity"),
            offline: std::env::var("SANITY_OFFLINE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            sanity: postcms_build::sanity::SiteDefaults {
                project_id: cfg.sanity.as_ref().map(|c| c.project_id.clone()),
                dataset: cfg.sanity.as_ref().map(|c| c.dataset.clone()),
            },
            fetch,
        }
    }
}

/// Load typed collections: every directory under content/ other than posts/
/// pages/ is a collection if it has a schema.yaml. Each item is
/// content/<collection>/<slug>/ with meta.yaml + index.mdx. Required fields
/// (and optional type checks) from schema.yaml are enforced at build time —
/// a missing or wrongly-typed field fails the build naming the exact file.
/// A schema with `source: {type: sanity, ...}` reads its items from a GROQ
/// query instead, materialized under `.cache/sanity/<collection>/items/` in
/// the same layout, so the same validation applies (see `sanity` module).
/// Returns (collections, deferred reference checks).
fn load_collections(
    content_dir: &Path,
    sources: &SourceCtx,
) -> Result<(Vec<Collection>, Vec<RefCheck>)> {
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
        let references: std::collections::BTreeMap<String, String> = schema
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
        let expose_to_pages = match schema.get("expose_to_pages") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| {
                anyhow::anyhow!("{}: `expose_to_pages` must be true or false", schema_path.display())
            })?,
        };
        let source = postcms_build::sanity::SanitySource::from_schema(schema.get("source"))
            .with_context(|| format!("{}: source", schema_path.display()))?;
        let items_dir = match &source {
            Some(src) => postcms_build::sanity::sync(
                src,
                &sources.sanity,
                &sources.cache_root.join(name),
                sources.offline,
                sources.fetch,
            )
            .with_context(|| {
                format!("collection `{name}`: Sanity source ({})", schema_path.display())
            })?,
            None => path.clone(),
        };
        for item_entry in fs::read_dir(&items_dir)
            .with_context(|| format!("read_dir {}", items_dir.display()))?
        {
            let item_path = item_entry
                .with_context(|| format!("entry in {}", items_dir.display()))?
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
            let fields = yaml_to_fields(&map);
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
            references,
            expose_to_pages,
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
            let mut ctx = item_ctx(coll, item, collections, &body)?;
            if let Some(serde_json::Value::String(notes)) =
                item.fields.get("agent_notes").cloned()
            {
                ctx.insert("agent_notes", &notes);
            }
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
            let body = item_body(item, tera, cfg)?;
            let mut ctx = item_ctx(coll, item, collections, &body)?;
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
            // Index rows carry resolved references too, unless the item has
            // its own `refs` field (kept as is).
            if let Some(refs) = resolve_refs(coll, item, collections)? {
                row.entry("refs").or_insert(serde_json::Value::Object(refs));
            }
            listed.push(serde_json::Value::Object(row));
        }
        if !coll.index {
            continue;
        }
        let mut ctx = TeraContext::new();
        ctx.insert("coll_name", &coll.name);
        ctx.insert("coll_title", &coll.title);
        ctx.insert("collection", &coll.summary());
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

/// Emit search-index.json: one row per published post and indexable
/// collection item (published, not `no_index: true`) with title, description,
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
        for item in indexable_items(coll) {
            idx.push(serde_json::json!({
                "title": item.title,
                "description": item.description,
                "url": format!("{}{}", site_base, page_url(&coll.item_path(&item.slug))),
                "date": item.date.map(|d| d.to_string()).unwrap_or_default(),
                "tags": [],
                "body": item_text(item),
            }));
        }
    }
    fs::write(dist.join("search-index.json"), serde_json::to_vec_pretty(&idx)?)
        .with_context(|| "write search-index.json".to_string())?;
    Ok(())
}

/// Tier 3: embed per-post chunks at build time into dist/embeddings.json.
/// Collection items follow `indexable_items`, so `no_index: true` items stay out.
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
        .flat_map(|c| indexable_items(c).map(move |i| (c, i)))
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
    // Collection items get the same treatment (indexable only).
    for (coll, item) in &live_items {
        let text = item_text(item);
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
        let mut meta: PageMeta =
            serde_yaml::from_str(&raw).with_context(|| format!("parse {}", meta_path.display()))?;
        if let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(&raw) {
            meta.fields = yaml_to_fields(&map);
        }
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

/// Source paths of redirects.yaml (slash-trimmed), which never belong in
/// sitemap.xml. Missing file: none. Same shape the origin loads: a mapping
/// whose keys are paths.
fn load_redirect_sources(root: &Path) -> Result<std::collections::HashSet<String>> {
    let path = root.join("redirects.yaml");
    if !path.exists() {
        return Ok(Default::default());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    match parsed {
        serde_yaml::Value::Null => Ok(Default::default()),
        serde_yaml::Value::Mapping(map) => Ok(map
            .keys()
            .filter_map(|k| k.as_str())
            .map(|k| k.trim_matches('/').to_string())
            .collect()),
        _ => bail!("{}: must be a mapping of from -> to", path.display()),
    }
}

fn render_sitemap(
    root: &Path,
    dist: &Path,
    posts: &[Post],
    pages: &[Page],
    collections: &[Collection],
    builtin_pages: bool,
    redirect_sources: &std::collections::HashSet<String>,
) -> Result<()> {
    let xml = sitemap_xml(
        &site_base_url(root),
        posts,
        pages,
        collections,
        builtin_pages,
        redirect_sources,
    );
    fs::write(dist.join("sitemap.xml"), xml).context("write sitemap.xml")?;
    Ok(())
}

/// sitemap.xml: published pages, posts, and collection items, minus any
/// page or item whose meta sets `no_index: true` and any path that is a
/// redirects.yaml source.
fn sitemap_xml(
    site_base: &str,
    posts: &[Post],
    pages: &[Page],
    collections: &[Collection],
    builtin_pages: bool,
    redirect_sources: &std::collections::HashSet<String>,
) -> String {
    let listed = |path: &str| !redirect_sources.contains(path.trim_matches('/'));
    let loc = |path: &str| escape_xml(&format!("{site_base}{path}"));
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let live_pages: Vec<&Page> = pages.iter().filter(|p| p.indexable()).collect();
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    if builtin_pages {
        for path in ["/", "/blog", "/contact"] {
            if listed(path) {
                xml.push_str(&format!("<url><loc>{}</loc></url>\n", loc(&page_url(path))));
            }
        }
    }
    for page in &live_pages {
        let path = page.meta.site_path();
        if listed(&path) {
            xml.push_str(&format!("<url><loc>{}</loc></url>\n", loc(&page_url(&path))));
        }
    }
    for post in published.iter().copied() {
        if !listed(&post.url_path()) {
            continue;
        }
        xml.push_str(&format!(
            "<url><loc>{}</loc><lastmod>{}</lastmod></url>\n",
            loc(&post.url_path()),
            post.meta.date
        ));
    }
    // Typed-collection items are first-class pages; they belong in the
    // sitemap too.
    for coll in collections {
        if !coll.items.iter().any(|i| i.published) {
            continue;
        }
        if coll.index && listed(&coll.url_prefix) {
            xml.push_str(&format!("<url><loc>{}</loc></url>\n", loc(&page_url(&coll.url_prefix))));
        }
        for item in indexable_items(coll) {
            let path = coll.item_path(&item.slug);
            if !listed(&path) {
                continue;
            }
            xml.push_str(&format!("<url><loc>{}</loc></url>\n", loc(&page_url(&path))));
        }
    }
    xml.push_str("</urlset>\n");
    xml
}

/// robots.txt from site.yaml `robots:`. With no config this is exactly
/// `User-agent: *`, `Allow: /`, and the sitemap line.
fn robots_txt(cfg: &RobotsCfg, site_base: &str) -> String {
    let mut out = String::from("User-agent: *\n");
    for path in &cfg.allow {
        out.push_str(&format!("Allow: {path}\n"));
    }
    for path in &cfg.disallow {
        out.push_str(&format!("Disallow: {path}\n"));
    }
    if cfg.sitemap {
        out.push_str(&format!("Sitemap: {site_base}/sitemap.xml\n"));
    }
    for line in &cfg.extra {
        out.push_str(line.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

/// One llms.txt list entry: `- [title](url): desc`, omitting an empty desc.
fn llms_entry(title: &str, url: &str, desc: &str) -> String {
    if desc.trim().is_empty() {
        format!("- [{title}]({url})\n")
    } else {
        format!("- [{title}]({url}): {desc}\n")
    }
}

/// llms.txt (https://llmstxt.org): a markdown index so LLM clients can
/// discover the site's content without crawling HTML. Lists published
/// posts, and pages and collection items by the sitemap's rule (published,
/// not `no_index: true`); header text comes from site.yaml.
fn llms_txt(
    cfg: &SiteConfig,
    site_base: &str,
    posts: &[Post],
    pages: &[Page],
    collections: &[Collection],
) -> String {
    let title = cfg.llms.title.clone().unwrap_or_else(|| cfg.title.clone());
    let summary = cfg
        .llms
        .summary
        .clone()
        .or_else(|| Some(cfg.tagline.clone()).filter(|t| !t.trim().is_empty()))
        .unwrap_or_else(|| "A machine-readable index of this site's published content.".to_string());
    let mut out = format!("# {title}\n\n> {}\n\n", summary.trim());
    if let Some(details) = cfg.llms.details.as_deref().filter(|d| !d.trim().is_empty()) {
        out.push_str(details.trim());
        out.push_str("\n\n");
    }
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    if !published.is_empty() {
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
        out.push('\n');
    }
    let mut page_lines = String::new();
    if cfg.builtin_pages {
        page_lines.push_str(&llms_entry("Home", &format!("{site_base}/"), ""));
        page_lines.push_str(&llms_entry("Blog", &format!("{site_base}{}", page_url("/blog")), ""));
        page_lines.push_str(&llms_entry("Contact", &format!("{site_base}{}", page_url("/contact")), ""));
    }
    for page in pages.iter().filter(|p| p.indexable()) {
        page_lines.push_str(&llms_entry(
            &page.meta.title,
            &format!("{site_base}{}", page_url(&page.meta.site_path())),
            &page.meta.description,
        ));
    }
    if !page_lines.is_empty() {
        out.push_str("## Pages\n\n");
        out.push_str(&page_lines);
        out.push('\n');
    }
    // Same rule as the sitemap: collections with no published items are
    // skipped; the index page is listed when the collection renders one.
    for coll in collections {
        if !coll.items.iter().any(|i| i.published) {
            continue;
        }
        out.push_str(&format!("## {}\n\n", coll.title));
        if coll.index {
            out.push_str(&llms_entry(
                &coll.title,
                &format!("{site_base}{}", page_url(&coll.url_prefix)),
                "",
            ));
        }
        for item in indexable_items(coll) {
            let desc = match item.date {
                Some(d) if !item.description.trim().is_empty() => format!("{} ({d})", item.description),
                Some(d) => format!("({d})"),
                None => item.description.clone(),
            };
            out.push_str(&llms_entry(
                &item.title,
                &format!("{site_base}{}", page_url(&coll.item_path(&item.slug))),
                &desc,
            ));
        }
        out.push('\n');
    }
    // MCP discovery section: agents reading llms.txt learn there is a
    // queryable tool endpoint (convention adopted by several MCP toolkits;
    // no standard makes clients auto-connect from here, but it costs nothing).
    if cfg.llms.mcp.unwrap_or(true) {
        out.push_str("## MCP Servers\n\n");
        out.push_str(&format!(
            "- [{title} MCP]({site_base}/mcp): read-only tools — list_posts, get_post, \
             search. Query published content without auth.\n",
        ));
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('\n');
    out
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

/// `base_url` must be `http(s)://host[:port]`: it lands in templates,
/// sitemap.xml, robots.txt, and RSS, so quotes, spaces, control characters,
/// paths, and credentials fail the build.
fn validate_base_url(url: &str) -> Result<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    let ok = rest.is_some_and(|rest| {
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        };
        !host.is_empty()
            && !host.starts_with(['.', '-'])
            && !host.ends_with(['.', '-'])
            && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            && port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit()))
    });
    if !ok {
        bail!(
            "base_url {url:?} (SITE_BASE_URL or site.yaml base_url) must be http(s)://host[:port] with no path, quotes, spaces, or control characters"
        );
    }
    Ok(url.to_string())
}

/// SITE_BASE_URL env, then site.yaml `base_url`, then the local origin.
fn site_base_url(root: &Path) -> String {
    if let Ok(v) = std::env::var("SITE_BASE_URL") {
        if !v.is_empty() {
            return v.trim_end_matches('/').to_string();
        }
    }
    fs::read_to_string(root.join("site.yaml"))
        .ok()
        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
        .and_then(|v| v.get("base_url").and_then(|b| b.as_str()).map(|s| s.trim_end_matches('/').to_string()))
        .unwrap_or_else(|| "http://localhost:8787".to_string())
}

/// Build-time image optimization (replaces hand-running `sips`): walks dist/,
/// downscales any raster image larger than MAX_DIM on its long edge, and
/// re-encodes JPEG/PNG in place when the re-encode is actually smaller.
/// Returns total bytes saved. Only ever overwrites when it wins bytes.
fn optimize_images(dist: &Path, skip: Option<&Path>) -> Result<u64> {
    use image::ImageReader;
    use std::io::Cursor;

    const MAX_DIM: u32 = 1600;
    const JPEG_QUALITY: u8 = 80;
    let mut saved: u64 = 0;
    for entry in walk_files(dist) {
        // The image pipeline's outputs are already sized and encoded, and
        // their width descriptors must stay true.
        if skip.is_some_and(|s| entry.starts_with(s)) {
            continue;
        }
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
            out_dir_for(dist, "/events/speakers/acme/"),
            PathBuf::from("/tmp/dist/events/speakers/acme")
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
            name: "speakers".into(),
            title: "Speakers".into(),
            required: vec![],
            types: Default::default(),
            item_template: "collection-item.html".into(),
            index_template: "collection.html".into(),
            url_prefix: "/events/speakers/".into(),
            index: false,
            references: Default::default(),
            expose_to_pages: false,
            items: vec![],
        };
        assert_eq!(c.item_path("acme"), "/events/speakers/acme");
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

    fn test_cfg(yaml: &str) -> SiteConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn robots_default_matches_previous_output() {
        let cfg = test_cfg("title: X\n");
        assert_eq!(
            robots_txt(&cfg.robots, "https://example.com"),
            "User-agent: *\nAllow: /\nSitemap: https://example.com/sitemap.xml\n"
        );
    }

    #[test]
    fn robots_reads_site_config() {
        let cfg = test_cfg(
            "robots:\n  disallow: [/preview/, /drafts/]\n  sitemap: false\n  extra: [\"User-agent: ExampleBot\", \"Disallow: /\"]\n",
        );
        assert_eq!(
            robots_txt(&cfg.robots, "https://example.com"),
            "User-agent: *\nAllow: /\nDisallow: /preview/\nDisallow: /drafts/\nUser-agent: ExampleBot\nDisallow: /\n"
        );
    }

    fn sample_content() -> (Vec<Page>, Vec<Collection>) {
        let page = |yaml: &str| Page { meta: serde_yaml::from_str(yaml).unwrap(), dir: PathBuf::new() };
        let pages = vec![
            page("title: Home\nslug: home\npath: /\n"),
            page("title: Ops\nslug: ops\npath: /docs/ops\ndescription: Run ops.\n"),
            page("title: Hidden\nslug: hidden\npublished: false\n"),
        ];
        let item = |slug: &str, published: bool| CollectionItem {
            slug: slug.into(),
            title: slug.to_uppercase(),
            description: format!("About {slug}."),
            date: None,
            published,
            fields: Default::default(),
            dir: PathBuf::new(),
        };
        let coll = Collection {
            name: "speakers".into(),
            title: "Speakers".into(),
            required: vec![],
            types: Default::default(),
            item_template: "collection-item.html".into(),
            index_template: "collection.html".into(),
            url_prefix: "/events/speakers".into(),
            index: true,
            references: Default::default(),
            expose_to_pages: false,
            items: vec![item("acme", true), item("draft", false)],
        };
        (pages, vec![coll])
    }

    #[test]
    fn llms_lists_pages_and_collection_items() {
        let cfg = test_cfg("title: Example\ntagline: An example site.\nbuiltin_pages: false\n");
        let (pages, colls) = sample_content();
        let out = llms_txt(&cfg, "https://example.com", &[], &pages, &colls);
        assert!(out.starts_with("# Example\n\n> An example site.\n\n"));
        assert!(out.contains("## Pages\n\n- [Home](https://example.com/)\n"));
        assert!(out.contains("- [Ops](https://example.com/docs/ops/): Run ops.\n"));
        assert!(!out.contains("Hidden"));
        assert!(out.contains("## Speakers\n\n- [Speakers](https://example.com/events/speakers/)\n"));
        assert!(out.contains("- [ACME](https://example.com/events/speakers/acme/): About acme.\n"));
        assert!(!out.contains("DRAFT"));
        assert!(!out.contains("## Posts"));
        assert!(out.contains("- [Example MCP](https://example.com/mcp)"));
        assert!(!out.to_lowercase().contains("post-cms"));
        assert!(out.ends_with("auth.\n"));
    }

    #[test]
    fn llms_header_and_mcp_are_configurable() {
        let cfg = test_cfg(
            "title: Example\nllms:\n  title: Example Docs\n  summary: Custom summary.\n  details: Extra notes.\n  mcp: false\n",
        );
        let out = llms_txt(&cfg, "https://example.com", &[], &[], &[]);
        assert!(out.starts_with("# Example Docs\n\n> Custom summary.\n\nExtra notes.\n\n## Pages\n"));
        assert!(!out.contains("MCP"));
        let cfg = test_cfg("title: Example\nbuiltin_pages: false\n");
        let out = llms_txt(&cfg, "https://example.com", &[], &[], &[]);
        assert!(out.contains("> A machine-readable index of this site's published content.\n"));
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sanity")
    }

    fn copy_tree(src: &Path, dst: &Path) {
        for file in walk_files(src) {
            let to = dst.join(file.strip_prefix(src).unwrap());
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(&file, &to).unwrap();
        }
    }

    fn no_net(_: &str, _: Option<&str>) -> Result<String> {
        panic!("offline build fetched")
    }

    /// A site root whose `articles` collection is Sanity-backed, with the
    /// recorded GROQ response in its cache (as left by an online build).
    fn sanity_site(dir: &Path, response: &str) -> PathBuf {
        let root = dir.join("sanity-site");
        fs::create_dir_all(root.join("content/articles")).unwrap();
        fs::copy(fixtures().join("articles/schema.yaml"), root.join("content/articles/schema.yaml"))
            .unwrap();
        fs::create_dir_all(root.join(".cache/sanity/articles")).unwrap();
        fs::write(root.join(".cache/sanity/articles/response.json"), response).unwrap();
        root
    }

    fn offline_ctx(root: &Path) -> SourceCtx<'static> {
        SourceCtx {
            cache_root: root.join(".cache/sanity"),
            offline: true,
            sanity: Default::default(),
            fetch: &no_net,
        }
    }

    fn render_site(root: &Path, dist: &Path) -> Vec<Collection> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let tera = load_tera(&repo, &repo.join("themes/default"), &repo.join("templates")).unwrap();
        let style = Styling { css: String::new(), hash: "h".into(), theme: "default".into() };
        let mut cfg = SiteConfig::default_theme_config();
        cfg.sanity = Some(SanityCfg { project_id: "exmpl123".into(), dataset: "production".into() });
        let (collections, _) = load_collections(&root.join("content"), &offline_ctx(root)).unwrap();
        render_collections(&tera, dist, &collections, &style, &cfg, "").unwrap();
        collections
    }

    #[test]
    fn offline_sanity_build_matches_file_build() {
        let tmp = tempfile::tempdir().unwrap();
        let file_root = tmp.path().join("file-site");
        copy_tree(&fixtures().join("files"), &file_root.join("content"));
        let sanity_root = sanity_site(
            tmp.path(),
            &fs::read_to_string(fixtures().join("articles/response.json")).unwrap(),
        );

        let file_dist = tmp.path().join("file-dist");
        let sanity_dist = tmp.path().join("sanity-dist");
        let from_files = render_site(&file_root, &file_dist);
        let from_sanity = render_site(&sanity_root, &sanity_dist);

        let items = |c: &[Collection]| -> Vec<(String, String, Option<NaiveDate>, serde_json::Value)> {
            c[0].items
                .iter()
                .map(|i| (i.slug.clone(), i.title.clone(), i.date, serde_json::Value::Object(i.fields.clone())))
                .collect()
        };
        assert_eq!(from_sanity[0].items.len(), 3);
        assert_eq!(items(&from_files), items(&from_sanity));

        let tree = |dist: &Path| -> Vec<(PathBuf, Vec<u8>)> {
            let mut files: Vec<_> = walk_files(dist)
                .into_iter()
                .map(|f| (f.strip_prefix(dist).unwrap().to_path_buf(), fs::read(&f).unwrap()))
                .collect();
            files.sort();
            files
        };
        let (a, b) = (tree(&file_dist), tree(&sanity_dist));
        assert_eq!(a.len(), 4, "3 item pages + the index");
        assert_eq!(
            a.iter().map(|f| &f.0).collect::<Vec<_>>(),
            b.iter().map(|f| &f.0).collect::<Vec<_>>()
        );
        for ((path, x), (_, y)) in a.iter().zip(&b) {
            assert!(x == y, "{} differs between file and Sanity builds", path.display());
        }
        // Portable Text from the response went through the PT renderer.
        let page = String::from_utf8(fs::read(sanity_dist.join("articles/edge-basics/index.html")).unwrap()).unwrap();
        assert!(page.contains("<ol>") && page.contains("cdn.sanity.io/images/exmpl123/production/"), "{page}");
    }

    #[test]
    fn sanity_items_get_the_same_validation_as_file_items() {
        let tmp = tempfile::tempdir().unwrap();
        let recorded = fs::read_to_string(fixtures().join("articles/response.json")).unwrap();

        let bad_enum = recorded.replacen("\"kind\": \"guide\"", "\"kind\": \"webinar\"", 1);
        let root = sanity_site(&tmp.path().join("enum"), &bad_enum);
        let err = format!("{:#}", load_collections(&root.join("content"), &offline_ctx(&root)).unwrap_err());
        assert!(err.contains("caching-guide/meta.yaml") && err.contains("`kind` value `webinar`"), "{err}");

        let no_title = recorded.replacen("\"title\": \"Rollout Story\",", "", 1);
        let root = sanity_site(&tmp.path().join("required"), &no_title);
        let err = format!("{:#}", load_collections(&root.join("content"), &offline_ctx(&root)).unwrap_err());
        assert!(err.contains("rollout-story/meta.yaml") && err.contains("required field `title`"), "{err}");

        let bad_url = recorded.replacen("https://cdn.example.com/edge.png", "not a url", 1);
        let root = sanity_site(&tmp.path().join("format"), &bad_url);
        let err = format!("{:#}", load_collections(&root.join("content"), &offline_ctx(&root)).unwrap_err());
        assert!(err.contains("`hero_url` value `not a url` is not a valid url"), "{err}");
    }

    #[test]
    fn unsafe_links_warn_and_unknown_blocks_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let tera = load_tera(&repo, &repo.join("themes/default"), &repo.join("templates")).unwrap();
        let cfg = SiteConfig::default_theme_config();
        let pt_path = tmp.path().join("index.pt.json");
        fs::write(
            &pt_path,
            r#"[{"_type": "block", "style": "normal", "markDefs": [{"_type": "link", "_key": "l1", "href": ""}],
                "children": [{"_type": "span", "text": "Press item", "marks": ["l1"]}]},
               {"_type": "image", "_sanityAsset": "image@data:image/png;base64,AA"}]"#,
        )
        .unwrap();
        let (html, warning) = pt_item_body(&pt_path, &tera, &cfg).unwrap();
        assert_eq!(html, "<p>Press item</p>");
        let warning = warning.unwrap();
        assert!(warning.starts_with("warning: ") && warning.contains("index.pt.json"), "{warning}");
        assert!(warning.contains("unsafe href \"\"") && warning.contains("unsafe src \"data:"), "{warning}");

        fs::write(&pt_path, r#"[{"_type": "mystery"}]"#).unwrap();
        let err = format!("{:#}", pt_item_body(&pt_path, &tera, &cfg).unwrap_err());
        assert!(err.contains("no renderer") && err.contains("mystery"), "{err}");
    }

    #[test]
    fn offline_build_without_a_recorded_response_fails_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = sanity_site(tmp.path(), "{}");
        fs::remove_file(root.join(".cache/sanity/articles/response.json")).unwrap();
        let err = format!("{:#}", load_collections(&root.join("content"), &offline_ctx(&root)).unwrap_err());
        assert!(err.contains("collection `articles`") && err.contains("SANITY_OFFLINE=1"), "{err}");
    }

    // ---- E12: template data ---------------------------------------------

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn raw_tera(templates: &[(&str, &str)]) -> Tera {
        let mut tera = Tera::default();
        tera.add_raw_templates(templates.to_vec()).unwrap();
        tera
    }

    /// Tera autoescapes `/` in .html templates; compare on plain slashes.
    fn unslash(html: &str) -> String {
        html.replace("&#x2F;", "/")
    }

    fn plain_style() -> Styling {
        Styling { css: String::new(), hash: "h".into(), theme: "default".into() }
    }

    /// `articles` reference `authors` (one author, a list of reviewers);
    /// authors reference `teams`, which must stay a slug (one level deep).
    fn reference_site(root: &Path) {
        write(root, "content/teams/schema.yaml", "title: Teams\n");
        write(root, "content/teams/core/meta.yaml", "title: Core\npublished: true\n");
        write(
            root,
            "content/authors/schema.yaml",
            "title: Authors\nurl_prefix: /people\nreferences:\n  team: teams\n",
        );
        write(
            root,
            "content/authors/ada/meta.yaml",
            "title: Ada\ndescription: Writes guides.\npublished: true\nteam: core\nrole: Editor\n",
        );
        write(root, "content/authors/lin/meta.yaml", "title: Lin\npublished: true\nteam: core\n");
        write(
            root,
            "content/articles/schema.yaml",
            "title: Articles\nreferences:\n  author: authors\n  reviewers: authors\n",
        );
        write(
            root,
            "content/articles/edge-basics/meta.yaml",
            "title: Edge Basics\npublished: true\nauthor: ada\nreviewers: [lin, ada]\n",
        );
        write(root, "content/articles/edge-basics/index.mdx", "Body.\n");
        write(root, "content/articles/no-author/meta.yaml", "title: No Author\npublished: true\n");
    }

    fn ref_templates() -> Tera {
        raw_tera(&[
            (
                "collection-item.html",
                "{{ base_url }}|{{ collection.name }}|{{ collection.title }}|{{ collection.url }}|\
                 {% if refs is defined %}{% if refs.author %}{{ refs.author.title }}@{{ refs.author.url }}\
                 ({{ refs.author.fields.role | default(value='') }};team={{ refs.author.fields.team }};\
                 nested={{ refs.author.refs is defined }})|{% for r in refs.reviewers %}{{ r.slug }},{% endfor %}\
                 {% else %}none{% endif %}{% else %}no-refs{% endif %}",
            ),
            (
                "collection.html",
                "{{ base_url }}|{{ coll_name }}|{{ collection.name }}|{{ collection.title }}|\
                 {% for i in items %}{{ i.title }}={% if i.refs is defined and i.refs.author %}{{ i.refs.author.title }}{% else %}-{% endif %};{% endfor %}",
            ),
        ])
    }

    fn built(root: &Path) -> Vec<Collection> {
        load_collections(&root.join("content"), &offline_ctx(root)).unwrap().0
    }

    #[test]
    fn references_resolve_one_level_deep_for_items_and_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        reference_site(&root);
        let collections = built(&root);
        let mut cfg = SiteConfig::default_theme_config();
        cfg.base_url = "https://example.com".into();
        let dist = tmp.path().join("dist");
        render_collections(&ref_templates(), &dist, &collections, &plain_style(), &cfg, "").unwrap();
        let read = |rel: &str| unslash(&fs::read_to_string(dist.join(rel)).unwrap());

        assert_eq!(
            read("articles/edge-basics/index.html"),
            "https://example.com|articles|Articles|/articles/|Ada@/people/ada/(Editor;team=core;nested=false)|lin,ada,"
        );
        assert_eq!(read("articles/no-author/index.html"), "https://example.com|articles|Articles|/articles/|none");
        // Authors resolve their own team reference; teams declare none.
        assert!(read("people/ada/index.html").starts_with("https://example.com|authors|Authors|/people/|"));
        assert!(read("teams/core/index.html").ends_with("|no-refs"), "{}", read("teams/core/index.html"));
        assert_eq!(
            read("articles/index.html"),
            "https://example.com|articles|articles|Articles|Edge Basics=Ada;No Author=-;"
        );
    }

    /// `people` is a data collection (drafts, no pages); `logos` has a
    /// draft; `articles` mixes dated and undated items and references people.
    fn page_collections_site(root: &Path, expose_people: bool) {
        write(
            root,
            "content/people/schema.yaml",
            &format!("title: People\nindex: false\nexpose_to_pages: {expose_people}\n"),
        );
        write(root, "content/people/ada/meta.yaml", "title: Ada\nrole: Editor\n");
        write(root, "content/people/lin/meta.yaml", "title: Lin\nrole: Writer\n");
        write(root, "content/logos/schema.yaml", "title: Logos\n");
        write(root, "content/logos/acme/meta.yaml", "title: Acme\npublished: true\n");
        write(root, "content/logos/secret/meta.yaml", "title: Secret\npublished: false\n");
        write(
            root,
            "content/articles/schema.yaml",
            "title: Articles\nreferences:\n  author: people\n",
        );
        write(root, "content/articles/a-undated/meta.yaml", "title: Undated\npublished: true\n");
        write(root, "content/articles/b-old/meta.yaml", "title: Old\npublished: true\ndate: 2024-01-01\n");
        write(
            root,
            "content/articles/c-new/meta.yaml",
            "title: New\npublished: true\ndate: 2025-06-01\nauthor: ada\n",
        );
        write(root, "content/articles/d-draft/meta.yaml", "title: Draft\ndate: 2026-01-01\nauthor: lin\n");
    }

    fn page_with_template(root: &Path, tpl: &str) -> (Tera, Vec<Page>) {
        write(root, "content/pages/about/meta.yaml", "title: About\nslug: about\ntemplate: about.html\n");
        write(root, "content/pages/about/index.mdx", "Hi.\n");
        let tera = raw_tera(&[("about.html", tpl)]);
        (tera, load_pages(&root.join("content/pages")).unwrap())
    }

    fn render_about(root: &Path, dist: &Path, tpl: &str) -> Result<String> {
        let collections = built(root);
        let (tera, pages) = page_with_template(root, tpl);
        let mut cfg = SiteConfig::default_theme_config();
        cfg.page_collections = page_collections(&collections)?;
        render_pages(&tera, dist, &pages, &plain_style(), &cfg, "")?;
        Ok(unslash(&fs::read_to_string(dist.join("about/index.html")).unwrap()))
    }

    #[test]
    fn page_collections_hide_drafts_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, true);
        let out = render_about(
            &root,
            &tmp.path().join("dist"),
            "{{ collections.logos.title }}@{{ collections.logos.url }}:\
             {% for i in collections.logos.items %}{{ i.slug }}={{ i.url }};{% endfor %}",
        )
        .unwrap();
        assert_eq!(out, "Logos@/logos/:acme=/logos/acme/;");
    }

    #[test]
    fn page_collections_sort_newest_first_with_undated_last() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, true);
        let out = render_about(
            &root,
            &tmp.path().join("dist"),
            "{% for i in collections.articles.items %}{{ i.slug }}({{ i.date | default(value='-') }}),{% endfor %}",
        )
        .unwrap();
        // d-draft is newest but unpublished, so it is not there.
        assert_eq!(out, "c-new(2025-06-01),b-old(2024-01-01),a-undated(-),");
    }

    #[test]
    fn expose_to_pages_exposes_data_items_without_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, true);
        let out = render_about(
            &root,
            &tmp.path().join("dist"),
            "{{ collections.people.url | default(value='no-index') }}:\
             {% for p in collections.people.items %}{{ p.title }}/{{ p.fields.role }}/{{ p.published }}/{{ p.url | default(value='no-page') }};{% endfor %}",
        )
        .unwrap();
        assert_eq!(out, "no-index:Ada/Editor/false/no-page;Lin/Writer/false/no-page;");
    }

    #[test]
    fn page_collections_resolve_references() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, true);
        let out = render_about(
            &root,
            &tmp.path().join("dist"),
            "{% for i in collections.articles.items %}{{ i.slug }}:\
             {% if i.refs.author %}{{ i.refs.author.title }}/{{ i.refs.author.fields.role }}/{{ i.refs.author.url | default(value='no-page') }}{% else %}-{% endif %};{% endfor %}",
        )
        .unwrap();
        assert_eq!(out, "c-new:Ada/Editor/no-page;b-old:-;a-undated:-;");
    }

    #[test]
    fn exposed_items_cannot_reference_hidden_drafts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, true);
        // A data item pointing at a draft logo would show the draft on a page.
        write(&root, "content/people/schema.yaml", "title: People\nexpose_to_pages: true\nreferences:\n  logo: logos\n");
        write(&root, "content/people/ada/meta.yaml", "title: Ada\nlogo: secret\n");
        let err = format!("{:#}", page_collections(&built(&root)).unwrap_err());
        assert!(
            err.contains("collection `people` item `ada`") && err.contains("`secret`") && err.contains("expose_to_pages"),
            "{err}"
        );
        // Pointing at a published logo is fine.
        write(&root, "content/people/ada/meta.yaml", "title: Ada\nlogo: acme\n");
        assert!(page_collections(&built(&root)).is_ok());
    }

    #[test]
    fn expose_to_pages_must_be_a_boolean() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, true);
        write(&root, "content/people/schema.yaml", "title: People\nexpose_to_pages: yes please\n");
        let err = format!("{:#}", load_collections(&root.join("content"), &offline_ctx(&root)).unwrap_err());
        assert!(err.contains("people/schema.yaml") && err.contains("expose_to_pages"), "{err}");
    }

    #[test]
    fn builtin_pages_and_item_templates_see_collections() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        page_collections_site(&root, false);
        // Without expose_to_pages, a published article cannot cite a person.
        let err = format!("{:#}", page_collections(&built(&root)).unwrap_err());
        assert!(err.contains("`ada`") && err.contains("expose_to_pages: true"), "{err}");
        write(&root, "content/articles/c-new/meta.yaml", "title: New\npublished: true\ndate: 2025-06-01\n");
        let collections = built(&root);
        let mut cfg = SiteConfig::default_theme_config();
        cfg.page_collections = page_collections(&collections).unwrap();
        // Without expose_to_pages the draft-only people collection is empty.
        assert_eq!(cfg.page_collections["people"]["items"], serde_json::json!([]));
        let tpl = "{% for i in collections.logos.items %}{{ i.slug }}{% endfor %}";
        let tera = raw_tera(&[
            ("home.html", tpl),
            ("404.html", tpl),
            ("collection-item.html", tpl),
            ("collection.html", tpl),
        ]);
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        render_home(&tera, &dist, &[], &plain_style(), &cfg, "").unwrap();
        render_404(&tera, &dist, &plain_style(), &cfg, "").unwrap();
        render_collections(&tera, &dist, &collections, &plain_style(), &cfg, "").unwrap();
        for f in ["index.html", "404.html", "articles/c-new/index.html", "logos/index.html"] {
            assert_eq!(fs::read_to_string(dist.join(f)).unwrap(), "acme", "{f}");
        }
    }

    #[test]
    fn unknown_reference_fails_the_build_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        reference_site(&root);
        write(&root, "content/articles/edge-basics/meta.yaml", "title: Edge Basics\npublished: true\nauthor: grace\n");
        let err = format!("{:#}", load_collections(&root.join("content"), &offline_ctx(&root)).unwrap_err());
        assert!(
            err.contains("edge-basics/meta.yaml") && err.contains("`author` references `grace`") && err.contains("`authors`"),
            "{err}"
        );

        // resolve_refs names the item too, if it is ever reached unvalidated.
        reference_site(&root);
        let mut collections = built(&root);
        let articles = collections.iter_mut().find(|c| c.name == "articles").unwrap();
        articles.items[0].fields.insert("author".into(), "grace".into());
        let articles = collections.iter().find(|c| c.name == "articles").unwrap();
        let err = format!("{:#}", resolve_refs(articles, &articles.items[0], &collections).unwrap_err());
        assert!(err.contains("collection `articles` item `edge-basics`") && err.contains("`grace`"), "{err}");
    }

    #[test]
    fn pages_expose_every_meta_key_as_page_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        write(
            &root,
            "content/pages/about/meta.yaml",
            "title: About\nslug: about\nno_index: true\nseo_title: About Example\nog_image: https://example.com/og.png\n",
        );
        write(&root, "content/pages/about/index.mdx", "Hi.\n");
        write(&root, "content/pages/plain/meta.yaml", "title: Plain\nslug: plain\n");
        write(&root, "content/pages/plain/index.mdx", "Hi.\n");
        let pages = load_pages(&root.join("content/pages")).unwrap();
        let tera = raw_tera(&[(
            "page.html",
            "{{ base_url }}|{{ page.fields.seo_title | default(value=title) }}|\
             {{ page.fields.og_image | default(value='') }}|{{ page.fields.no_index | default(value=false) }}|{{ page.fields.slug }}",
        )]);
        let mut cfg = SiteConfig::default_theme_config();
        cfg.base_url = "https://example.com".into();
        let dist = tmp.path().join("dist");
        render_pages(&tera, &dist, &pages, &plain_style(), &cfg, "").unwrap();
        assert_eq!(
            unslash(&fs::read_to_string(dist.join("about/index.html")).unwrap()),
            "https://example.com|About Example|https://example.com/og.png|true|about"
        );
        assert_eq!(
            unslash(&fs::read_to_string(dist.join("plain/index.html")).unwrap()),
            "https://example.com|Plain||false|plain"
        );
    }

    #[test]
    fn builtin_templates_get_base_url() {
        let tmp = tempfile::tempdir().unwrap();
        let tera = raw_tera(&[
            ("home.html", "home {{ base_url }}"),
            ("blog.html", "blog {{ base_url }}"),
            ("contact.html", "contact {{ base_url }}"),
            ("404.html", "404 {{ base_url }}"),
        ]);
        let mut cfg = SiteConfig::default_theme_config();
        cfg.base_url = "https://example.com".into();
        let dist = tmp.path();
        render_home(&tera, dist, &[], &plain_style(), &cfg, "").unwrap();
        render_blog(&tera, dist, &[], &plain_style(), &cfg, "").unwrap();
        render_contact(&tera, dist, &plain_style(), &cfg, "").unwrap();
        render_404(&tera, dist, &plain_style(), &cfg, "").unwrap();
        for (file, want) in [
            ("index.html", "home https://example.com"),
            ("blog/index.html", "blog https://example.com"),
            ("contact/index.html", "contact https://example.com"),
            ("404.html", "404 https://example.com"),
        ] {
            assert_eq!(unslash(&fs::read_to_string(dist.join(file)).unwrap()), want);
        }
        // site.yaml `base_url` parses straight into the config too.
        assert_eq!(test_cfg("base_url: https://example.com\n").base_url, "https://example.com");
    }

    #[test]
    fn sitemap_skips_no_index_and_redirect_sources() {
        let (mut pages, mut colls) = sample_content();
        let none = std::collections::HashSet::new();
        let before = sitemap_xml("https://example.com", &[], &pages, &colls, true, &none);
        assert!(before.contains("<loc>https://example.com/docs/ops/</loc>"));
        assert!(before.contains("<loc>https://example.com/events/speakers/acme/</loc>"));
        assert!(before.contains("<loc>https://example.com/blog/</loc>"));

        pages[1].meta.fields.insert("no_index".into(), true.into());
        colls[0].items[0].fields.insert("no_index".into(), true.into());
        let redirects: std::collections::HashSet<String> =
            ["blog".to_string(), "".to_string()].into_iter().collect();
        let after = sitemap_xml("https://example.com", &[], &pages, &colls, true, &redirects);
        assert!(!after.contains("/docs/ops/"), "{after}");
        assert!(!after.contains("/events/speakers/acme/"), "{after}");
        assert!(!after.contains("<loc>https://example.com/blog/</loc>"), "{after}");
        assert!(!after.contains("<loc>https://example.com/</loc>"), "{after}");
        assert!(after.contains("<loc>https://example.com/contact/</loc>"), "{after}");
        assert!(after.contains("<loc>https://example.com/events/speakers/</loc>"), "{after}");

        // `no_index: false` and string values change nothing.
        pages[1].meta.fields.insert("no_index".into(), false.into());
        colls[0].items[0].fields.insert("no_index".into(), "true".into());
        assert_eq!(sitemap_xml("https://example.com", &[], &pages, &colls, true, &none), before);
    }

    #[test]
    fn llms_skips_no_index_pages_and_items() {
        let cfg = test_cfg("title: Example\nbuiltin_pages: false\n");
        let (mut pages, mut colls) = sample_content();
        pages[1].meta.fields.insert("no_index".into(), true.into());
        colls[0].items[0].fields.insert("no_index".into(), true.into());
        let out = llms_txt(&cfg, "https://example.com", &[], &pages, &colls);
        assert!(!out.contains("/docs/ops/"), "{out}");
        assert!(!out.contains("/events/speakers/acme/"), "{out}");
        assert!(out.contains("- [Home](https://example.com/)\n"), "{out}");
        // As in the sitemap, the index page stays while a published item exists.
        assert!(out.contains("## Speakers\n\n- [Speakers](https://example.com/events/speakers/)\n"), "{out}");

        // `no_index: false` and string values change nothing.
        let before = llms_txt(&cfg, "https://example.com", &[], &sample_content().0, &sample_content().1);
        pages[1].meta.fields.insert("no_index".into(), false.into());
        colls[0].items[0].fields.insert("no_index".into(), "true".into());
        assert_eq!(llms_txt(&cfg, "https://example.com", &[], &pages, &colls), before);
    }

    #[test]
    fn llms_and_sitemap_list_the_same_pages_and_items() {
        let cfg = test_cfg("title: Example\nbuiltin_pages: false\n");
        let (mut pages, mut colls) = sample_content();
        pages[0].meta.fields.insert("no_index".into(), true.into());
        colls[0].items[0].fields.insert("no_index".into(), true.into());
        colls[0].items.push(CollectionItem {
            slug: "beta".into(),
            title: "BETA".into(),
            description: String::new(),
            date: None,
            published: true,
            fields: Default::default(),
            dir: PathBuf::new(),
        });
        let llms = llms_txt(&cfg, "https://example.com", &[], &pages, &colls);
        let xml = sitemap_xml("https://example.com", &[], &pages, &colls, false, &Default::default());
        let mut from_llms: Vec<&str> = llms
            .lines()
            .filter_map(|l| l.strip_prefix("- [")?.split_once("](")?.1.split_once(')').map(|(u, _)| u))
            .filter(|u| !u.ends_with("/mcp"))
            .collect();
        let mut from_xml: Vec<&str> = xml
            .lines()
            .filter_map(|l| l.strip_prefix("<url><loc>")?.split_once("</loc>").map(|(u, _)| u))
            .collect();
        from_llms.sort();
        from_xml.sort();
        assert_eq!(
            from_llms,
            vec![
                "https://example.com/docs/ops/",
                "https://example.com/events/speakers/",
                "https://example.com/events/speakers/beta/"
            ]
        );
        assert_eq!(from_llms, from_xml);
    }

    #[test]
    fn indexable_items_skip_drafts_and_no_index() {
        let (_, mut colls) = sample_content();
        let slugs = |c: &Collection| indexable_items(c).map(|i| i.slug.clone()).collect::<Vec<_>>();
        assert_eq!(slugs(&colls[0]), vec!["acme"]);
        colls[0].items[0].fields.insert("no_index".into(), "true".into());
        assert_eq!(slugs(&colls[0]), vec!["acme"]);
        colls[0].items[0].fields.insert("no_index".into(), true.into());
        assert!(slugs(&colls[0]).is_empty());
    }

    #[test]
    fn search_index_skips_no_index_items() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, mut colls) = sample_content();
        colls[0].items.push(CollectionItem {
            slug: "hidden".into(),
            title: "HIDDEN".into(),
            description: String::new(),
            date: None,
            published: true,
            fields: [("no_index".to_string(), serde_json::Value::Bool(true))].into_iter().collect(),
            dir: PathBuf::new(),
        });
        render_search_index(tmp.path(), tmp.path(), &[], &colls).unwrap();
        let idx: Vec<serde_json::Value> =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("search-index.json")).unwrap()).unwrap();
        let titles: Vec<&str> = idx.iter().map(|r| r["title"].as_str().unwrap()).collect();
        assert_eq!(titles, vec!["ACME"]);
    }

    #[test]
    fn redirect_sources_load_slash_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_redirect_sources(tmp.path()).unwrap().is_empty());
        write(tmp.path(), "redirects.yaml", "# comment only\n");
        assert!(load_redirect_sources(tmp.path()).unwrap().is_empty());
        write(tmp.path(), "redirects.yaml", "/old-page/: {to: /new-page/, status: 302}\n/events/old: /events/\n");
        let got = load_redirect_sources(tmp.path()).unwrap();
        assert!(got.contains("old-page") && got.contains("events/old") && got.len() == 2, "{got:?}");
        write(tmp.path(), "redirects.yaml", "- /a\n");
        let err = format!("{:#}", load_redirect_sources(tmp.path()).unwrap_err());
        assert!(err.contains("redirects.yaml") && err.contains("mapping"), "{err}");
    }

    #[test]
    fn published_items_cannot_reference_drafts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        reference_site(&root);
        write(&root, "content/authors/ada/meta.yaml", "title: Ada\npublished: false\nteam: core\n");
        let collections = built(&root);
        let articles = collections.iter().find(|c| c.name == "articles").unwrap();
        let live = articles.items.iter().find(|i| i.slug == "edge-basics").unwrap();
        let err = format!("{:#}", resolve_refs(articles, live, &collections).unwrap_err());
        assert!(
            err.contains("collection `articles` item `edge-basics`") && err.contains("`ada`") && err.contains("unpublished"),
            "{err}"
        );
        let dist = tmp.path().join("dist");
        let cfg = SiteConfig::default_theme_config();
        let err = format!(
            "{:#}",
            render_collections(&ref_templates(), &dist, &collections, &plain_style(), &cfg, "").unwrap_err()
        );
        assert!(err.contains("unpublished"), "{err}");

        // A draft may reference a draft.
        let mut draft = articles.items.iter().find(|i| i.slug == "edge-basics").unwrap().fields.clone();
        draft.insert("published".into(), false.into());
        let draft_item = CollectionItem {
            slug: "draft-piece".into(),
            title: "Draft".into(),
            description: String::new(),
            date: None,
            published: false,
            fields: draft,
            dir: PathBuf::new(),
        };
        let refs = resolve_refs(articles, &draft_item, &collections).unwrap().unwrap();
        assert_eq!(refs["author"]["title"], "Ada");
        assert_eq!(refs["author"]["published"], false);
    }

    #[test]
    fn sitemap_locs_are_xml_escaped() {
        let (pages, mut colls) = sample_content();
        colls[0].items[0].slug = "a&b".into();
        let xml = sitemap_xml("https://example.com", &[], &pages, &colls, false, &Default::default());
        assert!(xml.contains("<loc>https://example.com/events/speakers/a&amp;b/</loc>"), "{xml}");
        assert!(!xml.contains("a&b"), "{xml}");
    }

    #[test]
    fn base_url_must_be_a_plain_origin() {
        for ok in ["https://example.com", "http://localhost:8787", "https://www.example.com:443"] {
            assert_eq!(validate_base_url(ok).unwrap(), ok);
        }
        for bad in [
            "example.com",
            "ftp://example.com",
            "https://",
            "https://example.com/path",
            "https://exa mple.com",
            "https://example.com\"",
            "https://example.com\n",
            "https://user@example.com",
            "https://example.com:",
            "https://example.com:80a",
            "javascript:alert(1)",
        ] {
            let err = format!("{:#}", validate_base_url(bad).unwrap_err());
            assert!(err.contains("base_url") && err.contains("http(s)://host[:port]"), "{bad}: {err}");
        }
    }

    #[test]
    fn optimize_images_leaves_pipeline_outputs_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1700, 10).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        for d in ["assets/img", "other"] {
            fs::create_dir_all(dist.join(d)).unwrap();
            fs::write(dist.join(d).join("a.png"), &png).unwrap();
        }
        optimize_images(dist, Some(&dist.join("assets/img"))).unwrap();
        assert_eq!(fs::read(dist.join("assets/img/a.png")).unwrap(), png);
        assert_eq!(image::image_dimensions(dist.join("other/a.png")).unwrap(), (1600, 9));
    }

    #[test]
    fn args_accept_only_known_flags() {
        let args = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert_eq!(args(&[]), Ok(Args::Build(BuildOpts { force_clean: false })));
        assert_eq!(args(&["--force-clean"]), Ok(Args::Build(BuildOpts { force_clean: true })));
        assert_eq!(args(&["-h"]), Ok(Args::Help));
        assert_eq!(args(&["--force-clean", "--help"]), Ok(Args::Help));
        assert_eq!(args(&["--version"]), Ok(Args::Version));
        assert!(args(&["-V"]).unwrap_err().contains("unknown option"));
        assert!(args(&["--force-clean=1"]).is_err());
        assert!(args(&["."]).unwrap_err().contains("unexpected argument"));
    }

    #[test]
    fn prepare_dist_clears_only_marked_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // missing: created with the marker
        let dist = prepare_dist(root, false).unwrap();
        assert_eq!(fs::read_to_string(dist.join(DIST_MARKER)).unwrap(), DIST_MARKER_BODY);
        // marked: cleared
        fs::write(dist.join("old.html"), "x").unwrap();
        prepare_dist(root, false).unwrap();
        assert!(!dist.join("old.html").exists());
        // unmarked: refused, untouched
        fs::remove_file(dist.join(DIST_MARKER)).unwrap();
        fs::write(dist.join("mine.txt"), "x").unwrap();
        let err = format!("{:#}", prepare_dist(root, false).unwrap_err());
        assert!(err.contains(&dist.display().to_string()), "{err}");
        assert!(dist.join("mine.txt").exists());
        // a directory named like the marker is not a marker
        fs::create_dir(dist.join(DIST_MARKER)).unwrap();
        assert!(prepare_dist(root, false).is_err());
        assert!(dist.join("mine.txt").exists());
        // force: cleared
        prepare_dist(root, true).unwrap();
        assert!(!dist.join("mine.txt").exists());
        assert!(dist.join(DIST_MARKER).is_file());
    }
}

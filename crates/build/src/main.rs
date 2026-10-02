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
        }
    }
    fn base_ctx(&self, style: &Styling) -> TeraContext {
        let mut ctx = TeraContext::new();
        ctx.insert("css_hash", &style.hash);
        ctx.insert("style", &style.css);
        ctx.insert("site_title", &self.title);
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
    fn url_path(&self) -> String {
        format!("/posts/{}", self.meta.slug)
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
    let mut tera = Tera::new(
        templates_dir
            .join("**/*.html")
            .to_str()
            .context("templates path is not valid UTF-8")?,
    )
    .context("failed to compile templates")?;
    // Theme template overrides win over the repo defaults.
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
    let style = compile_theme_css(&root, &theme_dir, &theme_name)?;
    let css_hash = sha256_hex(style.as_bytes());
    let styling = Styling {
        css: style,
        hash: css_hash,
    };

    let cfg = SiteConfig::load(&root);
    let posts = load_posts(&root.join("content/posts"))?;
    if posts.is_empty() {
        bail!("no posts found under content/posts");
    }
    // Default pages (about, privacy, terms, ...) — optional directory.
    let pages = load_pages(&root.join("content/pages")).unwrap_or_default();

    render_posts(&tera, &dist, &posts, &styling, &cfg)?;
    render_pages(&tera, &dist, &pages, &styling, &cfg)?;
    render_home(&tera, &dist, &posts, &styling, &cfg)?;
    render_blog(&tera, &dist, &posts, &styling, &cfg)?;
    render_contact(&tera, &dist, &styling, &cfg)?;
    render_404(&tera, &dist, &styling, &cfg)?;
    render_rss(&root, &dist, &posts)?;
    render_sitemap(&root, &dist, &posts, &pages)?;
    render_robots(&dist)?;
    render_llms_txt(&root, &dist, &posts)?;

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

/// Compiled CSS + its content hash, threaded through every render step.
struct Styling {
    css: String,
    hash: String,
}

fn read_site_theme(root: &Path) -> Option<String> {
    let raw = fs::read_to_string(root.join("site.yaml")).ok()?;
    let val: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    val.get("theme")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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

fn render_posts(
    tera: &Tera,
    dist: &Path,
    posts: &[Post],
    style: &Styling,
    cfg: &SiteConfig,
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
        ctx.extend(cfg.base_ctx(style));
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
) -> Result<()> {
    for page in pages {
        if !page.meta.published {
            continue;
        }
        let md = fs::read_to_string(page.dir.join("index.mdx"))
            .with_context(|| format!("read {}", page.dir.join("index.mdx").display()))?;
        let body = render_markdown(&md)?;
        let mut ctx = page.meta.to_context(&body);
        ctx.extend(cfg.base_ctx(style));
        let html = tera.render("page.html", &ctx).context("render page.html")?;
        let out_dir = dist.join(&page.meta.slug);
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
) -> Result<()> {
    let recent: Vec<serde_json::Value> = posts
        .iter()
        .filter(|p| p.meta.published)
        .take(3)
        .map(Post::summary)
        .collect();
    let mut ctx = cfg.base_ctx(style);
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
) -> Result<()> {
    let listed: Vec<serde_json::Value> = posts
        .iter()
        .filter(|p| p.meta.published)
        .map(Post::summary)
        .collect();
    let mut ctx = cfg.base_ctx(style);
    ctx.insert("posts", &listed);
    let html = tera.render("blog.html", &ctx).context("render blog.html")?;
    let out_dir = dist.join("blog");
    fs::create_dir_all(&out_dir).context("mkdir blog/")?;
    fs::write(out_dir.join("index.html"), html).context("write blog/index.html")?;
    Ok(())
}

/// Contact page — template-driven from site.yaml `contact_email` (static-first:
/// a mailto link, no backend form to run).
fn render_contact(tera: &Tera, dist: &Path, style: &Styling, cfg: &SiteConfig) -> Result<()> {
    let mut ctx = cfg.base_ctx(style);
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

fn render_404(tera: &Tera, dist: &Path, style: &Styling, cfg: &SiteConfig) -> Result<()> {
    let ctx = cfg.base_ctx(style);
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

fn render_sitemap(root: &Path, dist: &Path, posts: &[Post], pages: &[Page]) -> Result<()> {
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let live_pages: Vec<&Page> = pages.iter().filter(|p| p.meta.published).collect();
    let site_base = site_base_url(root);
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    xml.push_str(&format!("<url><loc>{}/</loc></url>\n", site_base));
    xml.push_str(&format!("<url><loc>{}/blog/</loc></url>\n", site_base));
    xml.push_str(&format!("<url><loc>{}/contact/</loc></url>\n", site_base));
    for page in &live_pages {
        xml.push_str(&format!("<url><loc>{}/{}/</loc></url>\n", site_base, page.meta.slug));
    }
    for post in published {
        xml.push_str(&format!(
            "<url><loc>{}</loc><lastmod>{}</lastmod></url>\n",
            format!("{}{}", site_base, post.url_path()),
            post.meta.date
        ));
    }
    xml.push_str("</urlset>\n");
    fs::write(dist.join("sitemap.xml"), xml).context("write sitemap.xml")?;
    Ok(())
}

fn render_robots(dist: &Path) -> Result<()> {
    fs::write(
        dist.join("robots.txt"),
        "User-agent: *\nAllow: /\nSitemap: /sitemap.xml\n",
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
    fs::write(dist.join("llms.txt"), out).context("write llms.txt")?;
    Ok(())
}

fn site_base_url(_root: &Path) -> String {
    std::env::var("SITE_BASE_URL").unwrap_or_else(|_| "http://localhost:8787".to_string())
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

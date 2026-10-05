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
    let posts = load_posts(&root.join("content/posts"))?;
    if posts.is_empty() {
        bail!("no posts found under content/posts");
    }
    // Default pages (about, privacy, terms, ...) — optional directory.
    let pages = load_pages(&root.join("content/pages")).unwrap_or_default();

    // Live site renders at the site root.
    render_posts(&tera, &dist, &posts, &styling, &cfg, "")?;
    render_pages(&tera, &dist, &pages, &styling, &cfg, "")?;
    render_home(&tera, &dist, &posts, &styling, &cfg, "")?;
    render_blog(&tera, &dist, &posts, &styling, &cfg, "")?;
    render_contact(&tera, &dist, &styling, &cfg, "")?;
    render_404(&tera, &dist, &styling, &cfg, "")?;
    render_rss(&root, &dist, &posts)?;
    render_sitemap(&root, &dist, &posts, &pages)?;
    render_robots(&dist, &site_base_url(&root))?;
    render_llms_txt(&root, &dist, &posts)?;
    render_mcp_card(&root, &dist)?;
    render_search_index(&root, &dist, &posts)?;
    render_posts_markdown(&dist, &posts)?;
    render_embeddings(&root, &dist, &posts)?;
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
fn render_search_index(root: &Path, dist: &Path, posts: &[Post]) -> Result<()> {
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
            "url": format!("{}{}/", site_base, post.url_path()),
            "date": post.meta.date.to_string(),
            "tags": post.meta.tags,
            "body": markdown_to_text(&md),
        }));
    }
    fs::write(dist.join("search-index.json"), serde_json::to_vec_pretty(&idx)?)
        .with_context(|| "write search-index.json".to_string())?;
    Ok(())
}

/// Tier 3: embed per-post chunks at build time into dist/embeddings.json.
/// Uses fastembed (ONNX on CPU, weights cached in ~/.cache) — no API key.
/// Skips gracefully when the model can't be fetched (offline), leaving
/// /api/ask in Tier-2 inline mode.
fn render_embeddings(root: &Path, dist: &Path, posts: &[Post]) -> Result<()> {
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    if published.is_empty() {
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
                    "url": format!("{}{}/", site_base, post.url_path()),
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

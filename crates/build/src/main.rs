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

    let templates_dir = root.join("templates");
    let tera = Tera::new(
        templates_dir
            .join("**/*.html")
            .to_str()
            .context("templates path is not valid UTF-8")?,
    )
    .context("failed to compile templates")?;

    let css_hash = hash_file(&templates_dir.join("base.html"))?;

    let posts = load_posts(&root.join("content/posts"))?;
    if posts.is_empty() {
        bail!("no posts found under content/posts");
    }

    render_posts(&tera, &dist, &posts, &css_hash)?;
    render_index(&tera, &dist, &posts, &css_hash)?;
    render_404(&tera, &dist, &css_hash)?;
    render_rss(&root, &dist, &posts)?;
    render_sitemap(&root, &dist, &posts)?;
    render_robots(&dist)?;
    render_llms_txt(&root, &dist, &posts)?;

    let drafts = posts.iter().filter(|p| !p.meta.published).count();
    eprintln!(
        "built dist/: {} posts ({} drafts skipped), index, 404, rss.xml, sitemap.xml, robots.txt, llms.txt",
        posts.len() - drafts,
        drafts
    );
    Ok(())
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
    css_hash: &str,
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
        ctx.insert("css_hash", css_hash);
        let html = tera
            .render("post.html", &ctx)
            .context("render post.html")?;
        let out_dir = dist.join("posts").join(&post.meta.slug);
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;
        fs::write(out_dir.join("index.html"), html)
            .with_context(|| format!("write {}/index.html", out_dir.display()))?;
    }
    Ok(())
}

fn render_index(tera: &Tera, dist: &Path, posts: &[Post], css_hash: &str) -> Result<()> {
    let listed: Vec<serde_json::Value> = posts
        .iter()
        .filter(|p| p.meta.published)
        .map(Post::summary)
        .collect();
    let mut ctx = TeraContext::new();
    ctx.insert("posts", &listed);
    ctx.insert("css_hash", css_hash);
    ctx.insert("lang", "en");
    let html = tera.render("index.html", &ctx).context("render index.html")?;
    fs::write(dist.join("index.html"), html).context("write index.html")?;
    Ok(())
}

fn render_404(tera: &Tera, dist: &Path, css_hash: &str) -> Result<()> {
    let mut ctx = TeraContext::new();
    ctx.insert("css_hash", css_hash);
    ctx.insert("lang", "en");
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

fn render_sitemap(root: &Path, dist: &Path, posts: &[Post]) -> Result<()> {
    let published: Vec<&Post> = posts.iter().filter(|p| p.meta.published).collect();
    let site_base = site_base_url(root);
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    xml.push_str(&format!("<url><loc>{}/</loc></url>\n", site_base));
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

/// SHA-256 hex digest of a file, used as the content-hash CSS marker.
fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(sha256_hex(&bytes))
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

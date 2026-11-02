//! DRAFTS=1 renders every collection item with the same template data a
//! published item page gets (url, slug, collection, date, fields, refs,
//! body), so an item template that works on the site works for a draft. The
//! draft's `url` is its preview path under /preview/drafts/, and a build
//! without DRAFTS writes exactly what it wrote before.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            fs::copy(e.path(), t).unwrap();
        }
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// An item template that prints every field a published item page gets.
/// Tera fails the render on any undefined variable it prints.
const ITEM_TEMPLATE: &str = r#"<!DOCTYPE html><html><head><title>{{ title }}</title>
<link rel="canonical" href="{{ base_url }}{{ url }}"></head><body>
<p id="url">{{ url }}</p>
<p id="meta">{{ slug }}|{{ collection.name }}|{{ collection.title }}|{{ collection.url }}|{{ date }}|{{ description }}|{{ fields.kicker }}|{{ refs.author.title }}|{{ refs.author.url }}</p>
{% if draft %}<p id="draft">notes:{{ agent_notes }}</p>{% endif %}
<main>{{ body | safe }}</main></body></html>
"#;

/// A site with a `notes` collection (public prefix /journal, a reference to
/// `people`), one published note, one Markdown draft with a date and
/// agent_notes, and one Portable Text draft with no date. Published items set
/// `no_index: true`, so the build loads no embedding model (no network).
fn site(root: &Path, extra_site_yaml: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    write(
        root,
        "site.yaml",
        &format!("theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\n{extra_site_yaml}"),
    );
    write(root, "templates/note.html", ITEM_TEMPLATE);
    write(root, "content/people/schema.yaml", "title: People\nindex: false\n");
    write(root, "content/people/ada/meta.yaml", "title: Ada\npublished: true\nno_index: true\n");
    write(root, "content/people/ada/index.mdx", "Ada.\n");
    write(
        root,
        "content/notes/schema.yaml",
        "title: Notes\nurl_prefix: /journal\nindex: false\nitem_template: note.html\nreferences:\n  author: people\n",
    );
    write(
        root,
        "content/notes/open/meta.yaml",
        "title: Open\npublished: true\nno_index: true\ndate: 2026-01-01\ndescription: Shown.\nkicker: News\nauthor: ada\n",
    );
    write(root, "content/notes/open/index.mdx", "Open body.\n");
    write(
        root,
        "content/notes/soon/meta.yaml",
        "title: Soon\ndate: 2026-02-01\ndescription: Coming.\nkicker: Preview\nauthor: ada\nagent_notes: Check the dates.\n",
    );
    write(root, "content/notes/soon/index.mdx", "Soon body.\n");
    write(
        root,
        "content/notes/later/meta.yaml",
        "title: Later\ndescription: Undated.\nkicker: Someday\nauthor: ada\n",
    );
    write(
        root,
        "content/notes/later/index.pt.json",
        r#"[{"_type": "block", "style": "normal", "markDefs": [], "children": [{"_type": "span", "text": "Later body.", "marks": []}]}]"#,
    );
}

fn build(root: &Path, drafts: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_build-site"));
    cmd.current_dir(root)
        .env("SITE_ROOT", root)
        .env_remove("SITE_PREVIEW_THEMES")
        .env_remove("SITE_THEME")
        .env_remove("SITE_BASE_URL");
    if drafts {
        cmd.env("DRAFTS", "1");
    } else {
        cmd.env_remove("DRAFTS");
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "build-site (DRAFTS={drafts}) failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join("dist").join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn between<'a>(html: &'a str, open: &str) -> &'a str {
    let start = html.find(open).unwrap_or_else(|| panic!("no {open} in {html}")) + open.len();
    &html[start..start + html[start..].find("</p>").unwrap()]
}

/// Every file under dist/ outside preview/, by relative path.
fn public_files(dist: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(base).unwrap().to_path_buf();
            if rel.starts_with("preview") {
                continue;
            }
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dist, dist, &mut out);
    out
}

#[test]
fn draft_items_render_with_the_published_item_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    site(root, "");
    build(root, true);

    // The published item and its public page are unchanged in shape.
    let live = read(root, "journal/open/index.html");
    assert_eq!(between(&live, "<p id=\"url\">"), "/journal/open/");
    assert_eq!(
        between(&live, "<p id=\"meta\">"),
        "open|notes|Notes|/journal/|2026-01-01|Shown.|News|Ada|/people/ada/"
    );
    assert!(!live.contains("id=\"draft\""), "{live}");

    // A Markdown draft gets the same fields, with the preview url.
    let soon = read(root, "preview/drafts/notes/soon/index.html");
    assert_eq!(between(&soon, "<p id=\"url\">"), "/preview/drafts/notes/soon/");
    assert!(soon.contains("href=\"https://www.example.com/preview/drafts/notes/soon/\""), "{soon}");
    assert_eq!(
        between(&soon, "<p id=\"meta\">"),
        "soon|notes|Notes|/journal/|2026-02-01|Coming.|Preview|Ada|/people/ada/"
    );
    assert_eq!(between(&soon, "<p id=\"draft\">"), "notes:Check the dates.");
    assert!(soon.contains("<p>Soon body.</p>"), "{soon}");

    // A Portable Text draft with no date and no agent_notes: the body renders
    // as it would when published, and the missing values are empty strings.
    let later = read(root, "preview/drafts/notes/later/index.html");
    assert_eq!(between(&later, "<p id=\"url\">"), "/preview/drafts/notes/later/");
    assert_eq!(
        between(&later, "<p id=\"meta\">"),
        "later|notes|Notes|/journal/||Undated.|Someday|Ada|/people/ada/"
    );
    assert_eq!(between(&later, "<p id=\"draft\">"), "notes:");
    assert!(later.contains("<p>Later body.</p>"), "{later}");

    // The published item's preview copy carries the preview url too.
    let open = read(root, "preview/drafts/notes/open/index.html");
    assert_eq!(between(&open, "<p id=\"url\">"), "/preview/drafts/notes/open/");

    // Drafts have no public page.
    for slug in ["soon", "later"] {
        assert!(!root.join("dist/journal").join(slug).exists(), "public page for draft {slug}");
    }
}

#[test]
fn draft_url_is_under_preview_and_follows_the_slash_setting() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    site(root, "trailing_slash: false\n");
    build(root, true);
    let soon = read(root, "preview/drafts/notes/soon/index.html");
    let url = between(&soon, "<p id=\"url\">");
    assert_eq!(url, "/preview/drafts/notes/soon");
    assert!(url.starts_with("/preview/"), "{url}");
    // Every draft url names the preview, never the public prefix.
    for slug in ["open", "soon", "later"] {
        let html = read(root, &format!("preview/drafts/notes/{slug}/index.html"));
        let url = between(&html, "<p id=\"url\">");
        assert!(url.starts_with("/preview/drafts/") && !url.contains("/journal"), "{slug}: {url}");
    }
}

#[test]
fn drafts_leave_the_public_build_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    let drafts = tmp.path().join("drafts");
    site(&plain, "");
    site(&drafts, "");
    build(&plain, false);
    build(&drafts, true);
    assert!(!plain.join("dist/preview").exists(), "a plain build wrote preview/");
    assert!(drafts.join("dist/preview/drafts/notes/soon/index.html").exists());
    let a = public_files(&plain.join("dist"));
    let b = public_files(&drafts.join("dist"));
    assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
    for (rel, bytes) in &a {
        assert!(b[rel] == *bytes, "{} differs between a plain and a DRAFTS=1 build", rel.display());
    }
}

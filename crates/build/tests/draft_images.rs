//! DRAFTS=1 builds: an image only unpublished items use is written under
//! the gated preview directory and nowhere public; an image a published
//! item uses stays public. Sources are remote URLs served from a seeded
//! image cache (IMAGES_OFFLINE=1), so the tests need no network.
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHARED: &str = "https://img.example.com/shared.png";
const SECRET: &str = "https://img.example.com/unannounced.png";
const GATED: &str = "dist/preview/drafts/assets/img";
const PUBLIC: &str = "dist/assets/img";

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

fn hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// Put `url` in the image cache as if an earlier build had fetched it.
fn seed(root: &Path, url: &str, rgb: [u8; 3]) {
    let img = image::RgbImage::from_pixel(100, 50, image::Rgb(rgb));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let bytes = png.into_inner();
    let content = hex(&bytes);
    write(root, &format!(".cache/images/urls/{}", hex(url.as_bytes())), &content);
    fs::create_dir_all(root.join(".cache/images/src")).unwrap();
    fs::write(root.join(".cache/images/src").join(&content), bytes).unwrap();
}

/// Collection `notes`: `open` (published, the shared cover) and `soon`
/// (a draft with the shared cover inline and its own unannounced cover).
/// Published items carry `no_index: true` so the build loads no embedding
/// model; the index outputs are checked with indexable items in main.rs.
fn site(root: &Path, soon_published: bool) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    write(root, "content/pages/about/meta.yaml", "title: About\nslug: about\npublished: true\n");
    write(root, "content/pages/about/index.mdx", "Hello.\n");
    write(
        root,
        "site.yaml",
        "theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\n\
         images:\n  widths: [32, 64]\n  allowed_hosts: [img.example.com]\n",
    );
    write(root, "content/notes/schema.yaml", "title: Notes\n");
    write(root, "content/notes/open/meta.yaml", &format!("title: Open\nslug: open\npublished: true\nno_index: true\ncover: {SHARED}\n"));
    write(
        root,
        "content/notes/soon/meta.yaml",
        &format!("title: Soon\nslug: soon\npublished: {soon_published}\nno_index: true\ncover: {SECRET}\ninline: {SHARED}\n"),
    );
    let tpl = fs::read_to_string(root.join("templates/collection-item.html")).unwrap().replace(
        "<div class=\"prose\">",
        "{% if fields.cover %}{{ image(src=fields.cover, alt=title) }}\
         <meta property=\"og:image\" content=\"{{ fields.cover | image_url(width=64, absolute=true) }}\">{% endif %}\
         {% if fields.inline %}{{ image(src=fields.inline, alt=\"\") }}{% endif %}<div class=\"prose\">",
    );
    write(root, "templates/collection-item.html", &tpl);
    seed(root, SHARED, [10, 120, 200]);
    seed(root, SECRET, [200, 30, 30]);
}

fn build(root: &Path, drafts: bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_build-site"));
    cmd.env("SITE_ROOT", root).env("IMAGES_OFFLINE", "1").env_remove("SITE_PREVIEW_THEMES");
    if drafts {
        cmd.env("DRAFTS", "1");
    } else {
        cmd.env_remove("DRAFTS");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Output ids (16 hex digits) of the images a page links under `dir`.
fn ids_in(html: &str, dir: &str) -> Vec<String> {
    let needle = format!("/{dir}/");
    let mut ids: Vec<String> = html
        .match_indices(&needle)
        .map(|(at, _)| html[at + needle.len()..].chars().take(16).collect::<String>())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

/// Every file outside dist/preview/ (and every .gz copy, unzipped) that
/// names `needle` in its path or contents.
fn public_hits(root: &Path, needle: &str) -> Vec<String> {
    let dist = root.join("dist");
    let mut hits = Vec::new();
    for f in files(&dist) {
        if f.starts_with(dist.join("preview")) {
            continue;
        }
        let rel = f.strip_prefix(&dist).unwrap().display().to_string();
        let raw = fs::read(&f).unwrap();
        let text = if rel.ends_with(".gz") {
            let mut s = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&raw[..]), &mut s).unwrap();
            s
        } else {
            raw
        };
        if rel.contains(needle) || String::from_utf8_lossy(&text).contains(needle) {
            hits.push(rel);
        }
    }
    hits
}

#[test]
fn draft_only_images_are_written_under_the_gated_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    site(root, false);
    build(root, true);

    let open = fs::read_to_string(root.join("dist/notes/open/index.html")).unwrap();
    let shared = ids_in(&open, "assets/img");
    assert_eq!(shared.len(), 1, "{open}");
    let shared = &shared[0];
    let draft = fs::read_to_string(root.join("dist/preview/drafts/notes/soon/index.html")).unwrap();
    let secret = ids_in(&draft, "preview/drafts/assets/img");
    assert_eq!(secret.len(), 1, "{draft}");
    let secret = &secret[0];
    assert_ne!(secret, shared);

    // the draft-only image: under the gate, the draft links it there (img, srcset, og:image)
    let gated = names(&root.join(GATED));
    assert!(!gated.is_empty() && gated.iter().all(|n| n.starts_with(secret.as_str())), "{gated:?}");
    assert!(draft.contains(&format!("src=\"/preview/drafts/assets/img/{secret}-64.jpg\"")), "{draft}");
    assert!(draft.contains(&format!("content=\"https://www.example.com/preview/drafts/assets/img/{secret}-64.jpg\"")), "{draft}");
    // the shared image: public, and the draft links the public copy
    let public = names(&root.join(PUBLIC));
    assert!(!public.is_empty() && public.iter().all(|n| n.starts_with(shared.as_str())), "{public:?}");
    assert!(draft.contains(&format!("/assets/img/{shared}-64.jpg")), "{draft}");
    assert!(!draft.contains(&format!("/preview/drafts/assets/img/{shared}")), "{draft}");
    // gated files are the cached, content-addressed variants: same bytes as the cache
    for n in &gated {
        let cached = fs::read(root.join(".cache/images/out").join(n)).unwrap();
        assert_eq!(fs::read(root.join(GATED).join(n)).unwrap(), cached, "{n}");
    }

    // nothing public names the draft-only image: not its id, URL, or content hash
    for needle in [secret.as_str(), SECRET, "unannounced"] {
        assert!(public_hits(root, needle).is_empty(), "{needle}: {:?}", public_hits(root, needle));
    }
}

#[test]
fn public_build_has_no_draft_only_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    site(root, false);
    build(root, true);
    let draft = fs::read_to_string(root.join("dist/preview/drafts/notes/soon/index.html")).unwrap();
    let secret = ids_in(&draft, "preview/drafts/assets/img")[0].clone();
    // E75: a drafts build marks its dist so a public origin refuses it.
    assert!(root.join("dist/.post-cms-drafts").is_file());

    build(root, false);
    assert!(!root.join("dist/.post-cms-drafts").exists());
    assert!(!root.join("dist/preview").exists());
    let all: Vec<String> = files(&root.join("dist"))
        .iter()
        .map(|f| f.strip_prefix(root.join("dist")).unwrap().display().to_string())
        .collect();
    assert!(!all.iter().any(|f| f.contains(&secret)), "{all:?}");
    for needle in [secret.as_str(), SECRET, "unannounced"] {
        assert!(public_hits(root, needle).is_empty(), "{needle}: {:?}", public_hits(root, needle));
    }
    assert!(!names(&root.join(PUBLIC)).is_empty(), "the shared image is still built");
}

#[test]
fn a_draft_image_moves_to_public_once_its_item_publishes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    site(root, false);
    build(root, true);
    let draft = fs::read_to_string(root.join("dist/preview/drafts/notes/soon/index.html")).unwrap();
    let secret = ids_in(&draft, "preview/drafts/assets/img")[0].clone();
    let gated_before = names(&root.join(GATED));

    site(root, true);
    build(root, true);
    let live = fs::read_to_string(root.join("dist/notes/soon/index.html")).unwrap();
    assert_eq!(ids_in(&live, "assets/img").len(), 2, "{live}");
    assert!(live.contains(&format!("/assets/img/{secret}-64.jpg")), "{live}");
    let public = names(&root.join(PUBLIC));
    for n in &gated_before {
        assert!(public.contains(n), "{n} not public: {public:?}");
    }
    assert!(!root.join(GATED).exists(), "{:?}", names(&root.join(GATED)));
    let preview = fs::read_to_string(root.join("dist/preview/drafts/notes/soon/index.html")).unwrap();
    assert!(preview.contains(&format!("src=\"/assets/img/{secret}-64.jpg\"")), "{preview}");
}

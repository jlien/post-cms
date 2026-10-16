//! build-site with `images:` in site.yaml: the helper's markers become
//! responsive markup before precompression, outputs land in dist, and a
//! second (offline) build reuses the cache with identical HTML.
use std::fs;
use std::path::Path;
use std::process::Command;

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

fn build(root: &Path, offline: bool) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_build-site"))
        .env("SITE_ROOT", root)
        .env("IMAGES_OFFLINE", if offline { "1" } else { "0" })
        .env_remove("SITE_PREVIEW_THEMES")
        .env_remove("DRAFTS")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn build_renders_image_helper_and_reuses_cache() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    fs::create_dir_all(root.join("content/pages/about")).unwrap();
    fs::write(root.join("content/pages/about/meta.yaml"), "title: About\nslug: about\npublished: true\n").unwrap();
    fs::write(root.join("content/pages/about/index.mdx"), "Hello.\n").unwrap();
    fs::write(
        root.join("site.yaml"),
        "theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\nimages:\n  widths: [32, 64]\n",
    )
    .unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(100, 50).write_to(&mut png, image::ImageFormat::Png).unwrap();
    fs::write(root.join("themes/astra/assets/images/hero.png"), png.into_inner()).unwrap();
    let page = fs::read_to_string(root.join("templates/page.html")).unwrap().replace(
        "{{ body | safe }}",
        "{{ body | safe }}{{ image(src=\"/assets/astra/images/hero.png\", alt=\"Hero\", sizes=\"50vw\") }}<meta property=\"og:image\" content=\"{{ '/assets/astra/images/hero.png' | image_url(width=64, absolute=true) }}\">",
    );
    fs::write(root.join("templates/page.html"), page).unwrap();

    let stdout = build(root, false);
    assert!(stdout.contains("images: 1 sources, 3 files written"), "{stdout}");
    let html = fs::read_to_string(root.join("dist/about/index.html")).unwrap();
    assert!(!html.contains("__postcms_img_"), "marker left in HTML");
    let files: Vec<String> = fs::read_dir(root.join("dist/assets/img"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let id = &files[0][..16];
    assert!(html.contains(&format!(
        "<picture><source type=\"image/webp\" srcset=\"/assets/img/{id}-32.webp 32w, /assets/img/{id}-64.webp 64w\" sizes=\"50vw\"><img src=\"/assets/img/{id}-64.png\" alt=\"Hero\" width=\"64\" height=\"32\" loading=\"lazy\" decoding=\"async\"></picture>"
    )), "{html}");
    assert!(html.contains(&format!("content=\"https://www.example.com/assets/img/{id}-64.png\"")), "{html}");
    // the precompressed copy carries the final markup too
    let gz = fs::read(root.join("dist/about/index.html.gz")).unwrap();
    let mut unzipped = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut unzipped).unwrap();
    assert_eq!(unzipped, html);

    // offline rebuild from the cache: same page
    build(root, true);
    assert_eq!(fs::read_to_string(root.join("dist/about/index.html")).unwrap(), html);
}

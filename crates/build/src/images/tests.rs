use super::*;
use std::sync::atomic::AtomicUsize;

const HOST: &str = "img.example.com";

fn cfg_yaml(extra: &str) -> ImagesCfg {
    let y = format!("images:\n  allowed_hosts: [{HOST}]\n{extra}");
    ImagesCfg::from_site_yaml(&y).unwrap().unwrap()
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
    let mut b = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img).write_to(&mut b, ImageFormat::Png).unwrap();
    b.into_inner()
}

fn jpeg_with_exif_orientation(w: u32, h: u32, orientation: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, _| image::Rgb([x as u8, 0, 0]));
    let mut b = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img).write_to(&mut b, ImageFormat::Jpeg).unwrap();
    let jpg = b.into_inner();
    // APP1 Exif: big-endian TIFF, one IFD entry 0x0112 (Orientation), SHORT.
    let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01".to_vec();
    tiff.extend([0, orientation, 0, 0, 0, 0, 0, 0]);
    let mut app1 = b"Exif\x00\x00".to_vec();
    app1.extend(tiff);
    let len = (app1.len() + 2) as u16;
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend(len.to_be_bytes());
    out.extend(app1);
    out.extend(&jpg[2..]);
    out
}

/// A tiny PNG whose header claims `w` x `h` (a decompression bomb shape).
fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    let mut b = png(1, 1);
    b[16..20].copy_from_slice(&w.to_be_bytes());
    b[20..24].copy_from_slice(&h.to_be_bytes());
    let mut crc = flate2::Crc::new();
    crc.update(&b[12..29]);
    b[29..33].copy_from_slice(&crc.sum().to_be_bytes());
    b
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    dist: PathBuf,
    calls: Arc<AtomicUsize>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let dist = root.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        Fixture { _dir: dir, root, dist, calls: Arc::new(AtomicUsize::new(0)) }
    }

    /// A pipeline whose transport serves `files` (url -> response).
    fn pipeline(&self, cfg: ImagesCfg, offline: bool, files: Vec<(&str, RawResponse)>) -> Pipeline {
        let map: HashMap<String, RawResponse> = files.into_iter().map(|(u, r)| (u.to_string(), r)).collect();
        let calls = self.calls.clone();
        let t: Box<Transport> = Box::new(move |url, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            map.get(url).cloned().ok_or_else(|| format!("no fixture for {url}"))
        });
        Pipeline::new(cfg, &self.root, &self.dist, "https://example.com", offline, t)
    }

    /// Write `body` (with markers) to dist/page.html, run finish, return the page.
    fn render(&self, p: &Pipeline, body: &str) -> String {
        std::fs::write(self.dist.join("page.html"), body).unwrap();
        p.finish(4).unwrap();
        std::fs::read_to_string(self.dist.join("page.html")).unwrap()
    }

    fn out_files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dist.join("assets/img"))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

fn ok(body: Vec<u8>) -> RawResponse {
    RawResponse { status: 200, location: None, body }
}

fn redirect(to: &str) -> RawResponse {
    RawResponse { status: 302, location: Some(to.into()), body: vec![] }
}

fn img(src: &str) -> ImgArgs {
    ImgArgs { src: src.into(), alt: "A".into(), ..Default::default() }
}

fn dims_of(path: &Path) -> (u32, u32) {
    image::image_dimensions(path).unwrap()
}

// ---- config ----

#[test]
fn config_absent_disabled_and_defaults() {
    assert!(ImagesCfg::from_site_yaml("title: x\n").unwrap().is_none());
    assert!(ImagesCfg::from_site_yaml("images:\n  enabled: false\n").unwrap().is_none());
    let c = ImagesCfg::from_site_yaml("images: {}\n").unwrap().unwrap();
    assert_eq!(c.widths, DEFAULT_WIDTHS.to_vec());
    assert_eq!(c.formats, vec![Modern::Webp]);
    assert_eq!(c.output_dir, "assets/img");
    assert!(c.allowed_hosts.is_empty());
    let c = cfg_yaml("  widths: [640, 320, 640]\n  formats: [avif, webp, avif]\n");
    assert_eq!(c.widths, vec![320, 640]);
    assert_eq!(c.formats, vec![Modern::Avif, Modern::Webp]);
}

#[test]
fn config_rejects_unsafe_values() {
    for bad in [
        "images:\n  output_dir: ../x\n",
        "images:\n  output_dir: /abs\n",
        "images:\n  output_dir: a/../b\n",
        "images:\n  allowed_hosts: [https://x.example.com]\n",
        "images:\n  allowed_hosts: [x.example.com/path]\n",
        "images:\n  allowed_hosts: [\"x.example.com:8080\"]\n",
        "images:\n  widths: []\n",
        "images:\n  widths: [0]\n",
        "images:\n  webp_quality: 0\n",
        "images:\n  formats: [jpegxl]\n",
        "images:\n  unknown_key: 1\n",
    ] {
        assert!(ImagesCfg::from_site_yaml(bad).is_err(), "accepted: {bad}");
    }
}

// ---- SSRF ----

#[test]
fn url_check_requires_https_allowlisted_default_port() {
    let c = cfg_yaml("");
    assert_eq!(check_url(&format!("https://{HOST}/a.png"), &c), Ok(()));
    assert_eq!(check_url("https://IMG.EXAMPLE.COM/a.png", &c), Ok(()));
    assert!(matches!(check_url(&format!("http://{HOST}/a.png"), &c), Err(UrlCheck::Foreign(_))));
    assert!(matches!(check_url("https://other.example.com/a.png", &c), Err(UrlCheck::Foreign(_))));
    assert!(matches!(check_url(&format!("https://{HOST}.evil.test/a.png"), &c), Err(UrlCheck::Foreign(_))));
    assert!(matches!(check_url(&format!("https://u:p@{HOST}/a.png"), &c), Err(UrlCheck::Bad(_))));
    assert!(matches!(check_url(&format!("https://{HOST}:8443/a.png"), &c), Err(UrlCheck::Bad(_))));
    let w = ImagesCfg::from_site_yaml("images:\n  allowed_hosts: [\"*.example.com\"]\n").unwrap().unwrap();
    assert_eq!(check_url("https://a.b.example.com/x.png", &w), Ok(()));
    assert!(check_url("https://example.com/x.png", &w).is_err());
    assert!(check_url("https://badexample.com/x.png", &w).is_err());
}

#[test]
fn redirects_are_rechecked_every_hop() {
    let c = cfg_yaml("");
    let a = format!("https://{HOST}/a.png");
    let calls = Arc::new(AtomicUsize::new(0));
    let mk = |resps: Vec<(String, RawResponse)>| {
        let calls = calls.clone();
        let map: HashMap<String, RawResponse> = resps.into_iter().collect();
        move |u: &str, _: u64, _: u64| {
            calls.fetch_add(1, Ordering::SeqCst);
            map.get(u).cloned().ok_or_else(|| format!("unexpected {u}"))
        }
    };
    // to another host: refused before it is requested
    let t = mk(vec![(a.clone(), redirect("https://169.254.169.254/latest/meta-data"))]);
    let e = fetch_checked(&a, &c, &t).unwrap_err();
    assert!(e.contains("refused") && e.contains("169.254.169.254"), "{e}");
    // to plain http on the same host: refused
    let t = mk(vec![(a.clone(), redirect(&format!("http://{HOST}/b.png")))]);
    assert!(fetch_checked(&a, &c, &t).unwrap_err().contains("not https"));
    // relative and absolute redirects on the allowed host: followed
    let t = mk(vec![
        (a.clone(), redirect("/dir/b.png")),
        (format!("https://{HOST}/dir/b.png"), redirect("c.png")),
        (format!("https://{HOST}/dir/c.png"), ok(vec![1, 2, 3])),
    ]);
    assert_eq!(fetch_checked(&a, &c, &t).unwrap(), vec![1, 2, 3]);
    // a loop: stops after MAX_REDIRECTS
    let t = mk(vec![(a.clone(), redirect(&a))]);
    assert!(fetch_checked(&a, &c, &t).unwrap_err().contains("redirects"));
    // errors and oversized bodies
    let t = mk(vec![(a.clone(), RawResponse { status: 403, ..Default::default() })]);
    assert_eq!(fetch_checked(&a, &c, &t).unwrap_err(), "HTTP 403");
    let small = cfg_yaml("  max_bytes: 2\n");
    let t = mk(vec![(a.clone(), ok(vec![0; 3]))]);
    assert!(fetch_checked(&a, &small, &t).unwrap_err().contains("max_bytes"));
}

#[test]
fn private_and_reserved_addresses_are_not_public() {
    for ip in [
        "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0",
        "224.0.0.1", "255.255.255.255", "192.0.2.1", "198.18.0.1", "240.0.0.1", "::1", "::", "fc00::1",
        "fd12::1", "fe80::1", "ff02::1", "2001:db8::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1",
        "64:ff9b::a00:1", "::127.0.0.1",
    ] {
        assert!(!is_public_ip(ip.parse().unwrap()), "{ip} treated as public");
    }
    for ip in ["93.184.215.14", "1.1.1.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
        assert!(is_public_ip(ip.parse().unwrap()), "{ip} treated as private");
    }
}

#[test]
fn real_transport_refuses_loopback_hosts() {
    let c = ImagesCfg::from_site_yaml("images:\n  allowed_hosts: [localhost, 127.0.0.1]\n  timeout_secs: 5\n")
        .unwrap()
        .unwrap();
    let t = ureq_transport(5);
    for url in ["https://localhost/x.png", "https://127.0.0.1/x.png"] {
        let e = fetch_checked(url, &c, &*t).unwrap_err();
        assert!(e.contains("non-public"), "{url}: {e}");
    }
}

// ---- variants and markup ----

#[test]
fn widths_never_upscale_and_markup_is_complete() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/photo.png");
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(png(500, 250)))]);
    let m = p.request_img(img(&url));
    let html = f.render(&p, &format!("<p>{m}</p>"));
    let files = f.out_files();
    // 320, 480 and the source width 500 in WebP, plus one fallback (500 < 1280):
    // JPEG, since the PNG has no transparency
    assert_eq!(files.len(), 4, "{files:?}");
    let id = &files[0][..16];
    assert_eq!(
        html,
        format!(
            "<p><picture><source type=\"image/webp\" srcset=\"/assets/img/{id}-320.webp 320w, /assets/img/{id}-480.webp 480w, /assets/img/{id}-500.webp 500w\" sizes=\"auto, 100vw\">\
             <img src=\"/assets/img/{id}-500.jpg\" alt=\"A\" width=\"500\" height=\"250\" loading=\"lazy\" decoding=\"async\"></picture></p>"
        )
    );
    for (name, w, h) in [("320.webp", 320, 160), ("480.webp", 480, 240), ("500.webp", 500, 250), ("500.jpg", 500, 250)] {
        assert_eq!(dims_of(&f.dist.join(format!("assets/img/{id}-{name}"))), (w, h), "{name}");
    }
    for n in &files {
        let (stem, ext) = n.split_once('.').unwrap();
        let (hash, width) = stem.split_once('-').unwrap();
        assert!(hash.len() == 16 && hash.bytes().all(|c| c.is_ascii_hexdigit()), "{n}");
        assert!(width.bytes().all(|c| c.is_ascii_digit()) && matches!(ext, "webp" | "jpg"), "{n}");
    }
}

#[test]
fn large_sources_cap_at_the_largest_width_and_fallback_width() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/big.jpg");
    let mut jpg = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(2400, 1200).write_to(&mut jpg, ImageFormat::Jpeg).unwrap();
    let p = f.pipeline(cfg_yaml("  widths: [400, 1000, 2000]\n  fallback_width: 1000\n"), false, vec![(&url, ok(jpg.into_inner()))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.contains("-400.webp 400w,") && html.contains("-2000.webp 2000w\""), "{html}");
    assert!(html.contains("-1000.jpg\" alt=\"A\" width=\"2000\" height=\"1000\""), "{html}");
    assert!(!html.contains("2400"));
}

#[test]
fn options_are_passed_through_and_escaped() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/p.png");
    let p = f.pipeline(cfg_yaml("  widths: [100]\n  formats: []\n"), false, vec![(&url, ok(png(200, 100)))]);
    let a = ImgArgs {
        src: url.clone(),
        alt: "Say \"hi\" & <b>".into(),
        sizes: Some("(min-width: 60em) 33vw, 100vw".into()),
        class: Some("card__img".into()),
        loading: Some("eager".into()),
        fetchpriority: Some("high".into()),
        ..Default::default()
    };
    let html = f.render(&p, &p.request_img(a));
    let id = &f.out_files()[0][..16];
    // no modern formats: a plain <img> with a fallback srcset
    assert_eq!(
        html,
        format!(
            "<img src=\"/assets/img/{id}-100.jpg\" srcset=\"/assets/img/{id}-100.jpg 100w\" sizes=\"(min-width: 60em) 33vw, 100vw\" \
             alt=\"Say &quot;hi&quot; &amp; &lt;b&gt;\" class=\"card__img\" width=\"100\" height=\"50\" loading=\"eager\" decoding=\"async\" fetchpriority=\"high\">"
        )
    );
}

#[test]
fn avif_and_webp_sources_in_config_order() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/p.png");
    let p = f.pipeline(cfg_yaml("  widths: [48, 96]\n  formats: [avif, webp]\n  avif_speed: 10\n"), false, vec![(&url, ok(png(96, 64)))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    let a = html.find("type=\"image/avif\"").unwrap();
    let w = html.find("type=\"image/webp\"").unwrap();
    assert!(a < w, "{html}");
    let files = f.out_files();
    let avif = files.iter().find(|n| n.ends_with("-96.avif")).unwrap();
    let bytes = std::fs::read(f.dist.join("assets/img").join(avif)).unwrap();
    assert_eq!(&bytes[4..12], b"ftypavif");
    assert_eq!(files.len(), 5, "{files:?}"); // 2 avif + 2 webp + 1 jpg
}

#[test]
fn exif_orientation_is_applied_and_metadata_dropped() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/rot.jpg");
    let src = jpeg_with_exif_orientation(40, 20, 6);
    assert!(src.windows(4).any(|w| w == b"Exif"));
    let p = f.pipeline(cfg_yaml("  widths: [20]\n"), false, vec![(&url, ok(src))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.contains("width=\"20\" height=\"40\""), "{html}");
    for n in f.out_files() {
        let b = std::fs::read(f.dist.join("assets/img").join(&n)).unwrap();
        assert!(!b.windows(4).any(|w| w == b"Exif"), "{n} kept EXIF");
        assert_eq!(dims_of(&f.dist.join("assets/img").join(&n)), (20, 40), "{n}");
    }
}

#[test]
fn pixel_limit_is_checked_before_decoding() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/bomb.png");
    let p = f.pipeline(cfg_yaml("  max_pixels: 1000000\n"), false, vec![(&url, ok(png_claiming(50_000, 50_000)))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert_eq!(html, format!("<img src=\"{url}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">"));
    assert!(f.out_files().is_empty());
    assert!(probe(&png_claiming(50_000, 50_000), &cfg_yaml("  max_pixels: 1000000\n")).unwrap_err().contains("max_pixels"));
}

// ---- passthrough ----

#[test]
fn svg_is_copied_as_is_and_unsafe_svg_keeps_its_url() {
    let f = Fixture::new();
    let good = format!("https://{HOST}/logo.svg");
    let bad = format!("https://{HOST}/evil.svg");
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 120 40"><use xlink:href="#a"/><path id="a" d="M0 0h1"/></svg>"##.to_vec();
    let evil = br#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"><rect/></svg>"#.to_vec();
    let p = f.pipeline(cfg_yaml(""), false, vec![(&good, ok(svg.clone())), (&bad, ok(evil))]);
    let body = format!("{}|{}", p.request_img(img(&good)), p.request_img(img(&bad)));
    let html = f.render(&p, &body);
    let files = f.out_files();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with(".svg"));
    assert_eq!(std::fs::read(f.dist.join("assets/img").join(&files[0])).unwrap(), svg);
    assert_eq!(
        html,
        format!(
            "<img src=\"/assets/img/{}\" alt=\"A\" width=\"120\" height=\"40\" loading=\"lazy\" decoding=\"async\">|<img src=\"{bad}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">",
            files[0]
        )
    );
}

#[test]
fn svg_checks() {
    for bad in [
        &b"<svg><script>alert(1)</script></svg>"[..],
        b"<svg><a href=\"javascript:alert(1)\"><rect/></a></svg>",
        b"<svg ONCLICK = \"x()\"></svg>",
        b"<svg><rect/onmouseover='x()'/></svg>",
        b"<svg><foreignObject><div/></foreignObject></svg>",
        b"<svg><image href=\"https://tracker.example/x.png\"/></svg>",
        b"<svg><rect style=\"fill:url(https://x.example/a)\"/></svg>",
        b"<!DOCTYPE svg [<!ENTITY x \"y\">]><svg></svg>",
        b"<svg><image href=\"data:image/svg+xml;base64,PHN2Zz4=\"/></svg>",
        b"<svg><a href=\"#x\"><set attributeName=\"href\" to=\"https://x.example\"/></a></svg>",
        b"<svg><a href =\n \"https://x.example\"/><set attributeName = \"href\"/></svg>",
        b"<svg><a xlink:href=\"&#x6a;avascript:x\"/></svg>",
    ] {
        assert!(svg_problem(bad).is_some(), "{}", String::from_utf8_lossy(bad));
    }
    assert_eq!(svg_problem(b"<svg class=\"one\" opacity=\"1\"><rect fill=\"url(#g)\"/><use href=\"#a\"/></svg>"), None);
    assert_eq!(svg_problem(b"<svg><image xlink:href=\"data:image/png;base64,iVBO\"/><animate xlink:href=\"#a\" attributeName=\"opacity\"/></svg>"), None);
    assert_eq!(svg_dims(b"<svg width=\"24px\" height=\"12\"></svg>"), (24, 12));
    assert_eq!(svg_dims(b"<svg viewBox=\"0 0 300.4 150\"></svg>"), (300, 150));
    assert_eq!(svg_dims(b"<svg width=\"100%\"></svg>"), (0, 0));
}

#[test]
fn gif_and_animated_images_pass_through() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/a.gif");
    let mut gif = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(30, 10).write_to(&mut gif, ImageFormat::Gif).unwrap();
    let gif = gif.into_inner();
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(gif.clone()))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    let files = f.out_files();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(f.dist.join("assets/img").join(&files[0])).unwrap(), gif);
    assert!(html.contains(".gif\" alt=\"A\" width=\"30\" height=\"10\""), "{html}");
    // APNG: acTL before IDAT
    let mut apng = png(2, 2);
    let at = apng.windows(4).position(|w| w == b"IDAT").unwrap() - 4;
    apng.splice(at..at, [0, 0, 0, 8, b'a', b'c', b'T', b'L', 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(is_apng(&apng));
    assert!(!is_apng(&png(2, 2)));
    assert!(matches!(probe(&apng, &cfg_yaml("")).unwrap().2, Kind::Passthrough { ext: "png" }));
}

// ---- cache ----

#[test]
fn cache_is_reused_and_offline_builds_work_from_it() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/c.png");
    let p = f.pipeline(cfg_yaml("  widths: [50]\n"), false, vec![(&url, ok(png(100, 50)))]);
    let first = f.render(&p, &p.request_img(img(&url)));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    // a fresh dist, offline, with a transport that has nothing
    std::fs::remove_dir_all(&f.dist).unwrap();
    std::fs::create_dir_all(&f.dist).unwrap();
    let p2 = f.pipeline(cfg_yaml("  widths: [50]\n"), true, vec![]);
    let second = f.render(&p2, &p2.request_img(img(&url)));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1, "offline build fetched");
    assert_eq!(first, second);
    assert_eq!(f.out_files().len(), 2);
    // offline with an empty cache: original URL
    let other = format!("https://{HOST}/never.png");
    let html = f.render(&p2, &p2.request_img(img(&other)));
    assert!(html.starts_with(&format!("<img src=\"{other}\"")), "{html}");
    // a corrupted cached source is not trusted
    let idx = std::fs::read_to_string(p.url_index(&url)).unwrap();
    std::fs::write(p.src_file(idx.trim()), b"tampered").unwrap();
    let p3 = f.pipeline(cfg_yaml("  widths: [50]\n"), true, vec![]);
    let html = f.render(&p3, &p3.request_img(img(&url)));
    assert!(html.starts_with(&format!("<img src=\"{url}\"")), "{html}");
}

#[test]
fn changed_settings_change_output_names() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/c.png");
    let a = f.pipeline(cfg_yaml("  widths: [50]\n"), false, vec![(&url, ok(png(100, 50)))]);
    let b = f.pipeline(cfg_yaml("  widths: [50]\n  webp_quality: 90\n"), false, vec![(&url, ok(png(100, 50)))]);
    let ha = f.render(&a, &a.request_img(img(&url)));
    let hb = f.render(&b, &b.request_img(img(&url)));
    assert_ne!(ha, hb);
}

// ---- local files and traversal ----

#[test]
fn local_images_use_the_same_path_and_cannot_escape_dist() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.dist.join("media")).unwrap();
    std::fs::write(f.dist.join("media/a b.png"), png(64, 32)).unwrap();
    std::fs::write(f.root.join("secret.png"), png(64, 32)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(f.root.join("secret.png"), f.dist.join("media/link.png")).unwrap();
    let p = f.pipeline(cfg_yaml("  widths: [32]\n"), false, vec![]);
    let ok_m = p.request_img(img("/media/a%20b.png"));
    let bad_srcs = ["/../secret.png", "/%2e%2e/secret.png", "/media/..%2f..%2fsecret.png", "/media/link.png", "/media/missing.png"];
    let bad: Vec<String> = bad_srcs.iter().map(|s| p.request_img(img(s))).collect();
    let html = f.render(&p, &format!("{ok_m}\n{}", bad.join("\n")));
    let mut lines = html.lines();
    assert!(lines.next().unwrap().starts_with("<picture>"));
    for (line, src) in lines.zip(bad_srcs) {
        assert_eq!(line, format!("<img src=\"{src}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">"));
    }
    assert_eq!(f.out_files().len(), 2);
}

#[test]
fn foreign_and_unsafe_sources_are_left_alone() {
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml(""), false, vec![]);
    let foreign = p.request_img(img("https://elsewhere.example.org/x.png"));
    let http = p.request_img(img(&format!("http://{HOST}/x.png")));
    assert_eq!(p.request_img(img("javascript:alert(1)")), "");
    assert_eq!(p.request_url("data:image/png;base64,AA", None, false), "");
    let html = f.render(&p, &format!("{foreign}\n{http}"));
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        html,
        format!("<img src=\"https://elsewhere.example.org/x.png\" alt=\"A\" loading=\"lazy\" decoding=\"async\">\n<img src=\"http://{HOST}/x.png\" alt=\"A\" loading=\"lazy\" decoding=\"async\">")
    );
}

// ---- image_url ----

#[test]
fn image_url_picks_largest_or_requested_width() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/og.png");
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(png(2000, 1000)))]);
    let body = format!(
        "{}|{}|{}",
        p.request_url(&url, None, false),
        p.request_url(&url, Some(1200), true),
        p.request_url(&url, Some(5000), false)
    );
    let html = f.render(&p, &body);
    let id = &f.out_files()[0][..16];
    assert_eq!(
        html,
        format!("/assets/img/{id}-1920.jpg|https://example.com/assets/img/{id}-1200.jpg|/assets/img/{id}-2000.jpg")
    );
    assert_eq!(dims_of(&f.dist.join(format!("assets/img/{id}-1200.jpg"))), (1200, 600));
}

#[test]
fn unresolved_markers_fail_the_build() {
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml(""), false, vec![]);
    let m = p.request_img(img("/x.png"));
    let forged = m.replace("_0__", "_99__");
    std::fs::write(f.dist.join("page.html"), forged).unwrap();
    assert!(p.finish(2).unwrap_err().contains("unresolved image marker"));
}

// ---- Tera and Portable Text ----

#[test]
fn tera_helpers_without_a_pipeline_print_plain_markup() {
    let mut t = tera::Tera::default();
    register(&mut t, None, "https://example.com");
    t.add_raw_template(
        "a.html",
        r#"{{ image(src=s, alt='x"y', class="c", fetchpriority="high", width=10, height=5) }}|{{ s | image_url }}|{{ "/a.png" | image_url(absolute=true) }}|{{ image(src="javascript:x") }}"#,
    )
    .unwrap();
    let mut ctx = tera::Context::new();
    ctx.insert("s", "https://img.example.com/a.png?w=1&h=2");
    assert_eq!(
        t.render("a.html", &ctx).unwrap(),
        "<img src=\"https://img.example.com/a.png?w=1&amp;h=2\" alt=\"x&quot;y\" class=\"c\" width=\"10\" height=\"5\" loading=\"lazy\" decoding=\"async\" fetchpriority=\"high\">\
         |https://img.example.com/a.png?w=1&amp;h=2|https://example.com/a.png|"
    );
}

#[test]
fn templates_and_portable_text_render_through_the_pipeline() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/pt.png");
    let p = Arc::new(f.pipeline(cfg_yaml("  widths: [40]\n"), false, vec![(&url, ok(png(80, 40)))]));
    let mut t = tera::Tera::default();
    register(&mut t, Some(p.clone()), "https://example.com");
    t.add_raw_template("card.html", r#"<div style="background-image:url({{ s | image_url }})">{{ image(src=s, alt="c", sizes="50vw") }}</div>"#).unwrap();
    let mut ctx = tera::Context::new();
    ctx.insert("s", &url);
    let card = t.render("card.html", &ctx).unwrap();
    let opts = crate::pt::PtOptions { images: Some(p.clone()), ..Default::default() };
    let pt = serde_json::json!([{"_type": "image", "_sanityAsset": format!("image@{url}"), "alt": "Chart", "caption": "Q3"}]);
    let body = crate::pt::render(&pt, &opts, None).html;
    let html = f.render(&p, &format!("{card}\n{body}"));
    let id = &f.out_files()[0][..16];
    assert_eq!(
        html,
        format!(
            "<div style=\"background-image:url(/assets/img/{id}-40.jpg)\"><picture><source type=\"image/webp\" srcset=\"/assets/img/{id}-40.webp 40w\" sizes=\"50vw\"><img src=\"/assets/img/{id}-40.jpg\" alt=\"c\" width=\"40\" height=\"20\" loading=\"lazy\" decoding=\"async\"></picture></div>\n\
             <figure><picture><source type=\"image/webp\" srcset=\"/assets/img/{id}-40.webp 40w\" sizes=\"auto, 100vw\"><img src=\"/assets/img/{id}-40.jpg\" alt=\"Chart\" width=\"40\" height=\"20\" loading=\"lazy\" decoding=\"async\"></picture><figcaption>Q3</figcaption></figure>"
        )
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1, "one fetch per source");
}

// ---- fallbacks ----

fn rgba_png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, _| image::Rgba([x as u8, 9, 9, if x == 0 { alpha } else { 255 }]));
    let mut b = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img).write_to(&mut b, ImageFormat::Png).unwrap();
    b.into_inner()
}

#[test]
fn fallback_is_progressive_jpeg_unless_pixels_are_transparent() {
    let f = Fixture::new();
    let opaque = format!("https://{HOST}/opaque.png"); // RGBA channel, every pixel opaque
    let clear = format!("https://{HOST}/clear.png"); // one transparent column
    let p = f.pipeline(
        cfg_yaml("  widths: [32]\n"),
        false,
        vec![(&opaque, ok(rgba_png(64, 32, 255))), (&clear, ok(rgba_png(64, 32, 0)))],
    );
    let html = f.render(&p, &format!("{}\n{}", p.request_img(img(&opaque)), p.request_img(img(&clear))));
    let mut lines = html.lines();
    let (a, b) = (lines.next().unwrap(), lines.next().unwrap());
    assert!(a.contains("-32.jpg\" alt"), "{a}");
    assert!(b.contains("-32.png\" alt"), "{b}");
    let jpg = f.out_files().into_iter().find(|n| n.ends_with(".jpg")).unwrap();
    let bytes = std::fs::read(f.dist.join("assets/img").join(jpg)).unwrap();
    assert!(bytes.windows(2).any(|w| w == [0xFF, 0xC2]), "not progressive (no SOF2)");
    assert!(!bytes.windows(4).any(|w| w == b"Exif"));
    // the alpha answer is cached, so a warm build does not decode again
    assert_eq!(std::fs::read_dir(f.root.join(".cache/images/alpha")).unwrap().count(), 2);
}

#[test]
fn failed_sources_are_listed_in_the_stats() {
    let f = Fixture::new();
    let gone = format!("https://{HOST}/gone.png");
    let p = f.pipeline(cfg_yaml(""), false, vec![(&gone, RawResponse { status: 403, ..Default::default() })]);
    let body = format!("{}{}", p.request_img(img(&gone)), p.request_img(img("https://elsewhere.example.org/x.png")));
    std::fs::write(f.dist.join("page.html"), body).unwrap();
    let stats = p.finish(2).unwrap();
    // the foreign host is not ours, so it is not a failure
    assert_eq!(stats.kept, vec![(gone.clone(), "HTTP 403".to_string())]);
}

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
            map.get(url).cloned().ok_or_else(|| format!("no fixture for {url}").into())
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
            map.get(u).cloned().ok_or_else(|| format!("unexpected {u}").into())
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
        "64:ff9b::a00:1", "::127.0.0.1", "2002:7f00:1::1", "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        "64:ff9b:1::a00:1", "fec0::1",
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
    assert!(probe(&png_claiming(50_000, 50_000), &cfg_yaml("  max_pixels: 1000000\n"), false).unwrap_err().contains("max_pixels"));
}

// ---- passthrough ----

#[test]
fn remote_svg_keeps_its_url_and_local_svg_is_copied() {
    let f = Fixture::new();
    // Plain, plus the review's bypasses of a markup blocklist: a
    // namespace-prefixed script element and an entity-encoded link animation.
    let svgs: [(&str, &[u8]); 4] = [
        ("logo", br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 40"><path d="M0 0h1"/></svg>"##),
        ("ns-script", br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:s="http://www.w3.org/2000/svg"><s:script>alert(1)</s:script></svg>"#),
        ("html-script", br#"<svg xmlns="http://www.w3.org/2000/svg"><h:script xmlns:h="http://www.w3.org/1999/xhtml">alert(1)</h:script></svg>"#),
        ("entity-set", br#"<svg xmlns="http://www.w3.org/2000/svg"><a><set attributeName="&#104;ref" to="&#106;avascript:alert(1)"/><text>x</text></a></svg>"#),
    ];
    let urls: Vec<String> = svgs.iter().map(|(n, _)| format!("https://{HOST}/{n}.svg")).collect();
    let files: Vec<(&str, RawResponse)> = urls.iter().zip(svgs.iter()).map(|(u, (_, b))| (u.as_str(), ok(b.to_vec()))).collect();
    let local = svgs[0].1.to_vec();
    std::fs::create_dir_all(f.dist.join("media")).unwrap();
    std::fs::write(f.dist.join("media/logo.svg"), &local).unwrap();
    let p = f.pipeline(cfg_yaml(""), false, files);
    let mut body: Vec<String> = urls.iter().map(|u| p.request_img(img(u))).collect();
    body.push(p.request_img(img("/media/logo.svg")));
    std::fs::write(f.dist.join("page.html"), body.join("\n")).unwrap();
    let stats = p.finish(4).unwrap();
    let html = std::fs::read_to_string(f.dist.join("page.html")).unwrap();
    let lines: Vec<&str> = html.lines().collect();
    for (line, u) in lines.iter().zip(&urls) {
        assert_eq!(*line, format!("<img src=\"{u}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">"));
    }
    assert_eq!(stats.kept.len(), 4, "{:?}", stats.kept);
    assert!(stats.kept.iter().all(|(_, why)| why.contains("remote SVG")), "{:?}", stats.kept);
    // nothing remote reached dist (the local copy shares bytes with logo.svg)
    let out = f.out_files();
    assert_eq!(out.len(), 1);
    assert_eq!(std::fs::read(f.dist.join("assets/img").join(&out[0])).unwrap(), local);
    assert_eq!(lines[4], format!("<img src=\"/assets/img/{}\" alt=\"A\" width=\"120\" height=\"40\" loading=\"lazy\" decoding=\"async\">", out[0]));
}

#[test]
fn svg_dims_from_attributes_or_viewbox() {
    assert_eq!(svg_dims(b"<svg width=\"24px\" height=\"12\"></svg>"), (24, 12));
    assert_eq!(svg_dims(b"<svg viewBox=\"0 0 300.4 150\"></svg>"), (300, 150));
    assert_eq!(svg_dims(b"<svg width=\"100%\"></svg>"), (0, 0));
}

// ---- broken data ----

fn truncated_png() -> Vec<u8> {
    let b = png(300, 200);
    b[..b.len() / 2].to_vec()
}

fn truncated_jpeg() -> Vec<u8> {
    let img = image::RgbImage::from_fn(300, 200, |x, y| image::Rgb([x as u8, y as u8, 7]));
    let mut b = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img).write_to(&mut b, ImageFormat::Jpeg).unwrap();
    let b = b.into_inner();
    b[..b.len() / 2].to_vec()
}

fn cache_entries(f: &Fixture, sub: &str) -> usize {
    std::fs::read_dir(f.root.join(".cache/images").join(sub)).map_or(0, |rd| rd.count())
}

#[test]
fn truncated_images_keep_their_url_and_leave_no_cache_online_and_offline() {
    for (name, bytes) in [("cut.png", truncated_png()), ("cut.jpg", truncated_jpeg())] {
        let f = Fixture::new();
        let url = format!("https://{HOST}/{name}");
        let want = format!("<img src=\"{url}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">");
        // online: fetched, fails, original URL, cache evicted, build succeeds
        let p = f.pipeline(cfg_yaml("  widths: [100]\n"), false, vec![(&url, ok(bytes.clone()))]);
        std::fs::write(f.dist.join("page.html"), p.request_img(img(&url))).unwrap();
        let stats = p.finish(2).unwrap();
        assert_eq!(std::fs::read_to_string(f.dist.join("page.html")).unwrap(), want, "{name}");
        assert_eq!(stats.kept.len(), 1, "{name}: {:?}", stats.kept);
        assert_eq!((cache_entries(&f, "src"), cache_entries(&f, "urls"), cache_entries(&f, "out")), (0, 0, 0), "{name}");
        assert!(f.out_files().is_empty(), "{name}");
        // offline after that: not in the cache, original URL
        let p = f.pipeline(cfg_yaml("  widths: [100]\n"), true, vec![]);
        assert_eq!(f.render(&p, &p.request_img(img(&url))), want, "{name}");
        // offline with a poisoned cache (bad bytes indexed as good): same, and evicted
        let content = sha_hex(&bytes);
        write_atomic(&p.src_file(&content), &bytes).unwrap();
        write_atomic(&p.url_index(&url), content.as_bytes()).unwrap();
        let p = f.pipeline(cfg_yaml("  widths: [100]\n"), true, vec![]);
        assert_eq!(f.render(&p, &p.request_img(img(&url))), want, "{name}");
        assert_eq!((cache_entries(&f, "src"), cache_entries(&f, "urls")), (0, 0), "{name}");
    }
}

#[test]
fn a_decode_failure_after_the_header_check_does_not_fail_the_build() {
    // An intact header and a corrupt body: probe passes for JPEG-free PNG
    // data, decode fails in finish; the page keeps the URL, others still work.
    let f = Fixture::new();
    let bad = format!("https://{HOST}/bad.png");
    let good = format!("https://{HOST}/good.png");
    let mut b = png(300, 200);
    let idat = b.windows(4).position(|w| w == b"IDAT").unwrap() + 4;
    for x in &mut b[idat..idat + 40] {
        *x ^= 0x5a;
    }
    let p = f.pipeline(cfg_yaml("  widths: [100]\n"), false, vec![(&bad, ok(b)), (&good, ok(png(300, 200)))]);
    let html = f.render(&p, &format!("{}\n{}", p.request_img(img(&bad)), p.request_img(img(&good))));
    let mut lines = html.lines();
    assert_eq!(lines.next().unwrap(), format!("<img src=\"{bad}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">"));
    assert!(lines.next().unwrap().starts_with("<picture>"));
    assert_eq!(f.out_files().len(), 2, "only the good image's files");
}

#[test]
fn gif_with_copy_and_animated_images_pass_through() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/a.gif");
    let mut gif = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(30, 10).write_to(&mut gif, ImageFormat::Gif).unwrap();
    let gif = gif.into_inner();
    let p = f.pipeline(cfg_yaml("  animated_gifs: copy\n"), false, vec![(&url, ok(gif.clone()))]);
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
    assert!(matches!(probe(&apng, &cfg_yaml(""), false).unwrap().2, Kind::Passthrough { ext: "png" }));
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
    let e = p.finish(2).unwrap_err();
    assert!(e.contains("dist/page.html: image placeholder for an unknown source has no markup"), "{e}");
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

// ---- animated GIF ----

/// An animated GIF: one solid-color frame per `(rgb, delay in ms)`.
fn anim_gif(w: u32, h: u32, frames: &[([u8; 3], u32)], repeat: Option<image::codecs::gif::Repeat>) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        if let Some(r) = repeat {
            enc.set_repeat(r).unwrap();
        }
        for (rgb, ms) in frames {
            let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
            let d = image::Delay::from_numer_denom_ms(*ms, 1);
            enc.encode_frame(image::Frame::from_parts(buf, 0, 0, d)).unwrap();
        }
    }
    out
}

/// A GIF whose `w` x `h` canvas holds `n` one-pixel frames: a few hundred
/// bytes on the wire, `w * h * n` pixels once composited (a frame bomb).
fn frame_bomb(w: u16, h: u16, n: usize) -> Vec<u8> {
    let mut b = b"GIF89a".to_vec();
    b.extend(w.to_le_bytes());
    b.extend(h.to_le_bytes());
    b.extend([0x80, 0, 0]); // global table of 2 colors
    b.extend([0, 0, 0, 255, 255, 255]);
    b.extend([0x21, 0xFF, 11]);
    b.extend(b"NETSCAPE2.0");
    b.extend([3, 1, 0, 0, 0]);
    for _ in 0..n {
        b.extend([0x21, 0xF9, 4, 0, 1, 0, 0, 0]); // 10 ms
        b.extend([0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]); // 1x1 at 0,0
        b.extend([2, 2, 0x44, 0x01, 0]); // LZW: clear, 0, end
    }
    b.push(0x3B);
    b
}

/// Frames of an animated WebP: (width, height, delay ms, first pixel).
fn webp_frames(bytes: &[u8]) -> Vec<(u32, u32, u32, [u8; 4])> {
    use image::AnimationDecoder;
    let dec = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).unwrap();
    dec.into_frames()
        .map(|f| {
            let f = f.unwrap();
            let (n, d) = f.delay().numer_denom_ms();
            let buf = f.into_buffer();
            (buf.width(), buf.height(), n / d, buf.get_pixel(buf.width() / 2, buf.height() / 2).0)
        })
        .collect()
}

/// The WebP's loop count: 0 plays forever.
fn webp_loop_count(bytes: &[u8]) -> u32 {
    use image::AnimationDecoder;
    match image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).unwrap().loop_count() {
        image::metadata::LoopCount::Infinite => 0,
        image::metadata::LoopCount::Finite(n) => n.get(),
    }
}

fn close(a: [u8; 4], b: [u8; 3]) -> bool {
    a[3] == 255 && (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= 12)
}

const RED: [u8; 3] = [220, 30, 30];
const GREEN: [u8; 3] = [30, 200, 60];
const BLUE: [u8; 3] = [30, 60, 220];

#[test]
fn animated_gif_becomes_resized_animated_webp_with_frames_and_timing() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/anim.gif");
    let gif = anim_gif(200, 100, &[(RED, 70), (GREEN, 130), (BLUE, 250)], Some(image::codecs::gif::Repeat::Infinite));
    let p = f.pipeline(cfg_yaml("  widths: [50, 100, 400]\n"), false, vec![(&url, ok(gif))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    // srcset of animated WebP at each width up to the source (never upscaled), no GIF
    let files = f.out_files();
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(files.iter().all(|n| n.ends_with(".webp")), "{files:?}");
    let name = |w: u32| files.iter().find(|n| n.ends_with(&format!("-{w}.webp"))).unwrap().clone();
    let want = format!(
        "<img src=\"/assets/img/{}\" srcset=\"/assets/img/{} 50w, /assets/img/{} 100w, /assets/img/{} 200w\" sizes=\"auto, 100vw\" alt=\"A\" width=\"200\" height=\"100\" loading=\"lazy\" decoding=\"async\">",
        name(200),
        name(50),
        name(100),
        name(200)
    );
    assert_eq!(html, want);
    for (w, h) in [(50, 25), (100, 50), (200, 100)] {
        let bytes = std::fs::read(f.dist.join("assets/img").join(name(w))).unwrap();
        assert!(is_animated_webp(&bytes), "{w}w is not animated");
        let frames = webp_frames(&bytes);
        assert_eq!(frames.len(), 3, "{w}w frame count");
        for ((fw, fh, ms, px), (rgb, want_ms)) in frames.iter().zip([(RED, 70), (GREEN, 130), (BLUE, 250)]) {
            assert_eq!((*fw, *fh), (w, h));
            assert_eq!(*ms, want_ms, "{w}w timing");
            assert!(close(*px, rgb), "{w}w color {px:?} vs {rgb:?}");
        }
        assert_eq!(webp_loop_count(&bytes), 0);
    }
}

#[test]
fn animation_max_width_caps_every_variant_and_the_src() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/wide.gif");
    let gif = anim_gif(400, 100, &[(RED, 50), (GREEN, 50)], None);
    let cfg = cfg_yaml("  widths: [100, 200, 300, 800]\n  animation_max_width: 250\n");
    assert_eq!(cfg.animation_max_width, 250);
    assert_eq!(cfg_yaml("").animation_max_width, 640, "default");
    let p = f.pipeline(cfg, false, vec![(&url, ok(gif))]);
    let a = p.request_img(img(&url));
    let u = p.request_url(&url, Some(5000), false);
    let html = f.render(&p, &format!("{a}\n{u}"));
    let files = f.out_files();
    let name = |w: u32| files.iter().find(|n| n.ends_with(&format!("-{w}.webp"))).unwrap().clone();
    assert_eq!(files.len(), 3, "100, 200 and the cap 250: {files:?}");
    let want = format!(
        "<img src=\"/assets/img/{}\" srcset=\"/assets/img/{} 100w, /assets/img/{} 200w, /assets/img/{} 250w\" sizes=\"auto, 100vw\" alt=\"A\" width=\"250\" height=\"63\" loading=\"lazy\" decoding=\"async\">\n/assets/img/{}",
        name(250),
        name(100),
        name(200),
        name(250),
        name(250)
    );
    assert_eq!(html, want);
    assert_eq!(webp_frames(&std::fs::read(f.dist.join("assets/img").join(name(250))).unwrap())[0].0, 250);
    // stills are not capped
    let still = format!("https://{HOST}/still.png");
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml("  widths: [800]\n  animation_max_width: 250\n"), false, vec![(&still, ok(png(900, 100)))]);
    assert!(f.render(&p, &p.request_img(img(&still))).contains("-800.webp 800w"));
}

#[test]
fn animation_loop_count_and_tiny_delays_match_browsers() {
    let cfg = cfg_yaml("  widths: [40]\n");
    let frames = [(RED, 0), (GREEN, 10), (BLUE, 20)];
    // no loop extension: browsers play it once
    let once = anim_gif(40, 20, &frames, None);
    let s = gif_scan(&once, &cfg).unwrap();
    assert_eq!((s.width, s.height, s.frames, s.loop_count), (40, 20, 3, 1));
    let out = encode_animation(&once, &[40], s.loop_count, &cfg).unwrap();
    assert_eq!(webp_loop_count(&out[0]), 1);
    // delays of 10 ms or less play at 100 ms, as in browsers; 20 ms stays
    let ms: Vec<u32> = webp_frames(&out[0]).iter().map(|f| f.2).collect();
    assert_eq!(ms, vec![100, 100, 20]);
    // a GIF loop count of n repeats n times after the first play
    let twice = anim_gif(40, 20, &frames, Some(image::codecs::gif::Repeat::Finite(2)));
    assert_eq!(gif_scan(&twice, &cfg).unwrap().loop_count, 3);
}

#[test]
fn animation_limits_refuse_a_frame_bomb_before_decoding() {
    let url = format!("https://{HOST}/bomb.gif");
    let bomb = frame_bomb(4000, 4000, 5000);
    assert!(bomb.len() < 200_000, "the bomb is small on the wire: {} bytes", bomb.len());
    for (extra, why) in [
        ("", "over max_frames (1000)"),
        ("  max_frames: 10000\n", "over max_animation_pixels"),
        ("  max_frames: 10000\n  max_animation_pixels: 100000000000\n  max_pixels: 20000000\n", "per-image budget"),
    ] {
        let f = Fixture::new();
        let p = f.pipeline(cfg_yaml(extra), false, vec![(&url, ok(bomb.clone()))]);
        std::fs::write(f.dist.join("page.html"), p.request_img(img(&url))).unwrap();
        let stats = p.finish(2).unwrap();
        let page = std::fs::read_to_string(f.dist.join("page.html")).unwrap();
        assert_eq!(page, format!("<img src=\"{url}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">"), "{why}");
        assert_eq!(stats.kept.len(), 1);
        assert!(stats.kept[0].1.contains(why), "{:?} lacks {why}", stats.kept[0].1);
        assert!(f.out_files().is_empty());
        assert_eq!(cache_entries(&f, "src"), 0, "refused bytes are not kept");
    }
    // a frame larger than the screen is refused from its descriptor
    let mut big = frame_bomb(10, 10, 2);
    let at = big.windows(2).position(|w| w == [0x2C, 0]).unwrap();
    big[at + 5..at + 9].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    assert!(gif_scan(&big, &cfg_yaml("")).unwrap_err().contains("outside the 10x10 screen"));
}

/// `n` frames of `fw` x `fh` on a `w` x `h` screen, each a single LZW clear
/// and end code (the decoder still allocates and fills the whole frame).
fn oversized_frames(w: u16, h: u16, fw: u16, fh: u16, n: usize) -> Vec<u8> {
    let mut b = b"GIF89a".to_vec();
    b.extend(w.to_le_bytes());
    b.extend(h.to_le_bytes());
    b.extend([0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
    for _ in 0..n {
        b.extend([0x2C, 0, 0, 0, 0]);
        b.extend(fw.to_le_bytes());
        b.extend(fh.to_le_bytes());
        b.extend([0, 2, 2, 0x44, 0x01, 0]);
    }
    b.push(0x3B);
    b
}

#[test]
fn frames_larger_than_the_screen_are_refused_before_decoding() {
    // 2x2 screen, 780 frames of 7000x7000: tiny screen x frames, ~38 Gpx of frames
    let bomb = oversized_frames(2, 2, 7000, 7000, 780);
    let url = format!("https://{HOST}/big-frames.gif");
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(bomb.clone()))]);
    let t = std::time::Instant::now();
    std::fs::write(f.dist.join("page.html"), p.request_img(img(&url))).unwrap();
    let stats = p.finish(2).unwrap();
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "refused without decoding: {:?}", t.elapsed());
    assert_eq!(
        std::fs::read_to_string(f.dist.join("page.html")).unwrap(),
        format!("<img src=\"{url}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">")
    );
    assert!(stats.kept[0].1.contains("outside the 2x2 screen"), "{:?}", stats.kept);
    assert!(f.out_files().is_empty());
    // frames inside the screen still count at their own size against the total
    let cfg = cfg_yaml("  max_animation_pixels: 1000000\n");
    let many = oversized_frames(500, 500, 500, 500, 5);
    assert!(gif_scan(&many, &cfg).unwrap_err().contains("frames total over max_animation_pixels"));
    assert_eq!(gif_scan(&oversized_frames(500, 500, 500, 500, 4), &cfg).unwrap().frame_pixels, 1_000_000);
    // and each frame against max_pixels
    let cfg = cfg_yaml("  max_pixels: 100000\n");
    assert!(gif_scan(&oversized_frames(1000, 1000, 400, 400, 2), &cfg).unwrap_err().contains("over max_pixels"));
}

#[test]
fn animation_max_width_is_validated() {
    for bad in ["0", "16384"] {
        let y = format!("images:\n  animation_max_width: {bad}\n");
        assert!(ImagesCfg::from_site_yaml(&y).unwrap_err().contains("animation_max_width must be 1 to 16383"), "{bad}");
    }
    assert_eq!(cfg_yaml("  animation_max_width: 16383\n").animation_max_width, 16383);
}

#[test]
fn malformed_animations_keep_their_url() {
    let good = anim_gif(60, 30, &[(RED, 50), (GREEN, 50), (BLUE, 50)], None);
    // truncated mid-frame: refused by the block scan
    let truncated = good[..good.len() - 20].to_vec();
    assert!(gif_scan(&truncated, &cfg_yaml("")).unwrap_err().contains("truncated"));
    // intact structure, corrupt LZW data: the scan passes, the decode fails
    let mut corrupt = frame_bomb(60, 30, 3);
    let end = corrupt.len() - 1; // the last frame's data is [2, 0x44, 0x01, 0] before the trailer
    corrupt[end - 3..end - 1].copy_from_slice(&[0xFF, 0xFF]);
    assert!(gif_scan(&corrupt, &cfg_yaml("")).is_ok());
    // garbage after the header
    let mut junk = good[..13 + 6].to_vec();
    junk.extend([0x99, 1, 2, 3]);
    for (name, bytes) in [("cut.gif", truncated), ("lzw.gif", corrupt), ("junk.gif", junk)] {
        let f = Fixture::new();
        let url = format!("https://{HOST}/{name}");
        let ok_url = format!("https://{HOST}/ok.gif");
        let p = f.pipeline(cfg_yaml("  widths: [30]\n"), false, vec![(&url, ok(bytes)), (&ok_url, ok(good.clone()))]);
        std::fs::write(f.dist.join("page.html"), format!("{}\n{}", p.request_img(img(&url)), p.request_img(img(&ok_url))))
            .unwrap();
        let stats = p.finish(2).unwrap();
        let page = std::fs::read_to_string(f.dist.join("page.html")).unwrap();
        let mut lines = page.lines();
        assert_eq!(lines.next().unwrap(), format!("<img src=\"{url}\" alt=\"A\" loading=\"lazy\" decoding=\"async\">"), "{name}");
        assert!(lines.next().unwrap().contains(".webp 30w"), "{name}: the good animation still converts");
        assert_eq!(stats.kept.len(), 1, "{name}: {:?}", stats.kept);
        assert_eq!(f.out_files().len(), 1, "{name}: only the good animation's file");
    }
}

#[test]
fn animated_gif_image_url_copy_mode_and_single_frame_gif() {
    let gif = anim_gif(200, 100, &[(RED, 70), (GREEN, 70)], None);
    let url = format!("https://{HOST}/a.gif");
    // image_url: the animated WebP at the requested width, clamped to the source
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml("  widths: [100]\n"), false, vec![(&url, ok(gif.clone()))]);
    let a = p.request_url(&url, None, false);
    let b = p.request_url(&url, Some(5000), true);
    let html = f.render(&p, &format!("{a} {b}"));
    let files = f.out_files();
    assert_eq!(files.len(), 2, "{files:?}");
    assert_eq!(html, format!("/assets/img/{} https://example.com/assets/img/{}", files[0], files[1]), "{files:?}");
    assert!(files[0].ends_with("-100.webp") && files[1].ends_with("-200.webp"), "{files:?}");
    assert_eq!(webp_frames(&std::fs::read(f.dist.join("assets/img").join(&files[1])).unwrap()).len(), 2);
    // animated_gifs: copy keeps the GIF byte for byte
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml("  animated_gifs: copy\n"), false, vec![(&url, ok(gif.clone()))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.contains(".gif\" alt=\"A\" width=\"200\" height=\"100\""), "{html}");
    assert_eq!(std::fs::read(f.dist.join("assets/img").join(&f.out_files()[0])).unwrap(), gif);
    // a single-frame GIF is a still: srcset and a fallback like any image
    let mut still = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(120, 60, image::Rgb(RED))).write_to(&mut still, ImageFormat::Gif).unwrap();
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml("  widths: [60]\n"), false, vec![(&url, ok(still.into_inner()))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.starts_with("<picture><source type=\"image/webp\""), "{html}");
    assert!(f.out_files().iter().any(|n| n.ends_with(".jpg")), "{:?}", f.out_files());
}

#[test]
fn animation_memory_estimate_tracks_size_and_widths() {
    let one = animation_memory(1000, 500, &[1000]);
    assert_eq!(one, 1000 * 500 * 16 + 1000 * 500 * 16 + 1000 * 500 * 32);
    assert!(animation_memory(1000, 500, &[1000, 500, 250]) > one);
    let cfg = cfg_yaml("");
    let s = GifScan { width: 1600, height: 1080, frames: 300, frame_pixels: 1600 * 1080 * 300, loop_count: 0 };
    assert!(check_animation(&s, &cfg).is_ok(), "a large real-world animation fits the default budget");
}

/// Every composited RGBA frame of an animated WebP, from libwebp's own
/// `WebPAnimDecoder` (test-only FFI: the pipeline never decodes with libwebp).
fn libwebp_anim_frames(data: &[u8]) -> Vec<image::RgbaImage> {
    use libwebp_sys::*;
    // SAFETY: options and info are filled by libwebp and checked; the
    // decoder reads `data` (alive for the whole block) and is deleted once;
    // each frame buffer is owned by the decoder and copied before the next
    // call, with its length from the canvas size libwebp reported.
    unsafe {
        let mut opts = std::mem::MaybeUninit::<WebPAnimDecoderOptions>::uninit();
        assert_ne!(WebPAnimDecoderOptionsInitInternal(opts.as_mut_ptr(), WebPGetDemuxABIVersion()), 0);
        let mut opts = opts.assume_init();
        opts.color_mode = WEBP_CSP_MODE::MODE_RGBA;
        let wd = WebPData { bytes: data.as_ptr(), size: data.len() };
        let dec = WebPAnimDecoderNewInternal(&wd, &opts, WebPGetDemuxABIVersion());
        assert!(!dec.is_null(), "not an animated WebP libwebp accepts");
        let mut info = std::mem::MaybeUninit::<WebPAnimInfo>::zeroed();
        assert_ne!(WebPAnimDecoderGetInfo(dec, info.as_mut_ptr()), 0);
        let info = info.assume_init();
        let len = info.canvas_width as usize * info.canvas_height as usize * 4;
        let mut frames = Vec::new();
        while WebPAnimDecoderHasMoreFrames(dec) != 0 {
            let (mut buf, mut ts) = (std::ptr::null_mut(), 0);
            assert_ne!(WebPAnimDecoderGetNext(dec, &mut buf, &mut ts), 0);
            assert!(!buf.is_null());
            let px = std::slice::from_raw_parts(buf, len).to_vec();
            frames.push(image::RgbaImage::from_raw(info.canvas_width, info.canvas_height, px).unwrap());
        }
        WebPAnimDecoderDelete(dec);
        frames
    }
}

#[test]
fn transparent_animation_keeps_one_bit_alpha_without_dark_edges() {
    // left half transparent, right half a color that changes per frame
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        for rgb in [RED, GREEN] {
            let buf = image::RgbaImage::from_fn(90, 40, |x, _| if x < 45 { image::Rgba([0, 0, 0, 0]) } else { image::Rgba([rgb[0], rgb[1], rgb[2], 255]) });
            enc.encode_frame(image::Frame::from_parts(buf, 0, 0, image::Delay::from_numer_denom_ms(100, 1))).unwrap();
        }
    }
    let cfg = cfg_yaml("");
    let s = gif_scan(&out, &cfg).unwrap();
    let files = encode_animation(&out, &[60], s.loop_count, &cfg).unwrap();
    // libwebp's own animation decoder: the canvas starts transparent, as in browsers
    let frames = libwebp_anim_frames(&files[0]);
    assert_eq!(frames.len(), 2);
    for (f, rgb) in frames.iter().zip([RED, GREEN]) {
        assert_eq!(f.dimensions(), (60, 27));
        assert!(f.pixels().all(|p| p.0[3] == 0 || p.0[3] == 255), "alpha is on or off");
        // the first opaque pixel on a row keeps the color (no black blended in)
        let edge = (0..60).map(|x| f.get_pixel(x, 13)).find(|p| p.0[3] == 255).unwrap();
        assert!(close(edge.0, rgb), "edge {:?} vs {rgb:?}", edge.0);
        assert_eq!(f.get_pixel(5, 13).0[3], 0);
    }
}

// ---- AVIF sources ----

/// An AVIF encoded by the image crate (rav1e): a gradient, with alpha when
/// `alpha` is set (left half opaque, right half transparent).
fn avif(w: u32, h: u32, alpha: bool) -> Vec<u8> {
    let mut b = Cursor::new(Vec::new());
    let e = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut b, 10, 90);
    if alpha {
        let img = image::RgbaImage::from_fn(w, h, |x, _| image::Rgba([200, 40, 40, if x < w / 2 { 255 } else { 0 }]));
        DynamicImage::ImageRgba8(img).write_with_encoder(e).unwrap();
    } else {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 90]));
        DynamicImage::ImageRgb8(img).write_with_encoder(e).unwrap();
    }
    b.into_inner()
}

/// `avif::decode` in the child process `cargo test` builds (build-site).
fn decode_avif(bytes: &[u8], max_pixels: u64) -> Result<DynamicImage, String> {
    avif::decode(bytes, max_pixels, &default_decoder_exe()?, std::time::Duration::from_secs(60))
}

#[test]
fn avif_sources_are_decoded_and_re_encoded_like_other_stills() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/photo.avif");
    let src = avif(96, 48, false);
    assert_eq!(&src[4..12], b"ftypavif");
    let p = f.pipeline(cfg_yaml("  widths: [48, 128]\n"), false, vec![(&url, ok(src.clone()))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.contains("width=\"96\" height=\"48\""), "{html}");
    let files = f.out_files();
    // 48 and 96 in WebP plus a JPEG fallback (no alpha item)
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(files.iter().any(|n| n.ends_with("-96.jpg")), "{files:?}");
    let jpg = files.iter().find(|n| n.ends_with(".jpg")).unwrap();
    let out = image::open(f.dist.join("assets/img").join(jpg)).unwrap().to_rgb8();
    assert_eq!(out.dimensions(), (96, 48));
    // colors survive the YUV round trip (the gradient's corners)
    for (x, y, want) in [(2, 2, [5u8, 10, 90]), (93, 45, [250, 240, 90])] {
        let got = out.get_pixel(x, y).0;
        assert!(got.iter().zip(want).all(|(g, w)| (*g as i32 - w as i32).abs() <= 16), "({x},{y}) {got:?} vs {want:?}");
    }
}

#[test]
fn avif_with_alpha_gets_a_png_fallback_with_its_transparency() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/logo.avif");
    let p = f.pipeline(cfg_yaml("  widths: [64]\n"), false, vec![(&url, ok(avif(64, 32, true)))]);
    f.render(&p, &p.request_img(img(&url)));
    let files = f.out_files();
    let png = files.iter().find(|n| n.ends_with(".png")).unwrap_or_else(|| panic!("{files:?}"));
    let out = image::open(f.dist.join("assets/img").join(png)).unwrap().to_rgba8();
    assert!(out.get_pixel(4, 16).0[3] > 240 && out.get_pixel(60, 16).0[3] < 15);
    let c = out.get_pixel(4, 16).0;
    assert!((c[0] as i32 - 200).abs() < 20 && c[1] < 70 && c[2] < 70, "{c:?}");
}

#[test]
fn avif_pixel_limit_is_checked_before_decoding() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/huge.avif");
    let cfg = cfg_yaml("  max_pixels: 1000\n");
    let src = avif(64, 32, false);
    let err = probe(&src, &cfg, false).unwrap_err();
    assert!(err.contains("over max_pixels"), "{err}");
    // the decoder refuses too, from the header alone
    assert!(decode_avif(&src, 1000).unwrap_err().contains("max_pixels"));
    let p = f.pipeline(cfg, false, vec![(&url, ok(src))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.starts_with(&format!("<img src=\"{url}\"")), "{html}");
    assert!(f.out_files().is_empty());
}

#[test]
fn corrupt_avif_keeps_its_url() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/bad.avif");
    let mut src = avif(64, 32, false);
    // keep the container, overwrite the AV1 data (the mdat payload)
    let at = src.windows(4).position(|w| w == b"mdat").unwrap() + 4;
    for b in &mut src[at..] {
        *b = 0xff;
    }
    let p = f.pipeline(cfg_yaml("  widths: [32]\n"), false, vec![(&url, ok(src))]);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert!(html.starts_with(&format!("<img src=\"{url}\"")), "{html}");
    assert!(f.out_files().is_empty());
}

// ---- retries ----

/// What the loopback server does with each request, in order; the last
/// entry repeats.
#[derive(Clone)]
enum Reply {
    /// Wait this long before answering 200 (past the client's timeout).
    Slow(u64),
    Status(u16),
    Ok,
}

/// A plain-http server on a free loopback port serving `body` per `script`.
/// Returns the port and the request counter.
fn script_server(body: Vec<u8>, script: Vec<Reply>) -> (u16, Arc<AtomicUsize>) {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let n = Arc::new(AtomicUsize::new(0));
    let count = n.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut c) = conn else { continue };
            let i = count.fetch_add(1, Ordering::SeqCst);
            let reply = script[i.min(script.len() - 1)].clone();
            let body = body.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(k) => req.extend_from_slice(&buf[..k]),
                    }
                }
                let (status, body) = match reply {
                    Reply::Slow(ms) => {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        (200, body)
                    }
                    Reply::Status(s) => (s, Vec::new()),
                    Reply::Ok => (200, body),
                };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = c.write_all(head.as_bytes());
                let _ = c.write_all(&body);
            });
        }
    });
    (port, n)
}

fn loopback_pipeline(f: &Fixture, cfg: ImagesCfg, port: u16) -> Pipeline {
    Pipeline::new(cfg, &f.root, &f.dist, "https://example.com", false, loopback_transport(port))
}

#[test]
fn flaky_5xx_is_retried_until_it_succeeds() {
    let f = Fixture::new();
    let (port, hits) = script_server(png(40, 20), vec![Reply::Status(503), Reply::Status(502), Reply::Ok]);
    let url = format!("https://{HOST}/flaky.png");
    let p = loopback_pipeline(&f, cfg_yaml("  widths: [20]\n  timeout_secs: 5\n"), port);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    assert!(html.contains("/assets/img/"), "{html}");
}

#[test]
fn a_timed_out_fetch_is_retried() {
    let f = Fixture::new();
    let (port, hits) = script_server(png(40, 20), vec![Reply::Slow(2500), Reply::Ok]);
    let url = format!("https://{HOST}/slow.png");
    let p = loopback_pipeline(&f, cfg_yaml("  widths: [20]\n  timeout_secs: 1\n"), port);
    let html = f.render(&p, &p.request_img(img(&url)));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(html.contains("/assets/img/"), "{html}");
}

#[test]
fn retries_are_bounded_by_attempts_and_total_time_then_the_url_is_kept() {
    // attempts: 1 + 2 retries
    let f = Fixture::new();
    let (port, hits) = script_server(png(40, 20), vec![Reply::Status(503)]);
    let url = format!("https://{HOST}/down.png");
    let p = loopback_pipeline(&f, cfg_yaml("  retries: 2\n  timeout_secs: 1\n"), port);
    std::fs::write(f.dist.join("page.html"), p.request_img(img(&url))).unwrap();
    let stats = p.finish(1).unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    assert_eq!(stats.kept, vec![(url.clone(), "HTTP 503 (after 3 attempts)".to_string())]);
    let html = std::fs::read_to_string(f.dist.join("page.html")).unwrap();
    assert!(html.starts_with(&format!("<img src=\"{url}\"")), "{html}");
    // total time: many retries allowed, but the source gives up within its budget
    let f = Fixture::new();
    let (port, _) = script_server(png(40, 20), vec![Reply::Slow(5000)]);
    let p = loopback_pipeline(&f, cfg_yaml("  retries: 10\n  timeout_secs: 1\n  retry_total_secs: 2\n"), port);
    std::fs::write(f.dist.join("page.html"), p.request_img(img(&url))).unwrap();
    let t = std::time::Instant::now();
    let stats = p.finish(1).unwrap();
    assert!(t.elapsed() < std::time::Duration::from_secs(4), "{:?}", t.elapsed());
    assert_eq!(stats.kept.len(), 1, "{stats:?}");
}

#[test]
fn client_errors_are_not_retried() {
    let f = Fixture::new();
    let (port, hits) = script_server(Vec::new(), vec![Reply::Status(404)]);
    let url = format!("https://{HOST}/missing.png");
    let p = loopback_pipeline(&f, cfg_yaml(""), port);
    std::fs::write(f.dist.join("page.html"), p.request_img(img(&url))).unwrap();
    let stats = p.finish(1).unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(stats.kept, vec![(url, "HTTP 404".to_string())]);
}

#[test]
fn retry_config_is_validated() {
    let y = format!("images:\n  allowed_hosts: [{HOST}]\n  retries: 11\n");
    assert!(ImagesCfg::from_site_yaml(&y).unwrap_err().contains("retries"));
    let c = cfg_yaml("");
    assert_eq!((c.retries, c.retry_total_secs), (3, 120));
}

// ---- cache verification ----

#[test]
fn tampered_cache_entries_are_evicted_and_rebuilt() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/t.png");
    let cfg = || cfg_yaml("  widths: [50]\n");
    let p = f.pipeline(cfg(), false, vec![(&url, ok(png(100, 50)))]);
    let first = f.render(&p, &p.request_img(img(&url)));
    let names = f.out_files();
    let originals: Vec<Vec<u8>> = names.iter().map(|n| std::fs::read(f.dist.join("assets/img").join(n)).unwrap()).collect();
    let out = f.root.join(".cache/images/out");
    // every cached variant has a recorded hash of its bytes
    for n in &names {
        let rec = std::fs::read_to_string(out.join(format!("{n}.sha256"))).unwrap();
        assert_eq!(rec, sha_hex(&std::fs::read(out.join(n)).unwrap()));
    }
    // tamper one variant (a valid image, different bytes) and drop another's record
    std::fs::write(out.join(&names[0]), png(1, 1)).unwrap();
    std::fs::remove_file(out.join(format!("{}.sha256", names[1]))).unwrap();
    let rebuild = || {
        std::fs::remove_dir_all(&f.dist).unwrap();
        std::fs::create_dir_all(&f.dist).unwrap();
        let p = f.pipeline(cfg(), true, vec![]);
        f.render(&p, &p.request_img(img(&url)))
    };
    let second = rebuild();
    assert_eq!(first, second);
    for (n, want) in names.iter().zip(&originals) {
        assert_eq!(&std::fs::read(f.dist.join("assets/img").join(n)).unwrap(), want, "{n} not rebuilt");
        assert_eq!(&std::fs::read(out.join(n)).unwrap(), want, "{n} cache not repaired");
    }
    // a tampered source: evicted, fetched again online, same output
    let p = f.pipeline(cfg(), false, vec![]);
    let idx = std::fs::read_to_string(p.url_index(&url)).unwrap();
    std::fs::write(p.src_file(idx.trim()), png(3, 3)).unwrap();
    let calls = f.calls.load(Ordering::SeqCst);
    std::fs::remove_dir_all(&f.dist).unwrap();
    std::fs::create_dir_all(&f.dist).unwrap();
    let p = f.pipeline(cfg(), false, vec![(&url, ok(png(100, 50)))]);
    assert_eq!(f.render(&p, &p.request_img(img(&url))), first);
    assert_eq!(f.calls.load(Ordering::SeqCst), calls + 1, "tampered source was not refetched");
    assert_eq!(sha_hex(&std::fs::read(p.src_file(idx.trim())).unwrap()), idx.trim());
    // a source that changes between resolve and encode is refused, not used
    let s = Source { content: idx.trim().to_string(), id: "0".repeat(16), width: 100, height: 50, kind: Kind::Raster { fallback: Enc::Jpeg } };
    std::fs::write(p.src_file(&s.content), b"swapped").unwrap();
    assert!(p.source_bytes(&s).unwrap_err().contains("does not match"));
}

#[test]
fn ten_bit_avif_decodes() {
    let img = image::ImageBuffer::<image::Rgb<u16>, _>::from_fn(32, 16, |_, _| image::Rgb([60000u16, 30000, 5000]));
    let mut b = Cursor::new(Vec::new());
    let e = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut b, 10, 90);
    DynamicImage::ImageRgb16(img).write_with_encoder(e).unwrap();
    let out = decode_avif(&b.into_inner(), 1_000_000).unwrap().to_rgb8();
    let c = out.get_pixel(8, 8).0;
    let want = [233u8, 117, 19];
    assert!(c.iter().zip(want).all(|(g, w)| (*g as i32 - w as i32).abs() <= 12), "{c:?}");
}

// ---- AVIF decoder isolation ----

/// A fuzzed AVIF (1 to 8 bytes changed from a valid file) on which rav1d
/// panics inside `dav1d_send_data`, which aborts the process it runs in.
const RAV1D_ABORT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/avif-rav1d-abort.avif"));
/// 2.4 KB: 7000x7000, 12-bit 4:4:4 with an alpha item.
const AVIF_7000_12BIT: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/avif-7000-12bit-444-alpha.avif"));

#[test]
fn avif_that_crashes_the_decoder_fails_only_that_image() {
    let f = Fixture::new();
    let bad = format!("https://{HOST}/crash.avif");
    let good = format!("https://{HOST}/fine.avif");
    let photo = format!("https://{HOST}/photo.png");
    let p = f.pipeline(
        cfg_yaml("  widths: [32]\n"),
        false,
        vec![(&bad, ok(RAV1D_ABORT.to_vec())), (&good, ok(avif(64, 32, false))), (&photo, ok(png(64, 32)))],
    );
    let body = [&bad, &good, &photo].iter().map(|u| p.request_img(img(u))).collect::<Vec<_>>().join("\n");
    std::fs::write(f.dist.join("page.html"), body).unwrap();
    let stats = p.finish(4).unwrap();
    assert_eq!(stats.kept.len(), 1, "{stats:?}");
    assert_eq!(stats.kept[0].0, bad);
    assert!(stats.kept[0].1.contains("decoder crashed"), "{stats:?}");
    let html = std::fs::read_to_string(f.dist.join("page.html")).unwrap();
    assert!(html.contains(&format!("<img src=\"{bad}\"")), "{html}");
    // the other two sources are written: a WebP and a JPEG each
    assert_eq!(stats.sources, 2, "{stats:?}");
    assert_eq!(f.out_files().len(), 4, "{:?}", f.out_files());
}

/// A few hundred small mutants of valid AVIFs (with and without alpha):
/// every decode returns, as an image or an error, and this process lives.
#[test]
fn mutated_avifs_never_take_the_build_down() {
    let seeds = [avif(48, 24, false), avif(32, 32, true)];
    // a mutated header may claim a large frame: keep decodes small
    let cfg = cfg_yaml("  max_pixels: 1000000\n");
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut cases = vec![RAV1D_ABORT.to_vec()];
    for i in 0..300 {
        let mut m = seeds[i % 2].clone();
        // mutate past the ftyp box, mostly in the AV1 data where rav1d reads
        for _ in 0..1 + next() % 8 {
            let at = 32 + (next() as usize) % (m.len() - 32);
            m[at] = next() as u8;
        }
        cases.push(m);
    }
    let results: Vec<Result<DynamicImage, String>> = std::thread::scope(|s| {
        let chunks: Vec<_> = cases.chunks(cases.len().div_ceil(8)).collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|c| s.spawn(|| c.iter().map(|b| decode(b, &cfg)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(results.len(), 301);
    let crashed = results.iter().filter(|r| matches!(r, Err(e) if e.contains("crashed"))).count();
    let decoded = results.iter().filter(|r| r.is_ok()).count();
    eprintln!("{} mutants: {decoded} decoded, {crashed} decoder crashes, the rest refused", results.len());
    assert!(results[0].as_ref().is_err_and(|e| e.contains("crashed")), "{:?}", results[0].as_ref().err());
    // decoded images are the probed size
    for (b, r) in cases.iter().zip(&results) {
        if let Ok(img) = r {
            let p = avif::probe(b, cfg.max_pixels).unwrap();
            assert_eq!((img.width(), img.height()), (p.width, p.height));
        }
    }
}

#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn avif_decoder_hangs_crashes_and_bad_output_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let src = avif(16, 8, false);
    let run = |body: &str, timeout: u64| {
        let exe = script(dir.path(), &format!("d{}", sha_hex(body.as_bytes())), body);
        let mut cfg = cfg_yaml(&format!("  decode_timeout_secs: {timeout}\n"));
        cfg.decoder_exe = Some(exe);
        decode(&src, &cfg).map(|_| ()).unwrap_err()
    };
    let t = std::time::Instant::now();
    let e = run("cat >/dev/null; sleep 30", 1);
    assert!(e.contains("timed out after 1s"), "{e}");
    assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
    let e = run("cat >/dev/null; kill -SEGV $$", 5);
    assert!(e.contains("decoder crashed (signal 11)"), "{e}");
    let e = run("cat >/dev/null; echo 'avif: nope' >&2; exit 2", 5);
    assert_eq!(e, "avif: nope");
    // exit 0 with the wrong size, a short body, or junk: refused
    let e = run("cat >/dev/null; printf 'PAV1\\001\\000\\000\\000\\001\\000\\000\\000\\003abc'", 5);
    assert!(e.contains("bad decoder output: unexpected header"), "{e}");
    let e = run("cat >/dev/null; printf 'PAV1\\020\\000\\000\\000\\010\\000\\000\\000\\003abc'", 5);
    assert!(e.contains("bad decoder output") && e.contains("expected 384"), "{e}");
    let e = run("cat >/dev/null; echo hello", 5);
    assert!(e.contains("bad decoder output"), "{e}");
    let mut cfg = cfg_yaml("");
    cfg.decoder_exe = Some(dir.path().join("missing"));
    assert!(decode(&src, &cfg).unwrap_err().contains("cannot start the decoder"));
}

#[test]
fn avif_decode_memory_counts_bit_depth_chroma_and_alpha() {
    let f = |bit_depth, subsampling, monochrome| avif::Format { bit_depth, subsampling, monochrome };
    let mono12 = f(12, (true, true), true);
    // bytes per pixel: rav1d's working set (2x the picture) plus the output
    for (color, alpha, want) in [
        (f(8, (true, true), false), None, 6),               // 8-bit 4:2:0: 3 + RGB 3
        (f(10, (true, true), false), None, 9),              // 6 + 3
        (f(8, (false, false), false), Some(mono12), 11),    // alpha 1 + 6 + RGBA 4
        (f(12, (false, false), false), Some(mono12), 17),   // alpha 1 + 12 + RGBA 4
        (f(12, (true, false), true), Some(mono12), 9),      // monochrome: alpha 1 + 4 + 4
    ] {
        assert_eq!(avif::decode_memory(1_000_000, color, alpha), 1_000_000 * want, "{color:?}");
    }
    // a 2.4 KB file declaring 7000x7000 12-bit 4:4:4 with alpha: under
    // max_pixels, but its decode (about 800 MB) is over the 400 MB budget,
    // so it is refused from the header, before any pixel is decoded
    let p = avif::probe(AVIF_7000_12BIT, 50_000_000).unwrap();
    assert_eq!((p.width, p.height, p.alpha), (7000, 7000, true));
    assert_eq!(p.memory, 49_000_000 * 17);
    let cfg = cfg_yaml("");
    let e = probe(AVIF_7000_12BIT, &cfg, false).unwrap_err();
    assert!(e.contains("over the 381 MB per-image budget"), "{e}");
    let e = decode(AVIF_7000_12BIT, &cfg).unwrap_err();
    assert!(e.contains("per-image budget"), "{e}");
    // the same budget lets an 8-bit 4:2:0 image of max_pixels through
    assert!(avif::decode_memory(50_000_000, f(8, (true, true), false), None) <= 400_000_000);
}

#[test]
fn fetch_deadline_is_capped_and_long_settings_are_refused() {
    let now = std::time::Instant::now();
    let mut cfg = cfg_yaml("");
    assert_eq!(fetch_deadline(now, &cfg), now + std::time::Duration::from_secs(120));
    // a config built in code skips validation: still no overflow panic
    cfg.retry_total_secs = u64::MAX;
    assert_eq!(fetch_deadline(now, &cfg), now + std::time::Duration::from_secs(MAX_SECS));
    cfg.retry_total_secs = 1;
    cfg.timeout_secs = u64::MAX;
    assert_eq!(fetch_deadline(now, &cfg), now + std::time::Duration::from_secs(MAX_SECS));
    for key in ["retry_total_secs", "timeout_secs", "decode_timeout_secs"] {
        for bad in ["0", "601", "18446744073709551615"] {
            let y = format!("images:\n  allowed_hosts: [{HOST}]\n  {key}: {bad}\n");
            let e = ImagesCfg::from_site_yaml(&y).unwrap_err();
            assert!(e.contains(&format!("{key} must be 1 to 600")), "{key}={bad}: {e}");
        }
        let y = format!("images:\n  allowed_hosts: [{HOST}]\n  {key}: 600\n");
        assert!(ImagesCfg::from_site_yaml(&y).is_ok());
    }
}

// ---- leftover placeholders (a page or .gz that still has one) ----

fn gz(text: &str) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, text.as_bytes()).unwrap();
    enc.finish().unwrap()
}

#[test]
fn verify_passes_a_finished_dist() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/photo.png");
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(png(500, 250)))]);
    let m = p.request_img(img(&url));
    let html = f.render(&p, &format!("<p>{m}</p>"));
    std::fs::write(f.dist.join("page.html.gz"), gz(&html)).unwrap();
    verify_dist(&f.dist, Some(&p)).unwrap();
}

#[test]
fn verify_names_the_page_and_source_of_a_placeholder_left_in_a_gz_copy() {
    let f = Fixture::new();
    let url = format!("https://{HOST}/photo.png");
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(png(500, 250)))]);
    let m = p.request_img(img(&url));
    let html = f.render(&p, &format!("<p>{m}</p>"));
    // a .gz written from the page before its placeholders were replaced
    std::fs::create_dir_all(f.dist.join("events/a")).unwrap();
    std::fs::write(f.dist.join("events/a/index.html"), &html).unwrap();
    std::fs::write(f.dist.join("events/a/index.html.gz"), gz(&format!("<p>{m}</p>"))).unwrap();
    let e = verify_dist(&f.dist, Some(&p)).unwrap_err();
    assert!(e.starts_with("1 unreplaced image placeholder(s)"), "{e}");
    assert!(e.contains(&format!("dist/events/a/index.html.gz (precompressed copy): image placeholder for {url}")), "{e}");
}

#[test]
fn verify_flags_placeholders_from_another_build_and_cut_off_ones() {
    let f = Fixture::new();
    let p = f.pipeline(cfg_yaml(""), false, vec![]);
    std::fs::write(f.dist.join("a.html"), "<img src=\"__postcms_img_0123456789ab_7__\">").unwrap();
    std::fs::write(f.dist.join("b.json"), "{\"x\": \"__postcms_img_01\"}").unwrap();
    // images and other binary output are not scanned
    std::fs::write(f.dist.join("c.webp"), "__postcms_img_0123456789ab_7__").unwrap();
    let e = verify_dist(&f.dist, Some(&p)).unwrap_err();
    assert!(e.starts_with("2 unreplaced"), "{e}");
    assert!(e.contains("dist/a.html: image placeholder from another build (nonce 0123456789ab)"), "{e}");
    assert!(e.contains("dist/b.json: a cut-off image placeholder"), "{e}");
    // without a pipeline (images off) a placeholder is still an error
    assert!(verify_dist(&f.dist, None).unwrap_err().contains("another build"));
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_fails_the_build_instead_of_keeping_its_placeholders() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let url = format!("https://{HOST}/photo.png");
    let p = f.pipeline(cfg_yaml(""), false, vec![(&url, ok(png(500, 250)))]);
    let m = p.request_img(img(&url));
    let dir = f.dist.join("locked");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), format!("<p>{m}</p>")).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = p.finish(2);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::read_dir(&dir).is_ok() && unsafe { libc::geteuid() } == 0 {
        return; // root reads anything; nothing to test
    }
    let e = r.unwrap_err();
    assert!(e.contains("locked"), "{e}");
}

/// Cache keys and output names are SHA-256 hex digests; a hashing crate
/// upgrade must not change them, or every image cache is invalidated.
#[test]
fn sha_hex_matches_fips_180_vectors() {
    assert_eq!(sha_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(sha_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    assert_eq!(sha_hex(long), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
}

/// Retry jitter is derived from the URL hash: same input, same delay, within
/// a quarter of the base.
#[test]
fn retry_delay_is_deterministic_and_bounded() {
    let a = retry_delay("https://img.example.com/a.png", 1);
    assert_eq!(a, retry_delay("https://img.example.com/a.png", 1));
    let base = RETRY_BASE_MS * 2;
    assert!(a.as_millis() as u64 >= base && a.as_millis() as u64 <= base + base / 4, "{a:?}");
}

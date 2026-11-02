//! build-site sandboxes templates: `get_env` cannot read the build
//! environment, `throw` never puts its message into the build output, `now`
//! still works (in UTC), and a template symlink that points outside the
//! templates directory stops the build before the file is read.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SECRET: &str = "s3cr3t-probe-e2e";
const ECHO: &str = "echo-marker-e2e";

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

/// A one-page site whose page template ends with `probe`. The page sets
/// `no_index: true`, so the build loads no embedding model (no network).
fn site(root: &Path, probe: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    write(root, "site.yaml", "theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\n");
    write(
        root,
        "templates/page.html",
        &format!("<!DOCTYPE html><html><head><title>{{{{ title }}}}</title></head><body><p id=\"probe\">{probe}</p></body></html>\n"),
    );
    write(root, "content/pages/about/meta.yaml", "title: About\nslug: about\npublished: true\nno_index: true\n");
    write(root, "content/pages/about/index.mdx", "Hello.\n");
}

fn build(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_build-site"))
        .current_dir(root)
        .env("SITE_ROOT", root)
        .env("POSTCMS_LEAK_PROBE", SECRET)
        .env_remove("SITE_PREVIEW_THEMES")
        .env_remove("SITE_THEME")
        .env_remove("SITE_BASE_URL")
        .env_remove("DRAFTS")
        .output()
        .unwrap()
}

/// stdout, stderr and every file written under dist/.
fn everything(root: &Path, out: &Output) -> String {
    fn walk(dir: &Path, acc: &mut String) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, acc);
            } else {
                acc.push_str(&String::from_utf8_lossy(&fs::read(&p).unwrap()));
            }
        }
    }
    let mut acc = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    walk(&root.join("dist"), &mut acc);
    acc
}

#[test]
fn get_env_cannot_read_the_build_environment() {
    for probe in [
        r#"{{ get_env(name="POSTCMS_LEAK_PROBE") }}"#,
        r#"{{ get_env(name="POSTCMS_LEAK_PROBE", default="x") }}"#,
        r#"{{ get_env(name="POSTCMS_LEAK_PROBE") | int }}"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        site(tmp.path(), probe);
        let out = build(tmp.path());
        let seen = everything(tmp.path(), &out);
        assert!(!out.status.success(), "{probe}: build succeeded");
        assert!(seen.contains("get_env is disabled"), "{probe}: {seen}");
        assert!(!seen.contains(SECRET), "{probe} leaked the variable:\n{seen}");
    }
}

#[test]
fn throw_does_not_echo_its_message() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path(), &format!(r#"{{{{ throw(message="{ECHO}") }}}}"#));
    let out = build(tmp.path());
    let seen = everything(tmp.path(), &out);
    assert!(!out.status.success());
    assert!(seen.contains("a template called throw()"), "{seen}");
    assert!(!seen.contains(ECHO), "throw echoed its message:\n{seen}");
}

#[test]
fn now_still_renders_a_year() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path(), r#"{{ now() | date(format="%Y") }}"#);
    let out = build(tmp.path());
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let html = fs::read_to_string(tmp.path().join("dist/about/index.html")).unwrap();
    let year = chrono::Utc::now().format("%Y").to_string();
    assert!(html.contains(&format!("<p id=\"probe\">{year}</p>")), "{html}");
}

#[cfg(unix)]
#[test]
fn a_template_symlink_out_of_templates_stops_the_build() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root, "ok");
    let outside = tmp.path().join("private.html");
    fs::write(&outside, format!("{SECRET} {{{{ broken")).unwrap();
    for link in ["templates/partials/leak.html", "themes/astra/templates/leak.html"] {
        let at = root.join(link);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        symlink(&outside, &at).unwrap();
        let out = build(&root);
        let seen = everything(&root, &out);
        assert!(!out.status.success(), "{link}: build succeeded");
        assert!(seen.contains("points outside"), "{link}: {seen}");
        assert!(!seen.contains(SECRET), "{link}: the linked file was read:\n{seen}");
        fs::remove_file(&at).unwrap();
    }
    let out = build(&root);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// `site.yaml` pointing at `../../outside`, which holds only `files`.
fn outside_theme(tmp: &Path, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = tmp.join("site");
    site(&root, "ok");
    for (rel, body) in files {
        write(tmp, &format!("outside/{rel}"), body);
    }
    write(&root, "site.yaml", "theme: ../../outside\nbuiltin_pages: false\nbase_url: https://www.example.com\n");
    root
}

fn assert_refused(root: &Path, out: &Output, why: &str) {
    let seen = everything(root, out);
    assert!(!out.status.success(), "build succeeded");
    assert!(seen.contains(why), "expected {why:?}:\n{seen}");
    assert!(!seen.contains(SECRET), "a file outside the site was read:\n{seen}");
}

#[test]
fn a_theme_outside_the_site_cannot_supply_templates() {
    let tmp = tempfile::tempdir().unwrap();
    let root = outside_theme(tmp.path(), &[("templates/leak.html", &format!("{SECRET} {{{{ broken")), ("theme.css", "body{}\n")]);
    assert_refused(&root, &build(&root), "not a valid theme name");
}

#[test]
fn a_dotdot_theme_with_only_theme_css_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let css = format!(".{SECRET} {{ color: #000; }}\n");
    let root = outside_theme(tmp.path(), &[("theme.css", &css)]);
    assert_refused(&root, &build(&root), "not a valid theme name");
}

#[test]
fn bad_preview_theme_names_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root, "ok");
    write(tmp.path(), "outside/theme.css", &format!(".{SECRET} {{ color: #000; }}\n"));
    let out = Command::new(env!("CARGO_BIN_EXE_build-site"))
        .current_dir(&root)
        .env("SITE_ROOT", &root)
        .env("SITE_PREVIEW_THEMES", "astra,../../outside")
        .env_remove("SITE_THEME")
        .env_remove("SITE_BASE_URL")
        .env_remove("DRAFTS")
        .output()
        .unwrap();
    assert_refused(&root, &out, "SITE_PREVIEW_THEMES");
}

#[cfg(unix)]
#[test]
fn a_theme_directory_linked_outside_the_site_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root, "ok");
    write(tmp.path(), "outside/theme.css", &format!(".{SECRET} {{ color: #000; }}\n"));
    std::os::unix::fs::symlink(tmp.path().join("outside"), root.join("themes/linked")).unwrap();
    write(&root, "site.yaml", "theme: linked\nbuiltin_pages: false\nbase_url: https://www.example.com\n");
    assert_refused(&root, &build(&root), "outside the site root");
}

#[cfg(unix)]
#[test]
fn a_theme_file_linked_outside_the_theme_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root, "ok");
    let css = root.join("themes/astra/theme.css");
    write(tmp.path(), "outside.css", &format!(".{SECRET} {{ color: #000; }}\n"));
    fs::remove_file(&css).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("outside.css"), &css).unwrap();
    assert_refused(&root, &build(&root), "points outside");
}

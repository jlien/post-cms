//! `--require-css-compiler` and `POSTCMS_REQUIRE_TAILWIND=1` make a missing,
//! failing or empty `tailwindcss` fail the build; without them the build
//! warns and inlines the raw `theme.css` (the default).
//!
//! A fake `tailwindcss` on PATH stands in for the CLI.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const FAKE_MARK: &str = "/* fake-tailwind */";
const RAW_MARK: &str = "raw-theme-marker";

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

/// A one-page site whose theme.css carries [`RAW_MARK`].
fn site(root: &Path) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    let css = root.join("themes/astra/theme.css");
    let mut body = fs::read_to_string(&css).unwrap();
    body.push_str(&format!("\n.{RAW_MARK} {{ color: #000; }}\n"));
    fs::write(&css, body).unwrap();
    fs::create_dir_all(root.join("content/pages/about")).unwrap();
    fs::write(root.join("content/pages/about/meta.yaml"), "title: About\nslug: about\npublished: true\n").unwrap();
    fs::write(root.join("content/pages/about/index.mdx"), "Hello.\n").unwrap();
    fs::write(root.join("site.yaml"), "theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\n").unwrap();
}

#[derive(Clone, Copy)]
enum Fake {
    /// No `tailwindcss` anywhere on PATH.
    Missing,
    /// Writes compiled CSS (the input plus [`FAKE_MARK`]) and exits 0.
    Works,
    /// Writes nothing and exits 1.
    Fails,
    /// Writes an empty file and exits 0.
    Empty,
}

/// A PATH directory holding the fake CLI (or nothing, for [`Fake::Missing`]).
fn bin(dir: &Path, fake: Fake) -> std::path::PathBuf {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let body = match fake {
        Fake::Missing => return bin,
        Fake::Works => format!("cat \"$in\" > \"$out\"\nprintf '%s\\n' '{FAKE_MARK}' >> \"$out\"\nexit 0"),
        Fake::Fails => "echo boom >&2\nexit 1".to_string(),
        Fake::Empty => ": > \"$out\"\nexit 0".to_string(),
    };
    let script = format!(
        r#"#!/bin/sh
in=""; out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -i) in="$2"; shift 2 ;;
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
{body}
"#
    );
    let path = bin.join("tailwindcss");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Build the site at `root` with only `bin` and the system dirs on PATH
/// (so a `tailwindcss` installed on this machine never counts).
fn build(root: &Path, bin: &Path, args: &[&str], require_env: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_build-site"));
    cmd.args(args)
        .current_dir(root)
        .env("SITE_ROOT", root)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("SITE_PREVIEW_THEMES")
        .env_remove("DRAFTS")
        .env_remove("SITE_THEME")
        .env_remove("POSTCMS_TAILWIND_TIMEOUT_SECS");
    match require_env {
        Some(v) => cmd.env("POSTCMS_REQUIRE_TAILWIND", v),
        None => cmd.env_remove("POSTCMS_REQUIRE_TAILWIND"),
    };
    cmd.output().unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn page(root: &Path) -> String {
    fs::read_to_string(root.join("dist/about/index.html")).unwrap()
}

fn setup(fake: Fake) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root);
    let bin = bin(tmp.path(), fake);
    (tmp, root, bin)
}

fn assert_raw_fallback(o: &Output, root: &Path, why: &str) {
    assert!(o.status.success(), "default build failed: {}", stderr(o));
    let err = stderr(o);
    assert!(err.contains(why) && err.contains("inlining raw theme.css"), "{err}");
    let html = page(root);
    assert!(html.contains(RAW_MARK) && !html.contains(FAKE_MARK), "raw theme.css not inlined");
}

fn assert_required_failure(o: &Output, why: &str) {
    assert!(!o.status.success(), "strict build passed: {}", stderr(o));
    let err = stderr(o);
    assert!(err.contains(why), "{err}");
    assert!(err.contains("compiled theme CSS is required"), "{err}");
    assert!(!err.contains("inlining raw theme.css"), "{err}");
}

#[test]
fn default_missing_cli_warns_and_inlines_raw_css() {
    let (_t, root, bin) = setup(Fake::Missing);
    let o = build(&root, &bin, &[], None);
    assert_raw_fallback(&o, &root, "tailwindcss CLI unavailable");
}

#[test]
fn default_failing_cli_warns_and_inlines_raw_css() {
    let (_t, root, bin) = setup(Fake::Fails);
    let o = build(&root, &bin, &[], Some("0"));
    assert_raw_fallback(&o, &root, "exit");
}

#[test]
fn default_empty_output_warns_and_inlines_raw_css() {
    let (_t, root, bin) = setup(Fake::Empty);
    let o = build(&root, &bin, &[], Some(""));
    assert_raw_fallback(&o, &root, "tailwindcss produced empty CSS");
}

#[test]
fn missing_cli_fails_with_the_flag_and_with_the_env_var() {
    let (_t, root, bin) = setup(Fake::Missing);
    let o = build(&root, &bin, &["--require-css-compiler"], None);
    assert_required_failure(&o, "tailwindcss CLI unavailable");
    let o = build(&root, &bin, &[], Some("1"));
    assert_required_failure(&o, "tailwindcss CLI unavailable");
}

#[test]
fn failing_cli_fails_with_the_flag_and_with_the_env_var() {
    let (_t, root, bin) = setup(Fake::Fails);
    let o = build(&root, &bin, &["--require-css-compiler"], None);
    assert_required_failure(&o, "exit");
    let o = build(&root, &bin, &[], Some("1"));
    assert_required_failure(&o, "exit");
}

#[test]
fn empty_output_fails_when_required() {
    let (_t, root, bin) = setup(Fake::Empty);
    let o = build(&root, &bin, &[], Some("1"));
    assert_required_failure(&o, "tailwindcss produced empty CSS");
}

#[test]
fn present_cli_builds_with_compiled_css_when_required() {
    let (_t, root, bin) = setup(Fake::Works);
    for (args, env) in [(&["--require-css-compiler"][..], None), (&[][..], Some("1")), (&[][..], None)] {
        let o = build(&root, &bin, args, env);
        assert!(o.status.success(), "{args:?} {env:?}: {}", stderr(&o));
        assert!(!stderr(&o).contains("inlining raw theme.css"), "{}", stderr(&o));
        let html = page(&root);
        assert!(html.contains(FAKE_MARK) && html.contains(RAW_MARK), "compiled CSS not used");
    }
}

#[test]
fn invalid_env_value_fails_before_dist_is_touched() {
    let (_t, root, bin) = setup(Fake::Works);
    for bad in ["true", "yes", "2"] {
        let o = build(&root, &bin, &[], Some(bad));
        assert!(!o.status.success(), "{bad}: {}", stderr(&o));
        assert!(stderr(&o).contains("POSTCMS_REQUIRE_TAILWIND"), "{}", stderr(&o));
        assert!(!root.join("dist").exists(), "{bad}: dist/ created");
    }
}

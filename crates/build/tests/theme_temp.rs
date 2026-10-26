//! The compiled theme CSS goes through a private per-run temp directory:
//! concurrent builds of different site trees never read each other's CSS, a
//! file or symlink planted at the old fixed path (`$TMPDIR/postcms-theme-
//! <theme>.css`) is ignored, and the directory is removed afterwards.
//!
//! A fake `tailwindcss` on PATH copies the input to the output, appends a
//! marker, and sleeps, so two builds sharing one output path would collide.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_MARK: &str = "/* fake-tailwind */";

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

/// A one-page site whose theme.css carries `marker`.
fn site(root: &Path, marker: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    let css = root.join("themes/astra/theme.css");
    let mut body = fs::read_to_string(&css).unwrap();
    body.push_str(&format!("\n.{marker} {{ color: #000; }}\n"));
    fs::write(&css, body).unwrap();
    fs::create_dir_all(root.join("content/pages/about")).unwrap();
    fs::write(root.join("content/pages/about/meta.yaml"), "title: About\nslug: about\npublished: true\n").unwrap();
    fs::write(root.join("content/pages/about/index.mdx"), "Hello.\n").unwrap();
    fs::write(root.join("site.yaml"), "theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\n").unwrap();
}

/// A fake `tailwindcss` in `bin/`. It logs `ls -ld` of the output's
/// directory to `log`, writes the output, sleeps, then exits with `code`.
fn fake_tailwind(bin: &Path, log: &Path, code: i32) {
    fs::create_dir_all(bin).unwrap();
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
ls -ld "$(dirname "$out")" >> "{log}"
cat "$in" > "$out"
printf '%s\n' '{FAKE_MARK}' >> "$out"
sleep 2
exit {code}
"#,
        log = log.display()
    );
    let path = bin.join("tailwindcss");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn command(root: &Path, bin: &Path, tmpdir: &Path) -> Command {
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_build-site"));
    cmd.current_dir(root)
        .env("SITE_ROOT", root)
        .env("PATH", path)
        .env("TMPDIR", tmpdir)
        .env_remove("SITE_PREVIEW_THEMES")
        .env_remove("DRAFTS")
        .env_remove("SITE_THEME");
    cmd
}

fn check(o: &Output) {
    assert!(
        o.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn page(root: &Path) -> String {
    fs::read_to_string(root.join("dist/about/index.html")).unwrap()
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    v.sort();
    v
}

#[test]
fn concurrent_builds_keep_their_own_theme_css() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("site-a"), tmp.path().join("site-b"));
    site(&a, "only-in-site-a");
    site(&b, "only-in-site-b");
    let bin = tmp.path().join("bin");
    let log = tmp.path().join("tailwind.log");
    fake_tailwind(&bin, &log, 0);
    // One shared TMPDIR: with a fixed output path both builds would write
    // $TMPDIR/postcms-theme-astra.css and the slower read wins.
    let shared = tmp.path().join("tmp");
    fs::create_dir_all(&shared).unwrap();

    let ca = command(&a, &bin, &shared).spawn().unwrap();
    let cb = command(&b, &bin, &shared).spawn().unwrap();
    check(&ca.wait_with_output().unwrap());
    check(&cb.wait_with_output().unwrap());

    let (pa, pb) = (page(&a), page(&b));
    assert!(pa.contains(FAKE_MARK) && pb.contains(FAKE_MARK), "compiled CSS not used");
    assert!(pa.contains("only-in-site-a") && !pa.contains("only-in-site-b"), "site-a got site-b's CSS");
    assert!(pb.contains("only-in-site-b") && !pb.contains("only-in-site-a"), "site-b got site-a's CSS");

    // Each build used its own private (0700) directory, now removed.
    let lines: Vec<String> = fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    for l in &lines {
        assert!(l.starts_with("drwx------"), "temp dir not private: {l}");
    }
    assert_ne!(lines[0].split_whitespace().last(), lines[1].split_whitespace().last());
    assert!(entries(&shared).is_empty(), "temp files left behind: {:?}", entries(&shared));
}

#[test]
fn symlink_planted_at_the_old_fixed_path_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root, "own-theme-marker");
    let bin = tmp.path().join("bin");
    fake_tailwind(&bin, &tmp.path().join("tailwind.log"), 0);
    let tmpdir = tmp.path().join("tmp");
    fs::create_dir_all(&tmpdir).unwrap();
    let victim = tmp.path().join("victim.css");
    fs::write(&victim, ".planted { color: red; }\n").unwrap();
    let planted = tmpdir.join("postcms-theme-astra.css");
    std::os::unix::fs::symlink(&victim, &planted).unwrap();

    check(&command(&root, &bin, &tmpdir).output().unwrap());

    assert_eq!(fs::read_to_string(&victim).unwrap(), ".planted { color: red; }\n", "build wrote through the symlink");
    let html = page(&root);
    assert!(html.contains("own-theme-marker") && html.contains(FAKE_MARK));
    assert!(!html.contains(".planted"), "build read the planted file");
    assert_eq!(entries(&tmpdir), vec![planted.clone()], "temp files left behind");
    assert!(fs::symlink_metadata(&planted).unwrap().file_type().is_symlink());
}

#[test]
fn failed_tailwind_falls_back_to_raw_css_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    site(&root, "raw-theme-marker");
    let bin = tmp.path().join("bin");
    fake_tailwind(&bin, &tmp.path().join("tailwind.log"), 1);
    let tmpdir = tmp.path().join("tmp");
    fs::create_dir_all(&tmpdir).unwrap();

    let o = command(&root, &bin, &tmpdir).output().unwrap();
    check(&o);
    assert!(String::from_utf8_lossy(&o.stderr).contains("inlining raw theme.css"));
    let html = page(&root);
    assert!(html.contains("raw-theme-marker") && !html.contains(FAKE_MARK), "fallback changed");
    assert!(entries(&tmpdir).is_empty(), "temp files left behind: {:?}", entries(&tmpdir));
}

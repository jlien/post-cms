//! build-site only builds a directory that is a site (has site.yaml), only
//! deletes a dist/ it created (dist/.post-cms-build), and refuses unknown
//! arguments before touching anything.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MARKER: &str = ".post-cms-build";

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

/// A one-page site (no posts, so no embeddings and no network).
fn site(root: &Path) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_dir(&repo.join("templates"), &root.join("templates"));
    copy_dir(&repo.join("themes/astra"), &root.join("themes/astra"));
    fs::create_dir_all(root.join("content/pages/about")).unwrap();
    fs::write(root.join("content/pages/about/meta.yaml"), "title: About\nslug: about\npublished: true\n").unwrap();
    fs::write(root.join("content/pages/about/index.mdx"), "Hello.\n").unwrap();
    fs::write(root.join("site.yaml"), "theme: astra\nbuiltin_pages: false\nbase_url: https://www.example.com\n").unwrap();
}

/// Run build-site in `cwd`, with SITE_ROOT set to `site_root` or removed.
fn run(cwd: &Path, site_root: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_build-site"));
    cmd.current_dir(cwd)
        .args(args)
        .env_remove("SITE_PREVIEW_THEMES")
        .env_remove("DRAFTS")
        .env_remove("SITE_THEME");
    match site_root {
        Some(r) => cmd.env("SITE_ROOT", r),
        None => cmd.env_remove("SITE_ROOT"),
    };
    cmd.output().unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// dist/ with a sentinel file the build must never delete.
fn sentinel_dist(root: &Path) -> std::path::PathBuf {
    let dist = root.join("dist");
    fs::create_dir_all(dist.join("sub")).unwrap();
    fs::write(dist.join("keep.txt"), "not build output").unwrap();
    fs::write(dist.join("sub/keep.txt"), "nested").unwrap();
    dist
}

fn assert_sentinel(dist: &Path) {
    assert_eq!(fs::read_to_string(dist.join("keep.txt")).unwrap(), "not build output");
    assert_eq!(fs::read_to_string(dist.join("sub/keep.txt")).unwrap(), "nested");
    assert!(!dist.join(MARKER).exists(), "marker written into a refused dist");
    assert!(!dist.join("about").exists(), "pages written into a refused dist");
}

#[test]
fn directory_without_site_yaml_is_refused_from_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = sentinel_dist(tmp.path());
    let out = run(tmp.path(), None, &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("not a post-cms site (no site.yaml)"), "{err}");
    assert!(err.contains("Nothing was built or deleted"), "{err}");
    assert_sentinel(&dist);
}

#[test]
fn site_root_without_site_yaml_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let unrelated = tmp.path().join("other-repo");
    fs::create_dir_all(unrelated.join("src")).unwrap();
    fs::write(unrelated.join("README.md"), "x").unwrap();
    let dist = sentinel_dist(&unrelated);
    let cwd = tmp.path().join("cwd");
    site(&cwd); // a valid site in cwd must not matter when SITE_ROOT is set
    let out = run(&cwd, Some(&unrelated), &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains(&unrelated.display().to_string()), "{}", stderr(&out));
    assert_sentinel(&dist);
    assert!(!cwd.join("dist").exists());
}

#[test]
fn unknown_flags_and_arguments_are_usage_errors() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    // Even a dist the engine made is left alone.
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(dist.join(MARKER), "x").unwrap();
    fs::write(dist.join("keep.txt"), "still here").unwrap();
    for args in [&["--version"][..], &["-v"], &["--force-clean", "--verbose"], &["build"], &["--help-me"]] {
        let out = run(tmp.path(), Some(tmp.path()), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = stderr(&out);
        assert!(err.contains("Usage: build-site"), "{args:?}: {err}");
        assert_eq!(fs::read_to_string(dist.join("keep.txt")).unwrap(), "still here", "{args:?}");
    }
    let err = stderr(&run(tmp.path(), Some(tmp.path()), &["--version"]));
    assert!(err.contains("unknown option \"--version\""), "{err}");
}

#[test]
fn help_prints_usage_and_builds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    let out = run(tmp.path(), Some(tmp.path()), &["--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--force-clean"));
    assert!(!tmp.path().join("dist").exists());
}

#[test]
fn existing_dist_without_marker_is_refused_until_force_clean() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    let dist = sentinel_dist(tmp.path());
    let out = run(tmp.path(), Some(tmp.path()), &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains(&dist.display().to_string()) && err.contains("--force-clean"), "{err}");
    assert_sentinel(&dist);

    let out = run(tmp.path(), Some(tmp.path()), &["--force-clean"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(dist.join(MARKER).is_file());
    assert!(dist.join("about/index.html").is_file());
    assert!(!dist.join("keep.txt").exists());
}

#[test]
fn dist_symlink_or_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    let elsewhere = sentinel_dist(&tmp.path().join("elsewhere"));
    fs::write(elsewhere.join(MARKER), "x").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&elsewhere, tmp.path().join("dist")).unwrap();
        let out = run(tmp.path(), Some(tmp.path()), &["--force-clean"]);
        assert!(!out.status.success());
        assert!(stderr(&out).contains("not a directory"), "{}", stderr(&out));
        assert_eq!(fs::read_to_string(elsewhere.join("keep.txt")).unwrap(), "not build output");
        fs::remove_file(tmp.path().join("dist")).unwrap();
    }
    fs::write(tmp.path().join("dist"), "a file").unwrap();
    let out = run(tmp.path(), Some(tmp.path()), &["--force-clean"]);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(tmp.path().join("dist")).unwrap(), "a file");
}

#[test]
fn build_writes_marker_and_rebuild_replaces_its_own_dist() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    let dist = tmp.path().join("dist");
    let out = run(tmp.path(), Some(tmp.path()), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(dist.join(MARKER).is_file());
    let first = fs::read(dist.join("about/index.html")).unwrap();

    fs::write(dist.join("stale.html"), "old").unwrap();
    let out = run(tmp.path(), Some(tmp.path()), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(dist.join(MARKER).is_file());
    assert!(!dist.join("stale.html").exists(), "rebuild keeps stale output");
    assert_eq!(fs::read(dist.join("about/index.html")).unwrap(), first);
}

#[test]
fn site_in_cwd_builds_without_site_root() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    let out = run(tmp.path(), None, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(tmp.path().join("dist").join(MARKER).is_file());
}

#[test]
fn empty_existing_dist_is_used() {
    let tmp = tempfile::tempdir().unwrap();
    site(tmp.path());
    fs::create_dir_all(tmp.path().join("dist")).unwrap();
    let out = run(tmp.path(), Some(tmp.path()), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(tmp.path().join("dist").join(MARKER).is_file());
}

#[test]
fn decode_only_mode_rejects_extra_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = sentinel_dist(tmp.path());
    for args in [&["--postcms-decode-avif", "100", "100", "1", "extra"][..], &["--postcms-decode-avif", "100"]] {
        let out = run(tmp.path(), None, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(stderr(&out).contains("avif: usage"), "{}", stderr(&out));
    }
    assert_sentinel(&dist);
}

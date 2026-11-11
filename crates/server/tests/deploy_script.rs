//! `ci/deploy.sh` never ships a drafts or preview build into a public
//! origin's directory. The origin server refuses such a dist only at
//! startup; a running origin serves whatever the deploy copies in, so the
//! deploy refuses first.
//!
//! Each test copies `ci/deploy.sh` and `ci/build.sh` into a temp repo and puts
//! a stub `cargo` first on PATH, so "the build" writes a chosen dist/ without
//! compiling anything and the real repo's dist/ is never touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STUB_CARGO: &str = r#"#!/bin/sh
# Stub cargo: logs each call; `cargo run ...` writes dist/ from $STUB_DIST,
# a space-separated list of relative paths (a trailing / makes a directory).
echo "$*" >> "$STUB_LOG"
case "$1" in
  run)
    mkdir -p dist
    for p in $STUB_DIST; do
      case "$p" in
        */) mkdir -p "dist/$p" ;;
        *) mkdir -p "dist/$(dirname "$p")"; echo stub > "dist/$p" ;;
      esac
    done
    ;;
esac
"#;

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("post-cms-deploy-").tempdir().unwrap();
        let root = dir.path();
        let ci = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ci");
        std::fs::create_dir_all(root.join("ci")).unwrap();
        for f in ["deploy.sh", "build.sh"] {
            std::fs::copy(ci.join(f), root.join("ci").join(f)).unwrap();
        }
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let cargo = root.join("bin/cargo");
        std::fs::write(&cargo, STUB_CARGO).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A target that already holds a live site, as a running origin's would.
    fn target(&self) -> PathBuf {
        let t = self.root().join("target-origin");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("index.html"), "live").unwrap();
        t
    }

    fn deploy(&self, dist: &str, env: &[(&str, &str)]) -> Output {
        let path = format!("{}:{}", self.root().join("bin").display(), std::env::var("PATH").unwrap_or_default());
        let mut cmd = Command::new("bash");
        cmd.arg(self.root().join("ci/deploy.sh"))
            .env("PATH", path)
            .env("STUB_DIST", dist)
            .env("STUB_LOG", self.root().join("cargo.log"))
            .env("ORIGIN_TARGET", self.root().join("target-origin"))
            .env_remove("DRAFTS")
            .env_remove("SITE_PREVIEW_THEMES");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn cargo_calls(&self) -> String {
        std::fs::read_to_string(self.root().join("cargo.log")).unwrap_or_default()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The deploy refused, said why, and left the origin's files as they were.
fn assert_refused(repo: &Repo, out: &Output, names: &str) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(1), "deploy did not refuse; stderr:\n{err}");
    assert!(err.contains("refusing to deploy"), "no refusal message:\n{err}");
    assert!(err.contains(names), "message does not name {names:?}:\n{err}");
    let t = repo.root().join("target-origin");
    let mut entries: Vec<String> = std::fs::read_dir(&t)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["index.html"], "the deploy copied files into the origin");
    assert_eq!(std::fs::read_to_string(t.join("index.html")).unwrap(), "live");
}

#[test]
fn refuses_a_dist_with_the_drafts_marker() {
    // A DRAFTS=1 build with preview/ already stripped still carries the marker.
    let repo = Repo::new();
    repo.target();
    let out = repo.deploy("index.html .post-cms-drafts", &[]);
    assert_refused(&repo, &out, "dist/.post-cms-drafts");
}

#[test]
fn refuses_a_dist_with_preview() {
    let repo = Repo::new();
    repo.target();
    let out = repo.deploy("index.html preview/drafts/posts/x/index.html", &[]);
    assert_refused(&repo, &out, "dist/preview");
}

#[test]
fn refuses_preview_in_any_letter_case() {
    let repo = Repo::new();
    repo.target();
    let out = repo.deploy("index.html Preview/ .POST-CMS-DRAFTS", &[]);
    assert_refused(&repo, &out, "dist/Preview dist/.POST-CMS-DRAFTS");
}

#[test]
fn refuses_drafts_and_preview_themes_before_building() {
    for (var, value) in [("DRAFTS", "1"), ("SITE_PREVIEW_THEMES", "all")] {
        let repo = Repo::new();
        repo.target();
        let out = repo.deploy("index.html", &[(var, value)]);
        assert_refused(&repo, &out, &format!("{var} is set"));
        assert_eq!(repo.cargo_calls(), "", "{var}: the build ran before the refusal");
    }
}

#[test]
fn deploys_a_plain_build() {
    let repo = Repo::new();
    let target = repo.target();
    let out = repo.deploy("index.html blog/post/index.html .post-cms-build", &[]);
    assert!(out.status.success(), "plain deploy failed:\n{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(target.join("index.html")).unwrap(), "stub\n");
    assert!(target.join("blog/post/index.html").is_file());
    assert!(repo.cargo_calls().contains("run -p build"), "the build did not run");
}

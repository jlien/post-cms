//! `tailwindcss` runs with a cleared environment (only the allowlist in
//! src/tailwind.rs), is killed with its process group after
//! TAILWIND_TIMEOUT_SECS, and dies with the build on SIGTERM.
//!
//! Each test puts a fake `tailwindcss` (a shell script) first on PATH.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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

/// A fake `tailwindcss` in `bin/` running `body` after parsing -i and -o.
fn fake_tailwind(bin: &Path, body: &str) {
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
{body}
"#
    );
    let path = bin.join("tailwindcss");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    tmpdir: PathBuf,
    dir: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let root = dir.join("site");
    site(&root);
    let tmpdir = dir.join("tmp");
    fs::create_dir_all(&tmpdir).unwrap();
    Fixture { bin: dir.join("bin"), root, tmpdir, dir, _tmp: tmp }
}

/// build-site for the fixture, with the fake first on PATH and nothing
/// else inherited but PATH (so the test's own environment cannot leak in).
fn command(f: &Fixture) -> Command {
    let path = format!("{}:/usr/bin:/bin", f.bin.display());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_build-site"));
    cmd.current_dir(&f.root)
        .env_clear()
        .env("SITE_ROOT", &f.root)
        .env("PATH", path)
        .env("TMPDIR", &f.tmpdir);
    cmd
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    v.sort();
    v
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait up to `limit` for `pid` to be gone (an orphan is reaped by init).
fn gone_within(pid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

fn read_pids(path: &Path) -> Vec<i32> {
    fs::read_to_string(path).unwrap().split_whitespace().map(|p| p.parse().unwrap()).collect()
}

#[test]
fn tailwind_gets_only_allowlisted_env() {
    let f = fixture();
    let dump = f.dir.join("env.txt");
    fake_tailwind(
        &f.bin,
        &format!("env > \"{}\"\ncat \"$in\" > \"$out\"\nprintf '%s\\n' '{FAKE_MARK}' >> \"$out\"", dump.display()),
    );
    let home = f.dir.join("home");
    let o = command(&f)
        .env("HOME", &home)
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .env("NODE_PATH", "/opt/node-modules-example")
        .env("SANITY_READ_TOKEN", "sk-secret-read-token")
        .env("SANITY_PROJECT_ID", "proj-example")
        .env("AWS_SECRET_ACCESS_KEY", "aws-secret")
        .env("NODE_OPTIONS", "--require /tmp/evil.js")
        .env("GITHUB_TOKEN", "gh-secret")
        .env("SITE_BASE_URL", "https://www.example.com")
        .output()
        .unwrap();
    assert!(o.status.success(), "build failed: {}", stderr(&o));
    assert!(stderr(&o).contains("tailwindcss minified"), "{}", stderr(&o));
    let html = fs::read_to_string(f.root.join("dist/about/index.html")).unwrap();
    assert!(html.contains(FAKE_MARK), "compiled CSS not used");

    let env = fs::read_to_string(&dump).unwrap();
    for secret in ["sk-secret-read-token", "aws-secret", "gh-secret", "evil.js", "proj-example"] {
        assert!(!env.contains(secret), "{secret} leaked to tailwindcss:\n{env}");
    }
    // Every variable is on the allowlist, or one /bin/sh sets itself.
    let allowed = ["PATH", "HOME", "TMPDIR", "NODE_PATH", "LANG", "LC_ALL", "LC_CTYPE", "PWD", "OLDPWD", "SHLVL", "_"];
    for line in env.lines() {
        let name = line.split('=').next().unwrap();
        assert!(allowed.contains(&name), "{name} is not on the allowlist:\n{env}");
    }
    for (k, v) in [
        ("HOME", home.display().to_string()),
        ("TMPDIR", f.tmpdir.display().to_string()),
        ("NODE_PATH", "/opt/node-modules-example".into()),
        ("LANG", "en_US.UTF-8".into()),
        ("PATH", format!("{}:/usr/bin:/bin", f.bin.display())),
    ] {
        assert!(env.lines().any(|l| l == format!("{k}={v}")), "{k} not passed through:\n{env}");
    }
    assert!(entries(&f.tmpdir).is_empty(), "temp files left behind: {:?}", entries(&f.tmpdir));
}

/// The fake records its own pid and a background `sleep`'s, then waits.
fn hanging_tailwind(f: &Fixture, pids: &Path) {
    fake_tailwind(
        &f.bin,
        &format!("sleep 300 &\necho $$ $! > \"{p}.part\"\nmv \"{p}.part\" \"{p}\"\nwait", p = pids.display()),
    );
}

#[test]
fn tailwind_past_the_timeout_is_killed_and_fails_the_build() {
    let f = fixture();
    let pids = f.dir.join("pids");
    hanging_tailwind(&f, &pids);
    let start = Instant::now();
    let o = command(&f).env("TAILWIND_TIMEOUT_SECS", "1").output().unwrap();
    let took = start.elapsed();

    assert!(!o.status.success(), "build should fail on the timeout");
    let err = stderr(&o);
    assert!(err.contains("tailwindcss did not finish within 1s"), "{err}");
    assert!(err.contains("TAILWIND_TIMEOUT_SECS"), "{err}");
    assert!(!err.contains("inlining raw theme.css"), "a timeout must not fall back: {err}");
    assert!(took < Duration::from_secs(30), "took {took:?}");
    assert!(entries(&f.tmpdir).is_empty(), "temp dir left behind: {:?}", entries(&f.tmpdir));
    for pid in read_pids(&pids) {
        assert!(gone_within(pid, Duration::from_secs(5)), "process {pid} outlived the timeout");
    }
}

#[test]
fn bad_timeout_value_fails_before_dist_is_touched() {
    let f = fixture();
    fake_tailwind(&f.bin, "cat \"$in\" > \"$out\"");
    for bad in ["0", "abc", "9999"] {
        let o = command(&f).env("TAILWIND_TIMEOUT_SECS", bad).output().unwrap();
        assert!(!o.status.success(), "{bad} accepted");
        let err = stderr(&o);
        assert!(err.contains("TAILWIND_TIMEOUT_SECS") && err.contains("1 to 3600"), "{err}");
        assert!(!f.root.join("dist").exists(), "dist created for {bad}");
    }
}

#[test]
fn missing_tailwind_still_falls_back_to_raw_css() {
    let f = fixture();
    fs::create_dir_all(&f.bin).unwrap(); // empty: no tailwindcss on PATH
    let o = command(&f).output().unwrap();
    assert!(o.status.success(), "build failed: {}", stderr(&o));
    assert!(stderr(&o).contains("tailwindcss CLI unavailable"), "{}", stderr(&o));
    assert!(entries(&f.tmpdir).is_empty(), "temp files left behind: {:?}", entries(&f.tmpdir));
}

#[test]
fn sigterm_to_the_build_kills_tailwind_too() {
    let f = fixture();
    let pids = f.dir.join("pids");
    hanging_tailwind(&f, &pids);
    let mut build = command(&f).env("TAILWIND_TIMEOUT_SECS", "600").spawn().unwrap();
    let start = Instant::now();
    while !pids.exists() {
        assert!(start.elapsed() < Duration::from_secs(30), "fake tailwindcss never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: signals the build-site process this test started.
    unsafe { libc::kill(build.id() as i32, libc::SIGTERM) };
    let st = build.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(st.signal(), Some(libc::SIGTERM), "{st:?}");
    for pid in read_pids(&pids) {
        assert!(gone_within(pid, Duration::from_secs(5)), "process {pid} outlived the build");
    }
}

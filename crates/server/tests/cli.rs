//! Command-line and listen-address behavior of the `server` binary.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn server() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_server"));
    c.env_remove("POSTCMS_HOST").env_remove("SITE_ROOT").env_remove("PORT");
    c
}

/// A site with a one-page dist/, in a private temp directory (random name)
/// that is removed when the returned guard drops.
fn temp_site(name: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("post-cms-server-cli-{name}-"))
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("site.yaml"), "title: x\n").unwrap();
    std::fs::write(root.join("dist/index.html"), "<p>hello</p>").unwrap();
    dir
}

/// Waits up to 10 s for the process to exit and returns its exit code.
fn wait_exit(child: &mut Child) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    None
}

/// Kills the child on drop so a failing assertion never leaks a server.
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the server on an OS-picked port and returns it with the bound
/// address and every stderr line printed before the "serving" line.
fn start(site: &Path, args: &[&str], env_host: Option<&str>) -> (Running, SocketAddr, Vec<String>) {
    let mut cmd = server();
    cmd.args(args);
    if let Some(h) = env_host {
        cmd.env("POSTCMS_HOST", h);
    }
    let (child, addr, lines, _rest) = start_cmd(cmd, site);
    (child, addr, lines)
}

/// Like [`start`] for a prepared command; also returns the rest of stderr.
fn start_cmd(
    mut cmd: Command,
    site: &Path,
) -> (Running, SocketAddr, Vec<String>, BufReader<std::process::ChildStderr>) {
    cmd.env("SITE_ROOT", site)
        .env("PORT", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = Running(cmd.spawn().unwrap());
    let mut stderr = BufReader::new(child.0.stderr.take().unwrap());
    let mut lines = Vec::new();
    let mut line = String::new();
    while stderr.read_line(&mut line).unwrap() > 0 {
        let l = line.trim_end().to_string();
        line.clear();
        if let Some(addr) = l.strip_prefix("origin serving dist/ on http://") {
            let addr: SocketAddr = addr.parse().unwrap();
            return (child, addr, lines, stderr);
        }
        lines.push(l);
    }
    panic!("server exited before serving: {lines:?}");
}

fn get(addr: SocketAddr) -> String {
    use std::io::Write;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn help_prints_usage_and_exits_without_listening() {
    let site = temp_site("help");
    for flag in ["--help", "-h"] {
        // A real site and port, so only the flag keeps it from serving.
        let mut child = server()
            .arg(flag)
            .env("SITE_ROOT", site.path())
            .env("PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(wait_exit(&mut child), Some(0), "{flag}");
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.starts_with("Usage: server"), "{flag}: {stdout}");
        assert!(stdout.contains("--host") && stdout.contains("POSTCMS_HOST"), "{stdout}");
        assert!(!stderr.contains("origin serving"), "{flag} started serving: {stderr}");
    }
}

#[test]
fn unknown_flag_is_a_usage_error() {
    let site = temp_site("unknown");
    for args in [&["--version"][..], &["--bind", "0.0.0.0"], &["--host"], &["--host", "example.com"], &["."]] {
        let mut child = server()
            .args(args)
            .env("SITE_ROOT", site.path())
            .env("PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(wait_exit(&mut child), Some(2), "{args:?}");
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.starts_with("server: "), "{args:?}: {stderr}");
        assert!(stderr.contains("Usage: server"), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn bad_env_host_is_a_usage_error() {
    let site = temp_site("badenv");
    let mut child = server()
        .env("SITE_ROOT", site.path())
        .env("PORT", "0")
        .env("POSTCMS_HOST", "not-an-ip")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(wait_exit(&mut child), Some(2));
    let out = child.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("POSTCMS_HOST \"not-an-ip\""));
}

#[test]
fn default_bind_is_loopback_only() {
    let site = temp_site("default");
    let (_server, addr, warnings) = start(site.path(), &[], None);
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert!(warnings.iter().all(|l| !l.contains("warning")), "{warnings:?}");
    assert!(get(addr).contains("hello"));
}

#[test]
fn explicit_loopback_host_does_not_warn() {
    let site = temp_site("explicit");
    // ::1 from the flag; the flag wins over POSTCMS_HOST.
    let (_server, addr, warnings) = start(site.path(), &["--host", "::1"], Some("127.0.0.1"));
    assert_eq!(addr.ip().to_string(), "::1");
    assert!(warnings.iter().all(|l| !l.contains("warning")), "{warnings:?}");
    assert!(get(addr).contains("hello"));
}

#[test]
fn env_host_is_used_when_no_flag() {
    let site = temp_site("envhost");
    let (_server, addr, _) = start(site.path(), &[], Some("::1"));
    assert_eq!(addr.ip().to_string(), "::1");
    assert!(get(addr).contains("hello"));
}

#[test]
fn all_interfaces_bind_warns() {
    let site = temp_site("all");
    let (_server, addr, warnings) = start(site.path(), &["--host=0.0.0.0"], None);
    assert_eq!(addr.ip().to_string(), "0.0.0.0");
    assert!(
        warnings.iter().any(|l| l.starts_with("warning: listening on 0.0.0.0:") && l.contains("every network interface")),
        "{warnings:?}"
    );
    // Reachable over loopback too.
    let local = SocketAddr::from(([127, 0, 0, 1], addr.port()));
    assert!(get(local).contains("hello"));
}

#[test]
fn mapped_loopback_host_does_not_warn() {
    let site = temp_site("mapped");
    for host in ["::ffff:127.0.0.1", "[::ffff:127.0.0.1]"] {
        let (_server, addr, warnings) = start(site.path(), &["--host", host], None);
        assert_eq!(addr.ip().to_string(), "::ffff:127.0.0.1", "{host}");
        assert!(warnings.iter().all(|l| !l.contains("warning")), "{host}: {warnings:?}");
        assert!(get(addr).contains("hello"), "{host}");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_env_host_is_a_usage_error_before_listening() {
    use std::os::unix::ffi::OsStrExt;
    let site = temp_site("nonutf8");
    let bad = std::ffi::OsStr::from_bytes(b"127.0.0.\xff");
    // A bad flag value and a bad env value both stop before anything listens.
    for (args, env) in [(&[][..], Some(bad)), (&[bad][..], None)] {
        let mut cmd = server();
        if args.is_empty() {
            cmd.env("POSTCMS_HOST", env.unwrap());
        } else {
            cmd.arg("--host").args(args);
        }
        let mut child = cmd
            .env("SITE_ROOT", site.path())
            .env("PORT", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(wait_exit(&mut child), Some(2), "{args:?} {env:?}");
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.starts_with("server: ") && stderr.contains("not valid UTF-8"), "{stderr}");
        assert!(stderr.contains("Usage: server"), "{stderr}");
        assert!(!stderr.contains("origin serving"), "{stderr}");
    }
}

/// The path the server used for its model cache when HOME was unset.
const SHARED_CACHE: &str = "/tmp/.cache/post-cms-fastembed";

/// Plants a symlink at [`SHARED_CACHE`] when nothing is there, pointing at a
/// private directory holding a fake model file; removes what it created on
/// drop. When the path already exists (an older server left it), it is left
/// alone and only watched for changes.
struct Planted {
    _target: tempfile::TempDir,
    created_link: bool,
    created_parent: bool,
}

impl Planted {
    fn new() -> Planted {
        let target = tempfile::Builder::new().prefix("post-cms-planted-").tempdir().unwrap();
        std::fs::write(target.path().join("model.onnx"), b"planted").unwrap();
        let link = Path::new(SHARED_CACHE);
        let parent = link.parent().unwrap();
        let created_parent = std::fs::create_dir(parent).is_ok();
        let created_link = std::os::unix::fs::symlink(target.path(), link).is_ok();
        Planted { _target: target, created_link, created_parent }
    }

    /// Every entry under the shared path (following the link) with its
    /// modification time, so a download or a write shows up as a change.
    fn listing(&self) -> Vec<(String, std::time::SystemTime)> {
        fn walk(dir: &Path, out: &mut Vec<(String, std::time::SystemTime)>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for e in entries.flatten() {
                let path = e.path();
                let modified = std::fs::symlink_metadata(&path).and_then(|m| m.modified()).unwrap();
                out.push((path.display().to_string(), modified));
                if e.file_type().map_or(false, |t| t.is_dir()) {
                    walk(&path, out);
                }
            }
        }
        let mut v = Vec::new();
        walk(Path::new(SHARED_CACHE), &mut v);
        v.sort();
        v
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        if self.created_link {
            let _ = std::fs::remove_file(SHARED_CACHE);
        }
        if self.created_parent {
            let _ = std::fs::remove_dir(Path::new(SHARED_CACHE).parent().unwrap());
        }
    }
}

#[cfg(unix)]
#[test]
fn no_home_turns_embeddings_off_and_never_reads_the_shared_cache() {
    use std::io::Write;
    let site = temp_site("nohome");
    let root = site.path();
    std::fs::create_dir_all(root.join("dist/posts/a")).unwrap();
    std::fs::write(root.join("dist/posts/a/index.md"), "# A\n\nSome text about a thing.\n").unwrap();
    std::fs::write(
        root.join("dist/embeddings.json"),
        r#"[{"text":"Some text","url":"/posts/a/","title":"A","embedding":[0.1,0.2]}]"#,
    )
    .unwrap();
    let planted = Planted::new();
    let before = planted.listing();
    // A closed port, so the upstream call fails fast after retrieval ran.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();

    for home in [None, Some("")] {
        let mut cmd = server();
        match home {
            None => cmd.env_remove("HOME"),
            Some(h) => cmd.env("HOME", h),
        };
        cmd.env("LLM_BASE_URL", format!("http://{closed}"))
            .env("LLM_MODEL", "test")
            .env_remove("LLM_API_KEY");
        let (mut server, addr, startup, rest) = start_cmd(cmd, root);
        assert!(
            startup.iter().any(|l| l.starts_with("embeddings: HOME is not set; semantic retrieval is off")),
            "{home:?}: {startup:?}"
        );

        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let body = r#"{"question":"what is a thing?"}"#;
        write!(
            s,
            "POST /api/ask HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 502"), "{home:?}: {resp}");
        assert!(resp.contains("upstream unreachable"), "{home:?}: {resp}");

        let _ = server.0.kill();
        let _ = server.0.wait();
        let mut later = String::new();
        let _ = { rest }.read_to_string(&mut later);
        assert!(later.contains("retrieval failed (no embedding model)"), "{home:?}: {later}");
        assert!(!later.contains("model unavailable"), "{home:?}: {later}");
        assert_eq!(planted.listing(), before, "{home:?}: the shared cache path was used");
    }
}

/// A site with one markdown post, so /api/ask reaches the upstream call.
fn ask_site(name: &str) -> tempfile::TempDir {
    let site = temp_site(name);
    let root = site.path();
    std::fs::create_dir_all(root.join("dist/posts/a")).unwrap();
    std::fs::write(root.join("dist/posts/a/index.md"), "# A\n\nSome text about a thing.\n").unwrap();
    site
}

/// Starts the server with `LLM_BASE_URL=base`, POSTs one question to
/// /api/ask, stops the server, and returns the full HTTP response and the
/// stderr printed after the "serving" line.
fn ask_once(site: &Path, base: &str, key: Option<&str>) -> (String, String) {
    use std::io::Write;
    let mut cmd = server();
    cmd.env("LLM_BASE_URL", base).env("LLM_MODEL", "test");
    match key {
        Some(k) => cmd.env("LLM_API_KEY", k),
        None => cmd.env_remove("LLM_API_KEY"),
    };
    let (mut server, addr, _startup, rest) = start_cmd(cmd, site);
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = r#"{"question":"what is a thing?"}"#;
    write!(
        s,
        "POST /api/ask HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let _ = server.0.kill();
    let _ = server.0.wait();
    let mut log = String::new();
    let _ = { rest }.read_to_string(&mut log);
    (resp, log)
}

/// The JSON body of an HTTP/1.1 response.
fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

#[test]
fn unreachable_upstream_keeps_the_url_out_of_the_response() {
    let site = ask_site("unreachable");
    // A closed loopback port, and a host that never resolves.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let cases = [
        (format!("http://user:pw-secret@{closed}/internal-v1"), closed.to_string()),
        ("http://llm-upstream.invalid:4321/internal-v1".to_string(), "llm-upstream.invalid".to_string()),
    ];
    for (base, host) in cases {
        let (resp, log) = ask_once(site.path(), &base, None);
        let body = body_of(&resp);
        assert!(resp.starts_with("HTTP/1.1 502"), "{base}: {resp}");
        assert_eq!(body, r#"{"error":"upstream unreachable"}"#, "{base}: {resp}");
        for leak in [base.as_str(), host.as_str(), "internal-v1", "secret", "4321"] {
            assert!(!resp.contains(leak), "{base}: response leaks {leak:?}: {resp}");
        }
        // The detail, with the URL, goes to the server log only.
        let line = log
            .lines()
            .find(|l| l.starts_with("ask: upstream unreachable: POST "))
            .unwrap_or_else(|| panic!("{base}: no log line: {log}"));
        assert!(line.contains(host.as_str()) && line.contains("/internal-v1/chat/completions"), "{line}");
        assert!(line.contains("error sending request"), "{line}");
        assert!(!line.contains("secret"), "credentials in the log: {line}");
    }
}

#[test]
fn upstream_error_status_and_body_stay_in_the_log() {
    use std::io::Write;
    let site = ask_site("upstream500");
    // A one-shot upstream that answers 500 with a body naming internal detail.
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let up = upstream.local_addr().unwrap();
    let marker = "db-7.internal.example";
    let api_key = "sk-test-KEY123";
    let handle = std::thread::spawn(move || {
        let (mut c, _) = upstream.accept().unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Read the request head and its body before answering.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut auth = String::new();
        loop {
            let n = c.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let len = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if rest.len() >= len {
                    // Echo the request's Authorization header, as some
                    // upstream error pages do.
                    auth = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .unwrap_or("")
                        .to_string();
                    break;
                }
            }
        }
        let body = format!("upstream failure at {marker}; request {auth}");
        write!(
            c,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let base = format!("http://{up}/internal-v1");
    let (resp, log) = ask_once(site.path(), &base, Some(api_key));
    handle.join().unwrap();
    assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
    assert_eq!(body_of(&resp), r#"{"error":"upstream error"}"#, "{resp}");
    for leak in [base.as_str(), up.to_string().as_str(), "internal", api_key, "500"] {
        assert!(!body_of(&resp).contains(leak), "response leaks {leak:?}: {resp}");
    }
    let line = log
        .lines()
        .find(|l| l.starts_with("ask: upstream error: POST "))
        .unwrap_or_else(|| panic!("no log line: {log}"));
    assert!(line.contains(&format!("http://{up}/internal-v1/chat/completions returned 500")), "{line}");
    assert!(line.contains(marker), "{line}");
    // The upstream echoed the API key; the log has it redacted.
    assert!(line.contains("Bearer [redacted]"), "{line}");
    assert!(!log.contains(api_key), "API key in the log: {log}");
}

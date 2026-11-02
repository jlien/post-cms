//! The origin exits cleanly (status 0) on SIGTERM and SIGINT, so a container
//! runtime stopping it as PID 1 does not have to wait out its grace period
//! and SIGKILL it.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A minimal site: site.yaml plus a dist/ with one page.
fn site() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("site.yaml"), "title: Test\n").unwrap();
    std::fs::create_dir(dir.path().join("dist")).unwrap();
    std::fs::write(dir.path().join("dist/index.html"), "<h1>ok</h1>").unwrap();
    dir
}

/// Start the server on a free port; returns the child and its bound address.
fn start(site: &std::path::Path) -> (Child, String, mpsc::Receiver<String>) {
    start_with(site, &[])
}

fn start_with(site: &std::path::Path, extra: &[(&str, &str)]) -> (Child, String, mpsc::Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env_clear()
        .envs(extra.iter().copied())
        .env("SITE_ROOT", site)
        .env("HOME", site)
        .env("PORT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(l) => l,
            Err(_) => {
                let _ = child.kill();
                panic!("server did not print its address");
            }
        };
        if let Some(addr) = line.strip_prefix("origin serving dist/ on http://") {
            return (child, addr.trim().to_string(), rx);
        }
    }
}

fn get_health(addr: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn signal(child: &Child, sig: &str) {
    let st = Command::new("kill").args([sig, &child.id().to_string()]).status().unwrap();
    assert!(st.success(), "kill {sig} failed");
}

/// Wait for exit; panics (after killing the child) past the deadline.
fn wait_within(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server still running {limit:?} after the signal");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn exits_cleanly_on(sig: &str, name: &str) {
    let dir = site();
    let (mut child, addr, rx) = start(dir.path());
    let resp = get_health(&addr);
    assert!(resp.starts_with("HTTP/1.1 200"), "health: {resp}");
    assert!(resp.contains(r#""status":"ok""#), "health body: {resp}");
    // An idle keep-alive connection must not hold shutdown open.
    let mut idle = TcpStream::connect(&addr).unwrap();
    idle.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut buf = [0u8; 512];
    let _ = idle.read(&mut buf).unwrap();

    signal(&child, sig);
    let st = wait_within(&mut child, Duration::from_secs(10));
    assert_eq!(st.code(), Some(0), "exit status after {sig}: {st:?}");
    let lines: Vec<String> = rx.try_iter().collect();
    assert!(
        lines.iter().any(|l| l == &format!("origin: {name}, shutting down")),
        "no shutdown line in {lines:?}"
    );
    assert!(TcpStream::connect(&addr).is_err(), "still listening after exit");
}

#[test]
fn sigterm_exits_zero_within_ten_seconds() {
    exits_cleanly_on("-TERM", "SIGTERM");
}

#[test]
fn sigint_exits_zero_within_ten_seconds() {
    exits_cleanly_on("-INT", "SIGINT");
}

/// A client that sends half a request head and stalls.
fn stalled_client(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nHost: loc").unwrap();
    s
}

#[test]
fn partial_request_head_is_closed_after_the_header_timeout() {
    let dir = site();
    let (mut child, addr, _rx) = start_with(dir.path(), &[("POSTCMS_HEADER_READ_TIMEOUT_SECS", "1")]);
    let mut s = stalled_client(&addr);
    s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    let start = Instant::now();
    let mut buf = Vec::new();
    // Closed by the server: read_to_end returns (possibly after a 408) or the
    // socket errors with a reset; a timeout here means it is still open.
    let r = s.read_to_end(&mut buf);
    let waited = start.elapsed();
    if let Err(e) = &r {
        assert!(
            !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
            "connection still open after {waited:?}"
        );
    }
    assert!(waited < Duration::from_secs(5), "closed only after {waited:?}");
    // The server itself keeps serving.
    assert!(get_health(&addr).starts_with("HTTP/1.1 200"));
    signal(&child, "-TERM");
    assert_eq!(wait_within(&mut child, Duration::from_secs(10)).code(), Some(0));
}

#[test]
fn stalled_client_cannot_hold_shutdown_past_the_deadline() {
    let dir = site();
    let (mut child, addr, rx) = start_with(
        dir.path(),
        &[("POSTCMS_HEADER_READ_TIMEOUT_SECS", "600"), ("POSTCMS_SHUTDOWN_TIMEOUT_SECS", "2")],
    );
    let _stalled = stalled_client(&addr);
    // Let the server pick the connection up before the signal.
    std::thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    signal(&child, "-TERM");
    let st = wait_within(&mut child, Duration::from_secs(10));
    let took = start.elapsed();
    assert_eq!(st.code(), Some(0), "{st:?}");
    assert!(took >= Duration::from_millis(1900), "exited before the deadline: {took:?}");
    let lines: Vec<String> = rx.try_iter().collect();
    assert!(
        lines.iter().any(|l| l.contains("1 connection(s) still open after 2s")),
        "no deadline line in {lines:?}"
    );
}

#[test]
fn invalid_timeouts_refuse_to_start() {
    let dir = site();
    for (k, v) in [("POSTCMS_SHUTDOWN_TIMEOUT_SECS", "0"), ("POSTCMS_HEADER_READ_TIMEOUT_SECS", "ten")] {
        let out = Command::new(env!("CARGO_BIN_EXE_server"))
            .env_clear()
            .env("SITE_ROOT", dir.path())
            .env("HOME", dir.path())
            .env("PORT", "0")
            .env(k, v)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{k}={v}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(k));
    }
}

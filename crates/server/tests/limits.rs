//! Request-body limits (timeout, size cap, rate limit before the body) and
//! the connection cap, against the real binary.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    addr: String,
    _site: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(extra: &[(&str, &str)]) -> Server {
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("site.yaml"), "title: Test\n").unwrap();
    std::fs::create_dir(site.path().join("dist")).unwrap();
    std::fs::write(site.path().join("dist/index.html"), "<h1>ok</h1>").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env_clear()
        .env("SITE_ROOT", site.path())
        .env("HOME", site.path())
        .env("PORT", "0")
        .envs(extra.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("server did not print its address");
        if let Some(a) = line.strip_prefix("origin serving dist/ on http://") {
            return Server { child, addr: a.trim().to_string(), _site: site };
        }
    }
}

/// POST head declaring `len` body bytes, then `sent` of them.
fn post_partial(addr: &str, path: &str, len: usize, sent: &[u8]) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    s.write_all(sent).unwrap();
    s
}

/// The response on `s` within `limit` (status line plus body), or panic.
fn response_within(s: &mut TcpStream, limit: Duration) -> (String, Duration) {
    let start = Instant::now();
    s.set_read_timeout(Some(limit)).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let took = start.elapsed();
    let text = String::from_utf8_lossy(&buf).to_string();
    assert!(!text.is_empty(), "no response within {limit:?}");
    (text, took)
}

#[test]
fn stalled_body_gets_408_after_the_body_timeout() {
    let srv = start(&[("POSTCMS_BODY_READ_TIMEOUT_SECS", "1")]);
    for path in ["/api/ask", "/mcp"] {
        let mut s = post_partial(&srv.addr, path, 100, br#"{"quest"#);
        let (resp, took) = response_within(&mut s, Duration::from_secs(8));
        assert!(resp.starts_with("HTTP/1.1 408"), "{path}: {resp}");
        assert!(resp.contains("request body not received in time"), "{path}: {resp}");
        assert!(took < Duration::from_secs(5), "{path}: took {took:?}");
    }
}

#[test]
fn slow_drip_body_gets_408_at_the_total_deadline() {
    let srv = start(&[("POSTCMS_BODY_READ_TIMEOUT_SECS", "2")]);
    let mut s = post_partial(&srv.addr, "/mcp", 64, b"{");
    let start = Instant::now();
    // One byte every 300 ms: each read makes progress, the total does not.
    let mut w = s.try_clone().unwrap();
    let drip = std::thread::spawn(move || {
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(300));
            if w.write_all(b" ").is_err() {
                break;
            }
        }
    });
    let (resp, _) = response_within(&mut s, Duration::from_secs(10));
    let took = start.elapsed();
    assert!(resp.starts_with("HTTP/1.1 408"), "{resp}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    drop(s);
    let _ = drip.join();
}

#[test]
fn oversized_bodies_get_413() {
    let srv = start(&[]);
    // Declared too large: refused before any body byte is read.
    for (path, len) in [("/api/ask", 16 * 1024 + 1), ("/mcp", 256 * 1024 + 1)] {
        let mut s = post_partial(&srv.addr, path, len, b"");
        let (resp, took) = response_within(&mut s, Duration::from_secs(5));
        assert!(resp.starts_with("HTTP/1.1 413"), "{path}: {resp}");
        assert!(took < Duration::from_secs(2), "{path}: took {took:?}");
    }
    // Chunked, no length up front: cut off while reading.
    let mut s = TcpStream::connect(&srv.addr).unwrap();
    write!(
        s,
        "POST /api/ask HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let chunk = vec![b' '; 8192];
    for _ in 0..3 {
        let _ = write!(s, "{:x}\r\n", chunk.len());
        let _ = s.write_all(&chunk);
        let _ = s.write_all(b"\r\n");
    }
    let (resp, _) = response_within(&mut s, Duration::from_secs(5));
    assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
}

#[test]
fn bad_bodies_are_rejected_without_a_model() {
    let srv = start(&[]);
    let mut s = TcpStream::connect(&srv.addr).unwrap();
    s.write_all(b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
        .unwrap();
    let (resp, _) = response_within(&mut s, Duration::from_secs(5));
    assert!(resp.starts_with("HTTP/1.1 415"), "{resp}");
    let mut s = post_partial(&srv.addr, "/mcp", 5, b"nope!");
    let (resp, _) = response_within(&mut s, Duration::from_secs(5));
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    // A whole, valid MCP request still works.
    let req = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
    let mut s = post_partial(&srv.addr, "/mcp", req.len(), req);
    let (resp, _) = response_within(&mut s, Duration::from_secs(5));
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
}

#[test]
fn ask_rate_limit_applies_before_the_body_is_read() {
    let srv = start(&[("RATE_LIMIT_PER_MIN", "1"), ("POSTCMS_BODY_READ_TIMEOUT_SECS", "30")]);
    // Takes the only slot of the minute and stalls its body.
    let _first = post_partial(&srv.addr, "/api/ask", 100, b"{");
    std::thread::sleep(Duration::from_millis(300));
    // Refused at once, though its body never arrives either.
    let mut second = post_partial(&srv.addr, "/api/ask", 100, b"{");
    let (resp, took) = response_within(&mut second, Duration::from_secs(5));
    assert!(resp.starts_with("HTTP/1.1 429"), "{resp}");
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[test]
fn ask_in_flight_cap_applies_before_the_body_is_read() {
    let srv = start(&[
        ("ASK_MAX_CONCURRENT", "1"),
        ("RATE_LIMIT_PER_MIN", "100"),
        ("POSTCMS_BODY_READ_TIMEOUT_SECS", "30"),
    ]);
    let _first = post_partial(&srv.addr, "/api/ask", 100, b"{");
    std::thread::sleep(Duration::from_millis(300));
    let mut second = post_partial(&srv.addr, "/api/ask", 100, b"{");
    let (resp, took) = response_within(&mut second, Duration::from_secs(5));
    assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[test]
fn connections_over_the_cap_are_closed() {
    let srv = start(&[("POSTCMS_MAX_CONNECTIONS", "2"), ("POSTCMS_HEADER_READ_TIMEOUT_SECS", "60")]);
    let mut held: Vec<TcpStream> = (0..2)
        .map(|_| {
            let mut s = TcpStream::connect(&srv.addr).unwrap();
            s.write_all(b"GET / HTTP/1.1\r\nHost: x").unwrap();
            s
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    // The third is accepted by the kernel, then closed by the server.
    let mut third = TcpStream::connect(&srv.addr).unwrap();
    third.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let start = Instant::now();
    let mut buf = [0u8; 64];
    match third.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("over-cap connection got {n} bytes"),
        Err(e) => assert!(
            !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
            "over-cap connection still open after {:?}",
            start.elapsed()
        ),
    }
    // Freeing a slot lets new connections in again.
    drop(held.pop());
    let mut ok = false;
    for _ in 0..50 {
        let mut s = TcpStream::connect(&srv.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let _ = s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let mut r = String::new();
        let _ = s.read_to_string(&mut r);
        if r.starts_with("HTTP/1.1 200") {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ok, "no connection served after a slot was freed");
}

#[test]
fn invalid_limits_refuse_to_start() {
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("site.yaml"), "title: Test\n").unwrap();
    std::fs::create_dir(site.path().join("dist")).unwrap();
    for (k, v) in [("POSTCMS_BODY_READ_TIMEOUT_SECS", "0"), ("POSTCMS_MAX_CONNECTIONS", "0")] {
        let out = Command::new(env!("CARGO_BIN_EXE_server"))
            .env_clear()
            .env("SITE_ROOT", site.path())
            .env("HOME", site.path())
            .env("PORT", "0")
            .env(k, v)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{k}={v}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(k));
    }
}

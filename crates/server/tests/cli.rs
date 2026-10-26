//! Command-line and listen-address behavior of the `server` binary.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn server() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_server"));
    c.env_remove("POSTCMS_HOST").env_remove("SITE_ROOT").env_remove("PORT");
    c
}

/// A site with a one-page dist/, unique per test.
fn temp_site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("post-cms-server-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    std::fs::write(dir.join("site.yaml"), "title: x\n").unwrap();
    std::fs::write(dir.join("dist/index.html"), "<p>hello</p>").unwrap();
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
fn start(site: &PathBuf, args: &[&str], env_host: Option<&str>) -> (Running, SocketAddr, Vec<String>) {
    let mut cmd = server();
    cmd.args(args)
        .env("SITE_ROOT", site)
        .env("PORT", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(h) = env_host {
        cmd.env("POSTCMS_HOST", h);
    }
    let mut child = Running(cmd.spawn().unwrap());
    let stderr = child.0.stderr.take().unwrap();
    let mut lines = Vec::new();
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if let Some(addr) = line.strip_prefix("origin serving dist/ on http://") {
            let addr: SocketAddr = addr.parse().unwrap();
            return (child, addr, lines);
        }
        lines.push(line);
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
            .env("SITE_ROOT", &site)
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
            .env("SITE_ROOT", &site)
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
        .env("SITE_ROOT", &site)
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
    let (_server, addr, warnings) = start(&site, &[], None);
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert!(warnings.iter().all(|l| !l.contains("warning")), "{warnings:?}");
    assert!(get(addr).contains("hello"));
}

#[test]
fn explicit_loopback_host_does_not_warn() {
    let site = temp_site("explicit");
    // ::1 from the flag; the flag wins over POSTCMS_HOST.
    let (_server, addr, warnings) = start(&site, &["--host", "::1"], Some("127.0.0.1"));
    assert_eq!(addr.ip().to_string(), "::1");
    assert!(warnings.iter().all(|l| !l.contains("warning")), "{warnings:?}");
    assert!(get(addr).contains("hello"));
}

#[test]
fn env_host_is_used_when_no_flag() {
    let site = temp_site("envhost");
    let (_server, addr, _) = start(&site, &[], Some("::1"));
    assert_eq!(addr.ip().to_string(), "::1");
    assert!(get(addr).contains("hello"));
}

#[test]
fn all_interfaces_bind_warns() {
    let site = temp_site("all");
    let (_server, addr, warnings) = start(&site, &["--host=0.0.0.0"], None);
    assert_eq!(addr.ip().to_string(), "0.0.0.0");
    assert!(
        warnings.iter().any(|l| l.starts_with("warning: listening on 0.0.0.0:") && l.contains("every network interface")),
        "{warnings:?}"
    );
    // Reachable over loopback too.
    let local = SocketAddr::from(([127, 0, 0, 1], addr.port()));
    assert!(get(local).contains("hello"));
}

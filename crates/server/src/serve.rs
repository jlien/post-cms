//! HTTP/1 accept loop with a header-read timeout, a connection cap and a
//! bounded graceful shutdown. The body-read timeout is applied by the
//! handlers that read a body (see `read_json` in main.rs).
//!
//! `axum::serve` sets neither: a client that sends half a request head keeps
//! its connection, and graceful shutdown waits for every connection with no
//! deadline, so one stalled client holds the process open after SIGTERM until
//! the platform SIGKILLs it (cutting every other in-flight request too).

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tower::ServiceExt;

/// Seconds a client has to send a complete request head.
pub const HEADER_READ_TIMEOUT_ENV: &str = "POSTCMS_HEADER_READ_TIMEOUT_SECS";
/// Seconds open connections get to finish after SIGTERM/SIGINT.
pub const SHUTDOWN_TIMEOUT_ENV: &str = "POSTCMS_SHUTDOWN_TIMEOUT_SECS";
/// Seconds a client has to send a whole request body (POST /api/ask, /mcp).
pub const BODY_READ_TIMEOUT_ENV: &str = "POSTCMS_BODY_READ_TIMEOUT_SECS";
/// Most connections open at once; more are closed as soon as accepted.
pub const MAX_CONNECTIONS_ENV: &str = "POSTCMS_MAX_CONNECTIONS";
pub const DEFAULT_HEADER_READ_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_BODY_READ_TIMEOUT_SECS: u64 = 15;
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;
/// Seconds a response write may make no progress (a client not reading).
pub const WRITE_TIMEOUT_ENV: &str = "POSTCMS_WRITE_TIMEOUT_SECS";
pub const DEFAULT_WRITE_TIMEOUT_SECS: u64 = 15;
/// Proxies in front of the server that append to X-Forwarded-For.
pub const TRUSTED_PROXY_HOPS_ENV: &str = "POSTCMS_TRUSTED_PROXY_HOPS";
const MAX_TRUSTED_PROXY_HOPS: usize = 10;
const MAX_MAX_CONNECTIONS: usize = 65_535;
/// 25 s fits inside a 30 s platform stop grace period (Kubernetes default).
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 25;
const MAX_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeouts {
    pub header_read: Duration,
    pub body_read: Duration,
    pub shutdown: Duration,
    pub max_connections: usize,
    pub write: Duration,
    pub trusted_proxy_hops: usize,
}

/// Limits from the environment (`get` looks a variable up). Unset or empty
/// means the default; a timeout must be whole seconds from 1 to 3600 and the
/// connection cap a whole number from 1 to 65535. Anything else is an error,
/// so the server refuses to start rather than guess.
pub fn timeouts_from(get: impl Fn(&str) -> Option<String>) -> Result<Timeouts, String> {
    let secs = |name: &str, default: u64| -> Result<Duration, String> {
        match get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            None => Ok(Duration::from_secs(default)),
            Some(v) => match v.parse::<u64>() {
                Ok(n) if (1..=MAX_SECS).contains(&n) => Ok(Duration::from_secs(n)),
                _ => Err(format!("{name} {v:?} must be whole seconds from 1 to {MAX_SECS}")),
            },
        }
    };
    let trusted_proxy_hops = match get(TRUSTED_PROXY_HOPS_ENV).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        None => 0,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n <= MAX_TRUSTED_PROXY_HOPS => n,
            _ => return Err(format!("{TRUSTED_PROXY_HOPS_ENV} {v:?} must be a whole number from 0 to {MAX_TRUSTED_PROXY_HOPS}")),
        },
    };
    let max_connections = match get(MAX_CONNECTIONS_ENV).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        None => DEFAULT_MAX_CONNECTIONS,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if (1..=MAX_MAX_CONNECTIONS).contains(&n) => n,
            _ => return Err(format!("{MAX_CONNECTIONS_ENV} {v:?} must be a whole number from 1 to {MAX_MAX_CONNECTIONS}")),
        },
    };
    Ok(Timeouts {
        header_read: secs(HEADER_READ_TIMEOUT_ENV, DEFAULT_HEADER_READ_TIMEOUT_SECS)?,
        body_read: secs(BODY_READ_TIMEOUT_ENV, DEFAULT_BODY_READ_TIMEOUT_SECS)?,
        shutdown: secs(SHUTDOWN_TIMEOUT_ENV, DEFAULT_SHUTDOWN_TIMEOUT_SECS)?,
        max_connections,
        write: secs(WRITE_TIMEOUT_ENV, DEFAULT_WRITE_TIMEOUT_SECS)?,
        trusted_proxy_hops,
    })
}

/// Serve `app` on `listener` until `signal` resolves, then stop accepting,
/// let open connections finish for up to `t.shutdown`, and drop the rest.
/// Returns how many connections were dropped at the deadline.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    t: Timeouts,
    signal: impl Future<Output = ()>,
) -> usize {
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut conns = JoinSet::new();
    let slots = std::sync::Arc::new(Semaphore::new(t.max_connections));
    let mut last_full_log: Option<std::time::Instant> = None;
    tokio::pin!(signal);
    loop {
        while conns.try_join_next().is_some() {}
        let (stream, peer) = tokio::select! {
            _ = &mut signal => break,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // Out of file descriptors and the like: back off, keep serving.
                    eprintln!("origin: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        // Over the cap: close at once (dropping the stream), so stalled
        // clients cannot pile up file descriptors and memory.
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            drop(stream);
            if last_full_log.map_or(true, |at| at.elapsed() >= Duration::from_secs(10)) {
                eprintln!(
                    "origin: {} connections open ({MAX_CONNECTIONS_ENV}), closing new ones",
                    t.max_connections
                );
                last_full_log = Some(std::time::Instant::now());
            }
            continue;
        };
        let app = app.clone();
        let svc = TowerToHyperService::new(tower::service_fn(move |mut req: Request<Incoming>| {
            req.extensions_mut().insert(ConnectInfo::<SocketAddr>(peer));
            app.clone().oneshot(req)
        }));
        let mut stop = stop_rx.clone();
        let header_read = t.header_read;
        let write_timeout = t.write;
        conns.spawn(async move {
            let _slot = slot;
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(header_read);
            let conn = builder.serve_connection(TokioIo::new(WriteTimeout::new(stream, write_timeout)), svc);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => return,
                _ = stop.wait_for(|s| *s) => conn.as_mut().graceful_shutdown(),
            }
            let _ = conn.await;
        });
    }
    drop(listener);
    let _ = stop_tx.send(true);
    let drained = tokio::time::timeout(t.shutdown, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_ok() {
        return 0;
    }
    let open = conns.len();
    eprintln!(
        "origin: {open} connection(s) still open after {}s ({SHUTDOWN_TIMEOUT_ENV}), dropping them",
        t.shutdown.as_secs()
    );
    conns.shutdown().await;
    open
}

/// A stream whose writes fail with `TimedOut` once a write or flush has made
/// no progress for `limit`: a client that stops reading its response cannot
/// hold the connection (and its slot under the connection cap) open. The
/// clock restarts on every byte written, like a send timeout, so slow but
/// moving clients are not cut off.
struct WriteTimeout<S> {
    inner: S,
    limit: Duration,
    stalled: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<S> WriteTimeout<S> {
    fn new(inner: S, limit: Duration) -> Self {
        WriteTimeout { inner, limit, stalled: None }
    }

    /// After the inner write returned Pending: arm the timer once, and fail
    /// when it has run out.
    fn pending<T>(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<T>> {
        let limit = self.limit;
        let sleep = self.stalled.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        match sleep.as_mut().poll(cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "client stopped reading the response",
            ))),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for WriteTimeout<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for WriteTimeout<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match std::pin::Pin::new(&mut self.inner).poll_write(cx, buf) {
            std::task::Poll::Pending => self.pending(cx),
            ready => {
                self.stalled = None;
                ready
            }
        }
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            std::task::Poll::Pending => self.pending(cx),
            ready => {
                self.stalled = None;
                ready
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match std::pin::Pin::new(&mut self.inner).poll_flush(cx) {
            std::task::Poll::Pending => self.pending(cx),
            ready => {
                self.stalled = None;
                ready
            }
        }
    }

    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: std::collections::HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_when_unset_or_empty() {
        let want = Timeouts {
            header_read: Duration::from_secs(10),
            body_read: Duration::from_secs(15),
            shutdown: Duration::from_secs(25),
            max_connections: 512,
            write: Duration::from_secs(15),
            trusted_proxy_hops: 0,
        };
        assert_eq!(timeouts_from(env(&[])), Ok(want));
        assert_eq!(
            timeouts_from(env(&[(HEADER_READ_TIMEOUT_ENV, ""), (SHUTDOWN_TIMEOUT_ENV, " ")])),
            Ok(want)
        );
    }

    #[test]
    fn whole_seconds_in_range() {
        let t = timeouts_from(env(&[
            (HEADER_READ_TIMEOUT_ENV, "3"),
            (BODY_READ_TIMEOUT_ENV, "4"),
            (SHUTDOWN_TIMEOUT_ENV, "3600"),
            (MAX_CONNECTIONS_ENV, "65535"),
            (WRITE_TIMEOUT_ENV, "7"),
            (TRUSTED_PROXY_HOPS_ENV, "2"),
        ]))
        .unwrap();
        assert_eq!(t.write, Duration::from_secs(7));
        assert_eq!(t.trusted_proxy_hops, 2);
        assert_eq!(t.header_read, Duration::from_secs(3));
        assert_eq!(t.body_read, Duration::from_secs(4));
        assert_eq!(t.shutdown, Duration::from_secs(3600));
        assert_eq!(t.max_connections, 65535);
    }

    #[test]
    fn rejects_bad_values() {
        for v in ["0", "3601", "-1", "1.5", "10s", "abc"] {
            for name in [HEADER_READ_TIMEOUT_ENV, BODY_READ_TIMEOUT_ENV, SHUTDOWN_TIMEOUT_ENV, WRITE_TIMEOUT_ENV] {
                let e = timeouts_from(env(&[(name, v)])).unwrap_err();
                assert!(e.starts_with(name), "{e}");
            }
        }
        for v in ["0", "65536", "-1", "1.5", "many"] {
            let e = timeouts_from(env(&[(MAX_CONNECTIONS_ENV, v)])).unwrap_err();
            assert!(e.starts_with(MAX_CONNECTIONS_ENV), "{e}");
        }
        for v in ["11", "-1", "two"] {
            let e = timeouts_from(env(&[(TRUSTED_PROXY_HOPS_ENV, v)])).unwrap_err();
            assert!(e.starts_with(TRUSTED_PROXY_HOPS_ENV), "{e}");
        }
    }
}

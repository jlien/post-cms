//! HTTP/1 accept loop with a header-read timeout and a bounded graceful
//! shutdown.
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
use tokio::task::JoinSet;
use tower::ServiceExt;

/// Seconds a client has to send a complete request head.
pub const HEADER_READ_TIMEOUT_ENV: &str = "POSTCMS_HEADER_READ_TIMEOUT_SECS";
/// Seconds open connections get to finish after SIGTERM/SIGINT.
pub const SHUTDOWN_TIMEOUT_ENV: &str = "POSTCMS_SHUTDOWN_TIMEOUT_SECS";
pub const DEFAULT_HEADER_READ_TIMEOUT_SECS: u64 = 10;
/// 25 s fits inside a 30 s platform stop grace period (Kubernetes default).
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 25;
const MAX_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeouts {
    pub header_read: Duration,
    pub shutdown: Duration,
}

/// Timeouts from the environment (`get` looks a variable up). Unset or empty
/// means the default; anything other than whole seconds from 1 to 3600 is an
/// error, so the server refuses to start rather than guess.
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
    Ok(Timeouts {
        header_read: secs(HEADER_READ_TIMEOUT_ENV, DEFAULT_HEADER_READ_TIMEOUT_SECS)?,
        shutdown: secs(SHUTDOWN_TIMEOUT_ENV, DEFAULT_SHUTDOWN_TIMEOUT_SECS)?,
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
        let app = app.clone();
        let svc = TowerToHyperService::new(tower::service_fn(move |mut req: Request<Incoming>| {
            req.extensions_mut().insert(ConnectInfo::<SocketAddr>(peer));
            app.clone().oneshot(req)
        }));
        let mut stop = stop_rx.clone();
        let header_read = t.header_read;
        conns.spawn(async move {
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(header_read);
            let conn = builder.serve_connection(TokioIo::new(stream), svc);
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
        let want = Timeouts { header_read: Duration::from_secs(10), shutdown: Duration::from_secs(25) };
        assert_eq!(timeouts_from(env(&[])), Ok(want));
        assert_eq!(
            timeouts_from(env(&[(HEADER_READ_TIMEOUT_ENV, ""), (SHUTDOWN_TIMEOUT_ENV, " ")])),
            Ok(want)
        );
    }

    #[test]
    fn whole_seconds_in_range() {
        let t = timeouts_from(env(&[(HEADER_READ_TIMEOUT_ENV, "3"), (SHUTDOWN_TIMEOUT_ENV, "3600")])).unwrap();
        assert_eq!(t.header_read, Duration::from_secs(3));
        assert_eq!(t.shutdown, Duration::from_secs(3600));
    }

    #[test]
    fn rejects_bad_values() {
        for v in ["0", "3601", "-1", "1.5", "10s", "abc"] {
            for name in [HEADER_READ_TIMEOUT_ENV, SHUTDOWN_TIMEOUT_ENV] {
                let e = timeouts_from(env(&[(name, v)])).unwrap_err();
                assert!(e.starts_with(name), "{e}");
            }
        }
    }
}

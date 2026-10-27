//! Running the Tailwind CLI (`tailwindcss` from PATH) for the theme CSS.
//!
//! The CLI is third-party code (a Node script or a standalone binary), so it
//! gets a cleared environment with only [`ENV_ALLOWLIST`]: build secrets such
//! as `SANITY_READ_TOKEN` never reach it. It runs in its own process group
//! and is killed with everything it started when it runs past the timeout
//! (`POSTCMS_TAILWIND_TIMEOUT_SECS`, default [`DEFAULT_TIMEOUT_SECS`]) or when the
//! build itself gets SIGINT, SIGTERM or SIGHUP.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

/// Environment variable that overrides the timeout, in whole seconds.
pub const TIMEOUT_ENV: &str = "POSTCMS_TAILWIND_TIMEOUT_SECS";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;

/// The only variables passed to `tailwindcss`, when set:
/// - `PATH`: finds `tailwindcss`, and `node` for the npm CLI's
///   `#!/usr/bin/env node` line.
/// - `HOME`: Node and the standalone binary expect a home directory.
/// - `TMPDIR`: Node's `os.tmpdir()` (the CLI's config loader caches there).
/// - `NODE_PATH`: the CLI resolves `@import` and `@plugin` packages with it.
/// - `LANG`, `LC_ALL`, `LC_CTYPE`: locale, so file names and messages
///   outside ASCII decode the same way as in the build.
///
/// On Windows also `SYSTEMROOT` (needed by Node and system DLLs), `TEMP`,
/// `TMP` and `USERPROFILE` (the Windows forms of the temp and home dirs).
#[cfg(not(windows))]
pub const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "TMPDIR", "NODE_PATH", "LANG", "LC_ALL", "LC_CTYPE"];
#[cfg(windows)]
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "NODE_PATH", "LANG", "LC_ALL", "LC_CTYPE", "SYSTEMROOT", "TEMP", "TMP", "USERPROFILE",
];

/// The timeout from [`TIMEOUT_ENV`]: unset or empty is the default; anything
/// but a whole number of seconds from 1 to 3600 is an error.
pub fn timeout_from_env() -> Result<Duration> {
    let v = std::env::var_os(TIMEOUT_ENV);
    let v = v.as_deref().map(|s| s.to_str().unwrap_or("\u{fffd}"));
    parse_timeout(v).map_err(anyhow::Error::msg)
}

fn parse_timeout(v: Option<&str>) -> std::result::Result<Duration, String> {
    let v = v.map(str::trim).unwrap_or("");
    if v.is_empty() {
        return Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }
    match v.parse::<u64>() {
        Ok(n) if (1..=MAX_TIMEOUT_SECS).contains(&n) => Ok(Duration::from_secs(n)),
        _ => Err(format!(
            "{TIMEOUT_ENV}={v:?} is not a whole number of seconds from 1 to {MAX_TIMEOUT_SECS}. Nothing was built or deleted."
        )),
    }
}

/// How the CLI ended, when it ended on its own.
#[derive(Debug)]
pub enum Run {
    Exited(ExitStatus),
    /// It could not be started (most often: not on PATH).
    NotStarted(std::io::Error),
}

/// The CLI's file name on this platform.
#[cfg(not(windows))]
const CLI_NAMES: &[&str] = &["tailwindcss"];
#[cfg(windows)]
const CLI_NAMES: &[&str] = &["tailwindcss.exe"];

/// The first `tailwindcss` in `path` that is an executable file, searching
/// absolute entries only. An empty or relative entry would resolve against
/// the CLI's working directory (the site root), so a `tailwindcss` planted
/// in the site would run; such entries are skipped.
fn resolve_cli(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    for dir in std::env::split_paths(path?) {
        if !dir.is_absolute() {
            continue;
        }
        for name in CLI_NAMES {
            let cand = dir.join(name);
            if is_executable(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.is_file() && m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        m.is_file()
    }
}

/// Run `tailwindcss -i <input> -o <output> --minify` in `cwd` (Tailwind 4
/// detects template content from its working directory), with the cleared
/// environment and no stdin, stdout or stderr. The CLI is resolved to an
/// absolute path from the absolute `PATH` entries. A run past `timeout` is
/// killed (its whole process group) and is an error naming the timeout.
/// Anything the CLI leaves running in its group when it exits is killed too.
pub fn run(input: &Path, output: &Path, cwd: &Path, timeout: Duration) -> Result<Run> {
    let Some(exe) = resolve_cli(std::env::var_os("PATH").as_deref()) else {
        return Ok(Run::NotStarted(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no tailwindcss in the absolute PATH entries",
        )));
    };
    let mut cmd = Command::new(&exe);
    cmd.arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--minify")
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for k in ENV_ALLOWLIST {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    // its own process group, so a timeout or an interrupt kills anything it
    // started too (a wrapper script's node, say)
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);

    #[cfg(unix)]
    let _forward = signals::Forward::install();
    // Hold SIGINT, SIGTERM and SIGHUP until the handler knows the CLI's
    // group, so one arriving during the spawn still kills it. The child
    // starts with an empty signal mask (std resets it before exec).
    #[cfg(unix)]
    let held = signals::Hold::new();
    let spawned = cmd.spawn();
    #[cfg(unix)]
    if let Ok(c) = &spawned {
        signals::watch(c);
    }
    #[cfg(unix)]
    drop(held);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Ok(Run::NotStarted(e)),
    };
    let started = Instant::now();
    let result = loop {
        match exited(&mut child) {
            Ok(true) => {
                // the group may still hold processes the CLI started
                kill_group(&mut child);
                break match child.wait() {
                    Ok(st) => Ok(Run::Exited(st)),
                    Err(e) => Err(anyhow::anyhow!("waiting for tailwindcss failed: {e}")),
                };
            }
            Ok(false) if started.elapsed() >= timeout => {
                kill_group(&mut child);
                let _ = child.wait();
                break Err(anyhow::anyhow!(
                    "tailwindcss did not finish within {}s and was killed; set {TIMEOUT_ENV} (1 to {MAX_TIMEOUT_SECS} seconds) to allow longer",
                    timeout.as_secs()
                ));
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                kill_group(&mut child);
                let _ = child.wait();
                break Err(anyhow::anyhow!("waiting for tailwindcss failed: {e}"));
            }
        }
    };
    #[cfg(unix)]
    signals::unwatch();
    result
}

/// Whether the child has exited, without reaping it on Unix: while it is
/// an unreaped zombie its pid, and so its process group id, cannot be
/// reused, which keeps the group kill after a normal exit safe.
#[cfg(unix)]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    let pid = libc::id_t::from(child.id());
    // SAFETY: waitid writes only into `info`, a zeroed siginfo_t; WNOWAIT
    // leaves the child waitable for Child::wait.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

#[cfg(not(unix))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|s| s.is_some())
}

/// Kill the child's process group (Unix) and the child; the caller reaps it.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: signals the group this child leads (it was started with
        // process_group(0), so its group id is its pid, and it is not yet
        // reaped); no memory involved.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
}

/// While the CLI runs, SIGINT, SIGTERM and SIGHUP to the build also kill the
/// CLI's process group (it is not in the terminal's foreground group, so a
/// Ctrl-C would otherwise miss it), then end the build with the same signal
/// as before. A signal the build ignores (SIGINT in a background job) stays
/// ignored. The previous handlers come back when the CLI is done.
#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicI32, Ordering};

    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    /// Process group of the running CLI, 0 when none.
    static GROUP: AtomicI32 = AtomicI32::new(0);

    extern "C" fn forward(sig: libc::c_int) {
        let g = GROUP.load(Ordering::SeqCst);
        // SAFETY: kill, signal and raise are async-signal-safe.
        unsafe {
            if g > 0 {
                libc::kill(-g, libc::SIGKILL);
            }
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    pub(super) struct Forward {
        old: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl Forward {
        pub(super) fn install() -> Forward {
            let mut old = Vec::new();
            for sig in SIGNALS {
                // SAFETY: zeroed sigaction structs are valid; sigaction only
                // reads `new` and writes `prev`.
                unsafe {
                    let mut prev: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, std::ptr::null(), &mut prev) != 0 || prev.sa_sigaction == libc::SIG_IGN {
                        continue;
                    }
                    let mut new: libc::sigaction = std::mem::zeroed();
                    new.sa_sigaction = forward as extern "C" fn(libc::c_int) as libc::sighandler_t;
                    libc::sigemptyset(&mut new.sa_mask);
                    if libc::sigaction(sig, &new, &mut prev) == 0 {
                        old.push((sig, prev));
                    }
                }
            }
            Forward { old }
        }
    }

    impl Drop for Forward {
        fn drop(&mut self) {
            GROUP.store(0, Ordering::SeqCst);
            for (sig, prev) in &self.old {
                // SAFETY: restores the action saved by install.
                unsafe { libc::sigaction(*sig, prev, std::ptr::null_mut()) };
            }
        }
    }

    /// Blocks the forwarded signals until dropped (then any that arrived
    /// are delivered).
    pub(super) struct Hold {
        old: libc::sigset_t,
    }

    impl Hold {
        pub(super) fn new() -> Hold {
            // SAFETY: plain sigset manipulation on this thread's mask.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                for sig in SIGNALS {
                    libc::sigaddset(&mut set, sig);
                }
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
                Hold { old }
            }
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: restores the mask saved by new.
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut()) };
        }
    }

    pub(super) fn watch(child: &std::process::Child) {
        if let Ok(pid) = i32::try_from(child.id()) {
            GROUP.store(pid, Ordering::SeqCst);
        }
    }

    pub(super) fn unwatch() {
        GROUP.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_defaults_and_bounds() {
        assert_eq!(parse_timeout(None), Ok(Duration::from_secs(120)));
        assert_eq!(parse_timeout(Some("")), Ok(Duration::from_secs(120)));
        assert_eq!(parse_timeout(Some(" 30 ")), Ok(Duration::from_secs(30)));
        assert_eq!(parse_timeout(Some("1")), Ok(Duration::from_secs(1)));
        assert_eq!(parse_timeout(Some("3600")), Ok(Duration::from_secs(3600)));
        for bad in ["0", "3601", "-5", "1.5", "2m", "abc"] {
            let e = parse_timeout(Some(bad)).unwrap_err();
            assert!(e.contains(TIMEOUT_ENV) && e.contains("1 to 3600"), "{bad}: {e}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn cli_resolves_from_absolute_path_entries_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (abs, noexec) = (tmp.path().join("abs"), tmp.path().join("noexec"));
        for (d, mode) in [(&abs, 0o755), (&noexec, 0o644)] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("tailwindcss"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(d.join("tailwindcss"), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let path = |s: String| Some(std::ffi::OsString::from(s));
        let found = |s: String| resolve_cli(path(s).as_deref());
        assert_eq!(found(format!(":.:rel:{}", abs.display())), Some(abs.join("tailwindcss")));
        assert_eq!(found(format!("{}:{}", noexec.display(), abs.display())), Some(abs.join("tailwindcss")));
        assert_eq!(found(":.:rel:".into()), None);
        assert_eq!(resolve_cli(None), None);
    }

    #[test]
    fn allowlist_has_no_secrets_or_node_options() {
        for k in ENV_ALLOWLIST {
            assert!(!k.contains("TOKEN") && !k.contains("KEY") && !k.contains("SECRET"), "{k}");
        }
        assert!(!ENV_ALLOWLIST.contains(&"NODE_OPTIONS"));
    }
}

//! Template sandbox: what a Tera template may reach outside its context.
//!
//! Templates are authored content (an agent or an editor can propose one), so
//! they must not read the build environment or the file system. Tera's
//! default builtins were audited (tera 1.20) for that:
//!
//! - `get_env` reads any environment variable of the build process: replaced
//!   by a function that always fails with a fixed message.
//! - `throw` puts its `message` argument into the build error: replaced by a
//!   fixed message, so a template cannot write arbitrary text into build logs.
//! - `now` reads the clock and, without `utc=true`, the host time zone (`TZ`,
//!   `/etc/localtime`): replaced by a UTC-only `now`, so the host zone never
//!   reaches the output and builds do not depend on it.
//! - `range`, `get_random` and every builtin filter and tester work on their
//!   arguments only (`date` uses the time zone database compiled into the
//!   binary). They stay.
//! - `include`, `extends` and `import` look templates up by name among the
//!   ones already loaded; Tera never opens a file for them. The only file
//!   reads are the template loads themselves, which [`check_confined`] limits
//!   to the template directories (a symlink there must not point outside).
use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;
use tera::Tera;

/// Message of every `get_env` call.
pub const GET_ENV_REFUSED: &str = "get_env is disabled: templates cannot read the build environment";
/// Message of every `throw` call; the template's `message` is never shown.
pub const THROW_MESSAGE: &str = "a template called throw()";

/// Replace the Tera builtins that read the environment or echo template text
/// into errors. Call on every template set before rendering.
pub fn lock_down(tera: &mut Tera) {
    tera.register_function("get_env", |_: &HashMap<String, Value>| -> tera::Result<Value> {
        Err(tera::Error::msg(GET_ENV_REFUSED))
    });
    tera.register_function("throw", |_: &HashMap<String, Value>| -> tera::Result<Value> {
        Err(tera::Error::msg(THROW_MESSAGE))
    });
    tera.register_function("now", utc_now);
}

/// Tera's `now`, always in UTC: `timestamp=true` gives Unix seconds, else an
/// RFC 3339 string with a `+00:00` offset. `utc` is accepted and ignored.
fn utc_now(args: &HashMap<String, Value>) -> tera::Result<Value> {
    for key in ["utc", "timestamp"] {
        if args.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(tera::Error::msg(format!("Function `now`: `{key}` can only be a boolean")));
        }
    }
    let now = chrono::Utc::now();
    if args.get("timestamp").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(Value::from(now.timestamp()));
    }
    Ok(Value::String(now.fixed_offset().to_rfc3339()))
}

/// Refuse a template directory holding a symlink (to a file or a directory)
/// that resolves outside it, or that does not resolve. Tera follows symlinks
/// when it loads templates, and a parse error quotes the file, so the check
/// runs before anything is read. A missing directory passes (nothing loads).
pub fn check_confined(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let root = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
    walk(&root, &root)
}

fn walk(dir: &Path, root: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_symlink() {
            let inside = path.canonicalize().is_ok_and(|t| t.starts_with(root));
            if !inside {
                return Err(format!(
                    "template symlink {} points outside {} (or nowhere); templates may only link inside it",
                    path.display(),
                    root.display()
                ));
            }
        } else if kind.is_dir() {
            walk(&path, root)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tera_with(body: &str) -> Tera {
        let mut t = Tera::default();
        t.add_raw_template("t.html", body).unwrap();
        lock_down(&mut t);
        t
    }

    /// The whole error chain, as build-site prints it.
    fn render_err(t: &Tera) -> String {
        let e = t.render("t.html", &tera::Context::new()).unwrap_err();
        format!("{:?}", anyhow::Error::from(e))
    }

    #[test]
    fn get_env_cannot_read_the_environment() {
        std::env::set_var("POSTCMS_SANDBOX_PROBE", "s3cr3t-probe-unit");
        for body in [
            r#"{{ get_env(name="POSTCMS_SANDBOX_PROBE") }}"#,
            r#"{{ get_env(name="POSTCMS_SANDBOX_PROBE", default="fallback-text") }}"#,
            r#"{% set v = get_env(name="POSTCMS_SANDBOX_PROBE") %}{{ v | int }}"#,
        ] {
            let err = render_err(&tera_with(body));
            assert!(err.contains(GET_ENV_REFUSED), "{body}: {err}");
            assert!(!err.contains("s3cr3t-probe-unit"), "{body} leaked: {err}");
            assert!(!err.contains("fallback-text"), "{body} echoed: {err}");
        }
    }

    #[test]
    fn throw_never_echoes_its_message() {
        for body in [
            r#"{{ throw(message="arbitrary-echo-marker") }}"#,
            r#"{{ throw(message=["arbitrary-echo-marker"]) }}"#,
            r#"{{ throw() }}"#,
        ] {
            let err = render_err(&tera_with(body));
            assert!(err.contains(THROW_MESSAGE), "{body}: {err}");
            assert!(!err.contains("arbitrary-echo-marker"), "{body} echoed: {err}");
        }
    }

    #[test]
    fn now_is_utc_and_still_formats() {
        let t = tera_with(r#"{{ now() }}|{{ now(utc=false) }}|{{ now() | date(format="%Y") }}|{{ now(timestamp=true) }}"#);
        let out = t.render("t.html", &tera::Context::new()).unwrap();
        let parts: Vec<&str> = out.split('|').collect();
        assert!(parts[0].ends_with("+00:00"), "{out}");
        assert!(parts[1].ends_with("+00:00"), "{out}");
        assert_eq!(parts[2], chrono::Utc::now().format("%Y").to_string());
        let ts: i64 = parts[3].parse().unwrap();
        assert!((ts - chrono::Utc::now().timestamp()).abs() < 60, "{out}");
        assert!(render_err(&tera_with(r#"{{ now(utc="yes") }}"#)).contains("can only be a boolean"));
    }

    #[test]
    fn other_builtins_still_work() {
        let t = tera_with(r#"{% for i in range(end=3) %}{{ i }}{% endfor %}|{{ "A b" | slugify }}|{{ get_random(end=1) }}"#);
        assert_eq!(t.render("t.html", &tera::Context::new()).unwrap(), "012|a-b|0");
    }

    #[cfg(unix)]
    #[test]
    fn template_symlinks_must_stay_inside() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let tpl = tmp.path().join("templates");
        std::fs::create_dir_all(tpl.join("partials")).unwrap();
        std::fs::write(tpl.join("partials/footer.html"), "f").unwrap();
        std::fs::write(tmp.path().join("secret.html"), "s3cr3t").unwrap();
        assert!(check_confined(&tpl).is_ok());

        symlink(tpl.join("partials/footer.html"), tpl.join("footer.html")).unwrap();
        assert!(check_confined(&tpl).is_ok(), "a link inside the directory is fine");

        symlink(tmp.path().join("secret.html"), tpl.join("partials/leak.html")).unwrap();
        let err = check_confined(&tpl).unwrap_err();
        assert!(err.contains("leak.html") && err.contains("points outside"), "{err}");
        assert!(!err.contains("s3cr3t"), "{err}");
        std::fs::remove_file(tpl.join("partials/leak.html")).unwrap();

        symlink(tmp.path(), tpl.join("up")).unwrap();
        assert!(check_confined(&tpl).is_err(), "a directory link outside is refused");
        std::fs::remove_file(tpl.join("up")).unwrap();

        symlink(tmp.path().join("missing.html"), tpl.join("dangling.html")).unwrap();
        assert!(check_confined(&tpl).is_err(), "a dangling link is refused");
        std::fs::remove_file(tpl.join("dangling.html")).unwrap();

        assert!(check_confined(&tmp.path().join("absent")).is_ok());
    }
}

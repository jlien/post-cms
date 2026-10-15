//! pt-render honors built-in overrides from `--templates <dir>` and falls
//! back to the engine defaults without them.
use std::io::Write;
use std::process::{Command, Stdio};

const PT: &str = r#"[
  {"_type": "block", "style": "normal", "listItem": "bullet", "level": 1,
   "children": [{"_type": "span", "text": "a", "marks": ["l1"]}],
   "markDefs": [{"_type": "link", "_key": "l1", "href": "https://example.com/a"}]},
  {"_type": "divider"}
]"#;

fn run(args: &[&str]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pt-render"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pt-render");
    child.stdin.take().unwrap().write_all(PT.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn pt_render_uses_override_templates() {
    let dir = tempfile::tempdir().unwrap();
    let pt_dir = dir.path().join("pt");
    std::fs::create_dir_all(pt_dir.join("marks")).unwrap();
    std::fs::write(pt_dir.join("divider.html"), "<hr class=\"site-rule\">").unwrap();
    std::fs::write(
        pt_dir.join("list.html"),
        "<{{ tag }} class=\"site-list\">{% for i in items %}<li>{{ i | safe }}</li>{% endfor %}</{{ tag }}>",
    )
    .unwrap();
    std::fs::write(
        pt_dir.join("marks/link.html"),
        "<a class=\"site-link\" href=\"{{ href | safe }}\">{{ text_html | safe }}</a>",
    )
    .unwrap();

    let (code, html) = run(&["--templates", dir.path().to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(
        html,
        "<ul class=\"site-list\"><li><a class=\"site-link\" href=\"https://example.com/a\">a</a></li></ul><hr class=\"site-rule\">"
    );

    let (code, html) = run(&[]);
    assert_eq!(code, 0);
    assert_eq!(html, "<ul><li><a href=\"https://example.com/a\">a</a></li></ul><hr>");
}

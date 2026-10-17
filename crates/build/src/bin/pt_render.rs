//! pt-render: Portable Text JSON (a block array, or an object with `blocks`)
//! on stdin or from a file → HTML on stdout. Unrenderable block types go to
//! stderr and exit status 2, so corpus checks can fail loudly.
//!
//! Usage: pt-render [file.json] [--templates <dir>] [--project <id>] [--dataset <name>]
use postcms_build::pt::{render, PtOptions};
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut file = None;
    let mut templates = None;
    let mut opts = PtOptions { sanity_dataset: "production".into(), ..Default::default() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--templates" => { templates = args.get(i + 1).cloned(); i += 1; }
            "--project" => { opts.sanity_project_id = args.get(i + 1).cloned().unwrap_or_default(); i += 1; }
            "--dataset" => { opts.sanity_dataset = args.get(i + 1).cloned().unwrap_or_default(); i += 1; }
            other => file = Some(other.to_string()),
        }
        i += 1;
    }
    let mut raw = String::new();
    match &file {
        Some(f) => raw = std::fs::read_to_string(f).unwrap_or_else(|e| fail(&format!("read {f}: {e}"))),
        None => { std::io::stdin().read_to_string(&mut raw).unwrap_or_else(|e| fail(&format!("stdin: {e}"))); }
    }
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| fail(&format!("parse: {e}")));
    let blocks = json.get("blocks").cloned().unwrap_or(json);
    let tera = templates.map(|dir| {
        let mut t = tera::Tera::new(&format!("{dir}/**/*.html")).unwrap_or_else(|e| fail(&format!("templates: {e}")));
        postcms_build::pt::register_filters(&mut t);
        postcms_build::images::register(&mut t, None, "");
        t
    });
    let out = render(&blocks, &opts, tera.as_ref());
    print!("{}", out.html);
    for w in &out.warnings {
        eprintln!("warning: {w}");
    }
    if !out.unhandled.is_empty() {
        for u in &out.unhandled {
            eprintln!("unhandled: {u}");
        }
        std::process::exit(2);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("pt-render: {msg}");
    std::process::exit(1);
}

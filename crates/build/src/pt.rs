//! Portable Text (Sanity rich text) → HTML.
//!
//! Handles the standard block model (styles, lists with nesting, decorators,
//! link annotations), Sanity images, dividers, and tables. Every other block
//! type renders through a Tera template `pt/<_type>.html` with the block as
//! context; a type with no template is reported in `unhandled` and rendered
//! as nothing, so callers can fail loudly instead of shipping silent gaps.

use serde_json::Value;
use std::fmt::Write as _;
use tera::{Context as TeraContext, Tera};

/// Where Sanity image assets live, for turning asset references into URLs.
#[derive(Debug, Clone, Default)]
pub struct PtOptions {
    pub sanity_project_id: String,
    pub sanity_dataset: String,
    /// Optional width hint appended to Sanity image URLs (`?w=…&auto=format`).
    pub image_width: Option<u32>,
}

/// Result of a render: the HTML plus anything that could not be rendered.
#[derive(Debug, Default)]
pub struct PtOutput {
    pub html: String,
    /// One entry per unrenderable block: `"<_type>"` or `"<_type>: <reason>"`.
    pub unhandled: Vec<String>,
}

pub fn render(blocks: &Value, opts: &PtOptions, tera: Option<&Tera>) -> PtOutput {
    let mut out = PtOutput::default();
    let Some(arr) = blocks.as_array() else {
        if !blocks.is_null() {
            out.unhandled.push("not an array of blocks".into());
        }
        return out;
    };
    let mut lists: Vec<OpenList> = Vec::new();
    for block in arr {
        let ty = block.get("_type").and_then(Value::as_str).unwrap_or("");
        let list_item = block.get("listItem").and_then(Value::as_str);
        if ty == "block" && list_item.is_some() {
            let level = block.get("level").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            let tag = if list_item == Some("number") { "ol" } else { "ul" };
            open_list_item(&mut out.html, &mut lists, level, tag);
            out.html.push_str(&render_spans(block));
            continue;
        }
        close_lists(&mut out.html, &mut lists, 0);
        match ty {
            "block" => out.html.push_str(&render_text_block(block)),
            "image" => match image_url(block, opts) {
                Some(src) => {
                    let alt = block.get("alt").and_then(Value::as_str).unwrap_or("");
                    let _ = write!(
                        out.html,
                        "<figure><img src=\"{}\" alt=\"{}\" loading=\"lazy\">{}</figure>",
                        esc(&src),
                        esc(alt),
                        block
                            .get("caption")
                            .and_then(Value::as_str)
                            .map(|c| format!("<figcaption>{}</figcaption>", esc(c)))
                            .unwrap_or_default()
                    );
                }
                None => out.unhandled.push("image: no asset reference".into()),
            },
            "divider" => out.html.push_str("<hr>"),
            "table" => out.html.push_str(&render_table(block)),
            other => render_custom(other, block, opts, tera, &mut out),
        }
    }
    close_lists(&mut out.html, &mut lists, 0);
    out
}

struct OpenList {
    tag: &'static str,
    li_open: bool,
}

fn open_list_item(html: &mut String, lists: &mut Vec<OpenList>, level: usize, tag: &'static str) {
    close_lists(html, lists, level);
    if lists.len() == level && lists.last().map(|l| l.tag) != Some(tag) {
        close_lists(html, lists, level - 1);
    }
    while lists.len() < level {
        let _ = write!(html, "<{tag}>");
        lists.push(OpenList { tag, li_open: false });
    }
    let top = lists.last_mut().expect("list opened above");
    if top.li_open {
        html.push_str("</li>");
    }
    html.push_str("<li>");
    top.li_open = true;
}

/// Close open lists until only `keep` remain.
fn close_lists(html: &mut String, lists: &mut Vec<OpenList>, keep: usize) {
    while lists.len() > keep {
        let l = lists.pop().expect("non-empty");
        if l.li_open {
            html.push_str("</li>");
        }
        let _ = write!(html, "</{}>", l.tag);
    }
}

fn render_text_block(block: &Value) -> String {
    let style = block.get("style").and_then(Value::as_str).unwrap_or("normal");
    let tag = match style {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" => style,
        _ => "p",
    };
    format!("<{tag}>{}</{tag}>", render_spans(block))
}

fn render_spans(block: &Value) -> String {
    let mark_defs = block.get("markDefs").and_then(Value::as_array);
    let mut html = String::new();
    for span in block.get("children").and_then(Value::as_array).into_iter().flatten() {
        let text = span.get("text").and_then(Value::as_str).unwrap_or("");
        let mut inner = esc(text).replace('\n', "<br>");
        let marks: Vec<&str> = span
            .get("marks")
            .and_then(Value::as_array)
            .map(|m| m.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        // Decorators innermost, annotations (links) outermost.
        for mark in marks.iter().rev() {
            if let Some(tag) = decorator_tag(mark) {
                inner = format!("<{tag}>{inner}</{tag}>");
            }
        }
        for mark in &marks {
            if decorator_tag(mark).is_some() {
                continue;
            }
            let def = mark_defs.and_then(|defs| {
                defs.iter().find(|d| d.get("_key").and_then(Value::as_str) == Some(mark))
            });
            if let Some(def) = def {
                if def.get("_type").and_then(Value::as_str) == Some("link") {
                    let href = def.get("href").and_then(Value::as_str).unwrap_or("");
                    let blank = def.get("blank").and_then(Value::as_bool).unwrap_or(false);
                    let target = if blank { " target=\"_blank\" rel=\"noopener\"" } else { "" };
                    inner = format!("<a href=\"{}\"{target}>{inner}</a>", esc(href));
                }
            }
        }
        html.push_str(&inner);
    }
    html
}

fn decorator_tag(mark: &str) -> Option<&'static str> {
    Some(match mark {
        "strong" => "strong",
        "em" => "em",
        "code" => "code",
        "underline" => "u",
        "strike-through" => "s",
        "sup" => "sup",
        "sub" => "sub",
        _ => return None,
    })
}

/// Sanity image URL from `asset._ref` (`image-<id>-<w>x<h>-<ext>`), or the
/// migration placeholder `_sanityAsset: "image@<url>"`.
fn image_url(block: &Value, opts: &PtOptions) -> Option<String> {
    if let Some(r) = block.pointer("/asset/_ref").and_then(Value::as_str) {
        let rest = r.strip_prefix("image-")?;
        let (base, ext) = rest.rsplit_once('-')?;
        let mut url = format!(
            "https://cdn.sanity.io/images/{}/{}/{}.{}",
            opts.sanity_project_id, opts.sanity_dataset, base, ext
        );
        if let Some(w) = opts.image_width {
            let _ = write!(url, "?w={w}&auto=format");
        }
        return Some(url);
    }
    block
        .get("_sanityAsset")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix("image@"))
        .map(str::to_string)
}

fn render_table(block: &Value) -> String {
    let header = block.get("hasHeader").and_then(Value::as_bool).unwrap_or(false);
    let mut html = String::from("<table>");
    for (i, row) in block.get("rows").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let cell_tag = if header && i == 0 { "th" } else { "td" };
        html.push_str("<tr>");
        for cell in row.get("cells").and_then(Value::as_array).into_iter().flatten() {
            let _ = write!(html, "<{cell_tag}>{}</{cell_tag}>", esc(cell.as_str().unwrap_or("")));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    html
}

/// Custom block: `pt/<_type>.html` gets the block as `block`, plus `<field>_html`
/// for every field that is itself Portable Text (e.g. a callout body).
fn render_custom(ty: &str, block: &Value, opts: &PtOptions, tera: Option<&Tera>, out: &mut PtOutput) {
    let name = format!("pt/{ty}.html");
    let Some(tera) = tera.filter(|t| t.get_template_names().any(|n| n == name)) else {
        out.unhandled.push(ty.to_string());
        return;
    };
    let mut ctx = TeraContext::new();
    ctx.insert("block", block);
    if let Some(obj) = block.as_object() {
        for (k, v) in obj {
            let is_pt = v
                .as_array()
                .map(|a| !a.is_empty() && a.iter().all(|b| b.get("_type").is_some()))
                .unwrap_or(false);
            if is_pt {
                let nested = render(v, opts, Some(tera));
                out.unhandled.extend(nested.unhandled);
                ctx.insert(format!("{k}_html"), &nested.html);
            }
        }
    }
    match tera.render(&name, &ctx) {
        Ok(html) => out.html.push_str(&html),
        Err(e) => out.unhandled.push(format!("{ty}: template error: {e}")),
    }
}

/// Plain text of a block array (span text and table cells), for search and
/// embeddings indexes.
pub fn plain_text(blocks: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for b in blocks.as_array().into_iter().flatten() {
        for c in b.get("children").and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = c.get("text").and_then(Value::as_str) {
                parts.push(t.to_string());
            }
        }
        for row in b.get("rows").and_then(Value::as_array).into_iter().flatten() {
            for cell in row.get("cells").and_then(Value::as_array).into_iter().flatten() {
                if let Some(t) = cell.as_str() {
                    parts.push(t.to_string());
                }
            }
        }
        parts.push("\n".into());
    }
    parts.join(" ")
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span(text: &str, marks: &[&str]) -> Value {
        json!({"_type": "span", "text": text, "marks": marks})
    }

    fn opts() -> PtOptions {
        PtOptions { sanity_project_id: "p1".into(), sanity_dataset: "production".into(), image_width: None }
    }

    #[test]
    fn paragraphs_headings_and_escaping() {
        let pt = json!([
            {"_type": "block", "style": "h2", "children": [span("Fees & <costs>", &[])], "markDefs": []},
            {"_type": "block", "style": "normal", "children": [span("line\nbreak", &[])], "markDefs": []}
        ]);
        let out = render(&pt, &opts(), None);
        assert_eq!(out.html, "<h2>Fees &amp; &lt;costs&gt;</h2><p>line<br>break</p>");
        assert!(out.unhandled.is_empty());
    }

    #[test]
    fn decorators_and_links() {
        let pt = json!([{
            "_type": "block", "style": "normal",
            "markDefs": [{"_type": "link", "_key": "l1", "href": "https://happy.co/demo", "blank": true}],
            "children": [span("Book ", &["strong"]), span("a demo", &["strong", "l1"])]
        }]);
        let out = render(&pt, &opts(), None);
        assert_eq!(
            out.html,
            "<p><strong>Book </strong><a href=\"https://happy.co/demo\" target=\"_blank\" rel=\"noopener\"><strong>a demo</strong></a></p>"
        );
    }

    #[test]
    fn nested_and_switching_lists() {
        let item = |t: &str, kind: &str, level: u64| json!({
            "_type": "block", "style": "normal", "listItem": kind, "level": level,
            "children": [span(t, &[])], "markDefs": []
        });
        let pt = json!([
            item("a", "bullet", 1), item("a1", "bullet", 2), item("b", "bullet", 1),
            item("one", "number", 1),
            {"_type": "block", "style": "normal", "children": [span("after", &[])], "markDefs": []}
        ]);
        let out = render(&pt, &opts(), None);
        assert_eq!(
            out.html,
            "<ul><li>a<ul><li>a1</li></ul></li><li>b</li></ul><ol><li>one</li></ol><p>after</p>"
        );
    }

    #[test]
    fn sanity_and_placeholder_images() {
        let mut o = opts();
        o.image_width = Some(1200);
        let pt = json!([
            {"_type": "image", "asset": {"_ref": "image-abc123-800x600-png"}, "alt": "Dash"},
            {"_type": "image", "_sanityAsset": "image@https://cdn.example/x.jpg", "alt": null}
        ]);
        let out = render(&pt, &o, None);
        assert_eq!(
            out.html,
            "<figure><img src=\"https://cdn.sanity.io/images/p1/production/abc123-800x600.png?w=1200&amp;auto=format\" alt=\"Dash\" loading=\"lazy\"></figure>\
             <figure><img src=\"https://cdn.example/x.jpg\" alt=\"\" loading=\"lazy\"></figure>"
        );
    }

    #[test]
    fn tables_and_dividers() {
        let pt = json!([
            {"_type": "table", "hasHeader": true, "rows": [{"cells": ["Plan", "Price"]}, {"cells": ["Pro", "$1"]}]},
            {"_type": "divider"}
        ]);
        let out = render(&pt, &opts(), None);
        assert_eq!(
            out.html,
            "<table><tr><th>Plan</th><th>Price</th></tr><tr><td>Pro</td><td>$1</td></tr></table><hr>"
        );
    }

    #[test]
    fn custom_blocks_use_templates_and_report_gaps() {
        let mut tera = Tera::default();
        tera.add_raw_template("pt/wistiaEmbed.html", "<div data-wistia=\"{{ block.videoId }}\"></div>")
            .unwrap();
        tera.add_raw_template("pt/callout.html", "<aside>{{ body_html | safe }}</aside>").unwrap();
        let pt = json!([
            {"_type": "wistiaEmbed", "videoId": "abc"},
            {"_type": "callout", "body": [{"_type": "block", "style": "normal", "children": [span("Note", &[])], "markDefs": []}]},
            {"_type": "unconvertedEmbed", "html": "<div>raw</div>"}
        ]);
        let out = render(&pt, &opts(), Some(&tera));
        assert_eq!(out.html, "<div data-wistia=\"abc\"></div><aside><p>Note</p></aside>");
        assert_eq!(out.unhandled, vec!["unconvertedEmbed".to_string()]);
    }
}

//! Portable Text (Sanity rich text) → HTML.
//!
//! Handles the standard block model (styles, lists with nesting, decorators,
//! link annotations), Sanity images, dividers, and tables. Every other block
//! type renders through a Tera template `pt/<_type>.html` with the block as
//! context; a type with no template is reported in `unhandled` and rendered
//! as nothing, so callers can fail loudly instead of shipping silent gaps.
//!
//! Built-ins can be overridden the same way: when the template set has one of
//! the names below, it renders that element; otherwise the engine default is
//! used, byte for byte. Values are raw (Tera autoescapes `.html` templates);
//! `*_html` values are already-rendered HTML, so print them with `| safe`.
//! Never use `| safe` on anything else: `href`, `src`, `alt`, `caption` and
//! `block` fields come from content and must stay escaped.
//!
//! URLs are checked before they reach any template. A link `href` must be
//! http(s), `mailto:`, `tel:`, a site path (`/x`, not `//x`), a `#fragment` or
//! a `?query` ([`safe_href`]); anything else renders as plain text and is
//! reported in `unhandled`. An image `src` must be http(s) or a site path
//! ([`safe_src`]); anything else drops the image and is reported.
//!
//! | Template | Renders | Context |
//! |---|---|---|
//! | `pt/image.html` | image block | `src`, `alt`, `caption` (or null), `width`, `height` (from the asset reference, or null), `block` |
//! | `pt/table.html` | table block | `rows` (array of arrays of cell strings), `has_header`, `block` |
//! | `pt/divider.html` | divider block | `block` |
//! | `pt/blockquote.html` | `blockquote` style | `text_html`, `block` |
//! | `pt/list.html` | one list (each nesting level) | `kind` (`listItem`, e.g. `bullet`, `number`), `tag` (`ul`/`ol`), `level` (1 = outermost), `items` (array of item HTML, nested lists included) |
//! | `pt/marks/link.html` | link annotation | `href`, `blank`, `text` (plain), `text_html` (inner HTML with decorators), `mark` (the mark definition) |
//!
//! A template that fails to render is reported in `unhandled` as
//! `"<name>: template error: …"`.

use serde_json::{json, Value};
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

/// Deepest nesting of custom blocks inside custom blocks (a callout in a
/// callout ...). Deeper content is dropped and reported.
pub const MAX_CUSTOM_DEPTH: usize = 32;

/// A link target safe to put in `href`: http(s), mailto, tel, a site path,
/// a fragment or a query. Returns the trimmed value, or `None` to drop it.
pub fn safe_href(href: &str) -> Option<&str> {
    let h = checked_url(href)?;
    if h.starts_with('#') || h.starts_with('?') || is_site_path(h) {
        return Some(h);
    }
    match url_scheme(h)?.as_str() {
        "http" | "https" | "mailto" | "tel" => Some(h),
        _ => None,
    }
}

/// An image source safe to put in `src`: http(s) or a site path.
pub fn safe_src(src: &str) -> Option<&str> {
    let s = checked_url(src)?;
    if is_site_path(s) {
        return Some(s);
    }
    match url_scheme(s)?.as_str() {
        "http" | "https" => Some(s),
        _ => None,
    }
}

/// Trim surrounding spaces; refuse empty values and any control character
/// (browsers strip tabs and newlines inside a scheme, hiding it from a check).
fn checked_url(u: &str) -> Option<&str> {
    let u = u.trim_matches(' ');
    (!u.is_empty() && !u.chars().any(|c| c.is_control())).then_some(u)
}

/// `/x` on this site; `//x` and `/\x` name another host.
fn is_site_path(u: &str) -> bool {
    u.starts_with('/') && !u.starts_with("//") && !u.starts_with("/\\")
}

/// The lowercase scheme of an absolute URL, when it has a valid one.
fn url_scheme(u: &str) -> Option<String> {
    let (scheme, _) = u.split_once(':')?;
    let mut chars = scheme.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then(|| scheme.to_ascii_lowercase())
}

pub fn render(blocks: &Value, opts: &PtOptions, tera: Option<&Tera>) -> PtOutput {
    let mut r = Renderer { opts, tera, unhandled: Vec::new(), depth: 0 };
    let html = r.blocks(blocks);
    PtOutput { html, unhandled: r.unhandled }
}

struct Renderer<'a> {
    opts: &'a PtOptions,
    tera: Option<&'a Tera>,
    unhandled: Vec<String>,
    /// Current custom-block nesting depth.
    depth: usize,
}

/// A list being built: entries are items, or a nested list that opened
/// before any item at its parent level.
struct List {
    tag: &'static str,
    kind: String,
    level: usize,
    entries: Vec<Entry>,
}

enum Entry {
    Item { html: String, children: Vec<List> },
    Bare(List),
}

impl<'a> Renderer<'a> {
    fn blocks(&mut self, blocks: &Value) -> String {
        let mut html = String::new();
        let Some(arr) = blocks.as_array() else {
            if !blocks.is_null() {
                self.unhandled.push("not an array of blocks".into());
            }
            return html;
        };
        let mut lists: Vec<List> = Vec::new();
        for block in arr {
            let ty = block.get("_type").and_then(Value::as_str).unwrap_or("");
            let list_item = block.get("listItem").and_then(Value::as_str);
            if let (true, Some(kind)) = (ty == "block", list_item) {
                let level = block.get("level").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
                let tag = if kind == "number" { "ol" } else { "ul" };
                let content = self.spans(block);
                self.open_list_item(&mut html, &mut lists, level, tag, kind, content);
                continue;
            }
            self.close_lists(&mut html, &mut lists, 0);
            match ty {
                "block" => {
                    let s = self.text_block(block);
                    html.push_str(&s);
                }
                "image" => match image_url(block, self.opts) {
                    Some(src) => match safe_src(&src) {
                        Some(src) => {
                            let s = self.image(block, src);
                            html.push_str(&s);
                        }
                        None => self.unhandled.push(format!("image: unsafe src `{src}`")),
                    },
                    None => self.unhandled.push("image: no asset reference".into()),
                },
                "divider" => {
                    let s = self
                        .with_override("divider", || json!({ "block": block }))
                        .unwrap_or_else(|| "<hr>".into());
                    html.push_str(&s);
                }
                "table" => {
                    let s = self.table(block);
                    html.push_str(&s);
                }
                other => {
                    let s = self.custom(other, block);
                    html.push_str(&s);
                }
            }
        }
        self.close_lists(&mut html, &mut lists, 0);
        html
    }

    /// Render `pt/<name>.html` when the template set has it. `None` means use
    /// the engine default; a template error is reported and renders nothing.
    fn with_override(&mut self, name: &str, ctx: impl FnOnce() -> Value) -> Option<String> {
        let tpl = format!("pt/{name}.html");
        let tera = self.tera.filter(|t| t.get_template_names().any(|n| n == tpl))?;
        let ctx = match TeraContext::from_value(ctx()) {
            Ok(c) => c,
            Err(e) => {
                self.unhandled.push(format!("{name}: template error: {e}"));
                return Some(String::new());
            }
        };
        match tera.render(&tpl, &ctx) {
            Ok(html) => Some(html),
            Err(e) => {
                self.unhandled.push(format!("{name}: template error: {e}"));
                Some(String::new())
            }
        }
    }

    fn open_list_item(
        &mut self,
        html: &mut String,
        lists: &mut Vec<List>,
        level: usize,
        tag: &'static str,
        kind: &str,
        content: String,
    ) {
        self.close_lists(html, lists, level);
        if lists.len() == level && lists.last().map(|l| l.tag) != Some(tag) {
            self.close_lists(html, lists, level - 1);
        }
        while lists.len() < level {
            let level = lists.len() + 1;
            lists.push(List { tag, kind: kind.to_string(), level, entries: Vec::new() });
        }
        let top = lists.last_mut().expect("list opened above");
        top.entries.push(Entry::Item { html: content, children: Vec::new() });
    }

    /// Close open lists until only `keep` remain. A closed nested list joins
    /// its parent's last item; the outermost one is rendered into `html`.
    fn close_lists(&mut self, html: &mut String, lists: &mut Vec<List>, keep: usize) {
        while lists.len() > keep {
            let l = lists.pop().expect("non-empty");
            match lists.last_mut() {
                Some(parent) => match parent.entries.last_mut() {
                    Some(Entry::Item { children, .. }) => children.push(l),
                    _ => parent.entries.push(Entry::Bare(l)),
                },
                None => {
                    let s = self.list(l);
                    html.push_str(&s);
                }
            }
        }
    }

    fn list(&mut self, l: List) -> String {
        // (html, bare): a bare nested list sits directly in its parent.
        let mut entries: Vec<(String, bool)> = Vec::new();
        for entry in l.entries {
            entries.push(match entry {
                Entry::Item { mut html, children } => {
                    for c in children {
                        html.push_str(&self.list(c));
                    }
                    (html, false)
                }
                Entry::Bare(nested) => (self.list(nested), true),
            });
        }
        let (tag, kind, level) = (l.tag, l.kind, l.level);
        let items: Vec<&str> = entries.iter().map(|(h, _)| h.as_str()).collect();
        let ctx = || json!({ "kind": kind, "tag": tag, "level": level, "items": items });
        if let Some(html) = self.with_override("list", ctx) {
            return html;
        }
        let mut html = format!("<{tag}>");
        for (item, bare) in &entries {
            if *bare {
                html.push_str(item);
            } else {
                let _ = write!(html, "<li>{item}</li>");
            }
        }
        let _ = write!(html, "</{tag}>");
        html
    }

    fn text_block(&mut self, block: &Value) -> String {
        let style = block.get("style").and_then(Value::as_str).unwrap_or("normal");
        let text_html = self.spans(block);
        if style == "blockquote" {
            if let Some(html) =
                self.with_override("blockquote", || json!({ "text_html": text_html, "block": block }))
            {
                return html;
            }
        }
        let tag = match style {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" => style,
            _ => "p",
        };
        format!("<{tag}>{text_html}</{tag}>")
    }

    fn spans(&mut self, block: &Value) -> String {
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
                        let raw = def.get("href").and_then(Value::as_str).unwrap_or("");
                        let Some(href) = safe_href(raw) else {
                            // Keep the text, drop the link.
                            self.unhandled.push(format!("link: unsafe href `{raw}`"));
                            continue;
                        };
                        let blank = def.get("blank").and_then(Value::as_bool).unwrap_or(false);
                        let ctx = || {
                            json!({
                                "href": href, "blank": blank, "text": text,
                                "text_html": inner, "mark": def,
                            })
                        };
                        inner = match self.with_override("marks/link", ctx) {
                            Some(h) => h,
                            None => {
                                let target = if blank { " target=\"_blank\" rel=\"noopener\"" } else { "" };
                                format!("<a href=\"{}\"{target}>{inner}</a>", esc(href))
                            }
                        };
                    }
                }
            }
            html.push_str(&inner);
        }
        html
    }

    fn image(&mut self, block: &Value, src: &str) -> String {
        let alt = block.get("alt").and_then(Value::as_str).unwrap_or("");
        let caption = block.get("caption").and_then(Value::as_str);
        let (width, height) = image_dims(block).unzip();
        let ctx = || {
            json!({
                "src": src, "alt": alt, "caption": caption,
                "width": width, "height": height, "block": block,
            })
        };
        if let Some(html) = self.with_override("image", ctx) {
            return html;
        }
        format!(
            "<figure><img src=\"{}\" alt=\"{}\" loading=\"lazy\">{}</figure>",
            esc(src),
            esc(alt),
            caption.map(|c| format!("<figcaption>{}</figcaption>", esc(c))).unwrap_or_default()
        )
    }

    fn table(&mut self, block: &Value) -> String {
        let header = block.get("hasHeader").and_then(Value::as_bool).unwrap_or(false);
        let rows: Vec<Vec<&str>> = block
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|row| {
                row.get("cells")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|c| c.as_str().unwrap_or(""))
                    .collect()
            })
            .collect();
        if let Some(html) =
            self.with_override("table", || json!({ "rows": rows, "has_header": header, "block": block }))
        {
            return html;
        }
        let mut html = String::from("<table>");
        for (i, row) in rows.iter().enumerate() {
            let cell_tag = if header && i == 0 { "th" } else { "td" };
            html.push_str("<tr>");
            for cell in row {
                let _ = write!(html, "<{cell_tag}>{}</{cell_tag}>", esc(cell));
            }
            html.push_str("</tr>");
        }
        html.push_str("</table>");
        html
    }

    /// Custom block: `pt/<_type>.html` gets the block as `block`, plus `<field>_html`
    /// for every field that is itself Portable Text (e.g. a callout body).
    fn custom(&mut self, ty: &str, block: &Value) -> String {
        let name = format!("pt/{ty}.html");
        let Some(tera) = self.tera.filter(|t| t.get_template_names().any(|n| n == name)) else {
            self.unhandled.push(ty.to_string());
            return String::new();
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
                    if self.depth >= MAX_CUSTOM_DEPTH {
                        self.unhandled.push(format!(
                            "{ty}: custom blocks nested deeper than {MAX_CUSTOM_DEPTH} levels"
                        ));
                        return String::new();
                    }
                    self.depth += 1;
                    let nested = self.blocks(v);
                    self.depth -= 1;
                    ctx.insert(format!("{k}_html"), &nested);
                }
            }
        }
        match tera.render(&name, &ctx) {
            Ok(html) => html,
            Err(e) => {
                self.unhandled.push(format!("{ty}: template error: {e}"));
                String::new()
            }
        }
    }
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

/// Pixel size encoded in a Sanity asset reference (`image-<id>-<w>x<h>-<ext>`).
fn image_dims(block: &Value) -> Option<(u32, u32)> {
    let r = block.pointer("/asset/_ref").and_then(Value::as_str)?;
    let (base, _ext) = r.strip_prefix("image-")?.rsplit_once('-')?;
    let (w, h) = base.rsplit_once('-')?.1.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
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
            "markDefs": [{"_type": "link", "_key": "l1", "href": "https://example.com/demo", "blank": true}],
            "children": [span("Book ", &["strong"]), span("a demo", &["strong", "l1"])]
        }]);
        let out = render(&pt, &opts(), None);
        assert_eq!(
            out.html,
            "<p><strong>Book </strong><a href=\"https://example.com/demo\" target=\"_blank\" rel=\"noopener\"><strong>a demo</strong></a></p>"
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

    fn tera_with(templates: &[(&str, &str)]) -> Tera {
        let mut tera = Tera::default();
        for (name, body) in templates {
            tera.add_raw_template(name, body).unwrap();
        }
        tera
    }

    fn para(children: Vec<Value>, mark_defs: Value) -> Value {
        json!({"_type": "block", "style": "normal", "children": children, "markDefs": mark_defs})
    }

    #[test]
    fn override_table() {
        let tera = tera_with(&[(
            "pt/table.html",
            "<table class=\"site-table\">{% for row in rows %}<tr>{% for c in row %}\
             {% if has_header and loop.first %}<th>{% else %}<td>{% endif %}{{ c }}{% endfor %}</tr>{% endfor %}</table>",
        )]);
        let pt = json!([{"_type": "table", "hasHeader": false, "rows": [{"cells": ["a", "b"]}]}]);
        let out = render(&pt, &opts(), Some(&tera));
        assert_eq!(out.html, "<table class=\"site-table\"><tr><td>a<td>b</tr></table>");
        assert!(out.unhandled.is_empty());
    }

    #[test]
    fn override_blockquote() {
        let tera = tera_with(&[("pt/blockquote.html", "<blockquote class=\"site-quote\">{{ text_html | safe }}</blockquote>")]);
        let pt = json!([
            {"_type": "block", "style": "blockquote", "children": [span("Said", &["em"])], "markDefs": []},
            {"_type": "block", "style": "h2", "children": [span("Heading", &[])], "markDefs": []}
        ]);
        let out = render(&pt, &opts(), Some(&tera));
        assert_eq!(out.html, "<blockquote class=\"site-quote\"><em>Said</em></blockquote><h2>Heading</h2>");
    }

    #[test]
    fn override_link_mark() {
        let tera = tera_with(&[(
            "pt/marks/link.html",
            "<a class=\"site-link\" href=\"{{ href }}\"{% if blank %} target=\"_blank\"{% endif %} title=\"{{ text }}\">{{ text_html | safe }}</a>",
        )]);
        let pt = json!([para(
            vec![span("Read ", &[]), span("more", &["strong", "l1"])],
            json!([{"_type": "link", "_key": "l1", "href": "https://example.com/a", "blank": true}])
        )]);
        let out = render(&pt, &opts(), Some(&tera));
        assert_eq!(
            out.html,
            "<p>Read <a class=\"site-link\" href=\"https:&#x2F;&#x2F;example.com&#x2F;a\" target=\"_blank\" title=\"more\"><strong>more</strong></a></p>"
        );
    }

    #[test]
    fn override_divider() {
        let tera = tera_with(&[("pt/divider.html", "<hr class=\"site-rule\">")]);
        let out = render(&json!([{"_type": "divider"}]), &opts(), Some(&tera));
        assert_eq!(out.html, "<hr class=\"site-rule\">");
    }

    #[test]
    fn override_image_with_caption_and_size() {
        let tera = tera_with(&[(
            "pt/image.html",
            "<figure class=\"site-figure\"><img src=\"{{ src }}\" alt=\"{{ alt }}\"\
             {% if width %} width=\"{{ width }}\" height=\"{{ height }}\"{% endif %}>\
             {% if caption %}<figcaption>{{ caption }}</figcaption>{% endif %}</figure>",
        )]);
        let pt = json!([
            {"_type": "image", "asset": {"_ref": "image-abc123-800x600-png"}, "alt": "Chart", "caption": "Q3 & Q4"},
            {"_type": "image", "_sanityAsset": "image@https://cdn.example.com/x.jpg"}
        ]);
        let out = render(&pt, &opts(), Some(&tera));
        assert_eq!(
            out.html,
            "<figure class=\"site-figure\"><img src=\"https:&#x2F;&#x2F;cdn.sanity.io&#x2F;images&#x2F;p1&#x2F;production&#x2F;abc123-800x600.png\" alt=\"Chart\" width=\"800\" height=\"600\"><figcaption>Q3 &amp; Q4</figcaption></figure>\
             <figure class=\"site-figure\"><img src=\"https:&#x2F;&#x2F;cdn.example.com&#x2F;x.jpg\" alt=\"\"></figure>"
        );
    }

    #[test]
    fn override_lists_nested() {
        let tera = tera_with(&[(
            "pt/list.html",
            "<{{ tag }} class=\"site-list site-list-{{ kind }} level-{{ level }}\">{% for i in items %}<li>{{ i | safe }}</li>{% endfor %}</{{ tag }}>",
        )]);
        let item = |t: &str, kind: &str, level: u64| json!({
            "_type": "block", "style": "normal", "listItem": kind, "level": level,
            "children": [span(t, &[])], "markDefs": []
        });
        let pt = json!([item("a", "bullet", 1), item("a1", "bullet", 2), item("one", "number", 1)]);
        let out = render(&pt, &opts(), Some(&tera));
        assert_eq!(
            out.html,
            "<ul class=\"site-list site-list-bullet level-1\"><li>a<ul class=\"site-list site-list-bullet level-2\"><li>a1</li></ul></li></ul>\
             <ol class=\"site-list site-list-number level-1\"><li>one</li></ol>"
        );
    }

    #[test]
    fn unrelated_templates_leave_defaults_unchanged() {
        // A template set with custom block templates but no built-in
        // overrides renders built-ins exactly as with no templates at all.
        let tera = tera_with(&[("pt/callout.html", "<aside></aside>")]);
        let item = |t: &str, level: u64| json!({
            "_type": "block", "style": "normal", "listItem": "bullet", "level": level,
            "children": [span(t, &[])], "markDefs": []
        });
        let pt = json!([
            {"_type": "block", "style": "blockquote", "children": [span("q", &[])], "markDefs": []},
            para(vec![span("x", &["l1"])], json!([{"_type": "link", "_key": "l1", "href": "https://example.com"}])),
            item("deep", 2), item("top", 1),
            {"_type": "image", "asset": {"_ref": "image-abc-10x20-jpg"}, "alt": "a", "caption": "c"},
            {"_type": "table", "hasHeader": true, "rows": [{"cells": ["h"]}, {"cells": ["v"]}]},
            {"_type": "divider"}
        ]);
        let with = render(&pt, &opts(), Some(&tera));
        let without = render(&pt, &opts(), None);
        assert_eq!(with.html, without.html);
        assert_eq!(
            without.html,
            "<blockquote>q</blockquote><p><a href=\"https://example.com\">x</a></p>\
             <ul><ul><li>deep</li></ul><li>top</li></ul>\
             <figure><img src=\"https://cdn.sanity.io/images/p1/production/abc-10x20.jpg\" alt=\"a\" loading=\"lazy\"><figcaption>c</figcaption></figure>\
             <table><tr><th>h</th></tr><tr><td>v</td></tr></table><hr>"
        );
    }

    #[test]
    fn link_href_allowlist() {
        for ok in [
            "https://example.com/a", "HTTP://example.com", "mailto:a@example.com",
            "tel:+15550100", "/events/speakers", "#top", "?page=2", " /padded ",
        ] {
            assert!(safe_href(ok).is_some(), "{ok}");
        }
        for bad in [
            "javascript:alert(1)", "JavaScript:alert(1)", "java\tscript:alert(1)",
            "data:text/html,x", "vbscript:x", "//example.com", "/\\example.com",
            "relative/page", "", "  ", "\njavascript:x",
        ] {
            assert!(safe_href(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn image_src_allowlist() {
        assert!(safe_src("https://cdn.example.com/x.png").is_some());
        assert!(safe_src("/media/x.png").is_some());
        for bad in ["data:image/png;base64,AAAA", "javascript:x", "//example.com/x.png", "mailto:a@example.com"] {
            assert!(safe_src(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn unsafe_link_renders_text_and_is_reported() {
        let tera = tera_with(&[("pt/marks/link.html", "<a href=\"{{ href }}\">{{ text_html | safe }}</a>")]);
        let pt = json!([para(
            vec![span("click", &["strong", "l1"]), span(" ok", &["l2"])],
            json!([
                {"_type": "link", "_key": "l1", "href": "javascript:alert(1)"},
                {"_type": "link", "_key": "l2", "href": "/fine"}
            ])
        )]);
        for t in [None, Some(&tera)] {
            let out = render(&pt, &opts(), t);
            assert!(!out.html.contains("javascript"), "{}", out.html);
            assert!(out.html.starts_with("<p><strong>click</strong><a href="), "{}", out.html);
            assert_eq!(out.unhandled, vec!["link: unsafe href `javascript:alert(1)`".to_string()]);
        }
    }

    #[test]
    fn unsafe_image_src_is_dropped_and_reported() {
        let tera = tera_with(&[("pt/image.html", "<img src=\"{{ src }}\">")]);
        let pt = json!([{"_type": "image", "_sanityAsset": "image@javascript:alert(1)"}]);
        for t in [None, Some(&tera)] {
            let out = render(&pt, &opts(), t);
            assert_eq!(out.html, "");
            assert_eq!(out.unhandled, vec!["image: unsafe src `javascript:alert(1)`".to_string()]);
        }
    }

    #[test]
    fn custom_block_nesting_is_capped() {
        let tera = tera_with(&[("pt/callout.html", "<aside>{{ body_html | safe }}</aside>")]);
        let mut pt = json!([para(vec![span("leaf", &[])], json!([]))]);
        for _ in 0..(MAX_CUSTOM_DEPTH + 5) {
            pt = json!([{"_type": "callout", "body": pt}]);
        }
        let out = render(&pt, &opts(), Some(&tera));
        assert!(!out.html.contains("leaf"));
        assert_eq!(out.unhandled.len(), 1, "{:?}", out.unhandled);
        assert!(out.unhandled[0].contains("nested deeper than 32 levels"), "{:?}", out.unhandled);

        let mut ok = json!([para(vec![span("leaf", &[])], json!([]))]);
        for _ in 0..MAX_CUSTOM_DEPTH {
            ok = json!([{"_type": "callout", "body": ok}]);
        }
        let out = render(&ok, &opts(), Some(&tera));
        assert!(out.html.contains("leaf") && out.unhandled.is_empty(), "{:?}", out.unhandled);
    }

    #[test]
    fn override_template_errors_are_reported() {
        let tera = tera_with(&[("pt/divider.html", "{{ missing.field }}")]);
        let out = render(&json!([{"_type": "divider"}]), &opts(), Some(&tera));
        assert_eq!(out.html, "");
        assert_eq!(out.unhandled.len(), 1);
        assert!(out.unhandled[0].starts_with("divider: template error"), "{:?}", out.unhandled);
    }
}

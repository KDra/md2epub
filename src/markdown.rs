//! Markdown rendering (owned by T2).
//!
//! Contract for [`render`]: `comrak` with GFM tables, strikethrough, task
//! lists, autolinks, footnotes, header ids, front matter delimiter `---`,
//! `math_dollars` (emits `<span data-math-style="inline|display">LATEX</span>`)
//! and raw HTML passed through. Code blocks are highlighted by `syntect`
//! with inline styles; unknown languages fall back to plain `<pre><code>`.
//! Fenced blocks tagged `mermaid` become `<pre class="mermaid">SOURCE</pre>`
//! (source HTML-escaped). The YAML front matter, without its `---`
//! delimiters, is returned in [`Rendered::front_matter`].
//!
//! Details:
//!
//! - Headings carry their id on the heading element itself
//!   (`<h2 id="tables">Tables</h2>`), without the empty permalink anchor
//!   comrak adds by default, which would be a useless empty link in an EPUB.
//! - Highlighting uses syntect's `InspiredGitHub` theme with every colour
//!   darkened so that it keeps strong contrast once an e-ink screen turns it
//!   into gray; bold and italic from the theme are kept, backgrounds are not
//!   emitted. Highlighted blocks are `<pre class="highlight"><code
//!   class="language-x">` with `<span style="...">` tokens.
//! - A panic inside comrak or syntect is caught and reported as an error, so
//!   bad input never brings the process down.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow};
use comrak::adapters::{
    CodefenceMeta, CodefenceRendererAdapter, HeadingAdapter, HeadingMeta, SyntaxHighlightMeta,
    SyntaxHighlighterAdapter,
};
use comrak::nodes::NodeValue;
use comrak::options::Plugins;
use comrak::{Anchorizer, Arena, Options, format_html_with_plugins, parse_document};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, FontStyle, Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::dom::escape_into;

/// Output of [`render`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rendered {
    /// HTML fragment (no `<html>`/`<body>` wrapper).
    pub html: String,
    /// Raw YAML front matter without the `---` lines, if present.
    pub front_matter: Option<String>,
}

/// Front matter delimiter recognised at the very start of a document.
const FRONT_MATTER_DELIMITER: &str = "---";

/// Code blocks larger than this are not highlighted (syntect is roughly
/// linear but slow, and huge generated listings gain nothing from colour).
const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;

/// Syntect theme used for highlighting. It is a light theme; colours are
/// darkened further by [`eink_color`].
const THEME_NAME: &str = "InspiredGitHub";

/// Highest gray level (0 = black, 1 = white) a token colour may have once
/// converted to grayscale. Keeps every token clearly legible on e-ink.
const MAX_GRAY: f32 = 0.42;

/// Renders Markdown to an HTML fragment. See the module docs.
pub fn render(src: &str) -> Result<Rendered> {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    catch_unwind(AssertUnwindSafe(|| render_inner(src)))
        .unwrap_or_else(|_| Err(anyhow!("the Markdown renderer crashed on this input")))
}

fn render_inner(src: &str) -> Result<Rendered> {
    let options = comrak_options();

    let headings = HeadingIds::default();
    let highlighter = Highlighter;
    let mermaid = MermaidFence;
    let mut plugins = Plugins::default();
    plugins.render.heading_adapter = Some(&headings);
    plugins.render.codefence_syntax_highlighter = Some(&highlighter);
    for tag in ["mermaid", "Mermaid", "MERMAID"] {
        plugins
            .render
            .codefence_renderers
            .insert(tag.to_string(), &mermaid);
    }

    let arena = Arena::new();
    let root = parse_document(&arena, src, &options);

    let front_matter = root.children().find_map(|node| match &node.data().value {
        NodeValue::FrontMatter(raw) => strip_front_matter_delimiters(raw),
        _ => None,
    });

    let mut html = String::new();
    format_html_with_plugins(root, &options, &mut html, &plugins)
        .map_err(|_| anyhow!("failed to format Markdown as HTML"))?;

    Ok(Rendered { html, front_matter })
}

/// The comrak options required by the contract.
fn comrak_options() -> Options<'static> {
    let mut o = Options::default();
    o.extension.strikethrough = true;
    o.extension.table = true;
    o.extension.autolink = true;
    o.extension.tasklist = true;
    o.extension.footnotes = true;
    // Ids are produced by `HeadingIds`; the prefix only switches the
    // feature on for the default renderer and is unused with the adapter.
    o.extension.header_id_prefix = Some(String::new());
    o.extension.front_matter_delimiter = Some(FRONT_MATTER_DELIMITER.to_string());
    o.extension.math_dollars = true;
    o.render.r#unsafe = true;
    o
}

/// Turns comrak's raw front matter (delimiters included, maybe followed by
/// blank lines) into the YAML between the delimiters. Blank front matter
/// yields `None`.
fn strip_front_matter_delimiters(raw: &str) -> Option<String> {
    // Drop the opening delimiter line.
    let after_open = raw.split_once('\n').map_or("", |(_, rest)| rest);
    // Drop trailing blank lines, then the closing delimiter line.
    let trimmed = after_open.trim_end_matches(['\n', '\r', ' ', '\t']);
    let body = match trimmed.rfind('\n') {
        Some(i) if trimmed[i + 1..].trim() == FRONT_MATTER_DELIMITER => &trimmed[..=i],
        None if trimmed.trim() == FRONT_MATTER_DELIMITER => "",
        _ => trimmed,
    };
    let body = body.replace("\r\n", "\n");
    if body.trim().is_empty() {
        None
    } else {
        Some(body)
    }
}

// ---------------------------------------------------------------------------
// Headings

/// Writes `<hN id="slug">` with GitHub-style unique slugs and no permalink
/// anchor.
#[derive(Default)]
struct HeadingIds {
    anchorizer: Mutex<Anchorizer>,
}

impl HeadingAdapter for HeadingIds {
    fn enter(&self, output: &mut dyn fmt::Write, heading: &HeadingMeta) -> fmt::Result {
        let id = {
            let mut anchorizer = self
                .anchorizer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Headings without letters or digits ("!!!", "") would get an
            // empty or punctuation-only slug; give them a readable one.
            if heading.content.chars().any(char::is_alphanumeric) {
                anchorizer.anchorize(&heading.content)
            } else {
                anchorizer.anchorize("section")
            }
        };
        let level = heading.level.clamp(1, 6);
        let mut tag = String::with_capacity(id.len() + 12);
        write!(tag, "<h{level} id=\"")?;
        escape_into(&id, true, &mut tag);
        tag.push_str("\">");
        output.write_str(&tag)
    }

    fn exit(&self, output: &mut dyn fmt::Write, heading: &HeadingMeta) -> fmt::Result {
        writeln!(output, "</h{}>", heading.level.clamp(1, 6))
    }
}

// ---------------------------------------------------------------------------
// Mermaid

/// Emits the Mermaid placeholder `<pre class="mermaid">SOURCE</pre>`.
struct MermaidFence;

impl CodefenceRendererAdapter for MermaidFence {
    fn write(&self, output: &mut dyn fmt::Write, codefence: &CodefenceMeta<'_>) -> fmt::Result {
        let mut out = String::with_capacity(codefence.code.len() + 32);
        out.push_str("<pre class=\"mermaid\">");
        escape_into(codefence.code, false, &mut out);
        out.push_str("</pre>\n");
        output.write_str(&out)
    }
}

// ---------------------------------------------------------------------------
// Syntax highlighting

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        ThemeSet::load_defaults()
            .themes
            .remove(THEME_NAME)
            .unwrap_or_default()
    })
}

/// Maps a fence info token to a syntect syntax. `None` for missing, unknown
/// or plain-text languages.
fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let token = lang
        .trim()
        .trim_start_matches(['{', '.'])
        .trim_end_matches('}')
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if token.is_empty() {
        return None;
    }
    let token = match token.as_str() {
        "shell" | "console" | "zsh" | "shell-session" | "terminal" => "bash",
        "c++" => "cpp",
        "golang" => "go",
        "yml" => "yaml",
        "rs" => "rust",
        "py3" | "python3" => "python",
        "js" | "javascript" | "node" => "js",
        "objective-c" | "objc" => "m",
        "csharp" | "c#" => "cs",
        "markdown" | "md" => "md",
        "text" | "txt" | "plain" | "plaintext" | "none" => return None,
        other => other,
    };
    let syntax = syntax_set().find_syntax_by_token(token)?;
    (syntax.name != "Plain Text").then_some(syntax)
}

/// Darkens a theme colour so its grayscale value is at most [`MAX_GRAY`].
fn eink_color(c: Color) -> (u8, u8, u8) {
    let (r, g, b) = (f32::from(c.r), f32::from(c.g), f32::from(c.b));
    // Rec. 601 luma, which is what most e-ink grayscale conversions use.
    let gray = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0;
    if gray <= MAX_GRAY {
        return (c.r, c.g, c.b);
    }
    let k = MAX_GRAY / gray;
    let scale = |v: f32| (v * k).round().clamp(0.0, 255.0) as u8;
    (scale(r), scale(g), scale(b))
}

/// Inline CSS for one token, or `None` when it would be the default style.
fn token_css(style: &Style, default_fg: (u8, u8, u8)) -> Option<String> {
    let mut css = String::new();
    let fg = eink_color(style.foreground);
    if fg != default_fg {
        let _ = write!(css, "color:#{:02x}{:02x}{:02x};", fg.0, fg.1, fg.2);
    }
    if style.font_style.contains(FontStyle::BOLD) {
        css.push_str("font-weight:bold;");
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        css.push_str("font-style:italic;");
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        css.push_str("text-decoration:underline;");
    }
    if css.is_empty() {
        None
    } else {
        css.pop(); // trailing ';'
        Some(css)
    }
}

/// Highlights `code` as inline-styled HTML. Errors from syntect (for example
/// a regex that fails on an odd line) make the caller fall back to plain text.
fn highlight(code: &str, syntax: &SyntaxReference) -> Result<String, syntect::Error> {
    let theme = theme();
    let default_fg = eink_color(theme.settings.foreground.unwrap_or(Color::BLACK));
    let mut lines = HighlightLines::new(syntax, theme);
    let mut out = String::with_capacity(code.len() * 3);
    for line in LinesWithEndings::from(code) {
        let regions = lines.highlight_line(line, syntax_set())?;
        // Merge neighbouring regions that end up with identical CSS.
        let mut pending: Option<(Option<String>, String)> = None;
        for (style, text) in regions {
            if text.is_empty() {
                continue;
            }
            let css = token_css(&style, default_fg);
            match &mut pending {
                Some((cur, buf)) if *cur == css => buf.push_str(text),
                _ => {
                    if let Some((cur, buf)) = pending.take() {
                        push_token(&mut out, cur.as_deref(), &buf);
                    }
                    pending = Some((css, text.to_string()));
                }
            }
        }
        if let Some((cur, buf)) = pending {
            push_token(&mut out, cur.as_deref(), &buf);
        }
    }
    Ok(out)
}

fn push_token(out: &mut String, css: Option<&str>, text: &str) {
    // Keep newlines outside the span so line structure is plain text.
    let (body, newline) = match text.strip_suffix('\n') {
        Some(b) => (b.strip_suffix('\r').unwrap_or(b), "\n"),
        None => (text, ""),
    };
    match css {
        Some(css) if !body.is_empty() => {
            out.push_str("<span style=\"");
            escape_into(css, true, out);
            out.push_str("\">");
            escape_into(body, false, out);
            out.push_str("</span>");
        }
        _ => escape_into(body, false, out),
    }
    out.push_str(newline);
}

/// Writes an opening tag with comrak's attributes, optionally adding a class.
fn write_tag(
    output: &mut dyn fmt::Write,
    name: &str,
    attributes: &BTreeMap<&str, Cow<'_, str>>,
    extra_class: Option<&str>,
) -> fmt::Result {
    let mut tag = String::from("<");
    tag.push_str(name);
    let mut class_written = false;
    for (key, value) in attributes {
        tag.push(' ');
        tag.push_str(key);
        tag.push_str("=\"");
        if *key == "class" {
            if let Some(extra) = extra_class {
                tag.push_str(extra);
                tag.push(' ');
            }
            class_written = true;
        }
        escape_into(value, true, &mut tag);
        tag.push('"');
    }
    if let (Some(extra), false) = (extra_class, class_written) {
        tag.push_str(" class=\"");
        tag.push_str(extra);
        tag.push('"');
    }
    tag.push('>');
    output.write_str(&tag)
}

/// Renders `code` as a standalone code block, highlighted like a fenced
/// block tagged `lang` (a fence info token such as `rs`, `py` or `toml`):
/// `<pre class="highlight"><code class="language-x">` when the language is
/// known, else a plain `<pre><code>`.
pub fn code_block(code: &str, lang: &str) -> String {
    let token: String = lang
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '#'))
        .collect::<String>()
        .to_ascii_lowercase();
    let syntax = (code.len() <= MAX_HIGHLIGHT_BYTES)
        .then(|| find_syntax(&token))
        .flatten();
    let highlighted = syntax.and_then(|syntax| {
        catch_unwind(AssertUnwindSafe(|| highlight(code, syntax)))
            .ok()
            .and_then(Result::ok)
    });
    let mut out = String::with_capacity(code.len() * 2 + 64);
    match highlighted {
        Some(html) => {
            out.push_str("<pre class=\"highlight\"><code class=\"language-");
            out.push_str(&token);
            out.push_str("\">");
            out.push_str(&html);
        }
        None => {
            out.push_str("<pre><code>");
            escape_into(code, false, &mut out);
        }
    }
    out.push_str("</code></pre>\n");
    out
}

/// syntect-based highlighter plugged into comrak.
struct Highlighter;

impl Highlighter {
    fn highlightable(meta: &SyntaxHighlightMeta<'_>) -> Option<&'static SyntaxReference> {
        if meta.code.len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        find_syntax(meta.lang)
    }
}

impl SyntaxHighlighterAdapter for Highlighter {
    fn write_highlighted(
        &self,
        output: &mut dyn fmt::Write,
        meta: &SyntaxHighlightMeta<'_>,
    ) -> fmt::Result {
        if let Some(syntax) = Self::highlightable(meta) {
            // A panic inside syntect only loses the colours of this block.
            if let Ok(Ok(html)) = catch_unwind(AssertUnwindSafe(|| highlight(meta.code, syntax))) {
                return output.write_str(&html);
            }
        }
        let mut plain = String::with_capacity(meta.code.len() + 16);
        escape_into(meta.code, false, &mut plain);
        output.write_str(&plain)
    }

    fn write_pre_tag(
        &self,
        output: &mut dyn fmt::Write,
        meta: &SyntaxHighlightMeta<'_>,
        attributes: BTreeMap<&str, Cow<'_, str>>,
    ) -> fmt::Result {
        let class = Self::highlightable(meta).map(|_| "highlight");
        write_tag(output, "pre", &attributes, class)
    }

    fn write_code_tag(
        &self,
        output: &mut dyn fmt::Write,
        _meta: &SyntaxHighlightMeta<'_>,
        attributes: BTreeMap<&str, Cow<'_, str>>,
    ) -> fmt::Result {
        write_tag(output, "code", &attributes, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(src: &str) -> String {
        render(src).unwrap().html
    }

    /// Parses the fragment as XML after the same DOM round trip the pipeline
    /// does, to prove the output survives into XHTML.
    fn assert_xhtml_ok(fragment: &str) {
        let doc = crate::dom::parse(fragment);
        let body = crate::dom::body(&doc).expect("body");
        let xhtml = crate::dom::serialize_xhtml(&body);
        roxmltree::Document::parse(&xhtml).unwrap_or_else(|e| panic!("{e}\n{xhtml}"));
    }

    #[test]
    fn table() {
        let h = html("| a | b |\n|---|:-:|\n| 1 | 2 |\n");
        assert!(h.contains("<table>"), "{h}");
        assert!(h.contains("<th>a</th>"), "{h}");
        assert!(h.contains("<td align=\"center\">2</td>"), "{h}");
    }

    #[test]
    fn strikethrough_autolink_tasklist() {
        let h = html("~~gone~~ https://example.org\n\n- [x] done\n- [ ] todo\n");
        assert!(h.contains("<del>gone</del>"), "{h}");
        assert!(
            h.contains("<a href=\"https://example.org\">https://example.org</a>"),
            "{h}"
        );
        assert!(h.contains("type=\"checkbox\""), "{h}");
        assert!(h.contains("checked=\"\""), "{h}");
    }

    #[test]
    fn footnotes() {
        let h = html("Text.[^1]\n\n[^1]: The note.\n");
        assert!(h.contains("class=\"footnote-ref\""), "{h}");
        assert!(h.contains("href=\"#fn-1\""), "{h}");
        assert!(h.contains("class=\"footnotes\""), "{h}");
        assert!(h.contains("id=\"fn-1\""), "{h}");
        assert!(h.contains("The note."), "{h}");
    }

    #[test]
    fn heading_ids_on_heading_unique_and_no_anchor() {
        let h = html("# Kitchen Sink\n\n## Tables\n\n## Tables\n\n### **Bold** `x`\n\n## !!!\n");
        assert!(
            h.contains("<h1 id=\"kitchen-sink\">Kitchen Sink</h1>"),
            "{h}"
        );
        assert!(h.contains("<h2 id=\"tables\">Tables</h2>"), "{h}");
        assert!(h.contains("<h2 id=\"tables-1\">Tables</h2>"), "{h}");
        assert!(h.contains("<h3 id=\"bold-x\">"), "{h}");
        assert!(h.contains("<h2 id=\"section\">!!!</h2>"), "{h}");
        assert!(!h.contains("class=\"anchor\""), "{h}");
    }

    #[test]
    fn raw_html_passes_through() {
        let src = "<div class=\"callout\" style=\"border: 1px solid\">\n<p>Raw <abbr title=\"x\">HTML</abbr></p>\n</div>\n\nInline <sup>up</sup> and <svg width=\"1\" height=\"1\"><rect width=\"1\" height=\"1\"/></svg>.\n";
        let h = html(src);
        assert!(
            h.contains("<div class=\"callout\" style=\"border: 1px solid\">"),
            "{h}"
        );
        assert!(h.contains("<abbr title=\"x\">HTML</abbr>"), "{h}");
        assert!(h.contains("<sup>up</sup>"), "{h}");
        assert!(h.contains("<rect width=\"1\" height=\"1\"/>"), "{h}");
    }

    #[test]
    fn text_is_escaped() {
        let h = html("a & b < c\n");
        assert!(h.contains("a &amp; b &lt; c"), "{h}");
    }

    #[test]
    fn front_matter_extraction() {
        let r = render("---\ntitle: X\nauthor: Y\n---\n# Hi & bye\n").unwrap();
        assert_eq!(r.front_matter.as_deref(), Some("title: X\nauthor: Y\n"));
        assert!(
            r.html.contains("<h1 id=\"hi--bye\">Hi &amp; bye</h1>"),
            "{}",
            r.html
        );
        assert!(!r.html.contains("title: X"), "{}", r.html);

        // Blank lines after the closing delimiter, CRLF and a BOM.
        let r = render("\u{feff}---\r\ntitle: X\r\n---\r\n\r\n\r\nBody\r\n").unwrap();
        assert_eq!(r.front_matter.as_deref(), Some("title: X\n"));
        assert!(r.html.contains("<p>Body</p>"), "{}", r.html);

        let r = render("no front matter").unwrap();
        assert!(r.front_matter.is_none());
        assert_eq!(r.html, "<p>no front matter</p>\n");

        // A thematic break later on is not front matter.
        let r = render("Intro\n\n---\n\nmore\n").unwrap();
        assert!(r.front_matter.is_none());
        assert!(r.html.contains("<hr />"), "{}", r.html);

        // Empty front matter.
        let r = render("---\n---\nBody\n").unwrap();
        assert!(r.front_matter.is_none());
        assert!(r.html.contains("Body"), "{}", r.html);
    }

    #[test]
    fn strip_delimiters_helper() {
        assert_eq!(
            strip_front_matter_delimiters("---\na: 1\n---\n\n").as_deref(),
            Some("a: 1\n")
        );
        assert_eq!(strip_front_matter_delimiters("---\n---\n"), None);
        assert_eq!(strip_front_matter_delimiters(""), None);
    }

    #[test]
    fn inline_math_placeholder() {
        let h = html("Energy $E = mc^2$ and $a<b$.\n");
        assert!(
            h.contains("<span data-math-style=\"inline\">E = mc^2</span>"),
            "{h}"
        );
        assert!(
            h.contains("<span data-math-style=\"inline\">a&lt;b</span>"),
            "{h}"
        );
    }

    #[test]
    fn display_math_placeholder() {
        let h = html("$$\n\\sum_{i=0}^{n} i = \\frac{n(n+1)}{2}\n$$\n");
        assert!(h.contains("<span data-math-style=\"display\">"), "{h}");
        assert!(h.contains("\\sum_{i=0}^{n} i = \\frac{n(n+1)}{2}"), "{h}");
        let h = html("Inline display $$x^2$$ here.\n");
        assert!(
            h.contains("<span data-math-style=\"display\">x^2</span>"),
            "{h}"
        );
    }

    #[test]
    fn dollar_amounts_are_not_math() {
        let h = html("It costs $5 and $10.\n");
        assert!(!h.contains("data-math-style"), "{h}");
    }

    #[test]
    fn mermaid_placeholder() {
        let h = html("```mermaid\nflowchart TD\n  A[<b>x</b>] --> B & C\n```\n");
        assert!(
            h.contains(
                "<pre class=\"mermaid\">flowchart TD\n  A[&lt;b&gt;x&lt;/b&gt;] --&gt; B &amp; C\n</pre>"
            ),
            "{h}"
        );
        assert!(!h.contains("<code"), "{h}");
        assert!(!h.contains("<span"), "{h}");
    }

    #[test]
    fn highlighted_code() {
        let h = html("```rust\nfn main() { let s = \"<hi>\"; }\n```\n");
        assert!(
            h.starts_with("<pre class=\"highlight\"><code class=\"language-rust\">"),
            "{h}"
        );
        assert!(h.contains("<span style=\""), "{h}");
        assert!(h.contains("&lt;hi&gt;"), "{h}");
        assert!(h.contains("color:#"), "{h}");
        assert!(!h.contains("background"), "{h}");
        assert!(h.trim_end().ends_with("</code></pre>"), "{h}");

        let h = html("```python\ndef f(n):\n    return n  # comment\n```\n");
        assert!(h.contains("<span style=\""), "{h}");
        assert!(h.contains("language-python"), "{h}");

        // Aliases and attribute-style info strings.
        assert!(html("```sh\nls -la\n```\n").contains("class=\"highlight\""));
        assert!(html("```shell\nls -la\n```\n").contains("class=\"highlight\""));
        assert!(html("```{.rust}\nfn x() {}\n```\n").contains("class=\"highlight\""));
        assert!(html("```RUST\nfn x() {}\n```\n").contains("class=\"highlight\""));
    }

    #[test]
    fn highlight_colors_are_dark() {
        let h = html("```rust\n// a comment\nfn main() { let s = \"str\"; 42 }\n```\n");
        let mut seen = 0;
        for part in h.split("color:#").skip(1) {
            let hex = &part[..6];
            let v = u32::from_str_radix(hex, 16).unwrap();
            let (r, g, b) = ((v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff);
            let gray = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
            assert!(gray <= MAX_GRAY + 0.01, "{hex} too light");
            seen += 1;
        }
        assert!(seen > 0, "{h}");
    }

    #[test]
    fn eink_color_darkens_light_colors_only() {
        let light = Color {
            r: 0x96,
            g: 0x98,
            b: 0x96,
            a: 0xff,
        };
        let (r, g, b) = eink_color(light);
        assert!(r < 0x96 && g < 0x98 && b < 0x96);
        let dark = Color {
            r: 0x18,
            g: 0x36,
            b: 0x91,
            a: 0xff,
        };
        assert_eq!(eink_color(dark), (0x18, 0x36, 0x91));
        assert_eq!(eink_color(Color::WHITE).0, eink_color(Color::WHITE).1);
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let h = html("```nosuchlang\na < b\n```\n");
        assert_eq!(
            h,
            "<pre><code class=\"language-nosuchlang\">a &lt; b\n</code></pre>\n"
        );
        let h = html("```\nplain & simple\n```\n");
        assert_eq!(h, "<pre><code>plain &amp; simple\n</code></pre>\n");
        let h = html("```text\nplain\n```\n");
        assert_eq!(
            h,
            "<pre><code class=\"language-text\">plain\n</code></pre>\n"
        );
        let h = html("    indented <code>\n");
        assert_eq!(h, "<pre><code>indented &lt;code&gt;\n</code></pre>\n");
    }

    #[test]
    fn huge_block_is_not_highlighted() {
        let code = "let x = 1;\n".repeat(MAX_HIGHLIGHT_BYTES / 10);
        let h = html(&format!("```rust\n{code}```\n"));
        assert!(!h.contains("<span"));
        assert!(h.starts_with("<pre><code class=\"language-rust\">"));
    }

    #[test]
    fn garbage_input_does_not_panic() {
        let inputs = [
            "",
            "---",
            "---\n",
            "$$",
            "$",
            "```",
            "```rust",
            "[^",
            "[^1]:",
            "|",
            "| a |\n|",
            "<div>",
            "</div></div>",
            "\0\u{1}\u{feff}",
            "# \n## \n###### x ######",
            "``` mermaid\n",
            "```rust\n\"unterminated\n```",
        ];
        for input in inputs {
            render(input).unwrap();
        }
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&"> ".repeat(i % 30));
            s.push_str("- [ ] *_~`$<[");
            s.push('\n');
        }
        render(&s).unwrap();
    }

    #[test]
    fn kitchen_sink_fixture_renders_to_valid_xhtml() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/kitchen-sink.md"
        ))
        .unwrap();
        let r = render(&src).unwrap();
        let fm = r.front_matter.as_deref().unwrap();
        assert!(fm.contains("title: Kitchen Sink"), "{fm}");
        let h = &r.html;
        assert!(h.contains("<h2 id=\"tables\">"), "{h}");
        assert!(h.contains("<pre class=\"mermaid\">flowchart TD"), "{h}");
        assert!(h.contains("language-rust"), "{h}");
        assert!(h.contains("language-python"), "{h}");
        assert!(h.contains("data-math-style=\"inline\""), "{h}");
        assert!(h.contains("data-math-style=\"display\""), "{h}");
        assert!(h.contains("<svg"), "{h}");
        assert!(h.contains("<del>strikethrough</del>"), "{h}");
        assert_xhtml_ok(h);
    }

    #[test]
    fn standalone_code_blocks() {
        let h = code_block("fn main() { let x = 1 < 2; }\n", "rs");
        assert!(
            h.starts_with("<pre class=\"highlight\"><code class=\"language-rs\">"),
            "{h}"
        );
        assert!(h.contains("<span style="), "{h}");
        assert_xhtml_ok(&h);
        let plain = code_block("a < b & c", "txt");
        assert_eq!(plain, "<pre><code>a &lt; b &amp; c</code></pre>\n");
        let unknown = code_block("x", "no-such-language\"><script>");
        assert!(unknown.starts_with("<pre><code>"), "{unknown}");
        let big = "x\n".repeat(MAX_HIGHLIGHT_BYTES);
        assert!(code_block(&big, "rs").starts_with("<pre><code>"));
    }
}

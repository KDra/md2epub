//! HTML sanitizing, chapter splitting and link fixing (owned by T3).
//!
//! Contract:
//! - [`sanitize`]: for full HTML documents keep only `<body>` content (the
//!   `<title>` becomes `ctx.title_hint` when none is set yet, and `<style>`
//!   blocks from `<head>` move to the start of `<body>`); remove `script`,
//!   `iframe`, `form`, `object`, `embed`, `video`, `audio`, `link`, `meta`
//!   (and similar active or head-only elements), event-handler attributes and
//!   `javascript:` links; rename obsolete presentational elements (`center`,
//!   `font`, `tt`, ...) to valid ones, unwrap unknown elements, and drop
//!   attributes invalid in EPUB XHTML (presentational ones such as `align`
//!   or table `width` become inline CSS); keep `style` attributes and
//!   `<style>` contents. Every element `id` becomes a valid XML name unique
//!   across the book (`ctx.unique_id`), every heading gets one, and `#id`
//!   links inside the document follow renamed ids. A renamed element keeps
//!   its original id in [`ORIG_ID_ATTR`] until [`fix_links`] has used it.
//! - [`split`]: one chapter per input file, further split before each
//!   heading of level `<= ctx.opts.split_level` (headings nested in `div`,
//!   `section`, `article`, ... split their containers too). Content before
//!   the first split heading becomes its own chapter only when it holds real
//!   content. Fills `title`, `headings`, `file_name`
//!   (`ctx.next_chapter_file()`), `source` (`ctx.current_input`) and
//!   `body_xhtml`. The document's `<style>` elements are repeated at the
//!   start of every chapter body cut from it (the chapter wrapper may hoist
//!   them into `<head>`).
//! - [`fix_links`]: after all inputs are split, rewrite `#id` links that
//!   point into another chapter to `chNNN.xhtml#id`, and links between input
//!   files (`other.md#x`, `other.html`) to the matching chapter. Internal
//!   links whose target does not exist in the book lose their `href`, so the
//!   book never contains dangling references; [`fix_links_with_warnings`]
//!   reports those. Linked local files that `crate::linked` added are
//!   chapters with their own `source`, so links to them map the same way;
//!   [`fix_links_full`] explains why other linked files were left out.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::Result;
use markup5ever_rcdom::{Node, NodeData};

use crate::ctx::Ctx;
use crate::dom::{self, Handle, XHTML_NS};
use crate::model::{Chapter, Heading};

/// Marker attribute holding an element's original `id` when [`sanitize`]
/// had to change it. [`fix_links`] uses it to resolve links from other
/// files and then strips it.
pub const ORIG_ID_ATTR: &str = "data-md2epub-orig-id";

// ---------------------------------------------------------------------------
// Element and attribute policy
// ---------------------------------------------------------------------------

/// Elements removed together with their content.
const REMOVED_ELEMENTS: &[&str] = &[
    "script", "noscript", "iframe", "frame", "frameset", "noframes", "form", "object", "embed",
    "applet", "video", "audio", "link", "meta", "base", "title", "head", "template", "datalist",
    "param", "track", "keygen", "isindex", "basefont", "bgsound", "portal", "source", "menuitem",
];

/// HTML elements that are valid in EPUB 3 XHTML content and kept as they are.
const KNOWN_ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "area",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "blockquote",
    "br",
    "button",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "main",
    "map",
    "mark",
    "menu",
    "meter",
    "nav",
    "ol",
    "optgroup",
    "option",
    "output",
    "p",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "select",
    "small",
    "span",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "u",
    "ul",
    "var",
    "wbr",
];

/// Obsolete elements renamed to a valid element plus inline CSS.
const RENAMED_ELEMENTS: &[(&str, &str, &str)] = &[
    ("center", "div", "text-align: center"),
    ("font", "span", ""),
    ("big", "span", "font-size: larger"),
    ("tt", "code", ""),
    ("strike", "s", ""),
    ("acronym", "abbr", ""),
    ("marquee", "span", ""),
    ("blink", "span", ""),
    ("nobr", "span", "white-space: nowrap"),
    ("listing", "pre", ""),
    ("xmp", "pre", ""),
    ("plaintext", "pre", ""),
    ("dir", "ul", ""),
];

/// Attributes allowed on every HTML element (besides `data-*` and `aria-*`).
const GLOBAL_ATTRS: &[&str] = &[
    "id",
    "class",
    "style",
    "title",
    "lang",
    "xml:lang",
    "dir",
    "hidden",
    "role",
    "translate",
];

/// Element-specific attributes kept (validated further in [`attr_fate`]).
fn element_attrs(tag: &str) -> &'static [&'static str] {
    match tag {
        "a" => &["href", "hreflang", "type", "rel"],
        "area" => &["href", "alt", "shape", "coords", "hreflang", "type", "rel"],
        "img" => &["src", "alt", "width", "height"],
        "ol" => &["start", "reversed", "type"],
        "li" => &["value"],
        "td" => &["colspan", "rowspan", "headers"],
        // `abbr` is not allowed on `th` by the EPUB XHTML schema.
        "th" => &["colspan", "rowspan", "headers", "scope"],
        "col" | "colgroup" => &["span"],
        "blockquote" | "q" => &["cite"],
        "del" | "ins" => &["cite", "datetime"],
        "time" => &["datetime"],
        "data" => &["value"],
        "map" => &["name"],
        "details" => &["open"],
        "style" => &["media"],
        "input" => &["type", "checked", "disabled", "value", "name", "readonly"],
        "button" => &["type", "disabled", "name", "value"],
        "select" => &["disabled", "multiple", "name", "size"],
        "option" => &["value", "selected", "disabled", "label"],
        "optgroup" => &["label", "disabled"],
        "textarea" => &["rows", "cols", "readonly", "disabled", "name"],
        "label" => &["for"],
        "output" => &["for", "name"],
        "fieldset" => &["disabled", "name"],
        "meter" => &["value", "min", "max", "low", "high", "optimum"],
        "progress" => &["value", "max"],
        _ => &[],
    }
}

/// Attributes that carry a URL and must not use a script scheme.
const URL_ATTRS: &[&str] = &[
    "href",
    "src",
    "cite",
    "action",
    "formaction",
    "poster",
    "background",
    "longdesc",
    "data",
    "codebase",
    "xlink:href",
];

/// Containers a chapter split may cut through.
const SPLIT_CONTAINERS: &[&str] = &[
    "div", "section", "article", "main", "header", "footer", "hgroup", "aside", "nav",
];

/// What happens to an HTML element.
enum Action {
    Keep,
    Remove,
    Unwrap,
    Rename(&'static str, &'static str),
}

fn action_for(tag: &str) -> Action {
    if REMOVED_ELEMENTS.contains(&tag) {
        return Action::Remove;
    }
    if KNOWN_ELEMENTS.contains(&tag) {
        return Action::Keep;
    }
    if let Some((_, to, css)) = RENAMED_ELEMENTS.iter().find(|(from, _, _)| *from == tag) {
        return Action::Rename(to, css);
    }
    // svg and math are foreign roots, handled by the caller; html/body only
    // appear at the top. Anything else (custom elements, `picture`,
    // `canvas`, `dialog`, `slot`, ...) keeps its content but loses the tag.
    Action::Unwrap
}

/// What happens to one attribute.
enum AttrFate {
    Keep,
    Drop,
    Set(String),
    Style(String),
}

/// Whether a URL uses a scheme that runs code.
fn is_script_url(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect::<String>()
        .to_ascii_lowercase();
    compact.starts_with("javascript:")
        || compact.starts_with("vbscript:")
        || compact.starts_with("livescript:")
}

fn is_integer(v: &str, allow_negative: bool) -> bool {
    let v = v.trim();
    let digits = if allow_negative {
        v.strip_prefix('-').unwrap_or(v)
    } else {
        v
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Converts a legacy length (`200`, `200px`, `50%`) to CSS.
fn css_length(v: &str) -> Option<String> {
    let v = v.trim();
    let number = |s: &str| !s.is_empty() && s.parse::<f32>().is_ok_and(|n| n >= 0.0);
    if let Some(n) = v.strip_suffix('%') {
        return number(n.trim()).then(|| format!("{}%", n.trim()));
    }
    let n = v.strip_suffix("px").unwrap_or(v).trim();
    number(n).then(|| format!("{n}px"))
}

fn attr_fate(tag: &str, name: &str, value: &str) -> AttrFate {
    let v = value.trim();
    if name.starts_with("on") || name.starts_with("xmlns") || name == ORIG_ID_ATTR {
        return AttrFate::Drop;
    }
    if URL_ATTRS.contains(&name) && is_script_url(value) {
        return AttrFate::Drop;
    }
    if name.starts_with("data-") || name.starts_with("aria-") {
        return AttrFate::Keep;
    }
    match name {
        "id" => {
            return if v.is_empty() {
                AttrFate::Drop
            } else {
                AttrFate::Set(v.to_string())
            };
        }
        "style" => {
            let lower = value.to_ascii_lowercase();
            return if lower.contains("expression(") || is_script_url_in_css(&lower) {
                AttrFate::Drop
            } else {
                AttrFate::Keep
            };
        }
        "dir" => {
            return match v.to_ascii_lowercase().as_str() {
                d @ ("ltr" | "rtl" | "auto") => AttrFate::Set(d.to_string()),
                _ => AttrFate::Drop,
            };
        }
        "lang" | "xml:lang" | "class" | "role" | "epub:type" if v.is_empty() => {
            return AttrFate::Drop;
        }
        "epub:type" => return AttrFate::Keep,
        _ => {}
    }
    if GLOBAL_ATTRS.contains(&name) {
        return AttrFate::Keep;
    }
    // Presentational attributes that have a CSS equivalent.
    match (tag, name) {
        ("img", "align") => {
            return match v.to_ascii_lowercase().as_str() {
                "left" | "right" => AttrFate::Style(format!("float: {}", v.to_ascii_lowercase())),
                "middle" | "center" => AttrFate::Style("vertical-align: middle".into()),
                "top" | "texttop" => AttrFate::Style("vertical-align: top".into()),
                "bottom" | "baseline" | "absbottom" | "absmiddle" => {
                    AttrFate::Style("vertical-align: bottom".into())
                }
                _ => AttrFate::Drop,
            };
        }
        ("img", "width" | "height") => {
            return if is_integer(v, false) {
                AttrFate::Set(v.to_string())
            } else {
                css_length(v)
                    .map(|l| AttrFate::Style(format!("{name}: {l}")))
                    .unwrap_or(AttrFate::Drop)
            };
        }
        (_, "align") => {
            return match v.to_ascii_lowercase().as_str() {
                a @ ("left" | "right" | "center" | "justify") => {
                    AttrFate::Style(format!("text-align: {a}"))
                }
                "middle" => AttrFate::Style("text-align: center".into()),
                _ => AttrFate::Drop,
            };
        }
        (_, "valign") => {
            return match v.to_ascii_lowercase().as_str() {
                a @ ("top" | "middle" | "bottom" | "baseline") => {
                    AttrFate::Style(format!("vertical-align: {a}"))
                }
                _ => AttrFate::Drop,
            };
        }
        ("table" | "td" | "th" | "col" | "colgroup" | "hr" | "div" | "pre", "width") => {
            return css_length(v)
                .map(|l| AttrFate::Style(format!("width: {l}")))
                .unwrap_or(AttrFate::Drop);
        }
        ("td" | "th", "nowrap") => return AttrFate::Style("white-space: nowrap".into()),
        _ => {}
    }
    if !element_attrs(tag).contains(&name) {
        return AttrFate::Drop;
    }
    // Light validation of typed values.
    match name {
        "start" | "value" if tag == "ol" || tag == "li" => {
            if is_integer(v, true) {
                AttrFate::Set(v.to_string())
            } else {
                AttrFate::Drop
            }
        }
        "colspan" | "rowspan" | "span" => {
            if is_integer(v, false) && (name == "rowspan" || v.parse::<u32>().unwrap_or(0) > 0) {
                AttrFate::Set(v.to_string())
            } else {
                AttrFate::Drop
            }
        }
        "type" if tag == "ol" => {
            if ["1", "a", "A", "i", "I"].contains(&v) {
                AttrFate::Set(v.to_string())
            } else {
                AttrFate::Drop
            }
        }
        "href" if v.is_empty() => AttrFate::Drop,
        "href" | "src" | "cite" => {
            let encoded = encode_url(value);
            if encoded == value {
                AttrFate::Keep
            } else {
                AttrFate::Set(encoded)
            }
        }
        _ => AttrFate::Keep,
    }
}

/// Percent-encodes the characters a URL may not contain literally (spaces,
/// quotes, `<`, `>`, `\`, `^`, `` ` ``, `{`, `|`, `}`, controls) and a `%`
/// that does not start an escape. Leading and trailing whitespace is
/// removed. Non-ASCII characters are allowed (IRIs) and kept.
pub fn encode_url(value: &str) -> String {
    let v = value.trim_matches(|c: char| c.is_ascii_whitespace());
    let b = v.as_bytes();
    let mut out = String::with_capacity(v.len());
    for (i, c) in v.char_indices() {
        let escape = match c {
            ' ' | '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}' => true,
            '%' => {
                !(b.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                    && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit))
            }
            c => c.is_control(),
        };
        if escape {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_script_url_in_css(lower_css: &str) -> bool {
    let compact: String = lower_css.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("javascript:") || compact.contains("vbscript:")
}

// ---------------------------------------------------------------------------
// Sanitizing
// ---------------------------------------------------------------------------

/// Cleans the parsed document in place. See the module docs.
pub fn sanitize(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    take_title_hint(doc, ctx);
    take_lang(doc, ctx);
    let body = dom::body(doc).unwrap_or_else(|| doc.clone());
    hoist_head(doc, &body, ctx);
    clean_children(&body, ctx);
    assign_ids(&body, ctx);
    footnote_semantics(&body);
    if ctx.opts.profile.flatten_alpha {
        filter_device_css(&body, ctx);
    }
    Ok(())
}

/// Records the document's `<html lang>` (normalized, when it is a valid
/// tag) as the input language, and makes it the book language when it comes
/// from the first input and no language was configured.
fn take_lang(doc: &Handle, ctx: &mut Ctx) {
    let lang = dom::first_by_tag(doc, "html")
        .and_then(|h| dom::get_attr(&h, "lang").or_else(|| dom::get_attr(&h, "xml:lang")))
        .filter(|l| !l.trim().is_empty());
    let lang = match lang {
        Some(raw) => {
            let norm = crate::config::normalize_lang(&raw);
            if norm.is_none() {
                ctx.warn(format!(
                    "{}: invalid language tag '{}' on <html> ignored",
                    ctx.current_input.display(),
                    raw.trim()
                ));
            }
            norm
        }
        None => None,
    };
    if let Some(l) = &lang
        && !ctx.opts.lang_given
        && ctx.chapter_count == 0
    {
        ctx.opts.meta.lang = l.clone();
        ctx.opts.lang_given = true;
    }
    ctx.input_lang = lang;
}

/// Adds EPUB and DPUB-ARIA semantics to comrak's footnote markup, so
/// readers can show notes as pop-ups: `epub:type="noteref"` on references,
/// `endnotes`/`endnote` on the notes section and its items, `doc-backlink`
/// on the back references (whose arrow gets U+FE0E so it renders as text).
fn footnote_semantics(root: &Handle) {
    for el in html_elements(root) {
        if dom::is_tag(&el, "a") {
            if dom::has_attr(&el, "data-footnote-ref") {
                dom::set_attr(&el, "epub:type", "noteref");
                dom::set_attr(&el, "role", "doc-noteref");
            } else if dom::has_attr(&el, "data-footnote-backref")
                || dom::has_class(&el, "footnote-backref")
            {
                dom::set_attr(&el, "role", "doc-backlink");
                for t in dom::descendants(&el) {
                    if dom::is_text(&t) {
                        let text = dom::text_content(&t);
                        if text.contains('\u{21A9}') && !text.contains("\u{21A9}\u{FE0E}") {
                            let new = text.replace('\u{21A9}', "\u{21A9}\u{FE0E}");
                            dom::replace_with(&t, &dom::create_text(&new));
                        }
                    }
                }
            }
        } else if dom::is_tag(&el, "section")
            && (dom::has_class(&el, "footnotes") || dom::has_attr(&el, "data-footnotes"))
        {
            dom::set_attr(&el, "epub:type", "endnotes");
            dom::set_attr(&el, "role", "doc-endnotes");
            for list in dom::children(&el) {
                if !(dom::is_tag(&list, "ol") || dom::is_tag(&list, "ul")) {
                    continue;
                }
                for li in dom::children(&list) {
                    if dom::is_tag(&li, "li") {
                        dom::set_attr(&li, "epub:type", "endnote");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Device CSS
// ---------------------------------------------------------------------------

/// Removes CSS that e-ink readers lay out badly or not at all (flex and grid
/// layout, fixed/sticky positioning, viewport units, opacity, filters and
/// blend modes) from `style` attributes and `<style>` elements, and warns
/// once with the number of declarations removed.
fn filter_device_css(root: &Handle, ctx: &mut Ctx) {
    let mut dropped = 0;
    for el in html_elements(root) {
        if dom::is_tag(&el, "style") {
            let css = dom::text_content(&el);
            let (new, n) = filter_css(&css, true);
            if n > 0 {
                dom::set_text(&el, &new);
                dropped += n;
            }
        }
        if let Some(style) = dom::get_attr(&el, "style") {
            let (new, n) = filter_css(&style, false);
            if n > 0 {
                dropped += n;
                if new.trim().is_empty() {
                    dom::remove_attr(&el, "style");
                } else {
                    dom::set_attr(&el, "style", new.trim());
                }
            }
        }
    }
    if dropped > 0 {
        ctx.warn(format!(
            "{}: removed {dropped} CSS declaration{} that e-ink readers do not support \
             (flex/grid layout, fixed positioning, viewport units, opacity, filters)",
            ctx.current_input.display(),
            if dropped == 1 { "" } else { "s" }
        ));
    }
}

/// Whether a CSS declaration is unsupported on the device profiles.
fn unsupported_declaration(prop: &str, value: &str) -> bool {
    let prop = prop.trim().to_ascii_lowercase();
    if prop.starts_with("--") {
        return false;
    }
    let value = value.to_ascii_lowercase();
    let value = value.replace("!important", "");
    let value = value.trim();
    match prop.as_str() {
        "display" if ["flex", "inline-flex", "grid", "inline-grid"].contains(&value) => {
            return true;
        }
        "position" if ["fixed", "sticky", "-webkit-sticky"].contains(&value) => return true,
        "opacity" | "filter" | "-webkit-filter" | "backdrop-filter" | "mix-blend-mode" => {
            return true;
        }
        _ => {}
    }
    has_viewport_unit(value)
}

/// Whether a CSS value uses a viewport-relative length (`50vw`, `1vmin`).
fn has_viewport_unit(value: &str) -> bool {
    const UNITS: &[&str] = &[
        "vmin", "vmax", "dvw", "dvh", "svw", "svh", "lvw", "lvh", "vw", "vh", "vi", "vb",
    ];
    let b = value.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            let rest = &value[i..];
            for u in UNITS {
                if rest.starts_with(u)
                    && !rest[u.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
                {
                    return true;
                }
            }
        } else if b[i].is_ascii_alphabetic() || b[i] == b'-' || b[i] == b'_' {
            // Skip identifiers so `h1` or `var(--x2vw)` are not numbers.
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-' || b[i] == b'_') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    false
}

/// Removes unsupported declarations from CSS text. `sheet` is true for a
/// style sheet (rules with blocks) and false for a `style` attribute (a bare
/// declaration list). Comments and strings are respected. Returns the new
/// text and the number of declarations removed.
fn filter_css(css: &str, sheet: bool) -> (String, usize) {
    let mut out = String::with_capacity(css.len());
    let mut dropped = 0;
    let mut depth: usize = 0;
    let mut seg_start = 0;
    let mut paren = 0usize;
    let b = css.as_bytes();
    let mut i = 0;
    let flush =
        |seg: &str, delim: Option<u8>, depth: usize, out: &mut String, dropped: &mut usize| {
            let is_decl = (!sheet || depth > 0) && delim != Some(b'{');
            if is_decl
                && let Some((prop, value)) = seg.split_once(':')
                && unsupported_declaration(&strip_css_comments(prop), value)
            {
                *dropped += 1;
                // Keep the whitespace before the declaration; drop it and its `;`.
                let lead = seg.len() - seg.trim_start().len();
                out.push_str(&seg[..lead]);
                if delim == Some(b'}') {
                    out.push('}');
                }
                return;
            }
            out.push_str(seg);
            if let Some(d) = delim {
                out.push(d as char);
            }
        };
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = css[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
                continue;
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            d @ (b';' | b'{' | b'}') if paren == 0 => {
                let seg = &css[seg_start..i];
                flush(seg, Some(d), depth, &mut out, &mut dropped);
                match d {
                    b'{' => depth += 1,
                    b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                seg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if seg_start < css.len() {
        flush(&css[seg_start..], None, depth, &mut out, &mut dropped);
    }
    (out, dropped)
}

fn strip_css_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        rest = rest[i + 2..]
            .find("*/")
            .map_or("", |e| &rest[i + 2 + e + 2..]);
    }
    out.push_str(rest);
    out
}

fn is_html(node: &Handle) -> bool {
    dom::namespace(node).is_some_and(|ns| ns == XHTML_NS || ns.is_empty())
}

fn is_foreign_root(node: &Handle) -> bool {
    dom::is_element(node) && !is_html(node)
}

fn take_title_hint(doc: &Handle, ctx: &mut Ctx) {
    if ctx.title_hint.is_some() {
        return;
    }
    let title = html_elements(doc)
        .into_iter()
        .find(|n| dom::is_tag(n, "title"));
    if let Some(t) = title {
        let text = collapse_ws(&dom::text_content(&t));
        if !text.is_empty() {
            ctx.title_hint = Some(text);
        }
    }
}

/// Moves `<style>` blocks from `<head>` to the start of `<body>` and drops
/// the rest of `<head>`.
fn hoist_head(doc: &Handle, body: &Handle, ctx: &mut Ctx) {
    let heads: Vec<Handle> = dom::select_by_tag(doc, "head")
        .into_iter()
        .filter(|h| !dom::descendants(h).iter().any(|d| Rc::ptr_eq(d, body)))
        .collect();
    let mut styles = Vec::new();
    for head in &heads {
        for n in dom::select_by_tag(head, "*") {
            if dom::is_tag(&n, "style") && is_html(&n) {
                styles.push(n);
            } else if dom::is_tag(&n, "link") && is_stylesheet_link(&n) {
                let href = dom::get_attr(&n, "href").unwrap_or_default();
                ctx.warn(format!(
                    "{}: external stylesheet '{}' ignored (use --css to add styles)",
                    ctx.current_input.display(),
                    href
                ));
            }
        }
    }
    let first = dom::children(body).into_iter().next();
    for s in &styles {
        match &first {
            Some(f) => dom::insert_before(f, s),
            None => dom::append_child(body, s),
        }
    }
    for head in &heads {
        dom::remove(head);
    }
}

fn is_stylesheet_link(n: &Handle) -> bool {
    dom::get_attr(n, "rel").is_some_and(|r| {
        r.split_ascii_whitespace()
            .any(|t| t.eq_ignore_ascii_case("stylesheet"))
    })
}

fn clean_children(node: &Handle, ctx: &mut Ctx) {
    for child in dom::children(node) {
        if dom::is_element(&child) {
            clean_element(&child, ctx);
        }
    }
}

fn clean_element(el: &Handle, ctx: &mut Ctx) {
    if !is_html(el) {
        clean_foreign(el, ctx);
        return;
    }
    let tag = dom::tag_name(el).unwrap_or_default().to_ascii_lowercase();
    if tag == "svg" || tag == "math" {
        // Foreign roots parsed in the HTML namespace cannot happen with
        // html5ever, but be safe.
        clean_foreign(el, ctx);
        return;
    }
    if tag == "a"
        && !dom::has_attr(el, "id")
        && let Some(name) = dom::get_attr(el, "name").filter(|n| !n.trim().is_empty())
    {
        dom::set_attr(el, "id", name.trim());
    }
    match action_for(&tag) {
        Action::Remove => {
            if tag == "link" && is_stylesheet_link(el) {
                let href = dom::get_attr(el, "href").unwrap_or_default();
                ctx.warn(format!(
                    "{}: external stylesheet '{}' ignored (use --css to add styles)",
                    ctx.current_input.display(),
                    href
                ));
            }
            dom::remove(el);
        }
        Action::Unwrap => {
            if tag == "picture" {
                carry_picture_sources(el);
            }
            clean_children(el, ctx);
            dom::unwrap(el);
        }
        Action::Rename(to, css) => {
            let new = dom::create_element(to, &[]);
            for (k, v) in dom::attrs(el) {
                dom::set_attr(&new, &k, &v);
            }
            if !css.is_empty() {
                append_style(&new, css);
            }
            for c in dom::children(el) {
                dom::append_child(&new, &c);
            }
            dom::replace_with(el, &new);
            clean_element(&new, ctx);
        }
        Action::Keep => {
            if tag == "img"
                && let Some(srcset) = dom::get_attr(el, "srcset").filter(|s| !s.trim().is_empty())
            {
                dom::set_attr(el, SRCSET_ATTR, srcset.trim());
            }
            clean_html_attrs(el, &tag);
            if tag == "area" && !dom::has_attr(el, "alt") {
                dom::set_attr(el, "alt", "");
            }
            clean_children(el, ctx);
        }
    }
}

/// Attribute where [`sanitize`] keeps an `<img>`'s `srcset` (which is not
/// valid EPUB XHTML) for `assets::transform`, which uses its candidates
/// when `src` is missing or cannot be loaded, and then removes it.
pub const SRCSET_ATTR: &str = "data-md2epub-srcset";

/// Attribute where [`sanitize`] keeps the `srcset` values of a `<picture>`'s
/// `<source>` elements (one per line, in document order) on its `<img>`,
/// as further fallbacks for `assets::transform`.
pub const SOURCES_ATTR: &str = "data-md2epub-sources";

/// Copies the `srcset` of a `<picture>`'s `<source>` children onto its
/// `<img>` (see [`SOURCES_ATTR`]) before the sources are removed.
fn carry_picture_sources(picture: &Handle) {
    let sources: Vec<String> = dom::children(picture)
        .into_iter()
        .filter(|c| dom::is_tag(c, "source"))
        .filter_map(|c| dom::get_attr(&c, "srcset").or_else(|| dom::get_attr(&c, "src")))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if sources.is_empty() {
        return;
    }
    if let Some(img) = dom::first_by_tag(picture, "img") {
        dom::set_attr(&img, SOURCES_ATTR, &sources.join("\n"));
    }
}

fn append_style(el: &Handle, css: &str) {
    let mut style = dom::get_attr(el, "style").unwrap_or_default();
    let trimmed = style.trim_end();
    if !trimmed.is_empty() && !trimmed.ends_with(';') {
        style = format!("{trimmed}; ");
    } else if !trimmed.is_empty() {
        style = format!("{trimmed} ");
    } else {
        style.clear();
    }
    style.push_str(css);
    dom::set_attr(el, "style", &style);
}

fn clean_html_attrs(el: &Handle, tag: &str) {
    let mut css = Vec::new();
    for (name, value) in dom::attrs(el) {
        let lname = name.to_ascii_lowercase();
        match attr_fate(tag, &lname, &value) {
            AttrFate::Keep => {}
            AttrFate::Drop => dom::remove_attr(el, &name),
            AttrFate::Set(v) => {
                if v != value {
                    dom::set_attr(el, &name, &v);
                }
            }
            AttrFate::Style(s) => {
                dom::remove_attr(el, &name);
                css.push(s);
            }
        }
    }
    if !css.is_empty() {
        append_style(el, &css.join("; "));
    }
}

/// SVG/MathML subtrees: only remove active content.
fn clean_foreign(el: &Handle, ctx: &mut Ctx) {
    if dom::is_tag(el, "script") {
        dom::remove(el);
        return;
    }
    for (name, value) in dom::attrs(el) {
        let lname = name.to_ascii_lowercase();
        let url_attr = lname == "href" || lname.ends_with(":href") || lname == "src";
        if lname.starts_with("on") || (url_attr && is_script_url(&value)) {
            dom::remove_attr(el, &name);
        } else if url_attr {
            let encoded = encode_url(&value);
            if encoded != value {
                dom::set_attr(el, &name, &encoded);
            }
        }
    }
    for child in dom::children(el) {
        if !dom::is_element(&child) {
            continue;
        }
        if is_html(&child) {
            // HTML inside foreignObject (or annotation-xml).
            clean_element(&child, ctx);
        } else {
            clean_foreign(&child, ctx);
        }
    }
}

/// Elements of `root` in document order without descending into SVG or
/// MathML subtrees (the foreign roots themselves are not included).
fn html_elements(root: &Handle) -> Vec<Handle> {
    let mut out = Vec::new();
    let mut stack: Vec<Handle> = dom::children(root).into_iter().rev().collect();
    while let Some(n) = stack.pop() {
        if !dom::is_element(&n) || is_foreign_root(&n) {
            continue;
        }
        stack.extend(dom::children(&n).into_iter().rev());
        out.push(n);
    }
    out
}

fn heading_level(node: &Handle) -> Option<u8> {
    if !is_html(node) {
        return None;
    }
    match dom::tag_name(node)?.to_ascii_lowercase().as_str() {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// Removes comrak's empty `<a class="anchor">` links from headings, moving
/// their `id` to the heading when the heading has none.
fn strip_heading_anchors(heading: &Handle) {
    for a in dom::select_by_tag(heading, "a") {
        if !dom::has_class(&a, "anchor")
            || !dom::text_content(&a).trim().is_empty()
            || !dom::select_by_tag(&a, "*").is_empty()
        {
            continue;
        }
        if !dom::has_attr(heading, "id")
            && let Some(id) = dom::get_attr(&a, "id").filter(|i| !i.trim().is_empty())
        {
            dom::set_attr(heading, "id", id.trim());
        }
        dom::remove(&a);
    }
}

/// Makes every id a valid XML name unique across the book, gives headings
/// an id and updates same-document `#id` links.
fn assign_ids(root: &Handle, ctx: &mut Ctx) {
    let elements = html_elements(root);
    for el in &elements {
        if heading_level(el).is_some() {
            strip_heading_anchors(el);
        }
    }
    let mut renamed: HashMap<String, String> = HashMap::new();
    for el in html_elements(root) {
        let is_heading = heading_level(&el).is_some();
        let raw = dom::get_attr(&el, "id");
        let base = match &raw {
            Some(raw) => normalize_id(raw),
            None => String::new(),
        };
        let base = if base.is_empty() {
            if !is_heading {
                if raw.is_some() {
                    dom::remove_attr(&el, "id");
                }
                continue;
            }
            match normalize_id(&slugify(&heading_text(&el))) {
                s if s.is_empty() => "section".to_string(),
                s => s,
            }
        } else {
            base
        };
        let id = ctx.unique_id(&base);
        dom::set_attr(&el, "id", &id);
        if let Some(raw) = raw {
            // Only the first element with a given id in this document is a
            // link target; later duplicates neither redirect links nor
            // advertise the original id.
            if let std::collections::hash_map::Entry::Vacant(slot) = renamed.entry(raw) {
                if *slot.key() != id {
                    dom::set_attr(&el, ORIG_ID_ATTR, slot.key());
                }
                slot.insert(id);
            }
        }
    }
    fix_label_targets(root, &renamed);
    if renamed.iter().all(|(k, v)| k == v) {
        return;
    }
    for el in html_elements(root) {
        let Some(href) = dom::get_attr(&el, "href") else {
            continue;
        };
        let Some(frag) = href.strip_prefix('#') else {
            continue;
        };
        let decoded = percent_decode(frag);
        let new = renamed.get(frag).or_else(|| renamed.get(&decoded));
        if let Some(new) = new
            && new != frag
        {
            dom::set_attr(&el, "href", &format!("#{new}"));
        }
    }
}

/// Keeps `<label for>` only when it names a labelable element of the same
/// document (following renamed ids); a dangling `for` is invalid XHTML.
fn fix_label_targets(root: &Handle, renamed: &HashMap<String, String>) {
    const LABELABLE: &[&str] = &[
        "button", "input", "meter", "output", "progress", "select", "textarea",
    ];
    let elements = html_elements(root);
    let labelable: HashSet<String> = elements
        .iter()
        .filter(|e| LABELABLE.iter().any(|t| dom::is_tag(e, t)))
        .filter_map(|e| dom::get_attr(e, "id"))
        .collect();
    for label in elements.iter().filter(|e| dom::is_tag(e, "label")) {
        let Some(target) = dom::get_attr(label, "for") else {
            continue;
        };
        let target = target.trim();
        let mapped = renamed.get(target).map(String::as_str).unwrap_or(target);
        if labelable.contains(mapped) {
            if mapped != target {
                dom::set_attr(label, "for", mapped);
            }
        } else {
            dom::remove_attr(label, "for");
        }
    }
}

/// Turns an arbitrary id into a valid XML NCName (`""` when nothing usable
/// remains). Ids that are already valid come back unchanged.
pub fn normalize_id(raw: &str) -> String {
    let allowed = |c: char| {
        c.is_ascii_alphanumeric()
            || c == '-'
            || c == '_'
            || c == '.'
            || (!c.is_ascii() && c.is_alphabetic())
    };
    let starts_ok = |s: &str| s.starts_with(|c: char| c.is_alphabetic() || c == '_');
    if starts_ok(raw) && raw.chars().all(allowed) {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    for c in raw.trim().chars() {
        if allowed(c) {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        String::new()
    } else if starts_ok(out) {
        out.to_string()
    } else {
        format!("id-{out}")
    }
}

/// GitHub-style slug: lowercase, spaces to `-`, punctuation dropped.
fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if (c.is_whitespace() || c == '-' || c == '_') && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Heading text for the TOC: text content plus image alt texts, with
/// whitespace collapsed.
fn heading_text(node: &Handle) -> String {
    fn walk(n: &Handle, out: &mut String) {
        if dom::is_text(n) {
            out.push_str(&dom::text_content(n));
            return;
        }
        if dom::is_tag(n, "img") {
            if let Some(alt) = dom::get_attr(n, "alt") {
                out.push(' ');
                out.push_str(&alt);
                out.push(' ');
            }
            return;
        }
        if dom::is_tag(n, "rp") {
            return;
        }
        for c in dom::children(n) {
            walk(&c, out);
        }
    }
    let mut out = String::new();
    walk(node, &mut out);
    collapse_ws(&out)
}

// ---------------------------------------------------------------------------
// XHTML input
// ---------------------------------------------------------------------------

/// Whether an HTML input is really XML (XHTML): a `.xhtml` file, or one
/// starting with an XML declaration.
pub fn is_xhtml_input(path: &Path, text: &str) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xhtml"))
        || text
            .trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("<?xml")
}

/// HTML void elements (self-closing is fine for these in HTML parsing).
const HTML_VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];

/// Rewrites XHTML so the HTML parser reads it as an XML parser would:
/// `<![CDATA[...]]>` sections become escaped text (inside `<style>` and
/// `<script>` only the markers are removed) and self-closed non-void
/// elements (`<div/>`) get an end tag.
pub fn xhtml_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix("<!--") {
            let end = after.find("-->").map_or(tail.len(), |e| 4 + e + 3);
            out.push_str(&tail[..end]);
            rest = &tail[end..];
            continue;
        }
        if let Some(after) = tail.strip_prefix("<![CDATA[") {
            let (inner, end) = match after.find("]]>") {
                Some(e) => (&after[..e], 9 + e + 3),
                None => (after, tail.len()),
            };
            out.push_str(
                &inner
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;"),
            );
            rest = &tail[end..];
            continue;
        }
        if tail.starts_with("<!") || tail.starts_with("<?") || tail.starts_with("</") {
            let end = tail.find('>').map_or(tail.len(), |e| e + 1);
            out.push_str(&tail[..end]);
            rest = &tail[end..];
            continue;
        }
        let Some(end) = find_tag_end(tail) else {
            out.push_str(tail);
            return out;
        };
        let raw = &tail[..end];
        let name: String = raw[1..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '/' && *c != '>')
            .collect();
        let local = name
            .rsplit(':')
            .next()
            .unwrap_or(&name)
            .to_ascii_lowercase();
        rest = &tail[end..];
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            out.push_str(raw);
            continue;
        }
        let self_closed = raw.ends_with("/>");
        if self_closed && !HTML_VOID.contains(&local.as_str()) {
            out.push_str(raw[..raw.len() - 2].trim_end());
            out.push_str("></");
            out.push_str(&name);
            out.push('>');
            continue;
        }
        out.push_str(raw);
        if !self_closed && (local == "style" || local == "script") {
            // Raw text: copy up to the end tag, unwrapping CDATA markers.
            let close = format!("</{name}");
            let lower = rest.to_ascii_lowercase();
            let stop = lower
                .find(&close.to_ascii_lowercase())
                .unwrap_or(rest.len());
            let content = &rest[..stop];
            out.push_str(&content.replace("<![CDATA[", "").replace("]]>", ""));
            rest = &rest[stop..];
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Splitting
// ---------------------------------------------------------------------------

/// Splits the document into chapters. See the module docs.
pub fn split(doc: &Handle, ctx: &mut Ctx) -> Result<Vec<Chapter>> {
    let body = dom::body(doc).unwrap_or_else(|| doc.clone());

    // Styles go to the start of every chapter of this input.
    let mut style_xhtml = String::new();
    for s in html_elements(&body) {
        if dom::is_tag(&s, "style") {
            style_xhtml.push_str(&dom::serialize_xhtml(&s));
            dom::remove(&s);
        }
    }

    // Headings without an id (when `sanitize` did not run) still need one.
    for h in html_elements(&body) {
        if heading_level(&h).is_some() && !dom::has_attr(&h, "id") {
            let base = match normalize_id(&slugify(&heading_text(&h))) {
                s if s.is_empty() => "section".to_string(),
                s => s,
            };
            let id = ctx.unique_id(&base);
            dom::set_attr(&h, "id", &id);
        }
    }

    let mut tops: Vec<Handle> = Vec::new();
    if let Some(level) = ctx.opts.split_level {
        let points: Vec<Handle> = html_elements(&body)
            .into_iter()
            .filter(|h| heading_level(h).is_some_and(|l| l <= level) && splittable(&body, h))
            .collect();
        for h in points {
            if let Some(top) = cut_before(&body, &h) {
                tops.push(top);
            }
        }
    }

    // Group the body's children into chapters.
    let mut groups: Vec<Vec<Handle>> = Vec::new();
    let mut current: Vec<Handle> = Vec::new();
    for child in dom::children(&body) {
        if tops.iter().any(|t| Rc::ptr_eq(t, &child)) && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
        current.push(child);
    }
    if !current.is_empty() || groups.is_empty() {
        groups.push(current);
    }
    // A preamble (content before the first split heading) without real
    // content joins the first chapter.
    let is_top = |n: &Handle| tops.iter().any(|t| Rc::ptr_eq(t, n));
    if groups.len() > 1 && !groups[0].iter().any(is_top) && !groups[0].iter().any(has_content) {
        let pre = groups.remove(0);
        let mut first = pre;
        first.append(&mut groups[0]);
        groups[0] = first;
    }

    let stem = ctx
        .current_input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty());

    // An input whose language differs from the book's keeps it on a wrapper.
    let lang_wrapper = ctx
        .input_lang
        .clone()
        .filter(|l| !l.eq_ignore_ascii_case(&ctx.opts.meta.lang));

    let mut chapters = Vec::with_capacity(groups.len());
    for group in groups {
        let mut headings = Vec::new();
        let mut body_xhtml = style_xhtml.clone();
        if let Some(l) = &lang_wrapper {
            let l = dom::escape(l);
            body_xhtml.push_str(&format!("<div lang=\"{l}\" xml:lang=\"{l}\">"));
        }
        for node in &group {
            if let Some(level) = heading_level(node) {
                headings.push(make_heading(node, level));
            }
            if dom::is_element(node) && !is_foreign_root(node) {
                for d in html_elements(node) {
                    if let Some(level) = heading_level(&d) {
                        headings.push(make_heading(&d, level));
                    }
                }
            }
            body_xhtml.push_str(&dom::serialize_xhtml(node));
        }
        if lang_wrapper.is_some() {
            body_xhtml.push_str("</div>");
        }
        if ctx.title_hint.is_none()
            && let Some(h1) = headings.iter().find(|h| h.level == 1 && !h.text.is_empty())
        {
            ctx.title_hint = Some(h1.text.clone());
        }
        let title = headings
            .iter()
            .filter(|h| !h.text.is_empty())
            .min_by_key(|h| h.level)
            .map(|h| h.text.clone())
            .or_else(|| stem.clone())
            .unwrap_or_else(|| "Chapter".into());
        chapters.push(Chapter {
            title,
            file_name: ctx.next_chapter_file(),
            body_xhtml,
            headings,
            in_toc: true,
            source: Some(ctx.current_input.clone()),
        });
    }
    Ok(chapters)
}

fn make_heading(node: &Handle, level: u8) -> Heading {
    Heading {
        level,
        text: heading_text(node),
        id: dom::get_attr(node, "id").unwrap_or_default(),
    }
}

/// Whether the node holds something a reader would see.
fn has_content(node: &Handle) -> bool {
    if dom::is_text(node) {
        return !dom::text_content(node).trim().is_empty();
    }
    if !dom::is_element(node) {
        return false;
    }
    const VISIBLE: &[&str] = &[
        "img", "svg", "math", "table", "input", "hr", "video", "audio", "picture",
    ];
    let visible = |n: &Handle| {
        dom::tag_name(n).is_some_and(|t| VISIBLE.contains(&t.to_ascii_lowercase().as_str()))
    };
    visible(node)
        || !dom::text_content(node).trim().is_empty()
        || dom::descendants(node).iter().any(visible)
}

/// Whether every ancestor between `body` and `h` may be cut.
fn splittable(body: &Handle, h: &Handle) -> bool {
    let mut cur = h.clone();
    loop {
        let Some(p) = dom::parent(&cur) else {
            return false;
        };
        if Rc::ptr_eq(&p, body) {
            return true;
        }
        let ok = is_html(&p)
            && dom::tag_name(&p)
                .is_some_and(|t| SPLIT_CONTAINERS.contains(&t.to_ascii_lowercase().as_str()));
        if !ok {
            return false;
        }
        cur = p;
    }
}

/// Splits the ancestors of `h` so that `h` starts a direct child of `body`
/// and returns that child.
fn cut_before(body: &Handle, h: &Handle) -> Option<Handle> {
    let mut cur = h.clone();
    loop {
        let p = dom::parent(&cur)?;
        if Rc::ptr_eq(&p, body) {
            return Some(cur);
        }
        let kids = dom::children(&p);
        let idx = kids.iter().position(|k| Rc::ptr_eq(k, &cur))?;
        if !kids[..idx].iter().any(has_content_or_element) {
            cur = p;
            continue;
        }
        let clone = shallow_clone(&p);
        for k in &kids[idx..] {
            dom::append_child(&clone, k);
        }
        insert_after(&p, &clone);
        cur = clone;
    }
}

fn has_content_or_element(n: &Handle) -> bool {
    dom::is_element(n) || has_content(n)
}

fn shallow_clone(el: &Handle) -> Handle {
    let clone = match &el.data {
        NodeData::Element { name, attrs, .. } => Node::new(NodeData::Element {
            name: name.clone(),
            attrs: RefCell::new(attrs.borrow().clone()),
            template_contents: RefCell::new(None),
            mathml_annotation_xml_integration_point: false,
        }),
        _ => dom::create_element("div", &[]),
    };
    // Ids must stay unique: only the original keeps them.
    dom::remove_attr(&clone, "id");
    dom::remove_attr(&clone, ORIG_ID_ATTR);
    clone
}

fn insert_after(reference: &Handle, new: &Handle) {
    let Some(p) = dom::parent(reference) else {
        return;
    };
    let kids = dom::children(&p);
    let next = kids
        .iter()
        .position(|k| Rc::ptr_eq(k, reference))
        .and_then(|i| kids.get(i + 1).cloned());
    match next {
        Some(n) => dom::insert_before(&n, new),
        None => dom::append_child(&p, new),
    }
}

// ---------------------------------------------------------------------------
// Link fixing
// ---------------------------------------------------------------------------

/// Rewrites cross-chapter and cross-file links. See the module docs.
pub fn fix_links(chapters: &mut [Chapter]) {
    let _ = fix_links_with_warnings(chapters);
}

/// Like [`fix_links`], and returns a warning for every internal link that
/// had no target in the book (its `href` was removed).
pub fn fix_links_with_warnings(chapters: &mut [Chapter]) -> Vec<String> {
    fix_links_with_resources(chapters, &[])
}

/// Like [`fix_links_with_warnings`], also accepting parent-relative links
/// (`../images/x.png`) that name one of the book `resources` (hrefs
/// relative to `OEBPS/`, e.g. `images/ab12.png`).
pub fn fix_links_with_resources(chapters: &mut [Chapter], resources: &[String]) -> Vec<String> {
    fix_links_full(chapters, resources, &HashMap::new(), None)
}

/// Like [`fix_links_with_resources`]. `skipped` maps the [`source_key`] of
/// local files that were not added to the book to the reason why; a link to
/// one of them is removed with a warning that gives the reason (one warning
/// per linking file, target file and reason, whatever the href spelling).
/// `root` is the project root that root-relative links (`/docs/a.md`)
/// resolve against (see [`local_link_target`]).
pub fn fix_links_full(
    chapters: &mut [Chapter],
    resources: &[String],
    skipped: &HashMap<PathBuf, String>,
    root: Option<&Path>,
) -> Vec<String> {
    let mut index = LinkIndex::build(chapters);
    index.resources = resources.iter().cloned().collect();
    index.root = root.map(Path::to_path_buf);
    let mut warnings = Vec::new();
    let mut seen_warnings = HashSet::new();
    let mut seen_skipped: HashSet<(usize, PathBuf, String)> = HashSet::new();
    for (i, chapter) in chapters.iter_mut().enumerate() {
        let mut changed = false;
        let rewritten = rewrite_start_tags(&chapter.body_xhtml, |tag, attrs| {
            let before = attrs.len();
            attrs.retain(|(k, _)| k != ORIG_ID_ATTR);
            let mut touched = attrs.len() != before;
            if tag == "a" || tag == "area" {
                let mut drop_href = false;
                for (k, v) in attrs.iter_mut() {
                    if k != "href" {
                        continue;
                    }
                    match index.resolve(i, v) {
                        Resolved::Keep => {}
                        Resolved::Rewrite(new) => {
                            if *v != new {
                                *v = new;
                                touched = true;
                            }
                        }
                        Resolved::Broken => {
                            let skip = index.sources[i]
                                .as_deref()
                                .and_then(|src| local_link_target(&link_base(src), root, v))
                                .map(|p| source_key(&p))
                                .and_then(|key| skipped.get(&key).map(|r| (key, r)));
                            // Warnings quote the href as the source spells it.
                            let shown = percent_decode(v);
                            let msg = match skip {
                                Some((key, reason)) => {
                                    if !seen_skipped.insert((i, key, reason.clone())) {
                                        drop_href = true;
                                        continue;
                                    }
                                    format!(
                                        "{}: link to local file '{shown}' removed: {reason}",
                                        index.source_label(i),
                                    )
                                }
                                None => format!(
                                    "{}: link target '{shown}' not found in the book; link removed",
                                    index.source_label(i),
                                ),
                            };
                            if seen_warnings.insert(msg.clone()) {
                                warnings.push(msg);
                            }
                            drop_href = true;
                        }
                        Resolved::Unsupported(scheme) => {
                            let msg = format!(
                                "{}: '{scheme}:' link '{}' is not allowed in an EPUB; link removed",
                                index.source_label(i),
                                shorten_href(v)
                            );
                            if seen_warnings.insert(msg.clone()) {
                                warnings.push(msg);
                            }
                            drop_href = true;
                        }
                    }
                }
                if drop_href {
                    attrs.retain(|(k, _)| k != "href");
                    touched = true;
                }
            }
            changed |= touched;
            touched
        });
        if changed {
            chapter.body_xhtml = rewritten;
        }
    }
    warnings
}

enum Resolved {
    Keep,
    Rewrite(String),
    Broken,
    /// A URL scheme readers cannot follow (`data:`, `blob:`, ...).
    Unsupported(String),
}

/// URL schemes kept on links. `file:` links are mapped to chapters, and
/// every other scheme is removed (EPUB forbids `file:` and `data:` links,
/// and readers cannot follow `blob:`, `about:`, `chrome:`, ...).
const LINK_SCHEMES: &[&str] = &[
    "http", "https", "mailto", "tel", "sms", "ftp", "ftps", "news", "nntp", "irc", "ircs", "geo",
    "urn", "doi", "isbn",
];

/// A long href shortened for warnings (data URIs can be huge).
fn shorten_href(href: &str) -> String {
    const MAX: usize = 60;
    if href.chars().count() <= MAX {
        href.to_string()
    } else {
        format!("{}...", href.chars().take(MAX).collect::<String>())
    }
}

struct LinkIndex {
    files: Vec<String>,
    sources: Vec<Option<PathBuf>>,
    /// Normalized source path of each chapter.
    source_keys: Vec<Option<PathBuf>>,
    /// Element id -> chapter index (first occurrence).
    ids: HashMap<String, usize>,
    /// (source key, original id) -> (chapter index, current id).
    orig_ids: HashMap<(PathBuf, String), (usize, String)>,
    /// Source key -> first chapter index.
    first_chapter: HashMap<PathBuf, usize>,
    /// Book resource hrefs relative to `OEBPS/` (`images/ab.png`).
    resources: HashSet<String>,
    /// Project root for root-relative links.
    root: Option<PathBuf>,
}

impl LinkIndex {
    fn build(chapters: &[Chapter]) -> Self {
        let mut idx = LinkIndex {
            files: chapters.iter().map(|c| c.file_name.clone()).collect(),
            sources: chapters.iter().map(|c| c.source.clone()).collect(),
            source_keys: chapters
                .iter()
                .map(|c| c.source.as_deref().map(source_key))
                .collect(),
            ids: HashMap::new(),
            orig_ids: HashMap::new(),
            first_chapter: HashMap::new(),
            resources: HashSet::new(),
            root: None,
        };
        for (i, ch) in chapters.iter().enumerate() {
            if let Some(key) = &idx.source_keys[i] {
                idx.first_chapter.entry(key.clone()).or_insert(i);
            }
            let mut found: Vec<(String, Option<String>)> = Vec::new();
            rewrite_start_tags(&ch.body_xhtml, |_, attrs| {
                let id = attrs
                    .iter()
                    .find(|(k, _)| k == "id")
                    .map(|(_, v)| v.clone());
                let orig = attrs
                    .iter()
                    .find(|(k, _)| k == ORIG_ID_ATTR)
                    .map(|(_, v)| v.clone());
                if let Some(id) = id {
                    found.push((id, orig));
                }
                false
            });
            for (id, orig) in found {
                idx.ids.entry(id.clone()).or_insert(i);
                if let (Some(orig), Some(key)) = (orig, &idx.source_keys[i]) {
                    idx.orig_ids.entry((key.clone(), orig)).or_insert((i, id));
                }
            }
        }
        idx
    }

    fn source_label(&self, i: usize) -> String {
        match &self.sources[i] {
            Some(p) => p.display().to_string(),
            None => self.files[i].clone(),
        }
    }

    fn link_to(&self, from: usize, to: usize, id: Option<&str>) -> String {
        match id {
            Some(id) if from == to => format!("#{id}"),
            Some(id) => format!("{}#{}", self.files[to], id),
            None => self.files[to].clone(),
        }
    }

    /// Finds an element id inside the chapters of one source.
    fn find_in_source(&self, key: &Path, frag: &str) -> Option<(usize, String)> {
        let in_source = |cand: &str| {
            self.ids
                .get(cand)
                .filter(|&&i| self.source_keys[i].as_deref() == Some(key))
                .map(|&i| (i, cand.to_string()))
        };
        in_source(frag)
            .or_else(|| {
                self.orig_ids
                    .get(&(key.to_path_buf(), frag.to_string()))
                    .cloned()
            })
            .or_else(|| in_source(&normalize_id(frag)))
    }

    fn resolve(&self, from: usize, href: &str) -> Resolved {
        let href_t = href.trim();
        if href_t.is_empty() || href_t == "#" {
            return Resolved::Keep;
        }
        if let Some(frag) = href_t.strip_prefix('#') {
            let decoded = percent_decode(frag);
            for cand in [frag, decoded.as_str()] {
                if let Some(&to) = self.ids.get(cand) {
                    return if to == from {
                        Resolved::Keep
                    } else {
                        Resolved::Rewrite(self.link_to(from, to, Some(cand)))
                    };
                }
            }
            if let Some(key) = &self.source_keys[from]
                && let Some((to, id)) = self.find_in_source(key, &decoded)
            {
                return Resolved::Rewrite(self.link_to(from, to, Some(&id)));
            }
            return Resolved::Broken;
        }
        if href_t.starts_with("//") {
            return Resolved::Keep;
        }
        if has_scheme(href_t) {
            let scheme = href_t[..href_t.find(':').unwrap_or(0)].to_ascii_lowercase();
            if scheme == "file" {
                return self.resolve_file_url(from, href_t);
            }
            if LINK_SCHEMES.contains(&scheme.as_str()) {
                return Resolved::Keep;
            }
            return Resolved::Unsupported(scheme);
        }
        let (path_part, frag) = match href_t.split_once('#') {
            Some((p, f)) => (p, Some(f)),
            None => (href_t, None),
        };
        let path_part = path_part.split('?').next().unwrap_or("");
        if path_part.is_empty() {
            // `?query#frag` on the same document.
            return match frag {
                Some(f) => self.resolve(from, &format!("#{f}")),
                None => Resolved::Keep,
            };
        }
        let path = percent_decode(path_part);
        let target = resolve_local_path(&self.base_of(from), self.root.as_deref(), &path);
        let Some(key) = self.match_source(&target) else {
            // Already-resolved chapter links (`ch002.xhtml#x`) and links to
            // book resources (`../images/x.png`) stay as they are.
            if self.files.iter().any(|f| f == &path) || self.is_book_file(&path) {
                return Resolved::Keep;
            }
            return Resolved::Broken;
        };
        self.link_to_source(from, &key, frag)
    }

    /// Directory that relative links in chapter `from` resolve against.
    fn base_of(&self, from: usize) -> PathBuf {
        self.sources[from]
            .as_deref()
            .map(link_base)
            .unwrap_or_default()
    }

    /// Whether `path` (relative to `OEBPS/text/`) names a chapter or a book
    /// resource.
    fn is_book_file(&self, path: &str) -> bool {
        let mut parts: Vec<&str> = vec!["text"];
        for seg in path.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return false;
                    }
                }
                s => parts.push(s),
            }
        }
        let joined = parts.join("/");
        if let Some(name) = joined.strip_prefix("text/") {
            return self.files.iter().any(|f| f == name);
        }
        self.resources.contains(&joined)
    }

    /// A `file:` URL: links to an input file map to its chapter like a
    /// relative link; anything else is broken (a local path means nothing
    /// on a reader and EPUB forbids `file:` URLs).
    fn resolve_file_url(&self, from: usize, href: &str) -> Resolved {
        let rest = &href[5..];
        let (path_part, frag) = match rest.split_once('#') {
            Some((p, f)) => (p, Some(f)),
            None => (rest, None),
        };
        let path_part = path_part.split('?').next().unwrap_or("");
        let Some(path) = file_url_path(&self.base_of(from), path_part) else {
            return Resolved::Broken;
        };
        match self.match_source(&path) {
            Some(key) => self.link_to_source(from, &key, frag),
            None => Resolved::Broken,
        }
    }

    /// A link to the input file `key`, optionally to an element in it.
    fn link_to_source(&self, from: usize, key: &Path, frag: Option<&str>) -> Resolved {
        let Some(&first) = self.first_chapter.get(key) else {
            return Resolved::Broken;
        };
        match frag.map(percent_decode).filter(|f| !f.is_empty()) {
            Some(f) => match self.find_in_source(key, &f) {
                Some((to, id)) => Resolved::Rewrite(self.link_to(from, to, Some(&id))),
                None => Resolved::Rewrite(self.files[first].clone()),
            },
            None => Resolved::Rewrite(self.files[first].clone()),
        }
    }

    /// Finds the input file a link path points to: exact match first, then
    /// the same file name with another input extension (`a.html` -> `a.md`).
    fn match_source(&self, target: &Path) -> Option<PathBuf> {
        let key = source_key(target);
        if self.first_chapter.contains_key(&key) {
            return Some(key);
        }
        let is_doc_ext = |p: &Path| {
            p.extension().is_some_and(|e| {
                matches!(
                    e.to_string_lossy().to_ascii_lowercase().as_str(),
                    "md" | "markdown" | "mdown" | "mkd" | "html" | "htm" | "xhtml"
                )
            })
        };
        if !is_doc_ext(&key) {
            return None;
        }
        let stem = key.with_extension("");
        let mut candidates: Vec<&PathBuf> = self
            .first_chapter
            .keys()
            .filter(|k| is_doc_ext(k) && k.with_extension("") == stem)
            .collect();
        candidates.sort();
        candidates.first().map(|k| (*k).clone())
    }
}

/// Every `href` of an `<a>` or `<area>` start tag in serialized XHTML, in
/// document order (attribute values unescaped).
pub fn link_hrefs(xhtml: &str) -> Vec<String> {
    let mut out = Vec::new();
    rewrite_start_tags(xhtml, |tag, attrs| {
        if tag == "a" || tag == "area" {
            out.extend(
                attrs
                    .iter()
                    .filter(|(k, _)| k == "href")
                    .map(|(_, v)| v.clone()),
            );
        }
        false
    });
    out
}

/// Directory that relative links in a chapter from `source` resolve against.
pub fn link_base(source: &Path) -> PathBuf {
    source.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// The local file a link points to, if it names one: a relative path
/// (resolved against `base_dir`), a root-relative path or a `file:` URL,
/// with any `?query` and `#fragment` removed and `%XX` escapes decoded.
/// `None` for fragment-only links, links with another URL scheme and
/// `//host` links. See [`resolve_local_path`] for root-relative paths and
/// backslashes; relative `file:` URLs (`file:a.md`) resolve against
/// `base_dir` too.
pub fn local_link_target(base_dir: &Path, root: Option<&Path>, href: &str) -> Option<PathBuf> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("//") {
        return None;
    }
    let strip = |s: &str| -> String {
        let s = s.split('#').next().unwrap_or("");
        s.split('?').next().unwrap_or("").to_string()
    };
    if has_scheme(href) {
        let colon = href.find(':')?;
        if !href[..colon].eq_ignore_ascii_case("file") {
            return None;
        }
        return file_url_path(base_dir, &strip(&href[colon + 1..]));
    }
    let path = strip(href);
    if path.is_empty() {
        return None;
    }
    Some(resolve_local_path(base_dir, root, &percent_decode(&path)))
}

/// The file named by the part of a `file:` URL after the colon (without
/// query and fragment): `///abs`, `//localhost/abs`, `/abs`, `/C:/dir`, or a
/// relative path, which resolves against `base_dir`. `None` for another
/// host or an empty path.
fn file_url_path(base_dir: &Path, rest: &str) -> Option<PathBuf> {
    let path = match rest.strip_prefix("//") {
        Some(after) => {
            let (host, p) = after.split_at(after.find('/').unwrap_or(after.len()));
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            p
        }
        None => rest,
    };
    let mut decoded = percent_decode(path);
    // file:///C:/dir -> C:/dir
    let b = decoded.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        decoded.remove(0);
    }
    if decoded.is_empty() {
        return None;
    }
    let b = decoded.as_bytes();
    let has_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    if decoded.starts_with(['/', '\\']) || has_drive {
        Some(PathBuf::from(decoded))
    } else {
        Some(base_dir.join(decoded))
    }
}

/// Resolves a decoded link path from a file in `base_dir`:
///
/// - a path starting with a single `/` is root-relative: it names
///   `root/<path>` (the project root, see `crate::linked::LinkScope`). When
///   that does not exist but the path exists as an absolute file system
///   path, that file is meant (it is then subject to the same confinement
///   as every linked file). Without a `root`, the path is absolute;
/// - other paths are relative to `base_dir`. A path written with `\`
///   separators and no `/` (`docs\guide.md`) is tried with `/` when the
///   literal name does not exist.
pub fn resolve_local_path(base_dir: &Path, root: Option<&Path>, path: &str) -> PathBuf {
    if let Some(root) = root
        && path.starts_with('/')
        && !path.starts_with("//")
    {
        let in_root = root.join(path.trim_start_matches('/'));
        let absolute = Path::new(path);
        if !in_root.exists() && absolute.exists() {
            return absolute.to_path_buf();
        }
        return in_root;
    }
    let joined = base_dir.join(path);
    if path.contains('\\') && !path.contains('/') && !joined.exists() {
        let alt = base_dir.join(path.replace('\\', "/"));
        if alt.exists() {
            return alt;
        }
    }
    joined
}

/// Normalized key for comparing input paths: canonical when the file
/// exists, else lexically cleaned.
pub fn source_key(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    lexical_normalize(&abs)
}

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn has_scheme(href: &str) -> bool {
    let Some(colon) = href.find(':') else {
        return false;
    };
    let scheme = &href[..colon];
    // A single letter is a Windows drive (`C:\...`), not a scheme.
    scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// Decodes `%XX` escapes (invalid sequences are kept as they are).
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Minimal XML start-tag rewriter (works on serialized, well-formed XHTML)
// ---------------------------------------------------------------------------

/// Calls `f(tag_name, attrs)` for every start tag in `src` (attribute values
/// unescaped). When `f` returns `true` the tag is re-emitted from the
/// (possibly modified) attributes; everything else is copied verbatim.
fn rewrite_start_tags(
    src: &str,
    mut f: impl FnMut(&str, &mut Vec<(String, String)>) -> bool,
) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let end = if tail.starts_with("<!--") {
            tail.find("-->").map(|i| i + 3)
        } else if tail.starts_with("<![CDATA[") {
            tail.find("]]>").map(|i| i + 3)
        } else if tail.starts_with("<!") || tail.starts_with("<?") || tail.starts_with("</") {
            tail.find('>').map(|i| i + 1)
        } else {
            find_tag_end(tail)
        };
        let Some(end) = end else {
            out.push_str(tail);
            return out;
        };
        let raw = &tail[..end];
        let is_start = !(raw.starts_with("<!") || raw.starts_with("<?") || raw.starts_with("</"));
        match is_start.then(|| parse_start_tag(raw)).flatten() {
            Some((name, mut attrs, self_closing)) => {
                if f(&name, &mut attrs) {
                    out.push('<');
                    out.push_str(&name);
                    for (k, v) in &attrs {
                        out.push(' ');
                        out.push_str(k);
                        out.push_str("=\"");
                        out.push_str(&dom::escape(v));
                        out.push('"');
                    }
                    out.push_str(if self_closing { "/>" } else { ">" });
                } else {
                    out.push_str(raw);
                }
            }
            None => out.push_str(raw),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Byte offset just past the `>` closing the tag that starts `tail`,
/// respecting quoted attribute values.
fn find_tag_end(tail: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in tail.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i + 1),
            None => {}
        }
    }
    None
}

type StartTag = (String, Vec<(String, String)>, bool);

fn parse_start_tag(raw: &str) -> Option<StartTag> {
    let inner = raw.strip_prefix('<')?.strip_suffix('>')?;
    let (inner, self_closing) = match inner.strip_suffix('/') {
        Some(s) => (s, true),
        None => (inner, false),
    };
    let name_end = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut attrs = Vec::new();
    let mut s = inner[name_end..].trim_start();
    while !s.is_empty() {
        let eq = s.find('=')?;
        let key = s[..eq].trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return None;
        }
        let after = s[eq + 1..].trim_start();
        let q = after.chars().next()?;
        if q != '"' && q != '\'' {
            return None;
        }
        let close = after[1..].find(q)? + 1;
        attrs.push((key.to_string(), xml_unescape(&after[1..close])));
        s = after[close + 1..].trim_start();
    }
    Some((name.to_string(), attrs, self_closing))
}

/// Resolves XML character and predefined entity references.
fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let Some(semi) = tail.find(';').filter(|&i| i <= 12) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::tests_support::assert_well_formed;
    use crate::model::Options;

    fn ctx_with(split: Option<u8>, input: &str) -> Ctx {
        let mut ctx = Ctx::new(Options {
            split_level: split,
            ..Options::default()
        });
        ctx.current_input = input.into();
        ctx
    }

    /// Sanitizes and splits `html` as input `input`.
    fn convert(html: &str, ctx: &mut Ctx, input: &str) -> Vec<Chapter> {
        ctx.current_input = input.into();
        let doc = dom::parse(html);
        sanitize(&doc, ctx).unwrap();
        let chapters = split(&doc, ctx).unwrap();
        for ch in &chapters {
            assert_well_formed(&ch.body_xhtml);
        }
        chapters
    }

    fn sanitized_body(html: &str) -> String {
        let mut ctx = ctx_with(None, "t.html");
        let doc = dom::parse(html);
        sanitize(&doc, &mut ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        assert_well_formed(&out);
        out
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn removes_active_content() {
        let out = sanitized_body(
            r#"<p onclick="x()" onmouseover="y">a</p><script>bad()</script>
            <noscript><p>ns</p></noscript><iframe src="https://e.com"></iframe>
            <form action="/x"><input type="text" name="q"></form>
            <object data="a.swf"></object><embed src="a.swf">
            <video src="a.mp4"><p>fallback v</p></video><audio src="a.mp3"></audio>
            <a href="javascript:alert(1)">bad</a> <a href=" JaVaScRiPt :alert(1)">bad2</a>
            <a href="https://example.org/" target="_blank">ok</a>
            <img src="x.png" onerror="z()" alt="i">
            <svg xmlns="http://www.w3.org/2000/svg" onload="q()"><script>s()</script>
              <a xlink:href="javascript:x()"><rect width="1" height="1"/></a></svg>"#,
        );
        for bad in [
            "onclick",
            "onmouseover",
            "onerror",
            "onload",
            "script",
            "bad()",
            "noscript",
            "iframe",
            "<form",
            "name=\"q\"",
            "object",
            "embed",
            "video",
            "fallback",
            "audio",
            "javascript",
            "JaVaScRiPt",
            "target",
        ] {
            assert!(!out.contains(bad), "{bad} left in {out}");
        }
        assert!(
            out.contains(r#"<a href="https://example.org/">ok</a>"#),
            "{out}"
        );
        assert!(out.contains("<a>bad</a>"), "{out}");
        assert!(out.contains(r#"<img src="x.png" alt="i"/>"#), "{out}");
        assert!(out.contains("<rect"), "{out}");
    }

    #[test]
    fn full_document_keeps_body_title_and_styles() {
        let html = r#"<!DOCTYPE html><html lang="de"><head><meta charset="utf-8">
            <title> My  Page </title><link rel="stylesheet" href="x.css">
            <style>.a > b { color: red }</style></head>
            <body bgcolor="white"><p class="a" style="font-weight: bold">x</p>
            <style>p { margin: 0 }</style></body></html>"#;
        let mut ctx = ctx_with(None, "page.html");
        let doc = dom::parse(html);
        sanitize(&doc, &mut ctx).unwrap();
        assert_eq!(ctx.title_hint.as_deref(), Some("My Page"));
        assert!(ctx.warnings().iter().any(|w| w.contains("x.css")));
        assert!(dom::first_by_tag(&doc, "head").is_none());
        assert!(dom::first_by_tag(&doc, "meta").is_none());
        let ch = split(&doc, &mut ctx).unwrap();
        assert_eq!(ch.len(), 1);
        let body = &ch[0].body_xhtml;
        assert!(
            body.starts_with(
                "<style>.a &gt; b { color: red }</style><style>p { margin: 0 }</style>"
            ),
            "{body}"
        );
        assert!(body.contains(r#"<p class="a" style="font-weight: bold">x</p>"#));
        assert!(!body.contains("title"));
        assert!(!body.contains("bgcolor"));
        assert_well_formed(body);
    }

    #[test]
    fn invalid_attributes_and_obsolete_elements() {
        let out = sanitized_body(
            r##"<center>c</center><font color="red" face="x" size="3">f</font>
            <table border="1" cellpadding="2" width="80%" summary="s" bgcolor="#fff">
            <tr><td align="right" valign="top" nowrap width="20" colspan="2" rowspan="x">t</td></tr></table>
            <img src="a.png" align="left" width="50%" height="20" border="0" hspace="3" srcset="b.png 2x">
            <p align="center" style="color: blue">p</p><ul type="disc"><li type="a" value="3">l</li></ul>
            <ol start="2" type="i" reversed><li>o</li></ol><tt>mono</tt><strike>gone</strike>
            <custom-el data-x="1"><b>kept</b></custom-el><a name="anchor1">n</a>
            <span lang="" dir="sideways" tabindex="1" contenteditable="true" aria-label="ok" role="note">s</span>
            <input type="checkbox" checked disabled><p style="background: url(javascript:x)">j</p>"##,
        );
        assert!(
            out.contains(r#"<div style="text-align: center">c</div>"#),
            "{out}"
        );
        assert!(out.contains("<span>f</span>"), "{out}");
        assert!(out.contains(r#"<table style="width: 80%">"#), "{out}");
        assert!(
            out.contains(r#"<td colspan="2" style="text-align: right; vertical-align: top; white-space: nowrap; width: 20px">t</td>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<img src="a.png" height="20" data-md2epub-srcset="b.png 2x" style="float: left; width: 50%"/>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<p style="color: blue; text-align: center">p</p>"#),
            "{out}"
        );
        assert!(out.contains(r#"<ul><li value="3">l</li></ul>"#), "{out}");
        assert!(
            out.contains(r#"<ol start="2" type="i" reversed="">"#),
            "{out}"
        );
        assert!(out.contains("<code>mono</code>"), "{out}");
        assert!(out.contains("<s>gone</s>"), "{out}");
        assert!(
            out.contains("<b>kept</b>") && !out.contains("custom-el"),
            "{out}"
        );
        assert!(out.contains(r#"<a id="anchor1">n</a>"#), "{out}");
        assert!(
            out.contains(r#"<span aria-label="ok" role="note">s</span>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<input type="checkbox" checked="" disabled=""/>"#),
            "{out}"
        );
        assert!(out.contains("<p>j</p>"), "{out}");
    }

    #[test]
    fn keeps_markdown_output_intact() {
        let html = r##"<h2>Tables<a href="#tables" aria-label="Link to heading 'Tables'" data-heading-content="Tables" class="anchor"></a></h2>
            <p>Text<sup class="footnote-ref"><a href="#fn-1" id="fnref-1" data-footnote-ref="">1</a></sup>
            <span data-math-style="inline">x^2</span></p>
            <pre class="mermaid">graph TD; A--&gt;B</pre>
            <pre style="background-color:#fff;"><code><span style="color:#333;">fn</span></code></pre>
            <ul><li><input type="checkbox" class="task-list-item-checkbox" checked="" disabled="" /> done</li></ul>
            <section class="footnotes" data-footnotes=""><ol><li id="fn-1"><p>Note <a href="#fnref-1" class="footnote-backref">↩</a></p></li></ol></section>"##;
        let out = sanitized_body(html);
        assert!(out.contains(r#"<h2 id="tables">Tables</h2>"#), "{out}");
        assert!(!out.contains("anchor"), "{out}");
        assert!(out.contains(r#"<span data-math-style="inline">x^2</span>"#));
        assert!(out.contains(r#"<pre class="mermaid">graph TD; A--&gt;B</pre>"#));
        assert!(out.contains(r#"<pre style="background-color:#fff;">"#));
        assert!(out.contains(r##"<a href="#fn-1" id="fnref-1" data-footnote-ref="" xmlns:epub="http://www.idpf.org/2007/ops" epub:type="noteref" role="doc-noteref">1</a>"##), "{out}");
        assert!(out.contains(r#"class="task-list-item-checkbox""#));
        assert!(
            out.contains(
                r#"<li id="fn-1" xmlns:epub="http://www.idpf.org/2007/ops" epub:type="endnote">"#
            ),
            "{out}"
        );
        assert!(
            out.contains(r#"epub:type="endnotes" role="doc-endnotes""#),
            "{out}"
        );
        assert!(
            out.contains("role=\"doc-backlink\">\u{21A9}\u{FE0E}</a>"),
            "{out}"
        );
    }

    #[test]
    fn real_comrak_output() {
        let mut o = comrak::Options::default();
        o.extension.header_id_prefix = Some(String::new());
        o.extension.footnotes = true;
        o.extension.tasklist = true;
        o.extension.table = true;
        o.extension.math_dollars = true;
        o.render.r#unsafe = true;
        let md = "# Intro\n\nSee [later](#later-part).[^n]\n\n- [x] done\n\n\
                  ## Later part\n\n$x$\n\n# Intro\n\n[^n]: Note.\n";
        let html = comrak::markdown_to_html(md, &o);
        let mut ctx = ctx_with(Some(1), "c.md");
        let mut ch = convert(&html, &mut ctx, "c.md");
        fix_links(&mut ch);
        assert_eq!(titles(&ch), ["Intro", "Intro"]);
        let ids: Vec<&str> = ch
            .iter()
            .flat_map(|c| c.headings.iter().map(|h| h.id.as_str()))
            .collect();
        assert_eq!(ids, ["intro", "later-part", "intro-1"]);
        assert_eq!(ch[0].headings[1].text, "Later part");
        let all: String = ch.iter().map(|c| c.body_xhtml.as_str()).collect();
        assert!(!all.contains("class=\"anchor\""), "{all}");
        assert!(
            all.contains(r##"<a href="#later-part">later</a>"##),
            "{all}"
        );
        // The footnote definitions ended up in the last chapter.
        assert!(ch[0].body_xhtml.contains("ch002.xhtml#fn-n"), "{all}");
        assert!(ch[1].body_xhtml.contains("ch001.xhtml#fnref-n"), "{all}");
    }

    #[test]
    fn heading_ids_unique_across_book() {
        let mut ctx = ctx_with(None, "a.md");
        let a = convert(
            r##"<h1 id="intro">Intro</h1><h2>Same</h2><h2>Same</h2><h3 id="9 lives">Odd</h3>
                <p id="intro">dup</p><a href="#intro">to intro</a><a href="#9%20lives">odd</a>"##,
            &mut ctx,
            "a.md",
        );
        let b = convert(
            r##"<h1 id="intro">Intro again</h1><a href="#intro">self</a><h2></h2>"##,
            &mut ctx,
            "b.md",
        );
        let ids: Vec<&str> = a
            .iter()
            .chain(&b)
            .flat_map(|c| c.headings.iter().map(|h| h.id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                "intro",
                "same",
                "same-2",
                "id-9-lives",
                "intro-3",
                "section"
            ]
        );
        let set: HashSet<&&str> = ids.iter().collect();
        assert_eq!(set.len(), ids.len());
        let a_body = &a[0].body_xhtml;
        // The duplicate paragraph id was renamed, links still go to the heading.
        assert!(a_body.contains(r#"<p id="intro-2">dup</p>"#), "{a_body}");
        assert!(
            a_body.contains(r##"<a href="#intro">to intro</a>"##),
            "{a_body}"
        );
        assert!(
            a_body.contains(r##"<a href="#id-9-lives">odd</a>"##),
            "{a_body}"
        );
        // In b.md the heading was renamed and its own link follows it.
        assert!(b[0].body_xhtml.contains(r##"<a href="#intro-3">self</a>"##));
        assert!(b[0].body_xhtml.contains(ORIG_ID_ATTR));
        assert_eq!(b[0].title, "Intro again");
    }

    #[test]
    fn normalizes_ids() {
        assert_eq!(normalize_id("intro"), "intro");
        assert_eq!(normalize_id("ünïcode-ok"), "ünïcode-ok");
        assert_eq!(normalize_id("1-intro"), "id-1-intro");
        assert_eq!(normalize_id(" a b:c "), "a-b-c");
        assert_eq!(normalize_id("--"), "");
        assert_eq!(normalize_id(""), "");
        assert_eq!(slugify("Hello, World! 2"), "hello-world-2");
    }

    const BOOK: &str = r#"<p>Preface text.</p>
        <h1>One</h1><p>1</p>
        <h2>One A</h2><p>1a</p>
        <h3>One A i</h3><p>1ai</p>
        <h2>One B</h2><p>1b</p>
        <h1>Two</h1><p>2</p>"#;

    fn titles(ch: &[Chapter]) -> Vec<&str> {
        ch.iter().map(|c| c.title.as_str()).collect()
    }

    #[test]
    fn split_level_none() {
        let mut ctx = ctx_with(None, "dir/book.md");
        let ch = convert(BOOK, &mut ctx, "dir/book.md");
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].title, "One");
        assert_eq!(ch[0].headings.len(), 5);
        assert_eq!(ch[0].file_name, "ch001.xhtml");
        assert_eq!(ch[0].source.as_deref(), Some(Path::new("dir/book.md")));
        assert!(ch[0].in_toc);
        assert_eq!(ctx.title_hint.as_deref(), Some("One"));
    }

    #[test]
    fn split_level_1() {
        let mut ctx = ctx_with(Some(1), "book.md");
        let ch = convert(BOOK, &mut ctx, "book.md");
        assert_eq!(titles(&ch), ["book", "One", "Two"]);
        assert!(ch[0].body_xhtml.contains("Preface"));
        assert!(ch[0].headings.is_empty());
        let levels: Vec<u8> = ch[1].headings.iter().map(|h| h.level).collect();
        assert_eq!(levels, [1, 2, 3, 2]);
        assert!(ch[1].body_xhtml.starts_with("<h1 id=\"one\">One</h1>"));
        assert!(ch[2].body_xhtml.contains("<p>2</p>"));
        let files: Vec<&str> = ch.iter().map(|c| c.file_name.as_str()).collect();
        assert_eq!(files, ["ch001.xhtml", "ch002.xhtml", "ch003.xhtml"]);
    }

    #[test]
    fn split_level_2() {
        let mut ctx = ctx_with(Some(2), "book.md");
        let ch = convert(BOOK, &mut ctx, "book.md");
        assert_eq!(titles(&ch), ["book", "One", "One A", "One B", "Two"]);
        assert_eq!(ch[2].headings.len(), 2);
        assert!(ch[2].body_xhtml.contains("1ai"));
        assert!(!ch[1].body_xhtml.contains("1a"));
    }

    #[test]
    fn whitespace_preamble_merges_and_numbering_continues() {
        let mut ctx = ctx_with(Some(1), "a.md");
        ctx.chapter_count = 4;
        let ch = convert("\n  \n<h1>A</h1><p>x</p><h1>B</h1>", &mut ctx, "a.md");
        assert_eq!(titles(&ch), ["A", "B"]);
        assert_eq!(ch[0].file_name, "ch005.xhtml");
        assert_eq!(ch[1].file_name, "ch006.xhtml");
    }

    #[test]
    fn split_through_containers() {
        let mut ctx = ctx_with(Some(1), "s.html");
        let ch = convert(
            r#"<div class="wrap" id="w"><section><h1>A</h1><p>a</p></section>
               <section><p>lead</p><h1>B</h1><p>b</p></section></div>
               <blockquote><h1>Quoted</h1></blockquote>"#,
            &mut ctx,
            "s.html",
        );
        assert_eq!(titles(&ch), ["A", "B"]);
        assert!(
            ch[0]
                .body_xhtml
                .starts_with("<div class=\"wrap\" id=\"w\"><section><h1"),
            "{}",
            ch[0].body_xhtml
        );
        assert!(ch[0].body_xhtml.contains("lead"));
        // The cut copy of the wrapper does not repeat its id.
        assert!(
            ch[1]
                .body_xhtml
                .starts_with("<div class=\"wrap\"><section><h1 id=\"b\">B</h1>"),
            "{}",
            ch[1].body_xhtml
        );
        // Headings inside a blockquote do not split, but are listed.
        assert!(ch[1].body_xhtml.contains("Quoted"));
        assert_eq!(ch[1].headings.len(), 2);
    }

    #[test]
    fn styles_repeat_in_every_chapter() {
        let mut ctx = ctx_with(Some(1), "s.md");
        let ch = convert(
            "<style>p { x: 1 }</style><h1>A</h1><h1>B</h1>",
            &mut ctx,
            "s.md",
        );
        assert_eq!(ch.len(), 2);
        for c in &ch {
            assert!(c.body_xhtml.starts_with("<style>p { x: 1 }</style><h1"));
        }
    }

    #[test]
    fn empty_and_garbage_input() {
        let mut ctx = ctx_with(Some(1), "empty.md");
        let ch = convert("", &mut ctx, "empty.md");
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].title, "empty");
        assert!(ch[0].headings.is_empty());

        let garbage = "<<<h1>>\u{0}<div<p </ x=\"<h2 id='\"'>&&#xZZ;<svg><h1>in svg</h1></svg>\
                       <table><h1>t</h1><tr><td><h2>cell</h2></table></h1></h1><a href='%zz#%'>x</a>";
        let ch = convert(garbage, &mut ctx, "garbage.html");
        assert!(!ch.is_empty());
        let mut chapters = ch;
        fix_links(&mut chapters);
        for c in &chapters {
            assert_well_formed(&c.body_xhtml);
        }
    }

    #[test]
    fn fix_links_across_chapters_and_files() {
        let mut ctx = ctx_with(Some(1), "docs/a.md");
        let mut chapters = convert(
            r##"<h1>Alpha</h1><p><a href="#beta">to beta</a> <a href="#alpha">self</a>
                <a href="b.html#intro">b intro</a> <a href="b.html">b</a>
                <a href="./b.md#missing">b missing</a> <a href="https://x.org/a.md#q">ext</a>
                <a href="mailto:a@b.c">mail</a> <a href="nowhere.md">nowhere</a>
                <a href="#nope">nope</a> <a href="#">top</a></p>
                <h1 id="beta">Beta</h1><p>b</p>"##,
            &mut ctx,
            "docs/a.md",
        );
        chapters.extend(convert(
            r##"<h1>Bee</h1><p><a href="a.md#beta">back</a> <a href="../docs/a.md">a</a></p>
                <h1 id="intro">Intro</h1><p id="beta">clash</p>"##,
            &mut ctx,
            "docs/b.html",
        ));
        // b.html's "intro" was free; its "beta" clashed and was renamed.
        let warnings = fix_links_with_warnings(&mut chapters);
        let names: Vec<&str> = chapters.iter().map(|c| c.file_name.as_str()).collect();
        assert_eq!(
            names,
            ["ch001.xhtml", "ch002.xhtml", "ch003.xhtml", "ch004.xhtml"]
        );
        let a1 = &chapters[0].body_xhtml;
        assert!(
            a1.contains(r#"<a href="ch002.xhtml#beta">to beta</a>"#),
            "{a1}"
        );
        assert!(a1.contains(r##"<a href="#alpha">self</a>"##), "{a1}");
        assert!(
            a1.contains(r#"<a href="ch004.xhtml#intro">b intro</a>"#),
            "{a1}"
        );
        assert!(a1.contains(r#"<a href="ch003.xhtml">b</a>"#), "{a1}");
        assert!(
            a1.contains(r#"<a href="ch003.xhtml">b missing</a>"#),
            "{a1}"
        );
        assert!(
            a1.contains(r#"<a href="https://x.org/a.md#q">ext</a>"#),
            "{a1}"
        );
        assert!(a1.contains(r#"<a href="mailto:a@b.c">mail</a>"#), "{a1}");
        assert!(a1.contains("<a>nowhere</a>"), "{a1}");
        assert!(a1.contains("<a>nope</a>"), "{a1}");
        assert!(a1.contains(r##"<a href="#">top</a>"##), "{a1}");
        let b1 = &chapters[2].body_xhtml;
        assert!(
            b1.contains(r#"<a href="ch002.xhtml#beta">back</a>"#),
            "{b1}"
        );
        assert!(b1.contains(r#"<a href="ch001.xhtml">a</a>"#), "{b1}");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("nowhere.md")));
        for c in &chapters {
            assert!(!c.body_xhtml.contains(ORIG_ID_ATTR));
            assert_well_formed(&c.body_xhtml);
        }
        // Running again changes nothing.
        let before: Vec<String> = chapters.iter().map(|c| c.body_xhtml.clone()).collect();
        assert!(fix_links_with_warnings(&mut chapters).is_empty());
        let after: Vec<String> = chapters.iter().map(|c| c.body_xhtml.clone()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn fix_links_follows_renamed_ids_across_files() {
        let mut ctx = ctx_with(None, "a.md");
        let mut chapters = convert(
            r##"<h1 id="setup">Setup</h1><a href="b.md#setup">b setup</a>"##,
            &mut ctx,
            "a.md",
        );
        chapters.extend(convert(
            r##"<h1 id="setup">Setup B</h1><p id="2nd">x</p>"##,
            &mut ctx,
            "b.md",
        ));
        chapters.extend(convert(
            r##"<a href="b.md#2nd">c</a><a href="a.md#setup">a</a>"##,
            &mut ctx,
            "c.md",
        ));
        fix_links(&mut chapters);
        assert!(
            chapters[0]
                .body_xhtml
                .contains(r#"<a href="ch002.xhtml#setup-2">b setup</a>"#),
            "{}",
            chapters[0].body_xhtml
        );
        assert!(
            chapters[2]
                .body_xhtml
                .contains(r#"<a href="ch002.xhtml#id-2nd">c</a><a href="ch001.xhtml#setup">a</a>"#),
            "{}",
            chapters[2].body_xhtml
        );
    }

    #[test]
    fn local_link_targets() {
        let base = Path::new("docs");
        let t = |h: &str| local_link_target(base, None, h);
        assert_eq!(t("a.md"), Some(PathBuf::from("docs/a.md")));
        assert_eq!(
            t("sub/my%20file.svg#x?y"),
            Some(PathBuf::from("docs/sub/my file.svg"))
        );
        assert_eq!(t("x.rs?plain=1#L3"), Some(PathBuf::from("docs/x.rs")));
        assert_eq!(t("/abs/x.txt"), Some(PathBuf::from("/abs/x.txt")));
        assert_eq!(
            t("file:///tmp/a%20b.md#s"),
            Some(PathBuf::from("/tmp/a b.md"))
        );
        assert_eq!(
            t("file://localhost/tmp/a.md"),
            Some(PathBuf::from("/tmp/a.md"))
        );
        // Relative file: URLs are relative to the linking file.
        assert_eq!(t("file:i.md"), Some(PathBuf::from("docs/i.md")));
        assert_eq!(
            t("file:sub/a%20b.md#x"),
            Some(PathBuf::from("docs/sub/a b.md"))
        );
        assert_eq!(t("file:/abs/i.md"), Some(PathBuf::from("/abs/i.md")));
        for none in [
            "",
            "#frag",
            "?q",
            "//cdn.example.com/x.png",
            "https://example.com/a.md",
            "mailto:a@b.c",
            "file://otherhost/x.md",
        ] {
            assert_eq!(t(none), None, "{none}");
        }
    }

    #[test]
    fn root_relative_and_backslash_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        std::fs::write(root.join("docs/guide.md"), "# G").unwrap();
        let base = root.join("docs/sub");
        let t = |h: &str| local_link_target(&base, Some(root), h);
        // `/x` is relative to the project root, not the file system root.
        assert_eq!(t("/docs/guide.md#s"), Some(root.join("docs/guide.md")));
        assert_eq!(t("/docs/missing.md"), Some(root.join("docs/missing.md")));
        // An absolute path that exists (and is not under the root) is kept,
        // so confinement can explain why it is not added.
        let outside = tempfile::NamedTempFile::new().unwrap();
        let abs = outside.path().to_string_lossy().replace('\\', "/");
        if abs.starts_with('/') {
            assert_eq!(t(&abs), Some(PathBuf::from(&abs)));
        }
        // `..\guide.md` written with backslashes (percent-encoded by the
        // Markdown renderer) is found with `/` separators.
        assert_eq!(t("..%5Cguide.md"), Some(base.join("../guide.md")));
        // ... but only when the literal name does not exist.
        assert_eq!(t("a%5Cb.md"), Some(base.join("a\\b.md")));
    }

    #[test]
    fn link_index_resolves_root_relative_file_url_and_backslash_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        let readme = root.join("README.md");
        let guide = root.join("docs/guide.md");
        let w = root.join("docs/w.html");
        for p in [&readme, &guide, &w] {
            std::fs::write(p, "x").unwrap();
        }
        let chapter = |file: &str, src: &Path, body: &str| Chapter {
            file_name: file.into(),
            body_xhtml: body.into(),
            source: Some(src.to_path_buf()),
            ..Chapter::default()
        };
        let mut chapters = vec![
            chapter(
                "ch001.xhtml",
                &readme,
                r#"<a href="/docs/guide.md">r</a><a href="docs%5Cguide.md">b</a>"#,
            ),
            chapter("ch002.xhtml", &guide, r#"<p>g</p>"#),
            chapter("ch003.xhtml", &w, r#"<a href="file:guide.md">f</a>"#),
        ];
        let warnings = fix_links_full(&mut chapters, &[], &HashMap::new(), Some(root));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            chapters[0].body_xhtml,
            r#"<a href="ch002.xhtml">r</a><a href="ch002.xhtml">b</a>"#
        );
        assert_eq!(chapters[2].body_xhtml, r#"<a href="ch002.xhtml">f</a>"#);
    }

    #[test]
    fn removal_warnings_are_decoded_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.md");
        let mut chapters = vec![Chapter {
            file_name: "ch001.xhtml".into(),
            body_xhtml: concat!(
                r#"<a href="docs/guide.md#setup">1</a><a href="./docs/../docs/guide.md">2</a>"#,
                r#"<a href="docs/guide.md">3</a><a href="docs/caf%C3%A9.md">4</a>"#
            )
            .into(),
            source: Some(src),
            ..Chapter::default()
        }];
        let mut skipped = HashMap::new();
        skipped.insert(
            source_key(&dir.path().join("docs/guide.md")),
            "turned off".to_string(),
        );
        let w = fix_links_full(&mut chapters, &[], &skipped, None);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(
            w[0].contains("'docs/guide.md#setup' removed: turned off"),
            "{w:?}"
        );
        assert!(w[1].contains("'docs/café.md'"), "{w:?}");
        assert_eq!(chapters[0].body_xhtml, "<a>1</a><a>2</a><a>3</a><a>4</a>");
    }

    #[test]
    fn link_hrefs_lists_anchor_and_area_targets() {
        let x = r#"<p><a href="a.md">a</a><img src="i.png"/><area href="b&amp;c.txt"/><a name="n">n</a></p>"#;
        assert_eq!(
            link_hrefs(x),
            vec!["a.md".to_string(), "b&c.txt".to_string()]
        );
    }

    #[test]
    fn skipped_files_explain_removed_links() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.md");
        let mut chapters = vec![Chapter {
            file_name: "ch001.xhtml".into(),
            body_xhtml: r#"<a href="blob.bin">b</a><a href="gone.md">g</a>"#.into(),
            source: Some(src),
            ..Chapter::default()
        }];
        let mut skipped = HashMap::new();
        skipped.insert(
            source_key(&dir.path().join("blob.bin")),
            "it is a binary file".to_string(),
        );
        let w = fix_links_full(&mut chapters, &[], &skipped, None);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(
            w[0].contains("'blob.bin' removed: it is a binary file"),
            "{}",
            w[0]
        );
        assert!(w[1].contains("'gone.md' not found in the book"), "{}", w[1]);
        assert_eq!(chapters[0].body_xhtml, "<a>b</a><a>g</a>");
    }

    #[test]
    fn tag_rewriter_round_trip() {
        let src = r#"<p a="1 &amp; 2" b='x>y'>t &lt; u<br/></p><!-- <a href="c"> --><x/>"#;
        let mut names = Vec::new();
        let out = rewrite_start_tags(src, |n, attrs| {
            names.push(n.to_string());
            if n == "p" {
                assert_eq!(attrs[0], ("a".into(), "1 & 2".into()));
                attrs.push(("c".into(), "\"".into()));
                return true;
            }
            false
        });
        assert_eq!(names, ["p", "br", "x"]);
        assert_eq!(
            out,
            r#"<p a="1 &amp; 2" b="x&gt;y" c="&quot;">t &lt; u<br/></p><!-- <a href="c"> --><x/>"#
        );
        assert_eq!(xml_unescape("&#65;&#x42;&bogus;&"), "AB&bogus;&");
        assert_eq!(percent_decode("a%20b%zz%"), "a b%zz%");
        assert!(has_scheme("https://x") && has_scheme("mailto:x"));
        assert!(!has_scheme("C:\\x") && !has_scheme("../a.md"));
    }

    #[test]
    fn fixtures_convert_and_link() {
        let mut ctx = ctx_with(Some(1), "");
        let ks = fixture("kitchen-sink.md");
        let page = fixture("page.html");
        let md = crate::markdown::render(&std::fs::read_to_string(&ks).unwrap()).unwrap();
        let mut chapters = convert(&md.html, &mut ctx, ks.to_str().unwrap());
        let html = std::fs::read_to_string(&page).unwrap();
        let first_page = chapters.len();
        chapters.extend(convert(&html, &mut ctx, page.to_str().unwrap()));
        // The first writer wins: the Markdown h1 once rendered, else the
        // HTML page's <title>.
        assert!(ctx.title_hint.is_some());
        let page_body = &chapters[first_page].body_xhtml;
        assert!(page_body.starts_with("<style>"), "{page_body}");
        assert!(!page_body.contains("onclick"));
        assert!(!page_body.contains("iframe"));
        assert!(!page_body.contains("<form"));
        let page_ids: Vec<&str> = chapters[first_page]
            .headings
            .iter()
            .map(|h| h.id.as_str())
            .collect();
        assert_eq!(
            page_ids,
            ["a-plain-html-page", "second-section", "second-section-2"]
        );
        fix_links_with_warnings(&mut chapters);
        for c in &chapters {
            assert_well_formed(&c.body_xhtml);
        }
    }

    #[test]
    fn invalid_raw_html_is_repaired() {
        let out = sanitized_body(
            r#"<p><a href="https://e.x/a b">sp</a> <a href=" x.html?q=a|b ">q</a>
            <a href="50%off.html">pct</a> <a href="a%20b.html">ok</a>
            <img src="sub\ab.png" alt="bs"></p>
            <map name="m"><area shape="rect" coords="0,0,1,1" href="https://e.x/"></map>
            <table><tr><th abbr="x" scope="col">h</th><td abbr="y">d</td></tr></table>
            <label for="nobody">dangling</label><label for="in">ok</label><input id="in">
            <label for="p1">not labelable</label><p id="p1">p</p>"#,
        );
        assert!(out.contains(r#"href="https://e.x/a%20b""#), "{out}");
        assert!(out.contains(r#"href="x.html?q=a%7Cb""#), "{out}");
        assert!(out.contains(r#"href="50%25off.html""#), "{out}");
        assert!(out.contains(r#"href="a%20b.html""#), "{out}");
        assert!(out.contains(r#"src="sub%5Cab.png""#), "{out}");
        assert!(out.contains(r#"alt="""#) && out.contains("<area"), "{out}");
        let area = &out[out.find("<area").unwrap()..];
        assert!(
            area[..area.find("/>").unwrap()].contains("alt=\"\""),
            "{out}"
        );
        assert!(!out.contains("abbr="), "{out}");
        assert!(out.contains("<label>dangling</label>"), "{out}");
        assert!(out.contains(r#"<label for="in">ok</label>"#), "{out}");
        assert!(out.contains("<label>not labelable</label>"), "{out}");
        assert_eq!(encode_url("a b\"c<d>"), "a%20b%22c%3Cd%3E");
        assert_eq!(encode_url("%41%zz%"), "%41%25zz%25");
        assert_eq!(encode_url("üml"), "üml");
    }

    #[test]
    fn label_targets_follow_renamed_ids() {
        let mut ctx = ctx_with(None, "a.html");
        ctx.unique_id("q");
        let doc = dom::parse(r#"<label for="q">l</label><input id="q">"#);
        sanitize(&doc, &mut ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        assert!(
            out.contains(r#"<label for="q-2">l</label><input id="q-2""#),
            "{out}"
        );
    }

    #[test]
    fn epub_type_from_input_is_kept() {
        let out = sanitized_body(r#"<aside epub:type="footnote" id="n">note</aside>"#);
        assert!(out.contains(r#"epub:type="footnote""#), "{out}");
    }

    #[test]
    fn device_profiles_drop_unsupported_css() {
        let html = r#"<style>.x{display:grid} .y { height: 100vh; color: red } @media print { .z { display: flex; margin: 0 } } a:hover { opacity: .5 }</style>
            <div style="display:flex;position:fixed;width:50vw;color:blue">d</div>
            <p style="opacity:0.5">o</p><p style="--w: 3vw; margin: 1vmin 2em">m</p>
            <svg xmlns="http://www.w3.org/2000/svg"><rect style="opacity:0.5" width="1" height="1"/></svg>"#;
        let mut ctx = ctx_with(None, "c.html");
        ctx.opts.profile = crate::profile::by_name("scribe").unwrap();
        let doc = dom::parse(html);
        sanitize(&doc, &mut ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        assert_well_formed(&out);
        for gone in [
            "grid",
            "100vh",
            "display: flex",
            "display:flex",
            "fixed",
            "50vw",
            "1vmin",
            "opacity: .5",
        ] {
            assert!(!out.contains(gone), "{gone} left in {out}");
        }
        assert!(out.contains(".x{}"), "{out}");
        assert!(out.contains("color: red"), "{out}");
        assert!(out.contains("margin: 0"), "{out}");
        assert!(out.contains(r#"style="color:blue""#), "{out}");
        assert!(out.contains("<p>o</p>"), "{out}");
        assert!(out.contains("--w: 3vw"), "{out}");
        // SVG keeps its own opacity.
        assert!(out.contains(r#"style="opacity:0.5""#), "{out}");
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        assert!(
            ctx.warnings()[0].contains("removed 9 CSS declarations"),
            "{:?}",
            ctx.warnings()
        );

        // The generic profile keeps everything.
        let mut ctx = ctx_with(None, "c.html");
        let doc = dom::parse(html);
        sanitize(&doc, &mut ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        assert!(
            out.contains("display:flex") && out.contains("100vh"),
            "{out}"
        );
        assert!(ctx.warnings().is_empty());
        assert!(has_viewport_unit("calc(100% - 2vh)"));
        assert!(!has_viewport_unit("h1vh2 var(--x2vw) 12px"));
    }

    #[test]
    fn html_lang_sets_book_language_and_wraps_other_languages() {
        // First input with <html lang="de"> and no configured language.
        let mut ctx = ctx_with(None, "a.html");
        let chapters = convert(
            r#"<!DOCTYPE html><html lang="de_DE"><head><title>Seite</title></head><body><p>Hallo</p></body></html>"#,
            &mut ctx,
            "a.html",
        );
        assert_eq!(ctx.opts.meta.lang, "de-DE");
        assert!(
            !chapters[0].body_xhtml.contains("lang="),
            "{}",
            chapters[0].body_xhtml
        );
        // A later input in another language keeps it on a wrapper.
        let chapters = convert(
            r#"<html lang="fr"><body><p>Bonjour</p></body></html>"#,
            &mut ctx,
            "b.html",
        );
        assert_eq!(ctx.opts.meta.lang, "de-DE");
        assert!(
            chapters[0]
                .body_xhtml
                .starts_with(r#"<div lang="fr" xml:lang="fr"><p>Bonjour</p>"#),
            "{}",
            chapters[0].body_xhtml
        );
        // An explicit language wins.
        let mut ctx = ctx_with(None, "a.html");
        ctx.opts.lang_given = true;
        convert(
            r#"<html lang="de"><body><p>x</p></body></html>"#,
            &mut ctx,
            "a.html",
        );
        assert_eq!(ctx.opts.meta.lang, "en");
        // Invalid tags are ignored with a warning.
        let mut ctx = ctx_with(None, "a.html");
        convert(
            r#"<html lang="12"><body><p>x</p></body></html>"#,
            &mut ctx,
            "a.html",
        );
        assert_eq!(ctx.opts.meta.lang, "en");
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
    }

    #[test]
    fn xhtml_input_is_read_like_xml() {
        let x = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head>
<style><![CDATA[ h1 { color: blue } p > b { x: "<&>" } ]]></style></head>
<body><p><![CDATA[cdata text & <tags>]]></p><div/><p>after</p><br/><span epub:type="z">s</span></body></html>"#;
        assert!(is_xhtml_input(Path::new("a.html"), x));
        assert!(is_xhtml_input(Path::new("a.XHTML"), "<p>"));
        assert!(!is_xhtml_input(Path::new("a.html"), "<p>"));
        let html = xhtml_to_html(x);
        let mut ctx = ctx_with(None, "x.xhtml");
        let chapters = convert(&html, &mut ctx, "x.xhtml");
        let body = &chapters[0].body_xhtml;
        assert!(
            body.contains("<style> h1 { color: blue } p &gt; b { x: \"&lt;&amp;&gt;\" } </style>"),
            "{body}"
        );
        assert!(
            body.contains("<p>cdata text &amp; &lt;tags&gt;</p>"),
            "{body}"
        );
        assert!(body.contains("<div></div><p>after</p><br/>"), "{body}");
        assert!(body.contains(r#"epub:type="z""#), "{body}");
        // Garbage does not panic.
        for junk in ["<![CDATA[", "<div/", "<style>a", "<a b='/>", "<"] {
            let _ = xhtml_to_html(junk);
        }
    }

    #[test]
    fn unresolvable_parent_and_scheme_links_are_removed() {
        let mut ctx = ctx_with(None, "d1/a.md");
        let mut chapters = convert(
            r#"<p><a href="../x.md">up</a> <a href="../images/ok.png">res</a>
            <a href="../text/ch001.xhtml">ch</a> <a href="../nope/zz.md">nope</a>
            <a href="file:///etc/hosts">file</a> <a href="data:text/plain,hi">data</a>
            <a href="blob:https://x/1">blob</a> <a href="tel:+123">tel</a></p>"#,
            &mut ctx,
            "d1/a.md",
        );
        let warnings = fix_links_with_resources(&mut chapters, &["images/ok.png".to_string()]);
        let body = &chapters[0].body_xhtml;
        assert!(body.contains("<a>up</a>"), "{body}");
        assert!(
            body.contains(r#"<a href="../images/ok.png">res</a>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="../text/ch001.xhtml">ch</a>"#),
            "{body}"
        );
        assert!(body.contains("<a>nope</a>"), "{body}");
        assert!(body.contains("<a>file</a>"), "{body}");
        assert!(body.contains("<a>data</a>"), "{body}");
        assert!(body.contains("<a>blob</a>"), "{body}");
        assert!(body.contains(r#"<a href="tel:+123">tel</a>"#), "{body}");
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("'data:'")),
            "{warnings:?}"
        );
    }

    #[test]
    fn file_urls_to_inputs_map_to_chapters() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "y").unwrap();
        let a_str = a.to_string_lossy().into_owned();
        let b_str = b.to_string_lossy().into_owned();
        // A real file URL: file:///tmp/x/b.md on Unix, file:///C:/x/b.md on Windows.
        let b_url_path = b_str.replace('\\', "/");
        let b_url = format!(
            "file://{}{b_url_path}",
            if b_url_path.starts_with('/') { "" } else { "/" }
        );
        let mut ctx = ctx_with(None, &a_str);
        let mut chapters = convert(
            &format!(r#"<p><a href="{b_url}#sec">to b</a></p>"#),
            &mut ctx,
            &a_str,
        );
        chapters.extend(convert(r#"<h2 id="sec">S</h2>"#, &mut ctx, &b_str));
        let warnings = fix_links_with_warnings(&mut chapters);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            chapters[0]
                .body_xhtml
                .contains(r#"<a href="ch002.xhtml#sec">to b</a>"#),
            "{}",
            chapters[0].body_xhtml
        );
    }

    #[test]
    fn picture_sources_and_srcset_are_kept_for_assets() {
        let out = sanitized_body(
            r#"<picture><source srcset="p.webp 1x, p2.webp 2x" type="image/webp"><source srcset="p.png"><img src="missing.png" srcset="i.png 2x" alt="a"></picture>"#,
        );
        assert!(
            !out.contains("<source") && !out.contains("<picture"),
            "{out}"
        );
        assert!(out.contains(r#"data-md2epub-srcset="i.png 2x""#), "{out}");
        assert!(
            out.contains("data-md2epub-sources=\"p.webp 1x, p2.webp 2x&#10;p.png\""),
            "{out}"
        );
    }
}

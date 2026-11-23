//! HTML sanitizing, chapter splitting and link fixing (owned by T3).
//!
//! Contract:
//! - [`sanitize`]: for full HTML documents keep only `<body>` content (the
//!   `<title>` becomes `ctx.title_hint` when none is set yet); remove
//!   `script`, `iframe`, `form`, `object`, `embed`, `video`, `audio`, `link`,
//!   `meta`, event-handler attributes and `javascript:` links; drop attributes
//!   invalid in EPUB XHTML; keep `style` attributes and `<style>` contents.
//!   Give every heading an `id` unique across the book (`ctx.unique_id`).
//! - [`split`]: one chapter per input file, further split before each
//!   heading at `ctx.opts.split_level`. Fills `title`, `headings`,
//!   `file_name` (`ctx.next_chapter_file()`), `source` (`ctx.current_input`)
//!   and `body_xhtml` via `dom::serialize_inner_xhtml`.
//! - [`fix_links`]: after all inputs are split, rewrite `#id` links that
//!   point into another chapter to `chNNN.xhtml#id`, and links between input
//!   files (`other.md#x`, `other.html`) to the matching chapter.
//!
//! The T0 stub removes scripts, takes the title hint and emits one chapter
//! per input.

use anyhow::Result;

use crate::ctx::Ctx;
use crate::dom::{self, Handle};
use crate::model::{Chapter, Heading};

/// Cleans the parsed document in place. See the module docs.
pub fn sanitize(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    if ctx.title_hint.is_none()
        && let Some(t) = dom::first_by_tag(doc, "title")
    {
        let text = dom::text_content(&t).trim().to_string();
        if !text.is_empty() {
            ctx.title_hint = Some(text);
        }
    }
    for tag in ["script", "noscript", "iframe", "object", "embed", "form"] {
        for n in dom::select_by_tag(doc, tag) {
            dom::remove(&n);
        }
    }
    Ok(())
}

/// Splits the document into chapters. See the module docs.
pub fn split(doc: &Handle, ctx: &mut Ctx) -> Result<Vec<Chapter>> {
    let body = dom::body(doc).unwrap_or_else(|| doc.clone());
    let mut headings = Vec::new();
    for n in dom::select_by_tag(&body, "*") {
        let level = match dom::tag_name(&n).as_deref() {
            Some("h1") => 1,
            Some("h2") => 2,
            Some("h3") => 3,
            Some("h4") => 4,
            Some("h5") => 5,
            Some("h6") => 6,
            _ => continue,
        };
        let text = dom::text_content(&n).trim().to_string();
        let id = match dom::get_attr(&n, "id") {
            Some(id) => id,
            None => {
                let id = ctx.unique_id("h");
                dom::set_attr(&n, "id", &id);
                id
            }
        };
        headings.push(Heading { level, text, id });
    }
    let title = headings
        .iter()
        .find(|h| h.level == 1)
        .map(|h| h.text.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            ctx.current_input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Chapter".into());
    if ctx.title_hint.is_none()
        && let Some(h1) = headings.iter().find(|h| h.level == 1)
    {
        ctx.title_hint = Some(h1.text.clone());
    }
    Ok(vec![Chapter {
        title,
        file_name: ctx.next_chapter_file(),
        body_xhtml: dom::serialize_inner_xhtml(&body),
        headings,
        in_toc: true,
        source: Some(ctx.current_input.clone()),
    }])
}

/// Rewrites cross-chapter and cross-file links. See the module docs.
pub fn fix_links(chapters: &mut [Chapter]) {
    let _ = chapters;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;

    #[test]
    fn stub_one_chapter() {
        let mut ctx = Ctx::new(Options::default());
        ctx.current_input = "x/intro.md".into();
        let doc = dom::parse("<title>T</title><script>x</script><h1>Hello</h1><p>a</p>");
        sanitize(&doc, &mut ctx).unwrap();
        let ch = split(&doc, &mut ctx).unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].title, "Hello");
        assert_eq!(ch[0].file_name, "ch001.xhtml");
        assert!(!ch[0].body_xhtml.contains("script"));
        assert_eq!(ctx.title_hint.as_deref(), Some("T"));
        fix_links(&mut []);
    }
}

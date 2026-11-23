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
//! The T0 stub extracts the front matter and wraps the rest in `<pre>`.

use anyhow::Result;

use crate::dom::escape_into;

/// Output of [`render`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rendered {
    /// HTML fragment (no `<html>`/`<body>` wrapper).
    pub html: String,
    /// Raw YAML front matter without the `---` lines, if present.
    pub front_matter: Option<String>,
}

/// Renders Markdown to an HTML fragment. See the module docs.
pub fn render(src: &str) -> Result<Rendered> {
    let (front_matter, body) = split_front_matter(src);
    let mut html = String::from("<pre>");
    escape_into(body, false, &mut html);
    html.push_str("</pre>");
    Ok(Rendered { html, front_matter })
}

/// Splits a leading `---` YAML block off `src`.
fn split_front_matter(src: &str) -> (Option<String>, &str) {
    let s = src.strip_prefix('\u{feff}').unwrap_or(src);
    let Some(rest) = s
        .strip_prefix("---\n")
        .or_else(|| s.strip_prefix("---\r\n"))
    else {
        return (None, s);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" || line.trim_end() == "..." {
            let fm = rest[..offset].to_string();
            return (Some(fm), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_extracts_front_matter() {
        let r = render("---\ntitle: X\n---\n# Hi & bye\n").unwrap();
        assert_eq!(r.front_matter.as_deref(), Some("title: X\n"));
        assert_eq!(r.html, "<pre># Hi &amp; bye\n</pre>");
        let r = render("no front matter").unwrap();
        assert!(r.front_matter.is_none());
    }
}

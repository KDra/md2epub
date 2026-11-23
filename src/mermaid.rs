//! Mermaid diagrams (owned by T7, behind the `mermaid` cargo feature; best
//! effort).
//!
//! Contract:
//! - [`render`]: Mermaid source to an SVG string, using an in-process
//!   renderer if available, otherwise `mmdc` when it is on `PATH` (temp files
//!   in a temp dir, 30 s timeout).
//! - [`transform`]: replace each `<pre class="mermaid">SOURCE</pre>` with
//!   `<figure class="mermaid"><img data-res="1" ...></figure>` through
//!   `ctx.add_vector`. When nothing can render a diagram, leave it as a normal
//!   code block and warn once per run.
//!
//! The T0 stub cannot render: it leaves the blocks as code and warns once.

use anyhow::{Result, bail};

use crate::ctx::Ctx;
use crate::dom::{self, Handle};

/// Renders Mermaid source to SVG. See the module docs.
pub fn render(src: &str) -> Result<String> {
    let _ = src;
    bail!("Mermaid rendering is not available in this build")
}

/// Replaces Mermaid placeholders in the document. See the module docs.
pub fn transform(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    let blocks: Vec<Handle> = dom::select_by_tag(doc, "pre")
        .into_iter()
        .filter(|p| dom::has_class(p, "mermaid"))
        .collect();
    if !blocks.is_empty() {
        ctx.warn(format!(
            "{} Mermaid diagram(s) left as code: no renderer available",
            blocks.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;

    #[test]
    fn stub_warns_and_keeps_code() {
        let mut ctx = Ctx::new(Options::default());
        let doc = dom::parse(r#"<pre class="mermaid">graph TD; A--&gt;B</pre>"#);
        transform(&doc, &mut ctx).unwrap();
        assert_eq!(ctx.warnings().len(), 1);
        assert_eq!(dom::select_by_tag(&doc, "pre").len(), 1);
        assert!(render("graph TD").is_err());
    }
}

//! LaTeX math (owned by T6, behind the `math` cargo feature).
//!
//! Contract:
//! - [`render_svg`]: LaTeX to a standalone SVG plus metrics in `em`
//!   (`depth_em` = distance below the baseline, used for `vertical-align`).
//! - [`render_mathml`]: LaTeX to a `<math>` element string.
//! - [`transform`]: replace each `<span data-math-style="inline|display">LATEX</span>`
//!   according to `ctx.opts.math`:
//!   `image` → `ctx.add_vector` then
//!   `<img data-res="1" class="math-inline|math-display" alt="LATEX">` sized in
//!   `em` with `vertical-align` from the depth; `mathml` → inline `<math>`;
//!   `off` → the LaTeX in `<code>`. On a render failure keep the source in
//!   `<code class="math-error">` and warn. Under the `scribe` profile,
//!   `mathml` mode warns that Send to Kindle may reject the file.
//!
//! The T0 stub cannot render: it turns every placeholder into `<code>`
//! (class `math-inline`/`math-display`).

use anyhow::{Result, bail};

use crate::ctx::Ctx;
use crate::dom::{self, Handle};

/// Rendered math as SVG with baseline metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct MathSvg {
    /// Standalone SVG document.
    pub svg: String,
    /// Width in `em`.
    pub width_em: f32,
    /// Total height in `em`.
    pub height_em: f32,
    /// Depth below the baseline in `em` (positive).
    pub depth_em: f32,
}

/// Renders LaTeX math to SVG. See the module docs.
pub fn render_svg(latex: &str, display: bool) -> Result<MathSvg> {
    let _ = (latex, display);
    bail!("math rendering is not available in this build")
}

/// Converts LaTeX math to a MathML `<math>` element. See the module docs.
pub fn render_mathml(latex: &str, display: bool) -> Result<String> {
    let _ = (latex, display);
    bail!("MathML conversion is not available in this build")
}

/// Replaces math placeholders in the document. See the module docs.
pub fn transform(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    let _ = &ctx;
    for span in dom::select_by_tag(doc, "span") {
        let Some(style) = dom::get_attr(&span, "data-math-style") else {
            continue;
        };
        let class = if style == "display" {
            "math-display"
        } else {
            "math-inline"
        };
        let latex = dom::text_content(&span);
        let code = dom::create_element("code", &[("class", class)]);
        dom::append_child(&code, &dom::create_text(&latex));
        dom::replace_with(&span, &code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;

    #[test]
    fn stub_falls_back_to_code() {
        let mut ctx = Ctx::new(Options::default());
        let doc = dom::parse(r#"<p><span data-math-style="inline">x^2</span></p>"#);
        transform(&doc, &mut ctx).unwrap();
        let out = dom::serialize_xhtml(&dom::body(&doc).unwrap());
        assert!(
            out.contains(r#"<code class="math-inline">x^2</code>"#),
            "{out}"
        );
        assert!(render_svg("x", false).is_err());
        assert!(render_mathml("x", false).is_err());
    }
}

//! LaTeX math (owned by T6). Image rendering sits behind the `math` cargo
//! feature; MathML conversion is always available.
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
//!   `<code class="math-error">` and warn. (`config::finalize` warns when
//!   `mathml` is chosen for a device profile.)
//! - Under the device profiles (`profile.flatten_alpha`) no `<math>` may
//!   remain unless `mathml` was chosen: raw MathML from the input is
//!   rendered from its TeX annotation (or `alttext`) like a placeholder, or
//!   replaced by its text in `<code class="math-error">` with a warning.
//! - Rendering runs on a worker thread with a time budget
//!   ([`RENDER_TIMEOUT`]); self-referential `\newcommand`/`\def`
//!   definitions are refused up front, since they never terminate.
//! - Math images are rasterized at [`MATH_RASTER_SCALE`] (about 160 px per
//!   em), so formulas stay sharp when the reader enlarges the text.
//!
//! Rendering route (see the T6 spike notes in the final report):
//! LaTeX is translated to Typst math with `mitex`, evaluated against the
//! vendored MiTeX command scope (`assets/math/mitex/`), laid out by the
//! `typst` library with the embedded New Computer Modern Math font and
//! exported with `typst-svg` (glyphs become paths, so the SVG needs no
//! fonts). MathML comes from `pulldown-latex`.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use pulldown_latex::{Parser, RenderConfig, Storage, config::DisplayMode, push_mathml};

use crate::ctx::Ctx;
use crate::dom::{self, Handle};
use crate::model::MathMode;

/// Longest one formula may take to render before it is given up.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(20);

/// Raster scale for math images. The SVG's nominal size is 20 pt (about
/// 26.7 px) per em, so this gives about 160 px per em: crisp at 300 ppi
/// even when the reader enlarges the text.
pub const MATH_RASTER_SCALE: f32 = 6.0;

/// Longest LaTeX source accepted by the renderers (guards against runaway input).
const MAX_LATEX_LEN: usize = 20_000;

/// Fallback text size in CSS pixels per `em`, used for the `width`/`height`
/// attributes of math images (CSS in `em` takes precedence where supported).
const FALLBACK_PX_PER_EM: f32 = 16.0;

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
///
/// `display` selects display style (large operators with limits above and
/// below, full-size fractions). Errors on invalid LaTeX, on commands the
/// renderer does not know and in builds without the `math` feature.
pub fn render_svg(latex: &str, display: bool) -> Result<MathSvg> {
    let latex = check_source(latex)?;
    check_macros(latex)?;
    render_within(latex, display, RENDER_TIMEOUT)
}

/// Renders on a worker thread and gives up after `timeout` (the thread is
/// then left to finish on its own).
fn render_within(latex: &str, display: bool, timeout: Duration) -> Result<MathSvg> {
    let latex = latex.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("md2epub-math".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let result = std::panic::catch_unwind(|| render::render(&latex, display))
                .unwrap_or_else(|_| Err(anyhow!("the math renderer failed on this input")));
            let _ = tx.send(result);
        })
        .context("starting the math renderer")?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => bail!(
            "rendering took longer than {} s and was abandoned",
            timeout.as_secs()
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("the math renderer failed on this input")
        }
    }
}

/// Refuses macro definitions whose body uses the macro itself
/// (`\newcommand{\z}[1]{\z{#1}}`, `\def\a{\a}`): expanding them never ends.
fn check_macros(latex: &str) -> Result<()> {
    const DEFINERS: &[&str] = &[
        "newcommand",
        "renewcommand",
        "providecommand",
        "DeclareMathOperator",
        "def",
        "gdef",
        "edef",
        "xdef",
    ];
    let b = latex.as_bytes();
    let mut i = 0;
    while let Some(off) = latex[i..].find('\\') {
        let at = i + off + 1;
        let word_end = at
            + latex[at..]
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(latex.len() - at);
        let word = &latex[at..word_end];
        i = word_end.max(at);
        if !DEFINERS.contains(&word) {
            continue;
        }
        // The macro name: `{\name}` or `\name`, after an optional `*`.
        let mut k = word_end;
        while k < b.len() && (b[k].is_ascii_whitespace() || b[k] == b'*') {
            k += 1;
        }
        if b.get(k) == Some(&b'{') {
            k += 1;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
        }
        if b.get(k) != Some(&b'\\') {
            continue;
        }
        let name_start = k + 1;
        let name_end = name_start
            + latex[name_start..]
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(latex.len() - name_start);
        let name = &latex[name_start..name_end];
        if name.is_empty() {
            continue;
        }
        // The body: the first balanced group after the name.
        let Some(open) = latex[name_end..].find('{').map(|o| name_end + o) else {
            continue;
        };
        let mut depth = 0usize;
        let mut close = latex.len();
        for (j, c) in latex[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + j;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &latex[open..close];
        let needle = format!("\\{name}");
        let recursive = body
            .match_indices(&needle)
            .any(|(p, _)| !body[p + needle.len()..].starts_with(|c: char| c.is_ascii_alphabetic()));
        if recursive {
            bail!("the macro \\{name} is defined in terms of itself");
        }
    }
    Ok(())
}

/// Converts LaTeX math to a MathML `<math>` element. See the module docs.
///
/// The element carries `display="inline|block"` and a TeX annotation with
/// the source. Errors when the LaTeX does not parse.
pub fn render_mathml(latex: &str, display: bool) -> Result<String> {
    let latex = check_source(latex)?;
    let storage = Storage::new();
    let events: Vec<_> = Parser::new(latex, &storage).collect();
    if let Some(err) = events.iter().find_map(|e| e.as_ref().err()) {
        bail!("{}", first_line(&err.to_string()));
    }
    let escaped = dom::escape(latex);
    let config = RenderConfig {
        display_mode: if display {
            DisplayMode::Block
        } else {
            DisplayMode::Inline
        },
        // The annotation is copied into the output as is: escape it.
        annotation: Some(&escaped),
        ..RenderConfig::default()
    };
    let mut out = String::new();
    let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        push_mathml(&mut out, events.into_iter(), config)
    }));
    match written {
        Ok(Ok(())) => Ok(out),
        Ok(Err(e)) => Err(anyhow!("writing MathML: {e}")),
        Err(_) => Err(anyhow!("the MathML converter failed on this input")),
    }
}

/// Replaces math placeholders in the document. See the module docs.
pub fn transform(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    let spans: Vec<Handle> = dom::select_by_tag(doc, "span")
        .into_iter()
        .filter(|s| dom::has_attr(s, "data-math-style"))
        .collect();
    // Raw MathML must go on device profiles unless MathML was asked for.
    let convert_raw = ctx.opts.profile.flatten_alpha && ctx.opts.math != MathMode::Mathml;
    let raw: Vec<Handle> = if convert_raw {
        dom::select_by_tag(doc, "math")
            .into_iter()
            .filter(|m| is_mathml(m) && !has_math_ancestor(m))
            .collect()
    } else {
        Vec::new()
    };
    if spans.is_empty() && raw.is_empty() {
        return Ok(());
    }

    let mut mode = ctx.opts.math;
    if mode == MathMode::Image && !cfg!(feature = "math") {
        ctx.warn(
            "math: this build has no math renderer (feature `math` is off); \
             equations are kept as LaTeX source",
        );
        mode = MathMode::Off;
    }
    for m in raw {
        replace_raw_math(&m, mode, ctx);
    }

    for span in spans {
        let display = dom::get_attr(&span, "data-math-style")
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("display"));
        let latex = dom::text_content(&span);
        let latex = latex.trim();
        let class = if display {
            "math-display"
        } else {
            "math-inline"
        };

        let replacement = match mode {
            MathMode::Off => Ok(vec![code_node(class, latex)]),
            MathMode::Image => image_node(latex, display, class, ctx).map(|n| vec![n]),
            MathMode::Mathml => mathml_nodes(latex, display, class),
        };
        match replacement {
            Ok(nodes) => dom::replace_with_many(&span, &nodes),
            Err(err) => {
                ctx.warn(format!(
                    "math: could not render `{}`: {err:#}",
                    shorten(latex)
                ));
                dom::replace_with(&span, &code_node(&format!("math-error {class}"), latex));
            }
        }
    }
    Ok(())
}

fn is_mathml(node: &Handle) -> bool {
    dom::namespace(node).is_some_and(|ns| ns == dom::MATHML_NS)
}

fn has_math_ancestor(node: &Handle) -> bool {
    let mut cur = dom::parent(node);
    while let Some(p) = cur {
        if dom::is_tag(&p, "math") && is_mathml(&p) {
            return true;
        }
        cur = dom::parent(&p);
    }
    false
}

/// The TeX source of a raw `<math>` element: its
/// `<annotation encoding="application/x-tex">`, else its `alttext`.
fn raw_math_source(math: &Handle) -> Option<String> {
    let annotation = dom::select_by_tag(math, "annotation")
        .into_iter()
        .find(|a| {
            dom::get_attr(a, "encoding").is_some_and(|e| {
                let e = e.trim().to_ascii_lowercase();
                e == "application/x-tex" || e == "text/x-tex" || e == "application/x-latex"
            })
        });
    annotation
        .map(|a| dom::text_content(&a))
        .or_else(|| dom::get_attr(math, "alttext"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Replaces a raw `<math>` element (from the input) by an image, the TeX
/// source in `<code>`, or its text in `<code class="math-error">`.
fn replace_raw_math(math: &Handle, mode: MathMode, ctx: &mut Ctx) {
    let display = dom::get_attr(math, "display")
        .or_else(|| dom::get_attr(math, "mode"))
        .is_some_and(|d| {
            let d = d.trim().to_ascii_lowercase();
            d == "block" || d == "display"
        });
    let class = if display {
        "math-display"
    } else {
        "math-inline"
    };
    let source = raw_math_source(math);
    let replacement = match (&source, mode) {
        (Some(tex), MathMode::Image) => match image_node(tex, display, class, ctx) {
            Ok(img) => img,
            Err(err) => {
                ctx.warn(format!(
                    "math: could not render MathML `{}`: {err:#}",
                    shorten(tex)
                ));
                code_node(&format!("math-error {class}"), tex)
            }
        },
        (Some(tex), _) => code_node(class, tex),
        (None, _) => {
            let text = dom::text_content(math)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            ctx.warn(format!(
                "math: MathML without a TeX annotation cannot be shown under the {} profile; \
                 kept as text: `{}`",
                ctx.opts.profile.name,
                shorten(&text)
            ));
            code_node(&format!("math-error {class}"), &text)
        }
    };
    dom::replace_with(math, &replacement);
}

/// `<code class="...">LATEX</code>`.
fn code_node(class: &str, latex: &str) -> Handle {
    let code = dom::create_element("code", &[("class", class)]);
    dom::append_child(&code, &dom::create_text(latex));
    code
}

/// Renders to SVG, stores it through `ctx.add_vector` and builds the `<img>`.
fn image_node(latex: &str, display: bool, class: &str, ctx: &mut Ctx) -> Result<Handle> {
    let math = render_svg(latex, display)?;
    let dir = ctx.base_dir.clone();
    let img = ctx.add_vector_scaled(&math.svg, Some(&dir), MATH_RASTER_SCALE)?;
    let style = if display {
        // Width only, so `max-width: 100%` from the stylesheet can shrink a
        // wide equation without distorting it.
        format!("width:{}em;max-width:100%;height:auto", em(math.width_em))
    } else {
        format!(
            "width:{}em;height:{}em;vertical-align:{}em",
            em(math.width_em),
            em(math.height_em),
            em(-math.depth_em)
        )
    };
    let width = px_attr(math.width_em);
    let height = px_attr(math.height_em);
    let mut attrs = vec![
        ("src", img.src.as_str()),
        ("alt", latex),
        ("class", class),
        ("style", style.as_str()),
        ("data-res", "1"),
    ];
    if !display {
        attrs.push(("width", width.as_str()));
        attrs.push(("height", height.as_str()));
    }
    Ok(dom::create_element("img", &attrs))
}

/// Converts to MathML and parses it into DOM nodes.
fn mathml_nodes(latex: &str, display: bool, class: &str) -> Result<Vec<Handle>> {
    let mathml = render_mathml(latex, display)?;
    let nodes = dom::parse_fragment(&mathml);
    let math = nodes
        .iter()
        .find(|n| dom::is_tag(n, "math"))
        .ok_or_else(|| anyhow!("the MathML converter produced no <math> element"))?;
    dom::set_attr(math, "class", class);
    dom::set_attr(math, "alttext", latex);
    Ok(nodes)
}

/// Formats an `em` length with up to three decimals, trimming zeros.
fn em(v: f32) -> String {
    let v = if v.is_finite() { v } else { 0.0 };
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

fn px_attr(em: f32) -> String {
    let px = if em.is_finite() {
        (em * FALLBACK_PX_PER_EM).round()
    } else {
        1.0
    };
    (px.max(1.0) as u32).to_string()
}

fn check_source(latex: &str) -> Result<&str> {
    let latex = latex.trim();
    if latex.is_empty() {
        bail!("empty math");
    }
    if latex.len() > MAX_LATEX_LEN {
        bail!("math source is longer than {MAX_LATEX_LEN} bytes");
    }
    Ok(latex)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

/// LaTeX shortened for warnings.
fn shorten(latex: &str) -> String {
    const MAX: usize = 60;
    let one_line = latex.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let cut: String = one_line.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

#[cfg(not(feature = "math"))]
mod render {
    use super::MathSvg;
    use anyhow::{Result, bail};

    pub(super) fn render(_latex: &str, _display: bool) -> Result<MathSvg> {
        bail!("math rendering is not available in this build (feature `math` is off)")
    }
}

#[cfg(feature = "math")]
mod render {
    //! Typst-based renderer: LaTeX → (mitex) → Typst math → layout → SVG.

    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::LazyLock;

    use anyhow::{Result, anyhow, bail};
    use typst::diag::{FileError, FileResult};
    use typst::foundations::{Bytes, Datetime, Duration};
    use typst::introspection::Tag;
    use typst::layout::{Abs, Frame, FrameItem, Point};
    use typst::math::EquationElem;
    use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
    use typst::text::{Font, FontBook};
    use typst::utils::LazyHash;
    use typst::{Library, LibraryExt, World};
    use typst_layout::PagedDocument;

    use super::{MathSvg, first_line};

    /// Typst text size; all metrics are divided by it to get `em`. Large
    /// enough that the 2× rasterization stays crisp at 300 ppi.
    const FONT_PT: f64 = 20.0;

    static MATH_FONT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/math/NewCMMath-Book.otf"
    ));
    static TEXT_FONT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/math/LibertinusSerif-Regular.otf"
    ));
    static PRELUDE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/math/mitex/prelude.typ"
    ));
    static STANDARD: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/math/mitex/standard.typ"
    ));

    /// State shared by every compilation: library, fonts and the MiTeX scope
    /// sources. Reusing the same sources lets Typst's memoization skip
    /// re-evaluating the scope for each equation.
    struct Shared {
        library: LazyHash<Library>,
        book: LazyHash<FontBook>,
        fonts: Vec<Font>,
        main_id: FileId,
        prelude: Source,
        standard: Source,
    }

    fn file_id(path: &str) -> Option<FileId> {
        let vpath = VirtualPath::new(path).ok()?;
        Some(RootedPath::new(VirtualRoot::Project, vpath).intern())
    }

    static SHARED: LazyLock<Option<Shared>> = LazyLock::new(|| {
        let fonts: Vec<Font> = [MATH_FONT, TEXT_FONT]
            .into_iter()
            .filter_map(|data| Font::new(Bytes::new(data), 0))
            .collect();
        Some(Shared {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            main_id: file_id("/main.typ")?,
            prelude: Source::new(file_id("/prelude.typ")?, PRELUDE.to_string()),
            standard: Source::new(file_id("/standard.typ")?, STANDARD.to_string()),
        })
    });

    /// A virtual project: `/main.typ` (one equation), the MiTeX scope files
    /// and the embedded fonts. No file system, no packages, no clock.
    struct MathWorld {
        shared: &'static Shared,
        main: Source,
    }

    impl World for MathWorld {
        fn library(&self) -> &LazyHash<Library> {
            &self.shared.library
        }

        fn book(&self) -> &LazyHash<FontBook> {
            &self.shared.book
        }

        fn main(&self) -> FileId {
            self.shared.main_id
        }

        fn source(&self, id: FileId) -> FileResult<Source> {
            if id == self.shared.main_id {
                Ok(self.main.clone())
            } else if id == self.shared.prelude.id() {
                Ok(self.shared.prelude.clone())
            } else if id == self.shared.standard.id() {
                Ok(self.shared.standard.clone())
            } else {
                Err(FileError::AccessDenied)
            }
        }

        fn file(&self, _id: FileId) -> FileResult<Bytes> {
            Err(FileError::AccessDenied)
        }

        fn font(&self, index: usize) -> Option<Font> {
            self.shared.fonts.get(index).cloned()
        }

        fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
            None
        }
    }

    pub(super) fn render(latex: &str, display: bool) -> Result<MathSvg> {
        let shared = SHARED
            .as_ref()
            .ok_or_else(|| anyhow!("math renderer failed to initialize"))?;
        let code = mitex::convert_math(latex, None)
            .map_err(|e| anyhow!("{}", first_line(&e).trim_start_matches("error: ")))?;
        let main = Source::new(shared.main_id, main_source(&code, display));
        let world = MathWorld { shared, main };
        match catch_unwind(AssertUnwindSafe(|| compile(&world))) {
            Ok(result) => result,
            Err(_) => bail!("the math renderer failed on this input"),
        }
    }

    /// The Typst document for one equation: an auto-sized page holding only
    /// the inline equation, with text edges at the glyph bounds so the page
    /// is exactly the equation's box.
    fn main_source(code: &str, display: bool) -> String {
        let body = if display {
            "math.display(body)"
        } else {
            "body"
        };
        format!(
            "#import \"/standard.typ\": package as latex-std\n\
             #set page(width: auto, height: auto, margin: 0pt, fill: none)\n\
             #set text(font: (\"New Computer Modern Math\", \"Libertinus Serif\"), \
             size: {FONT_PT}pt, fill: black, top-edge: \"bounds\", bottom-edge: \"bounds\")\n\
             #show math.equation: set text(font: \"New Computer Modern Math\")\n\
             #let body = eval(\"$\" + {} + \"$\", scope: latex-std.scope)\n\
             #math.equation(block: false, {body})\n",
            typst_string(code)
        )
    }

    fn compile(world: &MathWorld) -> Result<MathSvg> {
        let warned = typst::compile::<PagedDocument>(world);
        // Bound the memoization cache; recent entries (the MiTeX scope) stay.
        typst::comemo::evict(30);
        let doc = warned.output.map_err(|diags| {
            let msgs: Vec<String> = diags.iter().map(|d| d.message.to_string()).collect();
            anyhow!("{}", msgs.join("; "))
        })?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| anyhow!("the math renderer produced no output"))?;
        let frame = &page.frame;
        let (width, height) = (frame.width(), frame.height());
        if width <= Abs::zero() || height <= Abs::zero() {
            bail!("the formula renders to nothing");
        }
        let baseline = equation_baseline(frame, Point::zero()).unwrap_or(height);
        let depth = (height - baseline).max(Abs::zero());
        let svg = typst_svg::svg(page, &typst_svg::SvgOptions::default());
        Ok(MathSvg {
            svg,
            width_em: (width.to_pt() / FONT_PT) as f32,
            height_em: (height.to_pt() / FONT_PT) as f32,
            depth_em: (depth.to_pt() / FONT_PT) as f32,
        })
    }

    /// Absolute y of the equation's baseline. Inline layout places the
    /// equation's start tag at the line baseline.
    fn equation_baseline(frame: &Frame, offset: Point) -> Option<Abs> {
        for (pos, item) in frame.items() {
            let pos = offset + *pos;
            match item {
                FrameItem::Tag(Tag::Start(content, _)) if content.is::<EquationElem>() => {
                    return Some(pos.y);
                }
                FrameItem::Group(group) => {
                    // Groups here are never transformed (plain inline layout),
                    // so adding the offset is enough.
                    if let Some(y) = equation_baseline(&group.frame, pos) {
                        return Some(y);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// A Typst string literal.
    fn typst_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;
    use crate::profile;

    fn ctx_with(mode: MathMode) -> Ctx {
        Ctx::new(Options {
            math: mode,
            ..Options::default()
        })
    }

    fn run(html: &str, ctx: &mut Ctx) -> String {
        let doc = dom::parse(html);
        transform(&doc, ctx).unwrap();
        let out = dom::serialize_xhtml(&dom::body(&doc).unwrap());
        roxmltree::Document::parse(&out).unwrap_or_else(|e| panic!("not XML ({e}): {out}"));
        out
    }

    const SAMPLE: &str = r#"<p>Inline <span data-math-style="inline">E = mc^2</span> and</p><p><span data-math-style="display">\sum_{i=0}^n i^2</span></p>"#;

    #[test]
    fn off_mode_keeps_latex_in_code() {
        let mut ctx = ctx_with(MathMode::Off);
        let out = run(SAMPLE, &mut ctx);
        assert!(
            out.contains(r#"<code class="math-inline">E = mc^2</code>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<code class="math-display">\sum_{i=0}^n i^2</code>"#),
            "{out}"
        );
        assert!(!out.contains("data-math-style"));
        assert!(ctx.warnings().is_empty());
    }

    #[test]
    fn mathml_conversion() {
        let inline = render_mathml(r"\frac{a}{b}", false).unwrap();
        assert!(inline.starts_with("<math"), "{inline}");
        assert!(inline.contains("<mfrac>"), "{inline}");
        assert!(inline.contains(r#"display="inline""#), "{inline}");
        let block = render_mathml(r"\begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix}", true).unwrap();
        assert!(block.contains(r#"display="block""#), "{block}");
        assert!(block.contains("<mtable"), "{block}");
        assert!(render_mathml(r"\frac{1}{", false).is_err());
        assert!(render_mathml(r"\nosuchcommand{x}", false).is_err());
        assert!(render_mathml("   ", false).is_err());
    }

    #[test]
    fn mathml_mode_inserts_math_elements() {
        let mut ctx = ctx_with(MathMode::Mathml);
        let html = r#"<p>a <span data-math-style="inline">\alpha < \beta \&amp; x</span> b <span data-math-style="display">\frac{1}{2}</span></p>"#;
        let out = run(html, &mut ctx);
        assert_eq!(out.matches("<math").count(), 2, "{out}");
        assert!(out.contains(dom::MATHML_NS), "{out}");
        assert!(out.contains(r#"class="math-inline""#), "{out}");
        assert!(out.contains(r#"class="math-display""#), "{out}");
        assert!(!out.contains("data-math-style"), "{out}");
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
    }

    #[test]
    fn mathml_mode_under_scribe_warns_once_in_config() {
        // The device-profile warning comes from `config::finalize`, once per
        // run, not from every document.
        let mut ctx = ctx_with(MathMode::Mathml);
        ctx.opts.profile = profile::by_name("scribe").unwrap();
        ctx.opts.svg = crate::model::SvgMode::Rasterize;
        let out = run(SAMPLE, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert_eq!(out.matches("<math").count(), 2, "{out}");
        let mut opts = ctx.opts.clone();
        let w = crate::config::finalize(&mut opts);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("Send to Kindle"));
    }

    #[test]
    fn mathml_annotation_is_escaped() {
        let tex = r"\text{</annotation></semantics></math><b>bold</b> & <script>x</script>}";
        let m = render_mathml(tex, false).unwrap();
        let mut ctx = ctx_with(MathMode::Mathml);
        let out = run(
            &format!(
                r#"<p><span data-math-style="inline">{}</span></p>"#,
                dom::escape(tex)
            ),
            &mut ctx,
        );
        assert!(
            !out.contains("<script") && !out.contains("<b>"),
            "{m}\n{out}"
        );
        assert_eq!(out.matches("<math").count(), 1, "{out}");
        // `a<b` keeps both sides.
        let mut ctx = ctx_with(MathMode::Mathml);
        let out = run(
            r#"<p><span data-math-style="inline">a&lt;b</span></p>"#,
            &mut ctx,
        );
        assert!(out.contains("a&lt;b</annotation>"), "{out}");
    }

    #[test]
    fn raw_mathml_is_converted_on_device_profiles() {
        let html = r#"<p>a <math><mi>x</mi></math> b
            <math display="block"><semantics><mrow><mi>z</mi></mrow><annotation encoding="application/x-tex">z^2</annotation></semantics></math>
            <math alttext="\alpha"><mi>α</mi></math></p>"#;
        // Off mode: TeX in code, text otherwise.
        let mut ctx = ctx_with(MathMode::Off);
        ctx.opts.profile = profile::by_name("scribe").unwrap();
        let out = run(html, &mut ctx);
        assert!(!out.contains("<math"), "{out}");
        assert!(
            out.contains(r#"<code class="math-display">z^2</code>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<code class="math-inline">\alpha</code>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<code class="math-error math-inline">x</code>"#),
            "{out}"
        );
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        // Generic profile and MathML mode keep raw MathML.
        let mut ctx = ctx_with(MathMode::Image);
        assert_eq!(run(html, &mut ctx).matches("<math").count(), 3);
        let mut ctx = ctx_with(MathMode::Mathml);
        ctx.opts.profile = profile::by_name("remarkable").unwrap();
        assert_eq!(run(html, &mut ctx).matches("<math").count(), 3);
    }

    #[test]
    fn recursive_macros_are_refused() {
        for bad in [
            r"\newcommand{\z}[1]{\z{#1}}\z{a}",
            r"\renewcommand\z{x\z}",
            r"\def\a#1{\a{#1}}\a1",
            r"\newcommand*{ \q }{\q}",
        ] {
            let err = check_macros(bad).unwrap_err().to_string();
            assert!(err.contains("defined in terms of itself"), "{bad}: {err}");
            assert!(render_svg(bad, false).is_err());
        }
        for ok in [
            r"\newcommand{\z}[1]{\zeta{#1}}\z{a}",
            r"\def\a{b}",
            r"\newcommand",
            r"x \\ y",
        ] {
            assert!(check_macros(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn render_timeout_gives_up() {
        let start = std::time::Instant::now();
        // A zero budget can never be met; the call must return promptly.
        let r = render_within("x", false, Duration::from_millis(0));
        assert!(start.elapsed() < Duration::from_secs(5));
        if let Err(e) = r {
            let msg = e.to_string();
            assert!(
                msg.contains("abandoned") || msg.contains("not available"),
                "{msg}"
            );
        }
    }

    #[test]
    fn mathml_failure_falls_back_to_code() {
        let mut ctx = ctx_with(MathMode::Mathml);
        let out = run(
            r#"<p><span data-math-style="inline">\frac{1}{</span></p>"#,
            &mut ctx,
        );
        assert!(
            out.contains(r#"<code class="math-error math-inline">\frac{1}{</code>"#),
            "{out}"
        );
        assert_eq!(ctx.warnings().len(), 1);
    }

    #[test]
    fn helpers() {
        assert_eq!(em(1.5), "1.5");
        assert_eq!(em(-0.25), "-0.25");
        assert_eq!(em(0.0), "0");
        assert_eq!(em(-0.0001), "0");
        assert_eq!(em(f32::NAN), "0");
        assert_eq!(em(2.0), "2");
        assert_eq!(px_attr(1.0), "16");
        assert_eq!(px_attr(0.0), "1");
        assert_eq!(shorten("a\n  b"), "a b");
        assert!(shorten(&"x".repeat(100)).ends_with('…'));
        assert!(check_source(&"x".repeat(MAX_LATEX_LEN + 1)).is_err());
    }

    #[cfg(not(feature = "math"))]
    #[test]
    fn image_mode_without_renderer_keeps_source() {
        assert!(render_svg("x", false).is_err());
        let mut ctx = ctx_with(MathMode::Image);
        let out = run(SAMPLE, &mut ctx);
        assert!(
            out.contains(r#"<code class="math-inline">E = mc^2</code>"#),
            "{out}"
        );
        assert_eq!(ctx.warnings().len(), 1);
    }

    #[cfg(feature = "math")]
    mod rendering {
        use super::*;
        use crate::model::SvgMode;

        fn assert_svg(m: &MathSvg) {
            let doc = roxmltree::Document::parse(&m.svg).expect("SVG is XML");
            assert_eq!(doc.root_element().tag_name().name(), "svg");
            assert!(m.svg.contains("<path"), "glyphs are outlined");
            assert!(m.width_em > 0.0 && m.height_em > 0.0, "{m:?}");
            assert!(m.depth_em >= 0.0 && m.depth_em < m.height_em, "{m:?}");
        }

        #[test]
        fn renders_inline_with_baseline_metrics() {
            let x = render_svg("x", false).unwrap();
            assert_svg(&x);
            // A lone x has (almost) no descender and is about half an em tall.
            assert!(x.depth_em < 0.05, "{x:?}");
            assert!(x.height_em > 0.3 && x.height_em < 0.8, "{x:?}");

            let frac = render_svg(r"\frac{a}{b}", false).unwrap();
            assert_svg(&frac);
            // A fraction hangs below the baseline and is taller than x.
            assert!(frac.depth_em > 0.2, "{frac:?}");
            assert!(frac.height_em > x.height_em, "{frac:?}");

            let y = render_svg("y", false).unwrap();
            assert!(y.depth_em > 0.1, "descender gives depth: {y:?}");
        }

        #[test]
        fn renders_display_style() {
            let inline = render_svg(r"\sum_{i=0}^n i^2", false).unwrap();
            let display = render_svg(r"\sum_{i=0}^n i^2", true).unwrap();
            assert_svg(&inline);
            assert_svg(&display);
            // Display style puts the limits above and below: taller.
            assert!(
                display.height_em > inline.height_em * 1.3,
                "{inline:?} {display:?}"
            );
        }

        #[test]
        fn renders_common_constructs() {
            for latex in [
                r"\frac{a}{b}",
                r"\sum_{i=0}^n i",
                r"\begin{aligned} a &= b + c \\ d &= e \end{aligned}",
                r"\begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix}",
                r"\begin{bmatrix} a & b \\ c & d \end{bmatrix}",
                r"\begin{cases} 1 & x > 0 \\ 0 & \text{otherwise} \end{cases}",
                r"\alpha + \beta = \Gamma \cdot \Omega",
                r"\sqrt{x} + \sqrt[3]{y}",
                r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}",
                r"\mathbb{R} \to \mathbf{v} \xrightarrow{f} \mathrm{d}x",
                r"\left( \frac{1}{2} \right) \leq \hat{x} \neq \vec{y}",
                r#"\text{quote " and backslash \\ inside}"#,
            ] {
                let m = render_svg(latex, true).unwrap_or_else(|e| panic!("{latex}: {e:#}"));
                assert_svg(&m);
            }
        }

        #[test]
        fn typst_code_in_text_is_not_executed() {
            // mitex escapes `#`, so these render as literal text (and the
            // world has no file access anyway).
            for latex in [r"\text{#while true {}}", r#"\text{#read("/etc/passwd")}"#] {
                let m = render_svg(latex, false).unwrap_or_else(|e| panic!("{latex}: {e:#}"));
                assert_svg(&m);
            }
            assert!(render_svg(r"\text{Grüße αβγ}", false).is_ok());
        }

        #[test]
        fn render_failures_are_errors() {
            assert!(render_svg(r"\nosuchcommand{x}", false).is_err());
            assert!(render_svg("", false).is_err());
            assert!(render_svg(&"x".repeat(MAX_LATEX_LEN + 1), false).is_err());
            // Garbage must never panic.
            for junk in [
                r"\frac{1}{",
                "}}}{{{",
                r"\begin{matrix}",
                r"#",
                r"$$",
                r"\\\\",
                "^_^",
            ] {
                let _ = render_svg(junk, false);
                let _ = render_svg(junk, true);
                let _ = render_mathml(junk, false);
            }
        }

        #[test]
        fn image_mode_keep_svg() {
            let mut ctx = ctx_with(MathMode::Image);
            ctx.opts.svg = SvgMode::Keep;
            let out = run(SAMPLE, &mut ctx);
            assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
            assert_eq!(out.matches("<img").count(), 2, "{out}");
            assert!(out.contains(r#"class="math-inline""#), "{out}");
            assert!(out.contains(r#"class="math-display""#), "{out}");
            assert!(out.contains(r#"alt="E = mc^2""#), "{out}");
            assert!(out.contains(r#"data-res="1""#), "{out}");
            assert!(out.contains("vertical-align:-"), "{out}");
            assert!(out.contains("em;height:"), "{out}");
            assert!(!out.contains("data-math-style"), "{out}");
            let res = ctx.resources();
            assert_eq!(res.len(), 2);
            assert!(res.iter().all(|r| r.media_type == "image/svg+xml"));
            assert!(out.contains(&format!("../{}", res[0].href)), "{out}");
        }

        #[test]
        fn image_mode_rasterized() {
            let mut ctx = ctx_with(MathMode::Image);
            ctx.opts.profile = profile::by_name("scribe").unwrap();
            ctx.opts.svg = SvgMode::Rasterize;
            let out = run(SAMPLE, &mut ctx);
            assert_eq!(out.matches("<img").count(), 2, "{out}");
            assert!(ctx.resources().iter().all(|r| r.media_type == "image/png"));
            assert!(!out.contains("<svg") && !out.contains("<math"), "{out}");
        }

        #[test]
        fn raw_mathml_becomes_images_under_scribe() {
            let mut ctx = ctx_with(MathMode::Image);
            ctx.opts.profile = profile::by_name("scribe").unwrap();
            ctx.opts.svg = SvgMode::Rasterize;
            let out = run(
                r#"<p><math display="block"><semantics><mi>z</mi><annotation encoding="application/x-tex">z^2</annotation></semantics></math></p>"#,
                &mut ctx,
            );
            assert!(!out.contains("<math"), "{out}");
            assert!(out.contains(r#"class="math-display""#), "{out}");
            assert!(out.contains(r#"alt="z^2""#), "{out}");
            assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        }

        #[test]
        fn math_rasters_have_about_160_px_per_em() {
            let mut ctx = ctx_with(MathMode::Image);
            ctx.opts.svg = SvgMode::Rasterize;
            run(
                r#"<p><span data-math-style="inline">E = mc^2</span></p>"#,
                &mut ctx,
            );
            let m = render_svg("E = mc^2", false).unwrap();
            let png = image::load_from_memory(&ctx.resources()[0].data).unwrap();
            let px_per_em = png.width() as f32 / m.width_em;
            assert!((140.0..200.0).contains(&px_per_em), "{px_per_em}");
        }

        #[test]
        fn image_mode_failure_falls_back_to_code() {
            let mut ctx = ctx_with(MathMode::Image);
            let out = run(
                r#"<p>ok <span data-math-style="inline">x</span> bad <span data-math-style="inline">\nosuchcommand</span></p>"#,
                &mut ctx,
            );
            assert_eq!(out.matches("<img").count(), 1, "{out}");
            assert!(
                out.contains(r#"<code class="math-error math-inline">\nosuchcommand</code>"#),
                "{out}"
            );
            assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
            assert!(ctx.warnings()[0].contains(r"\nosuchcommand"));
        }

        #[test]
        fn identical_math_is_stored_once() {
            let mut ctx = ctx_with(MathMode::Image);
            ctx.opts.svg = SvgMode::Keep;
            run(
                r#"<p><span data-math-style="inline">x</span><span data-math-style="inline">x</span></p>"#,
                &mut ctx,
            );
            assert_eq!(ctx.resources().len(), 1);
        }
    }
}

//! The conversion pipeline: inputs → DOM stages → chapters → EPUB.
//!
//! ```text
//! for each input file:
//!     html = markdown::render(src)?.html      (.md)   or file contents (.html)
//!     doc  = dom::parse(html)
//!     html::sanitize(&doc, ctx)
//!     mermaid::transform(&doc, ctx)
//!     math::transform(&doc, ctx)
//!     assets::transform(&doc, ctx)            (img, inline <svg>, CSS url())
//!     chapters += html::split(&doc, ctx)      (serializes each chapter body to XHTML)
//! html::fix_links(&mut chapters)
//! chapters = notes::insert(chapters, ctx)
//! epub::write(Book { meta, chapters, resources, css, cover }, out_path)
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::markdown::{self, Rendered};
use crate::model::{Book, Options};
use crate::{assets, config, dom, epub, html, math, mermaid, notes, style};

/// Kind of input file, decided by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Markdown,
    Html,
}

impl InputKind {
    /// Classifies a path by its extension (`.md`, `.markdown`, `.mdown`,
    /// `.mkd`, `.html`, `.htm`, `.xhtml`).
    pub fn of(path: &Path) -> Result<InputKind> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" | "mdown" | "mkd" => Ok(InputKind::Markdown),
            "html" | "htm" | "xhtml" => Ok(InputKind::Html),
            _ => bail!(
                "unsupported input '{}': expected a .md or .html file",
                path.display()
            ),
        }
    }
}

/// Result of a successful run.
#[derive(Debug, Clone)]
pub struct Report {
    /// Path of the written EPUB.
    pub output: PathBuf,
    /// Number of spine chapters (excluding the cover page).
    pub chapters: usize,
    /// Non-fatal problems collected during the run.
    pub warnings: Vec<String>,
}

/// Runs the whole conversion for a parsed command line.
pub fn run(cli: &Cli) -> Result<Report> {
    // First pass learns the inputs; the second applies the front matter of
    // the first Markdown input.
    let (_, inputs) = config::resolve(cli, None)?;
    let first = inputs.first().context("no input files")?;
    let mut first_rendered = None;
    if InputKind::of(first)? == InputKind::Markdown {
        first_rendered = Some(markdown::render(&read_input(first)?)?);
    }
    let front_matter = first_rendered
        .as_ref()
        .and_then(|r| r.front_matter.as_deref());
    let (opts, inputs) = config::resolve(cli, front_matter)?;
    let out = output_path(&opts, &inputs);
    let (book, ctx) = build_book(opts, &inputs, first_rendered)?;
    epub::write(&book, &out)?;
    Ok(Report {
        output: out,
        chapters: book.chapters.len(),
        warnings: ctx.warnings().to_vec(),
    })
}

/// Where the EPUB goes: `opts.output`, else the first input's stem with
/// `.epub` in the current directory.
pub fn output_path(opts: &Options, inputs: &[PathBuf]) -> PathBuf {
    if let Some(o) = &opts.output {
        return o.clone();
    }
    let stem = inputs
        .first()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".into());
    PathBuf::from(format!("{stem}.epub"))
}

/// Converts the inputs into an in-memory [`Book`]. `first_rendered` may hold
/// the already rendered first input to avoid rendering it twice.
pub fn build_book(
    opts: Options,
    inputs: &[PathBuf],
    mut first_rendered: Option<Rendered>,
) -> Result<(Book, Ctx)> {
    if inputs.is_empty() {
        bail!("no input files");
    }
    let mut ctx = Ctx::new(opts);
    let mut chapters = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        ctx.info(format!("processing {}", input.display()));
        ctx.current_input = input.clone();
        ctx.base_dir = match input.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let html = match InputKind::of(input)? {
            InputKind::Markdown => {
                let rendered = match first_rendered.take().filter(|_| i == 0) {
                    Some(r) => r,
                    None => markdown::render(&read_input(input)?)
                        .with_context(|| format!("rendering {}", input.display()))?,
                };
                rendered.html
            }
            InputKind::Html => read_input(input)?,
        };
        let doc = dom::parse(&html);
        html::sanitize(&doc, &mut ctx)?;
        mermaid::transform(&doc, &mut ctx)?;
        math::transform(&doc, &mut ctx)?;
        assets::transform(&doc, &mut ctx)?;
        chapters.extend(html::split(&doc, &mut ctx)?);
    }
    html::fix_links(&mut chapters);
    let chapters = notes::insert(chapters, &mut ctx)?;
    let css = style::build_css(&ctx.opts)?;

    let cover = match ctx.opts.cover.clone() {
        Some(path) => match assets::load_cover(&path, &mut ctx) {
            Ok(r) => Some(r),
            Err(e) => {
                ctx.warn(format!("cover not used: {e:#}"));
                None
            }
        },
        None => None,
    };

    let mut meta = ctx.opts.meta.clone();
    if meta.title.trim().is_empty() {
        meta.title = ctx
            .title_hint
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                inputs[0]
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "Untitled".into());
    }
    let book = Book {
        meta,
        chapters,
        resources: ctx.take_resources(),
        css,
        cover,
    };
    Ok((book, ctx))
}

/// Reads an input file as UTF-8 (invalid sequences are replaced).
fn read_input(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn input_kinds() {
        assert_eq!(
            InputKind::of(Path::new("a.MD")).unwrap(),
            InputKind::Markdown
        );
        assert_eq!(InputKind::of(Path::new("a.htm")).unwrap(), InputKind::Html);
        assert!(InputKind::of(Path::new("a.docx")).is_err());
        assert!(InputKind::of(Path::new("noext")).is_err());
    }

    #[test]
    fn builds_fixtures() {
        let inputs = vec![fixture("kitchen-sink.md"), fixture("page.html")];
        let (book, _ctx) = build_book(Options::default(), &inputs, None).unwrap();
        assert!(book.chapters.len() >= 2);
        assert!(!book.meta.title.is_empty());
        for ch in &book.chapters {
            crate::dom::tests_support::assert_well_formed(&ch.body_xhtml);
        }
    }

    #[test]
    fn writes_epub() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("k.epub");
        let opts = Options {
            output: Some(out.clone()),
            ..Options::default()
        };
        let inputs = vec![fixture("kitchen-sink.md")];
        assert_eq!(output_path(&opts, &inputs), out);
        let (book, _) = build_book(opts, &inputs, None).unwrap();
        epub::write(&book, &out).unwrap();
        assert!(out.is_file());
        assert_eq!(
            output_path(&Options::default(), &inputs),
            PathBuf::from("kitchen-sink.epub")
        );
    }
}

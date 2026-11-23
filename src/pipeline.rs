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

use anyhow::{Context as _, Result, anyhow, bail};

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::markdown::{self, Rendered};
use crate::model::{Book, Options};
use crate::{assets, config, dom, epub, html, math, mermaid, notes, style};

/// Stack size of the conversion thread. The DOM stages walk the tree
/// recursively, so deeply nested input needs more than the default stack.
const PIPELINE_STACK: usize = 256 * 1024 * 1024;

/// Deepest element nesting accepted in an input document. Real documents
/// stay far below this; deeper input is almost certainly broken or hostile.
pub const MAX_DOM_DEPTH: usize = 2_000;

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
    /// Whether the run was verbose (warnings were then already printed as
    /// they happened).
    pub verbose: bool,
}

/// Runs the whole conversion for a parsed command line, on a thread with a
/// large stack (see [`PIPELINE_STACK`]).
pub fn run(cli: &Cli) -> Result<Report> {
    let cli = cli.clone();
    let handle = std::thread::Builder::new()
        .name("md2epub".into())
        .stack_size(PIPELINE_STACK)
        .spawn(move || run_on_this_thread(&cli))
        .context("starting the conversion thread")?;
    match handle.join() {
        Ok(result) => result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown error".into());
            Err(anyhow!("internal error: {msg}"))
        }
    }
}

/// Runs the whole conversion on the calling thread.
pub fn run_on_this_thread(cli: &Cli) -> Result<Report> {
    // First pass learns the inputs; the second applies the front matter of
    // the first Markdown input.
    let (_, inputs) = config::resolve(cli, None)?;
    let first = inputs.first().context("no input files")?;
    let mut first_rendered = None;
    if InputKind::of(first)? == InputKind::Markdown {
        first_rendered = Some(markdown::render(&read_input(first)?.0)?);
    }
    let front_matter = first_rendered
        .as_ref()
        .and_then(|r| r.front_matter.as_deref());
    let (opts, inputs) = config::resolve(cli, front_matter)?;
    let out = output_path(&opts, &inputs);
    check_output(&out, &inputs, &opts)?;
    let (book, mut ctx) = build_book(opts, &inputs, first_rendered)?;
    let is_epub = out
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("epub"));
    if !is_epub {
        ctx.warn(format!(
            "output file {} does not end in .epub; readers and Send to Kindle may not recognize it",
            out.display()
        ));
    }
    epub::write(&book, &out)?;
    Ok(Report {
        output: out,
        chapters: book.chapters.len(),
        warnings: ctx.warnings().to_vec(),
        verbose: ctx.opts.verbose,
    })
}

/// Refuses an output path that would overwrite an input, the `--css` file
/// or the cover image, or that names a directory.
pub fn check_output(out: &Path, inputs: &[PathBuf], opts: &Options) -> Result<()> {
    if out.is_dir() {
        bail!("output path {} is a directory", out.display());
    }
    let out_key = normalized_output(out);
    let sources = inputs
        .iter()
        .map(|p| ("input", p))
        .chain(opts.extra_css.iter().map(|p| ("CSS file", p)))
        .chain(opts.cover.iter().map(|p| ("cover image", p)));
    for (what, p) in sources {
        if let Ok(c) = std::fs::canonicalize(p)
            && c == out_key
        {
            bail!(
                "output path {} is the same as the {what} {}; choose another -o",
                out.display(),
                p.display()
            );
        }
    }
    Ok(())
}

/// Absolute, symlink-free form of a path that may not exist yet.
fn normalized_output(out: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(out) {
        return c;
    }
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match (std::fs::canonicalize(parent), out.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name),
        _ => out.to_path_buf(),
    }
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
    let mut opts = opts;
    let notices = config::finalize(&mut opts);
    let mut ctx = Ctx::new(opts);
    for n in notices {
        ctx.warn(n);
    }
    let mut chapters = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        ctx.info(format!("processing {}", input.display()));
        ctx.current_input = input.clone();
        ctx.base_dir = match input.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let (text, encoding_warning) = read_input(input)?;
        if let Some(w) = encoding_warning {
            ctx.warn(w);
        }
        let html = match InputKind::of(input)? {
            InputKind::Markdown => {
                let rendered = match first_rendered.take().filter(|_| i == 0) {
                    Some(r) => r,
                    None => markdown::render(&text)
                        .with_context(|| format!("rendering {}", input.display()))?,
                };
                rendered.html
            }
            InputKind::Html if html::is_xhtml_input(input, &text) => html::xhtml_to_html(&text),
            InputKind::Html => text,
        };
        let doc = dom::parse(&html);
        let depth = dom::max_depth(&doc);
        if depth > MAX_DOM_DEPTH {
            bail!(
                "{}: elements are nested {depth} levels deep (the limit is {MAX_DOM_DEPTH})",
                input.display()
            );
        }
        html::sanitize(&doc, &mut ctx)?;
        mermaid::transform(&doc, &mut ctx)?;
        math::transform(&doc, &mut ctx)?;
        assets::transform(&doc, &mut ctx)?;
        chapters.extend(html::split(&doc, &mut ctx)?);
    }
    let resource_hrefs: Vec<String> = ctx.resources().iter().map(|r| r.href.clone()).collect();
    for w in html::fix_links_with_resources(&mut chapters, &resource_hrefs) {
        ctx.warn(w);
    }
    let chapters = notes::insert(chapters, &mut ctx)?;
    let css = style::build_book_css(&mut ctx)?;

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

/// Reads an input file as text (see [`decode_text`]). The second value is a
/// warning when the encoding was not clean UTF-8.
fn read_input(path: &Path) -> Result<(String, Option<String>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (text, problem) = decode_text(&bytes);
    let warning = problem.map(|p| format!("{}: {p}", path.display()));
    Ok((text, warning))
}

/// Decodes input bytes: UTF-8 (a BOM is stripped), or UTF-16 LE/BE when a
/// byte-order mark (or the NUL pattern of BOM-less UTF-16) says so. Bytes
/// that are not valid in the detected encoding are replaced, and the second
/// value then says what went wrong.
pub fn decode_text(bytes: &[u8]) -> (String, Option<String>) {
    let utf16 = |data: &[u8], le: bool| {
        let units: Vec<u16> = data
            .chunks(2)
            .map(|c| {
                let pair = [c[0], *c.get(1).unwrap_or(&0)];
                if le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        let text = String::from_utf16_lossy(&units);
        let problem = (!data.len().is_multiple_of(2) || text.contains('\u{FFFD}'))
            .then(|| "invalid UTF-16 sequences were replaced".to_string());
        (text, problem)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(body) {
        Ok(s) => (s.to_string(), None),
        Err(_) => {
            // UTF-16 without a BOM: ASCII text has a NUL in every other byte.
            let head = &body[..body.len().min(256)];
            let nul_at = |parity: usize| {
                head.len() >= 4
                    && head
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| i % 2 == parity)
                        .all(|(_, b)| *b == 0)
            };
            if nul_at(1) {
                return utf16(body, true);
            }
            if nul_at(0) {
                return utf16(body, false);
            }
            (
                String::from_utf8_lossy(body).into_owned(),
                Some(
                    "not valid UTF-8; invalid bytes were replaced (save the file as UTF-8)"
                        .to_string(),
                ),
            )
        }
    }
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

    #[test]
    fn decodes_utf16_and_flags_bad_utf8() {
        let text = "# Hello UTF16\r\n\r\nText \u{e9}.\r\n";
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_text(&le), (text.to_string(), None));
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode_text(&be), (text.to_string(), None));
        // BOM-less UTF-16 LE.
        let bare: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&bare).0, text);
        // UTF-8 BOM is stripped.
        assert_eq!(decode_text(b"\xEF\xBB\xBFabc"), ("abc".to_string(), None));
        // Latin-1 bytes are not UTF-8.
        let (t, w) = decode_text(b"caf\xe9 au lait");
        assert!(t.starts_with("caf"));
        assert!(w.unwrap().contains("not valid UTF-8"));
    }

    #[test]
    fn utf16_input_converts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u16.md");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            "# Hello UTF16\r\n\r\nText.\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        std::fs::write(&path, bytes).unwrap();
        let (book, ctx) = build_book(Options::default(), &[path], None).unwrap();
        assert_eq!(book.meta.title, "Hello UTF16");
        assert!(book.chapters[0].body_xhtml.contains("<p>Text.</p>"));
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
    }

    #[test]
    fn output_may_not_overwrite_sources() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("a2.md");
        let css = dir.path().join("x.css");
        std::fs::write(&md, "# A").unwrap();
        std::fs::write(&css, "p{}").unwrap();
        let opts = Options {
            extra_css: Some(css.clone()),
            ..Options::default()
        };
        let inputs = vec![md.clone()];
        let err = check_output(&md, &inputs, &opts).unwrap_err().to_string();
        assert!(err.contains("same as the input"), "{err}");
        // Through a different spelling of the same path.
        let other = dir.path().join(".").join("a2.md");
        assert!(check_output(&other, &inputs, &opts).is_err());
        let err = check_output(&css, &inputs, &opts).unwrap_err().to_string();
        assert!(err.contains("CSS file"), "{err}");
        assert!(check_output(dir.path(), &inputs, &opts).is_err());
        assert!(check_output(&dir.path().join("a2.epub"), &inputs, &opts).is_ok());
        assert!(check_output(&dir.path().join("new/x.epub"), &inputs, &opts).is_ok());
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep.html");
        std::fs::write(&path, "<div>".repeat(MAX_DOM_DEPTH + 10)).unwrap();
        let err = build_book(Options::default(), &[path], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nested"), "{err}");
    }

    #[test]
    fn config_problems_become_warnings() {
        let mut opts = Options::default();
        opts.meta.lang = "en_US".into();
        opts.lang_given = true;
        opts.meta.date = Some("15/01/2024".into());
        let (book, ctx) = build_book(opts, &[fixture("page.html")], None).unwrap();
        assert_eq!(book.meta.lang, "en-US");
        assert_eq!(book.meta.date.as_deref(), Some("2024-01-15"));
        assert!(ctx.warnings().iter().all(|w| !w.contains("language")));
    }
}

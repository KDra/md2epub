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
//! linked files (breadth-first, see `linked`) += chapters / "Linked files" pages
//! html::fix_links(&mut chapters)
//! chapters = notes::insert(chapters, ctx)
//! epub::write(Book { meta, chapters, resources, css, cover }, out_path)
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::cli::Cli;
use crate::config::UserConfig;
use crate::ctx::Ctx;
use crate::markdown::{self, Rendered};
use crate::model::{Book, Chapter, Heading, Options, OutputLocation};
use crate::{assets, config, dom, epub, html, linked, math, mermaid, notes, style};

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
    /// Informational notes for the user (not warnings), such as `.epub`
    /// having been added to the output name.
    pub notices: Vec<String>,
}

/// Runs the whole conversion for a parsed command line, on a thread with a
/// large stack (see [`PIPELINE_STACK`]). `user` holds the user defaults
/// (see [`crate::user_config`]).
pub fn run(cli: &Cli, user: &UserConfig) -> Result<Report> {
    let cli = cli.clone();
    let user = user.clone();
    let handle = std::thread::Builder::new()
        .name("md2epub".into())
        .stack_size(PIPELINE_STACK)
        .spawn(move || run_on_this_thread(&cli, &user))
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
pub fn run_on_this_thread(cli: &Cli, user: &UserConfig) -> Result<Report> {
    // First pass learns the inputs; the second applies the front matter of
    // the first Markdown input.
    let (_, inputs) = config::resolve_with(cli, None, user)?;
    let first = inputs.first().context("no input files")?;
    let mut first_rendered = None;
    if InputKind::of(first)? == InputKind::Markdown {
        first_rendered = Some(markdown::render(&read_input(first)?.0)?);
    }
    let front_matter = first_rendered
        .as_ref()
        .and_then(|r| r.front_matter.as_deref());
    let (opts, inputs) = config::resolve_with(cli, front_matter, user)?;
    let (out, notice) = output_path(&opts, &inputs);
    check_output(&out, &inputs, &opts)?;
    let (book, ctx) = build_book(opts, &inputs, first_rendered)?;
    epub::write(&book, &out)?;
    Ok(Report {
        output: out,
        chapters: book.chapters.len(),
        warnings: ctx.warnings().to_vec(),
        verbose: ctx.opts.verbose,
        notices: notice.into_iter().collect(),
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

/// Where the EPUB goes, plus a note for the user when the given name was
/// changed. With `name.epub` = the first input's stem plus `.epub`:
///
/// - no output path: `name.epub` in the current directory
///   ([`OutputLocation::Cwd`]) or beside the first input
///   ([`OutputLocation::Input`]);
/// - an existing directory, or a path ending in a separator: `name.epub`
///   inside it (the directory is created when the book is written);
/// - a name ending in `.epub` (any case): used as given;
/// - no extension (`-o book`): `.epub` is added (`book.epub`);
/// - another extension (`-o notes.txt`): `.epub` is added
///   (`notes.txt.epub`) and the note says so.
pub fn output_path(opts: &Options, inputs: &[PathBuf]) -> (PathBuf, Option<String>) {
    let first = inputs.first();
    let stem = first
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".into());
    let default_name = format!("{stem}.epub");
    let Some(out) = &opts.output else {
        let path = match (opts.output_location, first.and_then(|p| p.parent())) {
            (OutputLocation::Input, Some(dir)) if !dir.as_os_str().is_empty() => {
                dir.join(default_name)
            }
            _ => PathBuf::from(default_name),
        };
        return (path, None);
    };
    let ends_with_separator = out
        .to_string_lossy()
        .chars()
        .last()
        .is_some_and(std::path::is_separator);
    if ends_with_separator || out.is_dir() {
        return (out.join(default_name), None);
    }
    let Some(name) = out.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return (out.join(default_name), None);
    };
    let lower = name.to_ascii_lowercase();
    if lower.len() > ".epub".len() && lower.ends_with(".epub") {
        return (out.clone(), None);
    }
    let fixed = out.with_file_name(format!("{name}.epub"));
    let notice = Path::new(&name).extension().map(|_| {
        format!(
            "output name {} does not end in .epub; writing {}",
            out.display(),
            fixed.display()
        )
    });
    (fixed, notice)
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
    let scope = linked::LinkScope::new(inputs, ctx.opts.link_root.as_deref())?;
    let mut follower = linked::Follower::new(inputs, ctx.opts.follow_links, scope);
    for (i, input) in inputs.iter().enumerate() {
        let first = first_rendered.take().filter(|_| i == 0);
        let input_chapters = convert_document(input, first, &mut ctx)?;
        follower.scan(&input_chapters, &mut ctx);
        chapters.extend(input_chapters);
    }
    add_linked_files(&mut follower, &mut chapters, &mut ctx);
    let resource_hrefs: Vec<String> = ctx.resources().iter().map(|r| r.href.clone()).collect();
    for w in html::fix_links_full(
        &mut chapters,
        &resource_hrefs,
        follower.skipped(),
        Some(follower.scope().root()),
    ) {
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

/// Reads, renders and splits one Markdown or HTML document. `rendered` may
/// hold its already rendered Markdown.
fn convert_document(
    input: &Path,
    rendered: Option<Rendered>,
    ctx: &mut Ctx,
) -> Result<Vec<Chapter>> {
    ctx.info(format!("processing {}", input.display()));
    let (text, encoding_warning) = read_input(input)?;
    if let Some(w) = encoding_warning {
        ctx.warn(w);
    }
    let html = match InputKind::of(input)? {
        InputKind::Markdown => {
            let rendered = match rendered {
                Some(r) => r,
                None => markdown::render(&text)
                    .with_context(|| format!("rendering {}", input.display()))?,
            };
            rendered.html
        }
        InputKind::Html if html::is_xhtml_input(input, &text) => html::xhtml_to_html(&text),
        InputKind::Html => text,
    };
    convert_html(&html, input, ctx)
}

/// Runs the DOM stages on the HTML of `source` and splits it into chapters.
fn convert_html(html: &str, source: &Path, ctx: &mut Ctx) -> Result<Vec<Chapter>> {
    ctx.current_input = source.to_path_buf();
    ctx.base_dir = match source.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let doc = dom::parse(html);
    let depth = dom::max_depth(&doc);
    if depth > MAX_DOM_DEPTH {
        bail!(
            "{}: elements are nested {depth} levels deep (the limit is {MAX_DOM_DEPTH})",
            source.display()
        );
    }
    html::sanitize(&doc, ctx)?;
    mermaid::transform(&doc, ctx)?;
    math::transform(&doc, ctx)?;
    assets::transform(&doc, ctx)?;
    html::split(&doc, ctx)
}

/// Adds the local files linked from the chapters (see [`linked`]):
/// documents as chapters in breadth-first order, then a "Linked files"
/// section with one page per other file. Files that fail are recorded in
/// the follower, so their links are removed with the reason.
fn add_linked_files(follower: &mut linked::Follower, chapters: &mut Vec<Chapter>, ctx: &mut Ctx) {
    // Linked files never name the book.
    let title_hint = ctx.title_hint.clone();
    let mut pages = Vec::new();
    while let Some((path, kind)) = follower.next_file() {
        if kind.is_page() {
            pages.push((path, kind));
            continue;
        }
        ctx.info(format!("adding linked file {}", path.display()));
        match convert_document(&path, None, ctx) {
            Ok(mut new) => {
                let name = follower.scope().display_name(&path);
                give_toc_entry(&mut new, &name);
                follower.scan(&new, ctx);
                chapters.extend(new);
            }
            Err(e) => follower.skip(&path, format!("{e:#}")),
        }
    }
    if pages.is_empty() {
        ctx.title_hint = title_hint;
        return;
    }
    let section_file = ctx.next_chapter_file();
    let mut page_chapters = Vec::new();
    let mut toc = Vec::new();
    for (path, kind) in pages {
        ctx.info(format!("adding linked file {}", path.display()));
        let name = follower.scope().display_name(&path);
        let result = linked::page_html(&path, &name, kind)
            .map_err(|e| anyhow!(e))
            .and_then(|html| convert_html(&html, &path, ctx));
        match result {
            Ok(new) if !new.is_empty() => {
                toc.push((new[0].file_name.clone(), name));
                page_chapters.extend(new);
            }
            Ok(_) => follower.skip(&path, "nothing to show".into()),
            Err(e) => follower.skip(&path, format!("{e:#}")),
        }
    }
    ctx.title_hint = title_hint;
    if page_chapters.is_empty() {
        return;
    }
    let id = ctx.unique_id("linked-files");
    chapters.push(Chapter {
        title: linked::SECTION_TITLE.into(),
        file_name: section_file,
        body_xhtml: linked::section_html(&id, &toc),
        headings: vec![Heading {
            level: 1,
            text: linked::SECTION_TITLE.into(),
            id,
        }],
        in_toc: true,
        source: None,
    });
    chapters.extend(page_chapters);
}

/// Makes sure a linked document has a top-level table of contents entry:
/// when its first chapter does not start with a level-1 heading (a
/// changelog with only `##` sections), that chapter gets a level-1 entry
/// named `name` (the file's display name), and the document's own headings
/// nest under it instead of under the previous chapter.
fn give_toc_entry(chapters: &mut [Chapter], name: &str) {
    let Some(first) = chapters.first_mut() else {
        return;
    };
    if first.headings.first().is_some_and(|h| h.level == 1) {
        return;
    }
    first.title = name.to_string();
    first.headings.insert(
        0,
        Heading {
            level: 1,
            text: name.to_string(),
            id: String::new(),
        },
    );
    first.in_toc = true;
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
        assert_eq!(output_path(&opts, &inputs), (out.clone(), None));
        let (book, _) = build_book(opts, &inputs, None).unwrap();
        epub::write(&book, &out).unwrap();
        assert!(out.is_file());
        assert_eq!(
            output_path(&Options::default(), &inputs),
            (PathBuf::from("kitchen-sink.epub"), None)
        );
    }

    fn out_for(output: Option<&Path>, location: OutputLocation, input: &str) -> PathBuf {
        let opts = Options {
            output: output.map(Path::to_path_buf),
            output_location: location,
            ..Options::default()
        };
        output_path(&opts, &[PathBuf::from(input)]).0
    }

    #[test]
    fn output_path_rules() {
        use OutputLocation::{Cwd, Input};
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // No -o: current directory or beside the first input.
        assert_eq!(out_for(None, Cwd, "src/a.md"), PathBuf::from("a.epub"));
        assert_eq!(
            out_for(None, Input, "src/a.md"),
            PathBuf::from("src/a.epub")
        );
        assert_eq!(out_for(None, Input, "a.md"), PathBuf::from("a.epub"));
        // .epub is kept, in any case.
        assert_eq!(
            out_for(Some(Path::new("x/B.EPUB")), Input, "a.md"),
            PathBuf::from("x/B.EPUB")
        );
        // No extension: .epub is added silently.
        let opts = Options {
            output: Some("out/book".into()),
            ..Options::default()
        };
        assert_eq!(
            output_path(&opts, &[PathBuf::from("a.md")]),
            (PathBuf::from("out/book.epub"), None)
        );
        // Another extension: .epub is appended, with a note.
        let opts = Options {
            output: Some("notes.txt".into()),
            ..Options::default()
        };
        let (path, note) = output_path(&opts, &[PathBuf::from("a.md")]);
        assert_eq!(path, PathBuf::from("notes.txt.epub"));
        assert!(note.unwrap().contains("notes.txt.epub"));
        // An existing directory, or a path ending in a separator.
        assert_eq!(out_for(Some(d), Cwd, "x/a.md"), d.join("a.epub"));
        let new_dir = format!("{}/new/", d.display());
        assert_eq!(
            out_for(Some(Path::new(&new_dir)), Cwd, "x/a.md"),
            d.join("new").join("a.epub")
        );
        // The -o location wins over output-location.
        assert_eq!(out_for(Some(d), Input, "x/a.md"), d.join("a.epub"));
    }

    #[test]
    fn output_directory_is_created_and_input_is_protected() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("c.md");
        std::fs::write(&md, "# C\n").unwrap();
        let out_dir = format!("{}/made/", dir.path().display());
        let cli = <Cli as clap::Parser>::try_parse_from([
            "md2epub",
            md.to_str().unwrap(),
            "--offline",
            "-o",
            &out_dir,
        ])
        .unwrap();
        let report = run_on_this_thread(&cli, &UserConfig::default()).unwrap();
        assert_eq!(report.output, dir.path().join("made").join("c.epub"));
        assert!(report.output.is_file());
        // `-o c` next to `c.md` gives `c.epub`, never the input itself.
        let cli = <Cli as clap::Parser>::try_parse_from([
            "md2epub",
            md.to_str().unwrap(),
            "--offline",
            "-o",
            dir.path().join("c").to_str().unwrap(),
        ])
        .unwrap();
        let report = run_on_this_thread(&cli, &UserConfig::default()).unwrap();
        assert_eq!(report.output, dir.path().join("c.epub"));
        assert_eq!(std::fs::read_to_string(&md).unwrap(), "# C\n");
    }

    #[test]
    fn user_config_output_location_applies() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let md = sub.join("d.md");
        std::fs::write(&md, "# D\n").unwrap();
        let user =
            UserConfig::from_toml_str("output-location = \"input\"\n", Path::new(".")).unwrap();
        let cli =
            <Cli as clap::Parser>::try_parse_from(["md2epub", md.to_str().unwrap(), "--offline"])
                .unwrap();
        let report = run_on_this_thread(&cli, &user).unwrap();
        assert_eq!(report.output, sub.join("d.epub"));
        assert!(report.output.is_file());
    }

    #[test]
    fn linked_documents_without_h1_get_a_toc_entry() {
        let heading = |level, text: &str| Heading {
            level,
            text: text.into(),
            id: text.into(),
        };
        let mut chapters = vec![Chapter {
            title: "v1.1.0".into(),
            headings: vec![heading(2, "v1.1.0"), heading(2, "v1.0.0")],
            in_toc: true,
            ..Chapter::default()
        }];
        give_toc_entry(&mut chapters, "CHANGELOG.md");
        assert_eq!(chapters[0].title, "CHANGELOG.md");
        assert_eq!(chapters[0].headings.len(), 3);
        assert_eq!(chapters[0].headings[0].level, 1);
        assert_eq!(chapters[0].headings[0].text, "CHANGELOG.md");
        // A document that starts with an h1 is left alone.
        let mut with_h1 = vec![Chapter {
            title: "Guide".into(),
            headings: vec![heading(1, "Guide")],
            ..Chapter::default()
        }];
        let before = with_h1.clone();
        give_toc_entry(&mut with_h1, "guide.md");
        assert_eq!(with_h1, before);
        give_toc_entry(&mut [], "x.md");
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

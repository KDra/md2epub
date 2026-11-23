//! Linked local files: hyperlinks from chapters to files on disk.
//!
//! When `opts.follow_links` is on, the pipeline scans every chapter for
//! `<a href>`/`<area href>` links that name a local file (relative or
//! absolute path, or `file:` URL; see [`crate::html::local_link_target`]). Each
//! existing file is added to the book once (deduplicated by canonical path),
//! after all explicit inputs, in order of first reference (breadth-first):
//!
//! - documents (`.md`, `.markdown`, `.html`, `.htm`, `.xhtml`, ...) go
//!   through the normal pipeline and become ordinary chapters; their own
//!   links are followed in turn;
//! - images become a page with the file name as heading and the image;
//! - Mermaid sources (`.mmd`, `.mermaid`) become a page with the rendered
//!   diagram (a code block when it cannot be rendered);
//! - other UTF-8 text files up to [`MAX_TEXT_BYTES`] become a page with a
//!   syntax-highlighted code block (language from the extension).
//!
//! The non-document pages are nested in the table of contents under a
//! final "Linked files" section. Files that cannot be added (missing,
//! directories, binary, too large, over [`MAX_LINKED_FILES`]) are recorded
//! with the reason, and `html::fix_links_full` removes links to them with a
//! warning that says why.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::ctx::Ctx;
use crate::html::{link_base, link_hrefs, local_link_target, source_key};
use crate::markdown;
use crate::model::Chapter;

/// Most linked files added to one book.
pub const MAX_LINKED_FILES: usize = 500;

/// Largest text file shown as a code page.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// Title of the TOC section holding the non-document pages.
pub const SECTION_TITLE: &str = "Linked files";

/// How a linked file ends up in the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkedKind {
    /// Markdown or HTML, rendered as chapters.
    Document,
    /// An image page.
    Image,
    /// A Mermaid diagram page.
    Mermaid,
    /// A code page for a text file.
    Text,
}

impl LinkedKind {
    /// Whether the file becomes a page in the "Linked files" section.
    pub fn is_page(self) -> bool {
        self != LinkedKind::Document
    }
}

const DOCUMENT_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "html", "htm", "xhtml"];
const IMAGE_EXTS: &[&str] = &[
    "svg", "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff",
];
const MERMAID_EXTS: &[&str] = &["mmd", "mermaid"];

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Decides how `path` can be added to the book, or why it cannot.
pub fn classify(path: &Path) -> Result<LinkedKind, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("the file does not exist".into());
        }
        Err(e) => return Err(format!("it cannot be read: {e}")),
    };
    if meta.is_dir() {
        return Err("it is a directory".into());
    }
    if !meta.is_file() {
        return Err("it is not a regular file".into());
    }
    let ext = extension(path);
    if DOCUMENT_EXTS.contains(&ext.as_str()) {
        return Ok(LinkedKind::Document);
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return Ok(LinkedKind::Image);
    }
    if meta.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "it is larger than {} MB",
            MAX_TEXT_BYTES / 1024 / 1024
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("it cannot be read: {e}"))?;
    if !is_text(&bytes) {
        return Err("it is a binary file, not text, an image or a document".into());
    }
    if MERMAID_EXTS.contains(&ext.as_str()) {
        Ok(LinkedKind::Mermaid)
    } else {
        Ok(LinkedKind::Text)
    }
}

/// Valid UTF-8 without NUL bytes.
fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Breadth-first discovery of linked files.
#[derive(Debug)]
pub struct Follower {
    enabled: bool,
    /// Source keys of the inputs and of every file queued so far.
    known: HashSet<PathBuf>,
    /// Source key -> why the file was not added.
    skipped: HashMap<PathBuf, String>,
    queue: VecDeque<(PathBuf, LinkedKind)>,
    followed: usize,
    limit_warned: bool,
}

impl Follower {
    /// A follower that knows the explicit `inputs` (links to them are left
    /// to `html::fix_links`). When `enabled` is false nothing is queued.
    pub fn new(inputs: &[PathBuf], enabled: bool) -> Self {
        Follower {
            enabled,
            known: inputs.iter().map(|p| source_key(p)).collect(),
            skipped: HashMap::new(),
            queue: VecDeque::new(),
            followed: 0,
            limit_warned: false,
        }
    }

    /// Queues every new local file linked from `chapters`, in link order.
    pub fn scan(&mut self, chapters: &[Chapter], ctx: &mut Ctx) {
        for ch in chapters {
            let Some(source) = &ch.source else { continue };
            let base = link_base(source);
            for href in link_hrefs(&ch.body_xhtml) {
                let Some(target) = local_link_target(&base, &href) else {
                    continue;
                };
                self.consider(target, ctx);
            }
        }
    }

    fn consider(&mut self, target: PathBuf, ctx: &mut Ctx) {
        let key = source_key(&target);
        if self.known.contains(&key) || self.skipped.contains_key(&key) {
            return;
        }
        if !self.enabled {
            if target.exists() {
                self.skipped.insert(
                    key,
                    "following linked files is turned off by --no-follow-links or follow-links = false".into(),
                );
            }
            return;
        }
        let kind = match classify(&target) {
            Ok(kind) => kind,
            Err(reason) => {
                self.skipped.insert(key, reason);
                return;
            }
        };
        if self.followed >= MAX_LINKED_FILES {
            if !self.limit_warned {
                self.limit_warned = true;
                ctx.warn(format!(
                    "more than {MAX_LINKED_FILES} linked files; further linked files are not added"
                ));
            }
            self.skipped.insert(
                key,
                format!("the limit of {MAX_LINKED_FILES} linked files was reached"),
            );
            return;
        }
        self.followed += 1;
        self.known.insert(key);
        self.queue.push_back((target, kind));
    }

    /// The next file to add, in breadth-first order.
    pub fn next_file(&mut self) -> Option<(PathBuf, LinkedKind)> {
        self.queue.pop_front()
    }

    /// Records that `path` could not be added after all.
    pub fn skip(&mut self, path: &Path, reason: String) {
        self.skipped.insert(source_key(path), reason);
    }

    /// Source key -> reason, for `html::fix_links_full`.
    pub fn skipped(&self) -> &HashMap<PathBuf, String> {
        &self.skipped
    }
}

/// Display name of a linked file (its file name).
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Percent-encodes a file name for a relative `src`.
fn encode_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// HTML for the page of a non-document linked file: an `<h2>` with the file
/// name (it nests under the "Linked files" section) and the content.
pub fn page_html(path: &Path, kind: LinkedKind) -> Result<String, String> {
    let name = display_name(path);
    let mut html = format!("<h2>{}</h2>\n", crate::dom::escape(&name));
    match kind {
        LinkedKind::Document => return Err("not a page".into()),
        LinkedKind::Image => {
            html.push_str(&format!(
                "<figure class=\"linked-file\"><img src=\"{}\" alt=\"{}\"/></figure>\n",
                encode_component(&name),
                crate::dom::escape(&name)
            ));
        }
        LinkedKind::Mermaid | LinkedKind::Text => {
            let bytes = std::fs::read(path).map_err(|e| format!("it cannot be read: {e}"))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| "it is a binary file, not text, an image or a document".to_string())?;
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            if kind == LinkedKind::Mermaid {
                html.push_str("<pre class=\"mermaid\">");
                crate::dom::escape_into(text, false, &mut html);
                html.push_str("</pre>\n");
            } else {
                html.push_str(&markdown::code_block(text, &language_of(path)));
            }
        }
    }
    Ok(html)
}

/// Fence language token for a text file: its extension, or its lowercase
/// name when it has none (`Makefile`, `Dockerfile`).
fn language_of(path: &Path) -> String {
    let ext = extension(path);
    if !ext.is_empty() {
        return ext;
    }
    display_name(path).to_ascii_lowercase()
}

/// Body of the "Linked files" section chapter: a heading with `id` and a
/// list of links to the pages (`(chapter file, name)`).
pub fn section_html(id: &str, pages: &[(String, String)]) -> String {
    let esc = crate::dom::escape;
    let mut out = format!("<h1 id=\"{}\">{}</h1>\n<ul>\n", esc(id), esc(SECTION_TITLE));
    for (file, name) in pages {
        out.push_str(&format!(
            "<li><a href=\"{}\">{}</a></li>\n",
            esc(file),
            esc(name)
        ));
    }
    out.push_str("</ul>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn classifies_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(
            classify(&write(d, "a.md", b"# A")),
            Ok(LinkedKind::Document)
        );
        assert_eq!(classify(&write(d, "a.HTM", b"x")), Ok(LinkedKind::Document));
        assert_eq!(
            classify(&write(d, "a.svg", b"<svg/>")),
            Ok(LinkedKind::Image)
        );
        assert_eq!(
            classify(&write(d, "a.mmd", b"graph TD")),
            Ok(LinkedKind::Mermaid)
        );
        assert_eq!(
            classify(&write(d, "a.rs", b"fn main() {}")),
            Ok(LinkedKind::Text)
        );
        assert_eq!(classify(&write(d, "LICENSE", b"MIT")), Ok(LinkedKind::Text));
        let bin = classify(&write(d, "a.bin", &[0, 1, 2, 255])).unwrap_err();
        assert!(bin.contains("binary"), "{bin}");
        let latin1 = classify(&write(d, "a.txt", b"caf\xe9")).unwrap_err();
        assert!(latin1.contains("binary"), "{latin1}");
        let big = vec![b'a'; MAX_TEXT_BYTES as usize + 1];
        let big = classify(&write(d, "big.txt", &big)).unwrap_err();
        assert!(big.contains("larger than 1 MB"), "{big}");
        assert!(
            classify(&d.join("nope.md"))
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(classify(d).unwrap_err().contains("directory"));
    }

    #[test]
    fn follower_is_breadth_first_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let input = write(d, "in.md", b"");
        write(d, "b.md", b"");
        write(d, "c.txt", b"x");
        let mut ctx = Ctx::new(Options::default());
        let mut f = Follower::new(std::slice::from_ref(&input), true);
        let ch = Chapter {
            source: Some(input.clone()),
            body_xhtml: concat!(
                r##"<a href="c.txt">c</a><a href="b.md#x">b</a><a href="./c.txt?q">again</a>"##,
                r##"<a href="in.md">self</a><a href="https://x.org/a.md">web</a>"##,
                r##"<a href="#top">top</a><a href="missing.png">m</a><a href="mailto:a@b">mail</a>"##
            )
            .into(),
            ..Chapter::default()
        };
        f.scan(std::slice::from_ref(&ch), &mut ctx);
        let (p1, k1) = f.next_file().unwrap();
        assert_eq!(display_name(&p1), "c.txt");
        assert_eq!(k1, LinkedKind::Text);
        let (p2, k2) = f.next_file().unwrap();
        assert_eq!(display_name(&p2), "b.md");
        assert_eq!(k2, LinkedKind::Document);
        assert!(f.next_file().is_none());
        // Scanning again adds nothing.
        f.scan(std::slice::from_ref(&ch), &mut ctx);
        assert!(f.next_file().is_none());
        let reasons: Vec<&String> = f.skipped().values().collect();
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("does not exist"));
    }

    #[test]
    fn disabled_follower_explains() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let input = write(d, "in.md", b"");
        write(d, "b.md", b"");
        let mut ctx = Ctx::new(Options::default());
        let mut f = Follower::new(std::slice::from_ref(&input), false);
        let ch = Chapter {
            source: Some(input),
            body_xhtml: r#"<a href="b.md">b</a>"#.into(),
            ..Chapter::default()
        };
        f.scan(&[ch], &mut ctx);
        assert!(f.next_file().is_none());
        let reason = f.skipped().values().next().unwrap();
        assert!(reason.contains("turned off"), "{reason}");
    }

    #[test]
    fn limit_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let input = write(d, "in.md", b"");
        let mut body = String::new();
        for i in 0..=MAX_LINKED_FILES {
            write(d, &format!("f{i}.txt"), b"x");
            body.push_str(&format!("<a href=\"f{i}.txt\">x</a>"));
        }
        let mut ctx = Ctx::new(Options::default());
        let mut f = Follower::new(std::slice::from_ref(&input), true);
        let ch = Chapter {
            source: Some(input),
            body_xhtml: body,
            ..Chapter::default()
        };
        f.scan(&[ch], &mut ctx);
        let mut n = 0;
        while f.next_file().is_some() {
            n += 1;
        }
        assert_eq!(n, MAX_LINKED_FILES);
        assert_eq!(ctx.warnings().len(), 1);
        assert!(ctx.warnings()[0].contains("500"));
        assert!(f.skipped().values().any(|r| r.contains("limit")));
    }

    #[test]
    fn page_html_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let img = write(d, "my pic#1.png", b"x");
        let html = page_html(&img, LinkedKind::Image).unwrap();
        assert!(html.contains("<h2>my pic#1.png</h2>"), "{html}");
        assert!(html.contains("src=\"my%20pic%231.png\""), "{html}");
        let code = write(
            d,
            "main.rs",
            "\u{feff}fn main() { let a = 1 < 2; }".as_bytes(),
        );
        let html = page_html(&code, LinkedKind::Text).unwrap();
        assert!(html.contains("language-rs"), "{html}");
        assert!(html.contains("&lt;"), "{html}");
        assert!(!html.contains('\u{feff}'));
        let mmd = write(d, "g.mmd", b"graph TD\n A-->B");
        let html = page_html(&mmd, LinkedKind::Mermaid).unwrap();
        assert!(
            html.contains("<pre class=\"mermaid\">graph TD\n A--&gt;B</pre>"),
            "{html}"
        );
        assert!(page_html(&mmd, LinkedKind::Document).is_err());
        let s = section_html("linked-files", &[("ch009.xhtml".into(), "a&b.txt".into())]);
        assert!(s.contains("<h1 id=\"linked-files\">Linked files</h1>"));
        assert!(s.contains("<a href=\"ch009.xhtml\">a&amp;b.txt</a>"));
    }
}

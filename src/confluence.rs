//! Confluence Data Center / Server output (`--format confluence`).
//!
//! The pipeline builds the same in-memory [`Book`] as for EPUB; this module
//! turns its chapters into one page in Confluence *storage format* (the
//! XHTML-based format of the REST API, see
//! <https://confluence.atlassian.com/doc/confluence-storage-format-790796544.html>)
//! and writes an export directory:
//!
//! ```text
//! name.confluence/
//!   page.xml        storage-format fragment (UTF-8), no root element
//!   attachments/    every image, diagram and formula used by the page
//!   manifest.json   title, attachments with file name, media type, sha256
//! ```
//!
//! Conversion rules:
//!
//! - All inputs and linked documents are merged into one page, in order.
//! - Every element id of the book (heading ids, footnote ids, ids from HTML
//!   input) becomes an explicit `anchor` macro, and every link inside the
//!   book becomes `<ac:link ac:anchor="...">`. Confluence's own heading
//!   anchors depend on the page title and on heading text, so explicit
//!   anchors are the robust choice: they survive renames and duplicate
//!   headings. Ids are unique across the book, so they stay unique on the
//!   merged page.
//! - Images, rasterized SVG, Mermaid diagrams and math are attachments
//!   (`<ac:image><ri:attachment ri:filename=.../></ac:image>`), named by
//!   content hash (`<sha256 prefix>.png`), so an unchanged file keeps its
//!   name and re-publishing can skip it. Remote images that are not
//!   downloaded (`--offline`) are shown by URL (`<ri:url>`).
//! - Fenced code becomes the `code` macro with a `language` parameter (see
//!   [`code_language`]); highlighting spans are dropped.
//! - Task lists become `<ac:task-list>`; strikethrough is
//!   `<span style="text-decoration: line-through;">` as in the docs.
//! - Footnote sections are collected into one "Footnotes" section at the
//!   end of the page, with links back to the references.
//! - EPUB-only features (cover, note pages, margins, CSS, device profiles,
//!   navigation) are ignored.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use roxmltree::{Node, NodeType};
use sha2::{Digest, Sha256};

use crate::assets::REMOTE_ATTR;
use crate::model::{Book, Chapter, MathMode, NotesPlacement, Options, SvgMode};

/// Name of the page file inside an export directory.
pub const PAGE_FILE: &str = "page.xml";
/// Name of the manifest inside an export directory.
pub const MANIFEST_FILE: &str = "manifest.json";
/// Name of the attachment folder inside an export directory.
pub const ATTACHMENTS_DIR: &str = "attachments";
/// `format` value of the manifest.
const MANIFEST_FORMAT: &str = "md2epub-confluence";

/// Namespace URIs used to check the output (Confluence ignores them).
pub const AC_NS: &str = "http://atlassian.com/content";
pub const RI_NS: &str = "http://atlassian.com/resource/identifier";

/// One attachment of the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// File name on the page, e.g. `0a1b2c3d4e5f6a7b.png`.
    pub filename: String,
    pub media_type: String,
    /// Hex SHA-256 of `data`.
    pub sha256: String,
    pub data: Vec<u8>,
}

/// A converted page, ready to write to disk or to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub title: String,
    /// Storage-format body (a fragment without a root element).
    pub page_xml: String,
    /// Attachments the page references, in file name order.
    pub attachments: Vec<Attachment>,
}

/// Adjusts resolved options for Confluence output and returns warnings for
/// settings that do not apply.
pub fn prepare_options(opts: &mut Options) -> Vec<String> {
    let mut warnings = Vec::new();
    // Device profiles exist for e-ink readers; pages get full-colour
    // images with transparency, sized like the generic profile.
    opts.profile = crate::profile::generic();
    opts.svg = SvgMode::Rasterize;
    if opts.math == MathMode::Mathml {
        warnings.push(
            "math: Confluence cannot show MathML; formulas are rendered as images instead".into(),
        );
        opts.math = MathMode::Image;
    }
    if opts.notes != NotesPlacement::None {
        warnings.push("note pages are an EPUB feature and are left out of Confluence pages".into());
        opts.notes = NotesPlacement::None;
    }
    opts.cover = None;
    opts.extra_css = None;
    warnings
}

/// Converts a built book into a Confluence page.
pub fn from_book(book: &Book, toc: bool) -> Result<(Export, Vec<String>)> {
    let (page_xml, used, warnings) = to_storage(&book.chapters, toc)?;
    let mut attachments = Vec::new();
    for res in &book.resources {
        let Some(name) = attachment_name(&res.href) else {
            continue;
        };
        if !used.contains(name) {
            continue;
        }
        attachments.push(Attachment {
            filename: name.to_string(),
            media_type: res.media_type.clone(),
            sha256: sha256_hex(&res.data),
            data: res.data.clone(),
        });
    }
    attachments.sort_by(|a, b| a.filename.cmp(&b.filename));
    attachments.dedup_by(|a, b| a.filename == b.filename);
    Ok((
        Export {
            title: book.meta.title.clone(),
            page_xml,
            attachments,
        },
        warnings,
    ))
}

/// Hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The attachment file name of a book resource href (`images/x.png`).
fn attachment_name(href: &str) -> Option<&str> {
    let name = href.strip_prefix("images/")?;
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

// ---------------------------------------------------------------------------
// Export directory
// ---------------------------------------------------------------------------

/// Whether `dir` looks like an export directory (it has a manifest).
pub fn is_export_dir(dir: &Path) -> bool {
    dir.is_dir() && dir.join(MANIFEST_FILE).is_file()
}

/// Writes `export` to `dir` (created when missing). An existing directory
/// must be empty or an earlier export; its old attachments are replaced.
pub fn write_dir(export: &Export, dir: &Path) -> Result<()> {
    if dir.exists() {
        if !dir.is_dir() {
            bail!(
                "output path {} exists and is not a directory",
                dir.display()
            );
        }
        let empty = std::fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .next()
            .is_none();
        if !empty && !is_export_dir(dir) {
            bail!(
                "output directory {} is not empty and is not a Confluence export (no {MANIFEST_FILE}); \
                 choose another -o",
                dir.display()
            );
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let att_dir = dir.join(ATTACHMENTS_DIR);
    if att_dir.is_dir() {
        std::fs::remove_dir_all(&att_dir)
            .with_context(|| format!("removing old {}", att_dir.display()))?;
    }
    std::fs::create_dir_all(&att_dir).with_context(|| format!("creating {}", att_dir.display()))?;
    for a in &export.attachments {
        let p = att_dir.join(&a.filename);
        std::fs::write(&p, &a.data).with_context(|| format!("writing {}", p.display()))?;
    }
    let page = dir.join(PAGE_FILE);
    std::fs::write(&page, &export.page_xml)
        .with_context(|| format!("writing {}", page.display()))?;
    let manifest = manifest_json(export);
    let mpath = dir.join(MANIFEST_FILE);
    std::fs::write(&mpath, manifest).with_context(|| format!("writing {}", mpath.display()))?;
    Ok(())
}

/// The manifest of `export`, pretty-printed JSON.
pub fn manifest_json(export: &Export) -> String {
    let attachments: Vec<serde_json::Value> = export
        .attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "filename": a.filename,
                "mediaType": a.media_type,
                "sha256": a.sha256,
                "size": a.data.len(),
            })
        })
        .collect();
    let value = serde_json::json!({
        "format": MANIFEST_FORMAT,
        "version": 1,
        "generator": concat!("md2epub ", env!("CARGO_PKG_VERSION")),
        "title": export.title,
        "page": PAGE_FILE,
        "attachments": attachments,
    });
    let mut s = serde_json::to_string_pretty(&value).unwrap_or_default();
    s.push('\n');
    s
}

/// Reads an export directory written by [`write_dir`], checking every
/// attachment against its manifest hash.
pub fn read_dir(dir: &Path) -> Result<Export> {
    let mpath = dir.join(MANIFEST_FILE);
    let text =
        std::fs::read_to_string(&mpath).with_context(|| format!("reading {}", mpath.display()))?;
    let m: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", mpath.display()))?;
    if m["format"].as_str() != Some(MANIFEST_FORMAT) {
        bail!(
            "{} is not an md2epub Confluence manifest (format is not \"{MANIFEST_FORMAT}\")",
            mpath.display()
        );
    }
    let title = m["title"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow!("{}: missing title", mpath.display()))?
        .to_string();
    let page_name = m["page"].as_str().unwrap_or(PAGE_FILE);
    if page_name.contains(['/', '\\']) || page_name.starts_with('.') {
        bail!("{}: invalid page file name", mpath.display());
    }
    let page = dir.join(page_name);
    let page_xml =
        std::fs::read_to_string(&page).with_context(|| format!("reading {}", page.display()))?;
    let mut attachments = Vec::new();
    for (i, a) in m["attachments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let ctx = || format!("{}: attachment {}", mpath.display(), i + 1);
        let filename = a["filename"]
            .as_str()
            .ok_or_else(|| anyhow!("missing filename"))
            .with_context(ctx)?;
        if filename.is_empty()
            || filename.contains(['/', '\\'])
            || filename.starts_with('.')
            || filename.chars().any(char::is_control)
        {
            return Err(anyhow!("invalid file name {filename:?}")).with_context(ctx);
        }
        let path = dir.join(ATTACHMENTS_DIR).join(filename);
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let sha = sha256_hex(&data);
        if let Some(want) = a["sha256"].as_str()
            && !want.eq_ignore_ascii_case(&sha)
        {
            bail!(
                "{} does not match the sha256 in {} (was it changed after the export?)",
                path.display(),
                mpath.display()
            );
        }
        attachments.push(Attachment {
            filename: filename.to_string(),
            media_type: a["mediaType"]
                .as_str()
                .unwrap_or("application/octet-stream")
                .to_string(),
            sha256: sha,
            data,
        });
    }
    Ok(Export {
        title,
        page_xml,
        attachments,
    })
}

/// Where the export directory goes, plus a note when the name was changed.
/// Mirrors [`crate::pipeline::output_path`] with `.confluence` in place of
/// `.epub`; an `.epub` name is an error.
pub fn output_dir(
    output: Option<&Path>,
    location: crate::model::OutputLocation,
    inputs: &[PathBuf],
) -> Result<(PathBuf, Option<String>)> {
    const EXT: &str = ".confluence";
    let first = inputs.first();
    let stem = first
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".into());
    let default_name = format!("{stem}{EXT}");
    let Some(out) = output else {
        let path = match (location, first.and_then(|p| p.parent())) {
            (crate::model::OutputLocation::Input, Some(dir)) if !dir.as_os_str().is_empty() => {
                dir.join(default_name)
            }
            _ => PathBuf::from(default_name),
        };
        return Ok((path, None));
    };
    let ends_with_separator = out
        .to_string_lossy()
        .chars()
        .last()
        .is_some_and(std::path::is_separator);
    let Some(name) = out.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok((out.join(default_name), None));
    };
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(EXT) && lower.len() > EXT.len() {
        return Ok((out.to_path_buf(), None));
    }
    if ends_with_separator || (out.is_dir() && !is_export_dir(out)) {
        return Ok((out.join(default_name), None));
    }
    if out.is_dir() {
        return Ok((out.to_path_buf(), None));
    }
    if lower.ends_with(".epub") {
        bail!(
            "-o {}: with --format confluence the output is a directory (page.xml, attachments/, manifest.json); \
             use -o {} or drop --format confluence to write an EPUB",
            out.display(),
            out.with_extension("confluence").display()
        );
    }
    let fixed = out.with_file_name(format!("{name}{EXT}"));
    let notice = Path::new(&name).extension().map(|_| {
        format!(
            "output name {} does not end in {EXT}; writing {}",
            out.display(),
            fixed.display()
        )
    });
    Ok((fixed, notice))
}

// ---------------------------------------------------------------------------
// Storage format conversion
// ---------------------------------------------------------------------------

/// Converts chapters to one storage-format page. Returns the page, the
/// attachment file names it uses, and warnings.
pub fn to_storage(
    chapters: &[Chapter],
    toc: bool,
) -> Result<(String, BTreeSet<String>, Vec<String>)> {
    let mut docs = Vec::new();
    for ch in chapters {
        let wrapped = format!(
            "<body xmlns:epub=\"http://www.idpf.org/2007/ops\">{}</body>",
            ch.body_xhtml
        );
        docs.push(wrapped);
    }
    let parsed: Vec<roxmltree::Document> = docs
        .iter()
        .zip(chapters)
        .map(|(d, ch)| {
            roxmltree::Document::parse_with_options(
                d,
                roxmltree::ParsingOptions {
                    allow_dtd: false,
                    ..Default::default()
                },
            )
            .with_context(|| format!("reading chapter {}", ch.file_name))
        })
        .collect::<Result<_>>()?;

    let mut conv = Converter::default();
    // Ids used anywhere, to give chapters unique anchors.
    for doc in &parsed {
        for n in doc.descendants().filter(Node::is_element) {
            if let Some(id) = n.attribute("id") {
                conv.ids.insert(id.to_string());
            }
        }
    }
    // Chapters linked without a fragment get an anchor at their start.
    let files: HashSet<&str> = chapters.iter().map(|c| c.file_name.as_str()).collect();
    for doc in &parsed {
        for a in doc.descendants().filter(|n| n.has_tag_name("a")) {
            if let Some(href) = a.attribute("href") {
                let file = href.split('#').next().unwrap_or("");
                if files.contains(file) && !href.contains('#') {
                    conv.chapter_anchor.insert(file.to_string(), String::new());
                }
            }
        }
    }
    let wanted: Vec<String> = conv.chapter_anchor.keys().cloned().collect();
    let mut own_anchor = HashSet::new();
    for file in wanted {
        // A chapter that starts with a heading is linked to that heading.
        let first_heading_id = chapters
            .iter()
            .position(|c| c.file_name == file)
            .and_then(|i| parsed[i].root_element().children().find(Node::is_element))
            .filter(|e| matches!(e.tag_name().name(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
            .and_then(|e| e.attribute("id"))
            .filter(|id| !id.is_empty());
        if let Some(id) = first_heading_id {
            conv.chapter_anchor.insert(file, id.to_string());
            continue;
        }
        own_anchor.insert(file.clone());
        let stem = file.trim_end_matches(".xhtml");
        let mut name = format!("md2epub-{stem}");
        let mut n = 1;
        while conv.ids.contains(&name) {
            n += 1;
            name = format!("md2epub-{stem}-{n}");
        }
        conv.ids.insert(name.clone());
        conv.chapter_anchor.insert(file, name);
    }
    conv.files = chapters.iter().map(|c| c.file_name.clone()).collect();

    if toc {
        conv.out
            .push_str("<ac:structured-macro ac:name=\"toc\" ac:schema-version=\"1\" />\n");
    }
    for (doc, ch) in parsed.iter().zip(chapters) {
        if own_anchor.contains(&ch.file_name)
            && let Some(anchor) = conv.chapter_anchor.get(&ch.file_name).cloned()
        {
            conv.anchor(&anchor);
        }
        conv.blocks(doc.root_element());
        conv.out.push('\n');
    }
    if !conv.footnotes.is_empty() {
        conv.out.push_str("<h2>Footnotes</h2>\n");
        let notes = std::mem::take(&mut conv.footnotes);
        conv.out.push_str(&notes);
    }
    let page = conv.out.trim().to_string() + "\n";
    Ok((page, conv.attachments, conv.warnings))
}

#[derive(Default)]
struct Converter {
    out: String,
    /// Converted footnote lists, placed at the end of the page.
    footnotes: String,
    attachments: BTreeSet<String>,
    ids: HashSet<String>,
    /// Chapter file name -> anchor at its start.
    chapter_anchor: HashMap<String, String>,
    files: HashSet<String>,
    warnings: Vec<String>,
    /// Inside a `<ac:link-body>`: only simple inline markup is allowed.
    in_link_body: bool,
}

/// Tags dropped together with their content.
const DROP: &[&str] = &[
    "style", "script", "head", "title", "meta", "link", "svg", "colgroup", "col", "rp", "wbr",
    "button", "select", "textarea", "option", "source", "track", "audio", "video", "iframe",
    "canvas", "map", "area", "param", "embed",
];

/// Block-level containers that take the anchor of their id inside.
const ANCHOR_INSIDE: &[&str] = &[
    "p",
    "li",
    "td",
    "th",
    "blockquote",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "dd",
    "dt",
];

impl Converter {
    /// Emits an anchor macro.
    fn anchor(&mut self, name: &str) {
        let _ = write!(
            self.out,
            "<ac:structured-macro ac:name=\"anchor\" ac:schema-version=\"1\"><ac:parameter ac:name=\"\">{}</ac:parameter></ac:structured-macro>",
            esc(name)
        );
    }

    fn blocks(&mut self, node: Node) {
        for child in node.children() {
            self.node(child);
        }
    }

    fn node(&mut self, n: Node) {
        match n.node_type() {
            NodeType::Text => {
                let t = n.text().unwrap_or("");
                self.out.push_str(&esc(t));
            }
            NodeType::Element => self.element(n),
            _ => {}
        }
    }

    fn element(&mut self, n: Node) {
        let tag = n.tag_name().name().to_ascii_lowercase();
        let tag = tag.as_str();
        if DROP.contains(&tag) {
            return;
        }
        if tag == "math" {
            // MathML that was not rendered: keep the TeX or the text.
            let text = n
                .attribute("alttext")
                .map(str::to_string)
                .unwrap_or_else(|| text_of(n));
            let _ = write!(self.out, "<code>{}</code>", esc(&text));
            return;
        }
        if self.in_link_body {
            self.link_body_element(n, tag);
            return;
        }
        let id = n.attribute("id").filter(|id| !id.is_empty());
        let inside = ANCHOR_INSIDE.contains(&tag);
        if let Some(id) = id
            && !inside
            && tag != "a"
        {
            self.anchor(id);
        }
        let open_with_anchor = |me: &mut Converter, open: &str| {
            me.out.push_str(open);
            if let Some(id) = id
                && inside
            {
                me.anchor(id);
            }
        };
        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                open_with_anchor(self, &format!("<{tag}>"));
                self.blocks(n);
                let _ = write!(self.out, "</{tag}>");
            }
            "p" | "blockquote" => {
                open_with_anchor(self, &format!("<{tag}>"));
                self.blocks(n);
                let _ = write!(self.out, "</{tag}>");
            }
            "ul" if is_task_list(n) => self.task_list(n),
            "ul" | "menu" => {
                self.out.push_str("<ul>");
                self.blocks(n);
                self.out.push_str("</ul>");
            }
            "ol" => {
                match n.attribute("start").filter(|s| s.parse::<u32>().is_ok()) {
                    Some(start) => {
                        let _ = write!(self.out, "<ol start=\"{}\">", esc(start));
                    }
                    None => self.out.push_str("<ol>"),
                }
                self.blocks(n);
                self.out.push_str("</ol>");
            }
            "li" => {
                open_with_anchor(self, "<li>");
                self.blocks(n);
                self.out.push_str("</li>");
            }
            "dt" => {
                open_with_anchor(self, "<p><strong>");
                self.blocks(n);
                self.out.push_str("</strong></p>");
            }
            "dd" => {
                open_with_anchor(self, "<p style=\"margin-left: 30.0px;\">");
                self.blocks(n);
                self.out.push_str("</p>");
            }
            "figcaption" => {
                self.out.push_str("<p><em>");
                self.blocks(n);
                self.out.push_str("</em></p>");
            }
            "hr" => self.out.push_str("<hr />"),
            "br" => self.out.push_str("<br />"),
            "strong" | "b" => self.wrap(n, "strong"),
            "em" | "i" | "var" => self.wrap(n, "em"),
            "u" | "ins" => self.wrap(n, "u"),
            "sub" => self.wrap(n, "sub"),
            "sup" => self.wrap(n, "sup"),
            "code" | "kbd" | "samp" | "tt" => self.wrap(n, "code"),
            "del" | "s" | "strike" => {
                self.out
                    .push_str("<span style=\"text-decoration: line-through;\">");
                self.blocks(n);
                self.out.push_str("</span>");
            }
            "pre" => self.code_block(n),
            "img" => self.image(n),
            "a" => self.link(n),
            "table" => self.table(n),
            "details" => self.details(n),
            "input" => {
                if n.attribute("type") == Some("checkbox") {
                    let mark = if n.has_attribute("checked") {
                        "\u{2611} "
                    } else {
                        "\u{2610} "
                    };
                    self.out.push_str(mark);
                }
            }
            "section" if has_class(n, "footnotes") || n.has_attribute("data-footnotes") => {
                self.footnote_section(n)
            }
            // Layout-only and unknown elements (div, section, span, abbr,
            // figure, ...): the tag is dropped and the content kept.
            _ => self.blocks(n),
        }
    }

    fn wrap(&mut self, n: Node, tag: &str) {
        let _ = write!(self.out, "<{tag}>");
        self.blocks(n);
        let _ = write!(self.out, "</{tag}>");
    }

    /// Content of an `<ac:link-body>`: only b, strong, em, i, code, tt,
    /// sub, sup, br and span are allowed there.
    fn link_body_element(&mut self, n: Node, tag: &str) {
        let mapped = match tag {
            "strong" | "b" => Some("strong"),
            "em" | "i" | "var" => Some("em"),
            "code" | "kbd" | "samp" | "tt" => Some("code"),
            "sub" => Some("sub"),
            "sup" => Some("sup"),
            _ => None,
        };
        match (tag, mapped) {
            ("br", _) => self.out.push_str("<br />"),
            ("img", _) => {
                let alt = n.attribute("alt").unwrap_or("").trim();
                self.out.push_str(&esc(alt));
            }
            (_, Some(t)) => self.wrap(n, t),
            _ => self.blocks(n),
        }
    }

    fn image(&mut self, n: Node) {
        let src = n.attribute("src").unwrap_or("").trim();
        let alt = n.attribute("alt").unwrap_or("").trim();
        let mut attrs = String::new();
        if !alt.is_empty() {
            let _ = write!(attrs, " ac:alt=\"{}\"", esc(alt));
        }
        if let Some(t) = n.attribute("title").filter(|t| !t.trim().is_empty()) {
            let _ = write!(attrs, " ac:title=\"{}\"", esc(t.trim()));
        }
        let px = |name: &str| {
            n.attribute(name)
                .and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok())
                .filter(|v| *v > 0)
        };
        let is_math = has_class(n, "math-inline") || has_class(n, "math-display");
        if let Some(h) = px("height")
            && has_class(n, "math-inline")
        {
            let _ = write!(attrs, " ac:height=\"{h}\"");
        } else if let Some(w) = px("width") {
            let _ = write!(attrs, " ac:width=\"{w}\"");
        }
        if is_math && has_class(n, "math-display") {
            attrs.push_str(" ac:align=\"center\"");
        }
        if n.has_attribute(REMOTE_ATTR) || is_remote(src) {
            let _ = write!(
                self.out,
                "<ac:image{attrs}><ri:url ri:value=\"{}\" /></ac:image>",
                esc(src)
            );
            return;
        }
        match src.strip_prefix("../").and_then(attachment_name) {
            Some(name) => {
                self.attachments.insert(name.to_string());
                let _ = write!(
                    self.out,
                    "<ac:image{attrs}><ri:attachment ri:filename=\"{}\" /></ac:image>",
                    esc(name)
                );
            }
            None => {
                self.warnings.push(format!(
                    "image {src:?} cannot be shown on Confluence; its alt text is kept"
                ));
                self.out.push_str(&esc(alt));
            }
        }
    }

    /// The anchor that an in-book link points to, if it is one.
    fn anchor_for(&self, href: &str) -> Option<String> {
        let (file, frag) = match href.split_once('#') {
            Some((f, frag)) => (f, Some(frag)),
            None => (href, None),
        };
        let frag = frag.map(|f| {
            percent_encoding::percent_decode_str(f)
                .decode_utf8_lossy()
                .into_owned()
        });
        if file.is_empty() {
            return frag.filter(|f| !f.is_empty());
        }
        if !self.files.contains(file) {
            return None;
        }
        match frag.filter(|f| !f.is_empty()) {
            Some(f) => Some(f),
            None => self.chapter_anchor.get(file).cloned(),
        }
    }

    fn link(&mut self, n: Node) {
        if let Some(id) = n.attribute("id").filter(|i| !i.is_empty()) {
            self.anchor(id);
        }
        let href = n.attribute("href").unwrap_or("").trim().to_string();
        if href.is_empty() {
            self.blocks(n);
            return;
        }
        if is_remote(&href) || has_scheme(&href, &["mailto:", "tel:", "ftp://", "ftps://"]) {
            let _ = write!(self.out, "<a href=\"{}\">", esc(&href));
            self.blocks(n);
            self.out.push_str("</a>");
            return;
        }
        if let Some(name) = href.strip_prefix("../").and_then(attachment_name) {
            self.attachments.insert(name.to_string());
            let _ = write!(
                self.out,
                "<ac:link><ri:attachment ri:filename=\"{}\" />",
                esc(name)
            );
            self.link_body(n);
            self.out.push_str("</ac:link>");
            return;
        }
        match self.anchor_for(&href) {
            Some(anchor) => {
                let _ = write!(self.out, "<ac:link ac:anchor=\"{}\">", esc(&anchor));
                self.link_body(n);
                self.out.push_str("</ac:link>");
            }
            // Anything else (an unresolved local file) keeps only its text.
            None => self.blocks(n),
        }
    }

    /// The body of an `<ac:link>`: plain text in CDATA when the link has
    /// no markup, otherwise a rich `<ac:link-body>`.
    fn link_body(&mut self, n: Node) {
        let plain = n.children().all(|c| c.is_text());
        if plain {
            let text = text_of(n);
            if !text.is_empty() {
                let _ = write!(
                    self.out,
                    "<ac:plain-text-link-body>{}</ac:plain-text-link-body>",
                    cdata(&text)
                );
            }
            return;
        }
        self.out.push_str("<ac:link-body>");
        let was = std::mem::replace(&mut self.in_link_body, true);
        self.blocks(n);
        self.in_link_body = was;
        self.out.push_str("</ac:link-body>");
    }

    fn code_block(&mut self, pre: Node) {
        let code_el = pre.children().find(|c| c.has_tag_name("code"));
        let lang = code_el
            .and_then(language_class)
            .or_else(|| language_class(pre))
            .unwrap_or_default();
        let mut text = text_of(pre);
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        let _ = write!(
            self.out,
            "<ac:structured-macro ac:name=\"code\" ac:schema-version=\"1\"><ac:parameter ac:name=\"language\">{}</ac:parameter><ac:plain-text-body>{}</ac:plain-text-body></ac:structured-macro>",
            code_language(&lang),
            cdata(&text)
        );
    }

    fn task_list(&mut self, ul: Node) {
        self.out.push_str("<ac:task-list>");
        for li in ul.children().filter(|c| c.has_tag_name("li")) {
            let checked = task_checkbox(li).is_some_and(|i| i.has_attribute("checked"));
            let _ = write!(
                self.out,
                "<ac:task><ac:task-status>{}</ac:task-status><ac:task-body>",
                if checked { "complete" } else { "incomplete" }
            );
            if let Some(id) = li.attribute("id").filter(|i| !i.is_empty()) {
                self.anchor(id);
            }
            let input = task_checkbox(li).map(|i| i.id());
            let mut skip_space = true;
            for c in li.children() {
                // A loose list item wraps its text in <p>; tasks hold inline content.
                if c.has_tag_name("p") && input.is_some_and(|i| c.children().any(|x| x.id() == i)) {
                    for cc in c.children() {
                        self.task_child(cc, input, &mut skip_space);
                    }
                    continue;
                }
                self.task_child(c, input, &mut skip_space);
            }
            self.out.push_str("</ac:task-body></ac:task>");
        }
        self.out.push_str("</ac:task-list>");
    }

    fn task_child(&mut self, c: Node, input: Option<roxmltree::NodeId>, skip_space: &mut bool) {
        if Some(c.id()) == input {
            return;
        }
        if *skip_space && c.is_text() {
            let t = c.text().unwrap_or("").trim_start();
            if t.is_empty() {
                return;
            }
            *skip_space = false;
            self.out.push_str(&esc(t));
            return;
        }
        *skip_space = false;
        self.node(c);
    }

    fn table(&mut self, t: Node) {
        let caption = t.children().find(|c| c.has_tag_name("caption"));
        if let Some(c) = caption {
            self.out.push_str("<p><strong>");
            self.blocks(c);
            self.out.push_str("</strong></p>");
        }
        self.out.push_str("<table><tbody>");
        let mut rows = Vec::new();
        for c in t.children().filter(Node::is_element) {
            match c.tag_name().name() {
                "tr" => rows.push(c),
                "thead" | "tbody" | "tfoot" => {
                    rows.extend(c.children().filter(|r| r.has_tag_name("tr")))
                }
                _ => {}
            }
        }
        for row in rows {
            self.out.push_str("<tr>");
            for cell in row.children().filter(Node::is_element) {
                let tag = match cell.tag_name().name() {
                    "th" => "th",
                    "td" => "td",
                    _ => continue,
                };
                let _ = write!(self.out, "<{tag}");
                for a in ["colspan", "rowspan"] {
                    if let Some(v) = cell.attribute(a).filter(|v| v.parse::<u32>().is_ok()) {
                        let _ = write!(self.out, " {a}=\"{v}\"");
                    }
                }
                if let Some(align) = text_align(cell) {
                    let _ = write!(self.out, " style=\"text-align: {align};\"");
                }
                self.out.push('>');
                if let Some(id) = cell.attribute("id").filter(|i| !i.is_empty()) {
                    self.anchor(id);
                }
                self.blocks(cell);
                let _ = write!(self.out, "</{tag}>");
            }
            self.out.push_str("</tr>");
        }
        self.out.push_str("</tbody></table>");
    }

    fn details(&mut self, d: Node) {
        let summary = d.children().find(|c| c.has_tag_name("summary"));
        self.out
            .push_str("<ac:structured-macro ac:name=\"expand\" ac:schema-version=\"1\">");
        if let Some(s) = summary {
            let title = text_of(s);
            let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            if !title.is_empty() {
                let _ = write!(
                    self.out,
                    "<ac:parameter ac:name=\"title\">{}</ac:parameter>",
                    esc(&title)
                );
            }
        }
        self.out.push_str("<ac:rich-text-body>");
        for c in d.children() {
            if Some(c.id()) == summary.map(|s| s.id()) {
                continue;
            }
            self.node(c);
        }
        self.out
            .push_str("</ac:rich-text-body></ac:structured-macro>");
    }

    /// A footnote section: its lists go to the end of the page.
    fn footnote_section(&mut self, s: Node) {
        let main = std::mem::take(&mut self.out);
        for c in s.children() {
            if c.has_tag_name("ol") || c.has_tag_name("ul") {
                self.node(c);
            }
        }
        let notes = std::mem::replace(&mut self.out, main);
        self.footnotes.push_str(&notes);
        self.footnotes.push('\n');
    }
}

/// Whether every list item starts with a checkbox (a GFM task list).
fn is_task_list(ul: Node) -> bool {
    let items: Vec<Node> = ul.children().filter(Node::is_element).collect();
    !items.is_empty()
        && items
            .iter()
            .all(|li| li.has_tag_name("li") && task_checkbox(*li).is_some())
}

/// The leading checkbox of a task list item (directly or in its first `<p>`).
fn task_checkbox<'a, 'i>(li: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    fn first<'a, 'i>(n: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
        n.children()
            .find(|c| c.is_element() || c.text().is_some_and(|t| !t.trim().is_empty()))
    }
    let mut c = first(li)?;
    if c.has_tag_name("p") {
        c = first(c)?;
    }
    (c.has_tag_name("input") && c.attribute("type") == Some("checkbox")).then_some(c)
}

fn has_class(n: Node, class: &str) -> bool {
    n.attribute("class")
        .is_some_and(|c| c.split_whitespace().any(|x| x == class))
}

/// `left`, `center` or `right` from a cell's `style` or `align`.
fn text_align(cell: Node) -> Option<&'static str> {
    let from = |v: &str| {
        let v = v.to_ascii_lowercase();
        ["left", "center", "right"]
            .into_iter()
            .find(|a| v.contains(a))
    };
    if let Some(style) = cell.attribute("style") {
        for decl in style.split(';') {
            if let Some((k, v)) = decl.split_once(':')
                && k.trim().eq_ignore_ascii_case("text-align")
            {
                return from(v);
            }
        }
    }
    cell.attribute("align").and_then(from)
}

/// The language named by a `language-x` / `lang-x` class.
fn language_class(n: Node) -> Option<String> {
    n.attribute("class")?.split_whitespace().find_map(|c| {
        c.strip_prefix("language-")
            .or_else(|| c.strip_prefix("lang-"))
            .map(|l| l.to_ascii_lowercase())
    })
}

/// All text below `n`.
fn text_of(n: Node) -> String {
    n.descendants()
        .filter(|d| d.is_text())
        .filter_map(|d| d.text())
        .collect()
}

fn is_remote(s: &str) -> bool {
    has_scheme(s, &["http://", "https://"])
}

fn has_scheme(s: &str, schemes: &[&str]) -> bool {
    let l = s.to_ascii_lowercase();
    schemes.iter().any(|p| l.starts_with(p))
}

/// Removes characters that XML 1.0 does not allow.
fn xml_chars(s: &str) -> std::borrow::Cow<'_, str> {
    let ok = |c: char| {
        matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{FFFE}' && c != '\u{FFFF}')
    };
    if s.chars().all(ok) {
        std::borrow::Cow::Borrowed(s)
    } else {
        std::borrow::Cow::Owned(s.chars().filter(|c| ok(*c)).collect())
    }
}

/// Escapes text for XML content and attribute values.
fn esc(s: &str) -> String {
    let s = xml_chars(s);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// A CDATA section holding `s`; `]]>` inside is split across two sections.
pub fn cdata(s: &str) -> String {
    let s = xml_chars(s);
    format!("<![CDATA[{}]]>", s.replace("]]>", "]]]]><![CDATA[>"))
}

/// Maps a Markdown fence language to a Confluence code macro `language`
/// value; unknown languages give `none`.
///
/// The classic names (`java`, `js`, `py`, `bash`, `cpp`, `xml`, `yml`, ...)
/// work in every Confluence Data Center / Server version; the others
/// (`rust`, `go`, `kotlin`, `typescript`, ...) are in the language list of
/// current Data Center releases (7.x and later).
pub fn code_language(lang: &str) -> &'static str {
    let l = lang.trim().to_ascii_lowercase();
    match l.as_str() {
        "actionscript" | "actionscript3" | "as3" => "actionscript3",
        "applescript" => "applescript",
        "sh" | "bash" | "shell" | "zsh" | "console" | "shell-session" | "ksh" => "bash",
        "c#" | "cs" | "csharp" => "csharp",
        "coldfusion" | "cfm" => "coldfusion",
        "c++" | "cpp" | "cxx" | "hpp" | "cc" => "cpp",
        "css" => "css",
        "pascal" | "delphi" => "delphi",
        "diff" | "patch" => "diff",
        "erlang" | "erl" => "erl",
        "groovy" | "gradle" => "groovy",
        "html" | "xhtml" | "xml" | "svg" | "xsl" | "xslt" | "plist" => "xml",
        "java" => "java",
        "javafx" => "javafx",
        "js" | "javascript" | "mjs" | "cjs" | "node" => "js",
        "perl" | "pl" => "perl",
        "php" => "php",
        "powershell" | "ps1" | "pwsh" | "ps" => "powershell",
        "py" | "python" | "python3" | "py3" => "py",
        "ruby" | "rb" => "ruby",
        "sass" | "scss" => "sass",
        "scala" => "scala",
        "sql" | "mysql" | "postgresql" | "psql" | "plsql" | "tsql" => "sql",
        "vb" | "vbnet" | "vb.net" | "visualbasic" | "vba" => "vb",
        "yaml" | "yml" => "yml",
        "text" | "txt" | "plain" | "plaintext" => "text",
        // Newer Data Center languages.
        "c" | "h" => "c",
        "rust" | "rs" => "rust",
        "go" | "golang" => "go",
        "kotlin" | "kt" | "kts" => "kotlin",
        "typescript" | "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "json" | "jsonc" | "json5" => "json",
        "swift" => "swift",
        "dockerfile" | "docker" => "dockerfile",
        "haskell" | "hs" => "haskell",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "elixir" | "ex" | "exs" => "elixir",
        "clojure" | "clj" => "clojure",
        "julia" | "jl" => "julia",
        "matlab" => "matlab",
        "ocaml" | "ml" => "ocaml",
        "graphql" | "gql" => "graphql",
        "objective-c" | "objc" | "objectivec" => "objective-c",
        "hcl" | "terraform" | "tf" => "hcl",
        "nginx" => "nginx",
        "tex" | "latex" => "tex",
        "fortran" | "f90" => "fortran",
        "cuda" | "cu" => "cuda",
        "d" => "d",
        "ada" => "ada",
        "verilog" => "verilog",
        "vhdl" => "vhdl",
        "scheme" => "scheme",
        "racket" => "racket",
        "prolog" => "prolog",
        "protobuf" | "proto" => "protobuf",
        "puppet" => "puppet",
        "tcl" => "tcl",
        "coffeescript" | "coffee" => "coffeescript",
        "livescript" => "livescript",
        "smalltalk" => "smalltalk",
        "octave" => "octave",
        "haxe" => "haxe",
        "arduino" | "ino" => "arduino",
        "rst" | "restructuredtext" => "rest",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a storage fragment inside a root that declares the
    /// namespaces, as the task requires.
    fn parse_ok(xml: &str) {
        let wrapped = format!("<root xmlns:ac=\"{AC_NS}\" xmlns:ri=\"{RI_NS}\">{xml}</root>");
        if let Err(e) = roxmltree::Document::parse(&wrapped) {
            panic!("not well-formed: {e}\n{xml}");
        }
    }

    fn chapter(file: &str, body: &str) -> Chapter {
        Chapter {
            title: "T".into(),
            file_name: file.into(),
            body_xhtml: body.into(),
            ..Chapter::default()
        }
    }

    fn conv(body: &str) -> (String, BTreeSet<String>) {
        let (xml, used, _) = to_storage(&[chapter("ch001.xhtml", body)], false).unwrap();
        parse_ok(&xml);
        (xml, used)
    }

    #[test]
    fn headings_paragraphs_and_text_effects() {
        let (x, _) = conv(
            "<h1 id=\"intro\">Intro</h1><h6 id=\"deep\">Deep</h6><p>a <strong>b</strong> <em>c</em> <del>d</del> <sub>e</sub><sup>f</sup> <code>g &lt;h&gt;</code> <b>i</b> <u>u</u> &amp;</p>",
        );
        assert!(x.contains("<h1><ac:structured-macro ac:name=\"anchor\" ac:schema-version=\"1\"><ac:parameter ac:name=\"\">intro</ac:parameter></ac:structured-macro>Intro</h1>"), "{x}");
        assert!(x.contains("<h6>"), "{x}");
        assert!(x.contains("<strong>b</strong> <em>c</em>"), "{x}");
        assert!(
            x.contains("<span style=\"text-decoration: line-through;\">d</span>"),
            "{x}"
        );
        assert!(x.contains("<sub>e</sub><sup>f</sup>"), "{x}");
        assert!(x.contains("<code>g &lt;h&gt;</code>"), "{x}");
        assert!(x.contains("<strong>i</strong> <u>u</u> &amp;"), "{x}");
    }

    #[test]
    fn lists_blockquote_hr_and_tables() {
        let (x, _) = conv(
            "<ul><li>a<ul><li>b</li></ul></li></ul><ol start=\"3\"><li>c</li></ol><blockquote><p>q</p></blockquote><hr/><table><caption>Cap</caption><thead><tr><th>H</th><th style=\"text-align: center\">C</th></tr></thead><tbody><tr><td colspan=\"2\">x</td></tr></tbody></table>",
        );
        assert!(x.contains("<ul><li>a<ul><li>b</li></ul></li></ul>"), "{x}");
        assert!(x.contains("<ol start=\"3\"><li>c</li></ol>"), "{x}");
        assert!(x.contains("<blockquote><p>q</p></blockquote><hr />"), "{x}");
        assert!(x.contains("<p><strong>Cap</strong></p><table><tbody><tr><th>H</th><th style=\"text-align: center;\">C</th></tr><tr><td colspan=\"2\">x</td></tr></tbody></table>"), "{x}");
        assert!(!x.contains("thead"), "{x}");
    }

    #[test]
    fn images_become_attachments_or_urls() {
        let (x, used) = conv(
            "<p><img src=\"../images/abc.png\" alt=\"Chart &quot;1&quot;\" title=\"T\" width=\"300\" height=\"100\"/> <img src=\"https://e.org/x.png\" alt=\"R\" data-md2epub-remote=\"1\"/> <img src=\"../images/m.png\" alt=\"E=mc^2\" class=\"math-inline\" width=\"63\" height=\"13\"/></p>",
        );
        assert!(x.contains("<ac:image ac:alt=\"Chart &quot;1&quot;\" ac:title=\"T\" ac:width=\"300\"><ri:attachment ri:filename=\"abc.png\" /></ac:image>"), "{x}");
        assert!(
            x.contains(
                "<ac:image ac:alt=\"R\"><ri:url ri:value=\"https://e.org/x.png\" /></ac:image>"
            ),
            "{x}"
        );
        assert!(
            x.contains("ac:alt=\"E=mc^2\" ac:height=\"13\"><ri:attachment ri:filename=\"m.png\""),
            "{x}"
        );
        assert_eq!(
            used.into_iter().collect::<Vec<_>>(),
            vec!["abc.png".to_string(), "m.png".to_string()]
        );
    }

    #[test]
    fn code_blocks_use_the_code_macro_without_highlighting() {
        let (x, _) = conv(
            "<pre class=\"highlight\"><code class=\"language-rust\"><span style=\"color:#a71d5d\">fn</span> main() { let s = \"]]&gt;\"; }\n</code></pre><pre><code>plain &lt;x&gt;\n</code></pre><pre><code class=\"language-brainfuck\">+</code></pre>",
        );
        assert!(x.contains("<ac:structured-macro ac:name=\"code\" ac:schema-version=\"1\"><ac:parameter ac:name=\"language\">rust</ac:parameter><ac:plain-text-body><![CDATA[fn main() { let s = \"]]]]><![CDATA[>\"; }]]></ac:plain-text-body></ac:structured-macro>"), "{x}");
        assert!(!x.contains("color:"), "{x}");
        assert!(
            x.contains("<ac:parameter ac:name=\"language\">none</ac:parameter><ac:plain-text-body><![CDATA[plain <x>]]>"),
            "{x}"
        );
        assert!(x.matches("language\">none<").count() == 2, "{x}");
        // The CDATA content reads back unchanged.
        let wrapped = format!("<r xmlns:ac=\"{AC_NS}\" xmlns:ri=\"{RI_NS}\">{x}</r>");
        let doc = roxmltree::Document::parse(&wrapped).unwrap();
        let body = doc
            .descendants()
            .find(|n| n.tag_name().name() == "plain-text-body")
            .unwrap();
        assert_eq!(text_of(body), "fn main() { let s = \"]]>\"; }");
    }

    #[test]
    fn language_map() {
        assert_eq!(code_language("Python"), "py");
        assert_eq!(code_language("sh"), "bash");
        assert_eq!(code_language("yaml"), "yml");
        assert_eq!(code_language("html"), "xml");
        assert_eq!(code_language("ts"), "typescript");
        assert_eq!(code_language(""), "none");
        assert_eq!(code_language("mermaid"), "none");
    }

    #[test]
    fn links_inside_and_outside_the_page() {
        let chapters = [
            chapter(
                "ch001.xhtml",
                "<h1 id=\"a\">A</h1><p><a href=\"#b-sec\">same</a> <a href=\"ch002.xhtml\">other <em>doc</em></a> <a href=\"ch002.xhtml#b-sec\">sec</a> <a href=\"https://x.org/?a=1&amp;b=2\">web</a> <a href=\"mailto:a@b.c\">mail</a> <a href=\"missing.md\">gone</a> <a href=\"../images/f.png\">file</a></p>",
            ),
            chapter("ch002.xhtml", "<p>intro</p><h2 id=\"b-sec\">B</h2>"),
        ];
        let (x, used, _) = to_storage(&chapters, true).unwrap();
        parse_ok(&x);
        assert!(x.starts_with("<ac:structured-macro ac:name=\"toc\""), "{x}");
        assert!(x.contains("<ac:link ac:anchor=\"b-sec\"><ac:plain-text-link-body><![CDATA[same]]></ac:plain-text-link-body></ac:link>"), "{x}");
        assert!(x.contains("<ac:link ac:anchor=\"md2epub-ch002\"><ac:link-body>other <em>doc</em></ac:link-body></ac:link>"), "{x}");
        assert!(x.contains("<ac:parameter ac:name=\"\">md2epub-ch002</ac:parameter></ac:structured-macro><p>intro</p>"), "{x}");
        assert!(
            x.contains("<ac:link ac:anchor=\"b-sec\"><ac:plain-text-link-body><![CDATA[sec]]>"),
            "{x}"
        );
        assert!(
            x.contains("<a href=\"https://x.org/?a=1&amp;b=2\">web</a>"),
            "{x}"
        );
        assert!(x.contains("<a href=\"mailto:a@b.c\">mail</a>"), "{x}");
        assert!(x.contains(" gone "), "{x}");
        assert!(x.contains("<ac:link><ri:attachment ri:filename=\"f.png\" /><ac:plain-text-link-body><![CDATA[file]]>"), "{x}");
        assert!(used.contains("f.png"));
    }

    #[test]
    fn links_to_a_document_starting_with_a_heading_use_its_anchor() {
        let chapters = [
            chapter("ch001.xhtml", "<p><a href=\"ch002.xhtml\">next</a></p>"),
            chapter("ch002.xhtml", "<h1 id=\"part-two\">Two</h1>"),
        ];
        let (x, _, _) = to_storage(&chapters, false).unwrap();
        parse_ok(&x);
        assert!(x.contains("<ac:link ac:anchor=\"part-two\">"), "{x}");
        assert!(!x.contains("md2epub-ch002"), "{x}");
        assert_eq!(
            x.matches("<ac:parameter ac:name=\"\">part-two<").count(),
            1,
            "{x}"
        );
    }

    #[test]
    fn footnotes_go_to_one_section_with_back_links() {
        let body = "<p>Text.<sup class=\"footnote-ref\"><a href=\"#fn-1\" id=\"fnref-1\" data-footnote-ref=\"\" xmlns:epub=\"http://www.idpf.org/2007/ops\" epub:type=\"noteref\">1</a></sup></p><section class=\"footnotes\" data-footnotes=\"\"><ol><li id=\"fn-1\"><p>Note. <a href=\"#fnref-1\" class=\"footnote-backref\">\u{21A9}\u{FE0E}</a></p></li></ol></section><p>After.</p>";
        let (x, _) = conv(body);
        assert!(x.contains("<sup><ac:structured-macro ac:name=\"anchor\" ac:schema-version=\"1\"><ac:parameter ac:name=\"\">fnref-1</ac:parameter></ac:structured-macro><ac:link ac:anchor=\"fn-1\"><ac:plain-text-link-body><![CDATA[1]]></ac:plain-text-link-body></ac:link></sup>"), "{x}");
        let notes = x.find("<h2>Footnotes</h2>").expect("footnotes heading");
        assert!(x.find("After.").unwrap() < notes, "{x}");
        assert!(x[notes..].contains("<ol><li><ac:structured-macro ac:name=\"anchor\" ac:schema-version=\"1\"><ac:parameter ac:name=\"\">fn-1</ac:parameter></ac:structured-macro><p>Note. <ac:link ac:anchor=\"fnref-1\">"), "{x}");
    }

    #[test]
    fn task_lists() {
        let (x, _) = conv(
            "<ul><li><input type=\"checkbox\" checked=\"\" disabled=\"\"/> Done <strong>it</strong></li><li><p><input type=\"checkbox\" disabled=\"\"/> Loose</p><ul><li>sub</li></ul></li></ul><ul><li>normal</li></ul>",
        );
        assert!(x.contains("<ac:task-list><ac:task><ac:task-status>complete</ac:task-status><ac:task-body>Done <strong>it</strong></ac:task-body></ac:task><ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>Loose<ul><li>sub</li></ul></ac:task-body></ac:task></ac:task-list>"), "{x}");
        assert!(x.contains("<ul><li>normal</li></ul>"), "{x}");
        assert!(!x.contains("<input"), "{x}");
    }

    #[test]
    fn layout_elements_are_unwrapped_and_scripts_dropped() {
        let (x, _) = conv(
            "<div class=\"callout\" style=\"border:1px\"><p>in <abbr title=\"t\">HTML</abbr><br/>x</p></div><style>p{}</style><figure class=\"mermaid\"><img src=\"../images/d.png\" alt=\"Flowchart\" width=\"347\" height=\"454\"/><figcaption>Cap</figcaption></figure><details><summary>More</summary><p>hidden</p></details><dl><dt>T</dt><dd>D</dd></dl>",
        );
        assert!(x.contains("<p>in HTML<br />x</p>"), "{x}");
        assert!(
            !x.contains("<div") && !x.contains("p{}") && !x.contains("<figure"),
            "{x}"
        );
        assert!(
            x.contains("<ac:image ac:alt=\"Flowchart\" ac:width=\"347\">"),
            "{x}"
        );
        assert!(x.contains("<p><em>Cap</em></p>"), "{x}");
        assert!(x.contains("<ac:structured-macro ac:name=\"expand\" ac:schema-version=\"1\"><ac:parameter ac:name=\"title\">More</ac:parameter><ac:rich-text-body><p>hidden</p></ac:rich-text-body></ac:structured-macro>"), "{x}");
        assert!(
            x.contains("<p><strong>T</strong></p><p style=\"margin-left: 30.0px;\">D</p>"),
            "{x}"
        );
    }

    #[test]
    fn element_ids_become_anchors() {
        let (x, _) = conv("<table id=\"t\"><tr><td>x</td></tr></table><p id=\"p\">y</p>");
        assert!(x.starts_with("<ac:structured-macro ac:name=\"anchor\" ac:schema-version=\"1\"><ac:parameter ac:name=\"\">t</ac:parameter></ac:structured-macro><table>"), "{x}");
        assert!(x.contains("<p><ac:structured-macro ac:name=\"anchor\" ac:schema-version=\"1\"><ac:parameter ac:name=\"\">p</ac:parameter></ac:structured-macro>y</p>"), "{x}");
    }

    #[test]
    fn invalid_xml_characters_are_removed() {
        assert_eq!(esc("a\u{1}b<"), "ab&lt;");
        assert_eq!(
            cdata("x]]>y]]>"),
            "<![CDATA[x]]]]><![CDATA[>y]]]]><![CDATA[>]]>"
        );
    }

    #[test]
    fn output_dir_rules() {
        use crate::model::OutputLocation::{Cwd, Input};
        let inputs = [PathBuf::from("src/a.md")];
        let od = |o: Option<&str>, l| output_dir(o.map(Path::new), l, &inputs);
        assert_eq!(od(None, Cwd).unwrap().0, PathBuf::from("a.confluence"));
        assert_eq!(
            od(None, Input).unwrap().0,
            PathBuf::from("src/a.confluence")
        );
        assert_eq!(
            od(Some("x.confluence"), Cwd).unwrap().0,
            PathBuf::from("x.confluence")
        );
        assert_eq!(
            od(Some("out/x"), Cwd).unwrap().0,
            PathBuf::from("out/x.confluence")
        );
        assert_eq!(
            od(Some("out/"), Cwd).unwrap().0,
            PathBuf::from("out/a.confluence")
        );
        let (p, note) = od(Some("x.txt"), Cwd).unwrap();
        assert_eq!(p, PathBuf::from("x.txt.confluence"));
        assert!(note.is_some());
        let err = od(Some("book.epub"), Cwd).unwrap_err().to_string();
        assert!(
            err.contains("is a directory") && err.contains("book.confluence"),
            "{err}"
        );
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        assert_eq!(od(Some(d), Cwd).unwrap().0, dir.path().join("a.confluence"));
    }

    #[test]
    fn export_dir_round_trip_and_checks() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.confluence");
        let export = Export {
            title: "T".into(),
            page_xml: "<p>x</p>\n".into(),
            attachments: vec![Attachment {
                filename: "ab.png".into(),
                media_type: "image/png".into(),
                sha256: sha256_hex(b"png"),
                data: b"png".to_vec(),
            }],
        };
        write_dir(&export, &out).unwrap();
        assert!(is_export_dir(&out));
        assert_eq!(read_dir(&out).unwrap(), export);
        // Writing again replaces it; a stale attachment disappears.
        std::fs::write(out.join("attachments/old.png"), b"x").unwrap();
        write_dir(&export, &out).unwrap();
        assert!(!out.join("attachments/old.png").exists());
        // A changed attachment is refused.
        std::fs::write(out.join("attachments/ab.png"), b"changed").unwrap();
        assert!(format!("{:#}", read_dir(&out).unwrap_err()).contains("sha256"));
        // A non-empty foreign directory is never overwritten.
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("keep.txt"), b"x").unwrap();
        assert!(write_dir(&export, &other).is_err());
        assert!(other.join("keep.txt").is_file());
    }
}

//! EPUB 3 packaging (owned by T9).
//!
//! [`write`] produces
//!
//! ```text
//! mimetype                      (first entry, stored, no extra fields)
//! META-INF/container.xml
//! OEBPS/content.opf  nav.xhtml  toc.ncx
//! OEBPS/text/cover.xhtml  ch001.xhtml ...
//! OEBPS/images/<hash>.<ext>
//! OEBPS/css/base.css
//! ```
//!
//! with a complete EPUB 3.0 OPF (UUID identifier derived from the content
//! hash unless `meta.identifier` is set, title, one creator per author,
//! language, `dcterms:modified`, manifest with media types and `nav`,
//! `cover-image`, `svg`/`mathml` properties, spine in order, EPUB 2
//! `<meta name="cover">` and `<guide>`), a nested `nav.xhtml` (headings down
//! to level 3, skipping `in_toc: false`, plus landmarks) and a matching
//! `toc.ncx`. When `book.cover` is `None`, a title/author cover is generated
//! as SVG and rasterized through `svg::rasterize`; if even that fails, the
//! cover page shows the title as text. The file is written to a temporary
//! sibling and renamed, so a failed run never leaves a broken EPUB.
//!
//! `dcterms:modified` is the current UTC time, or `SOURCE_DATE_EPOCH` when
//! that environment variable holds a Unix timestamp (reproducible builds).

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context as _, Result, bail};
use sha2::{Digest, Sha256};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use crate::dom::escape;
use crate::model::{Book, Chapter, Metadata, Resource, media_type_for_ext};

/// Writes `book` as an EPUB file at `out`. See the module docs.
///
/// Chapter file names must be plain, unique file names (`ch001.xhtml`);
/// anything else is an error, because links between chapters depend on them.
/// Resources with unusable or duplicate `href`s are skipped.
pub fn write(book: &Book, out: &Path) -> Result<()> {
    validate_chapters(&book.chapters)?;
    let package = Package::build(book);

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let tmp = temp_path(out);
    if let Err(e) = write_zip(&package, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", out.display()));
    }
    Ok(())
}

/// A hidden temporary file next to `out`, unique within this process.
fn temp_path(out: &Path) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book.epub".into());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    out.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

/// File names that `write` generates itself inside `OEBPS/`.
const RESERVED: [&str; 5] = [
    "content.opf",
    "nav.xhtml",
    "toc.ncx",
    "css/base.css",
    "text/cover.xhtml",
];

fn validate_chapters(chapters: &[Chapter]) -> Result<()> {
    let mut seen = HashSet::new();
    for (i, ch) in chapters.iter().enumerate() {
        let name = ch.file_name.as_str();
        if name.is_empty()
            || name.contains(['/', '\\', ':'])
            || name.starts_with('.')
            || !is_safe_path(name)
        {
            bail!("chapter {} has an invalid file name {name:?}", i + 1);
        }
        if name.eq_ignore_ascii_case("cover.xhtml") {
            bail!("chapter file name {name:?} is reserved for the cover page");
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            bail!("duplicate chapter file name {name:?}");
        }
    }
    Ok(())
}

/// Whether `href` (relative to `OEBPS/`) is usable as a zip entry and
/// manifest href: relative, no `..`, no empty segments, no URL scheme.
fn is_safe_path(href: &str) -> bool {
    !href.is_empty()
        && !href.starts_with('/')
        && !href.contains(['\\', ':', '?', '#'])
        && href.chars().all(|c| !c.is_control())
        && href
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

// ---------------------------------------------------------------------------
// Package model
// ---------------------------------------------------------------------------

/// One file inside the zip (path relative to `OEBPS/`).
struct Entry<'a> {
    path: String,
    data: Cow<'a, [u8]>,
    stored: bool,
}

/// One `<item>` of the OPF manifest.
struct Item {
    id: String,
    href: String,
    media_type: String,
    properties: Vec<&'static str>,
}

/// Everything that goes into the zip, computed up front.
struct Package<'a> {
    /// `content.opf`, `nav.xhtml`, `toc.ncx`, CSS, cover page, chapters,
    /// images, in zip order (all relative to `OEBPS/`).
    entries: Vec<Entry<'a>>,
}

/// The resolved cover.
enum Cover<'a> {
    /// A cover image stored at `href` (relative to `OEBPS/`).
    Image {
        href: String,
        media_type: String,
        /// `None` when the image is already written as a regular resource.
        data: Option<Cow<'a, [u8]>>,
    },
    /// No image could be produced: the cover page shows the title as text.
    Text,
}

impl<'a> Package<'a> {
    fn build(book: &'a Book) -> Package<'a> {
        let meta = &book.meta;
        let lang = non_empty(meta.lang.trim(), "en").to_string();
        let title = clean_text(&meta.title);
        let title = non_empty(&title, "Untitled").to_string();

        let mut used: HashSet<String> = RESERVED.iter().map(|s| s.to_string()).collect();
        for ch in &book.chapters {
            used.insert(format!("text/{}", ch.file_name).to_ascii_lowercase());
        }

        // Regular resources, skipping unusable and duplicate hrefs.
        let mut resources: Vec<&Resource> = Vec::new();
        for res in &book.resources {
            if is_safe_path(&res.href) && used.insert(res.href.to_ascii_lowercase()) {
                resources.push(res);
            }
        }

        let cover = resolve_cover(book, &title, &resources, &mut used);

        let mut manifest: Vec<Item> = vec![
            Item {
                id: "nav".into(),
                href: "nav.xhtml".into(),
                media_type: "application/xhtml+xml".into(),
                properties: vec!["nav"],
            },
            Item {
                id: "ncx".into(),
                href: "toc.ncx".into(),
                media_type: "application/x-dtbncx+xml".into(),
                properties: vec![],
            },
            Item {
                id: "css".into(),
                href: "css/base.css".into(),
                media_type: "text/css".into(),
                properties: vec![],
            },
            Item {
                id: "cover".into(),
                href: "text/cover.xhtml".into(),
                media_type: "application/xhtml+xml".into(),
                properties: vec![],
            },
        ];
        let mut cover_image_id: Option<String> = None;
        let mut image_entries: Vec<Entry<'a>> = Vec::new();

        if let Cover::Image {
            href,
            media_type,
            data: Some(data),
        } = &cover
        {
            manifest.push(Item {
                id: "cover-image".into(),
                href: href.clone(),
                media_type: media_type.clone(),
                properties: vec!["cover-image"],
            });
            cover_image_id = Some("cover-image".into());
            image_entries.push(Entry {
                path: href.clone(),
                data: data.clone(),
                stored: is_precompressed(media_type),
            });
        }
        for (i, res) in resources.iter().enumerate() {
            let media_type = resource_media_type(res);
            let mut properties = Vec::new();
            let id = match &cover {
                Cover::Image {
                    href, data: None, ..
                } if *href == res.href => {
                    properties.push("cover-image");
                    cover_image_id = Some("cover-image".into());
                    "cover-image".to_string()
                }
                _ => format!("res{}", i + 1),
            };
            image_entries.push(Entry {
                path: res.href.clone(),
                data: Cow::Borrowed(&res.data),
                stored: is_precompressed(&media_type),
            });
            manifest.push(Item {
                id,
                href: res.href.clone(),
                media_type,
                properties,
            });
        }

        let mut spine = vec!["cover".to_string()];
        let mut chapter_entries = Vec::new();
        for (i, ch) in book.chapters.iter().enumerate() {
            let id = format!("ch{}", i + 1);
            let mut properties = Vec::new();
            if contains_element(&ch.body_xhtml, "svg") {
                properties.push("svg");
            }
            if contains_element(&ch.body_xhtml, "math") {
                properties.push("mathml");
            }
            manifest.push(Item {
                id: id.clone(),
                href: format!("text/{}", ch.file_name),
                media_type: "application/xhtml+xml".into(),
                properties,
            });
            spine.push(id);
            let ch_title = clean_text(&ch.title);
            let doc = chapter_document(non_empty(&ch_title, &title), &lang, &ch.body_xhtml);
            chapter_entries.push(Entry {
                path: format!("text/{}", ch.file_name),
                data: Cow::Owned(doc.into_bytes()),
                stored: false,
            });
        }

        let first_chapter = book
            .chapters
            .first()
            .map(|c| format!("text/{}", c.file_name));
        let toc = build_tree(toc_entries(&book.chapters));
        let identifier = match meta.identifier.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => format!("urn:uuid:{}", content_uuid(book)),
        };

        let opf = opf_document(&OpfInput {
            meta,
            title: &title,
            lang: &lang,
            identifier: &identifier,
            modified: &modified_timestamp(),
            manifest: &manifest,
            spine: &spine,
            cover_image_id: cover_image_id.as_deref(),
            first_chapter: first_chapter.as_deref(),
        });
        let nav = nav_document(&title, &lang, &toc, first_chapter.as_deref());
        let ncx = ncx_document(&title, &meta.authors, &identifier, &toc);
        let cover_page = cover_document(&title, &meta.authors, &lang, &cover);

        let mut entries = vec![
            text_entry("content.opf", opf),
            text_entry("nav.xhtml", nav),
            text_entry("toc.ncx", ncx),
            Entry {
                path: "css/base.css".into(),
                data: Cow::Borrowed(book.css.as_bytes()),
                stored: false,
            },
            text_entry("text/cover.xhtml", cover_page),
        ];
        entries.extend(chapter_entries);
        entries.extend(image_entries);
        Package { entries }
    }
}

fn text_entry<'a>(path: &str, text: String) -> Entry<'a> {
    Entry {
        path: path.into(),
        data: Cow::Owned(text.into_bytes()),
        stored: false,
    }
}

/// Picks the cover: the book's own cover (sharing a resource entry when it
/// is byte-identical to one), else a generated one, else a text cover.
fn resolve_cover<'a>(
    book: &'a Book,
    title: &str,
    resources: &[&Resource],
    used: &mut HashSet<String>,
) -> Cover<'a> {
    let (href, media_type, data): (String, String, Cow<'a, [u8]>) = match &book.cover {
        Some(c) if !c.data.is_empty() => {
            let media_type = resource_media_type(c);
            if let Some(r) = resources.iter().find(|r| r.href == c.href)
                && r.data == c.data
            {
                return Cover::Image {
                    href: r.href.clone(),
                    media_type,
                    data: None,
                };
            }
            (c.href.clone(), media_type, Cow::Borrowed(&c.data))
        }
        _ => match generate_cover(&book.meta, title) {
            Ok(png) => (
                "images/cover.png".into(),
                "image/png".into(),
                Cow::Owned(png),
            ),
            Err(_) => return Cover::Text,
        },
    };
    let href = if is_safe_path(&href) && used.insert(href.to_ascii_lowercase()) {
        href
    } else {
        // Clashes with another file or is unusable: pick a fresh name.
        let ext = ext_for_media_type(&media_type);
        let mut n = 0u32;
        loop {
            let candidate = if n == 0 {
                format!("images/cover.{ext}")
            } else {
                format!("images/cover-{n}.{ext}")
            };
            if used.insert(candidate.to_ascii_lowercase()) {
                break candidate;
            }
            n += 1;
        }
    };
    Cover::Image {
        href,
        media_type,
        data: Some(data),
    }
}

fn resource_media_type(res: &Resource) -> String {
    let mt = res.media_type.trim();
    if mt.is_empty() {
        let ext = res.href.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        media_type_for_ext(ext).to_string()
    } else {
        mt.to_string()
    }
}

fn ext_for_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// Formats that are already compressed and gain nothing from deflate.
fn is_precompressed(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Whether serialized XHTML contains an element named `name` (no prefix).
fn contains_element(xhtml: &str, name: &str) -> bool {
    let pat = format!("<{name}");
    xhtml.match_indices(&pat).any(|(i, _)| {
        matches!(
            xhtml[i + pat.len()..].chars().next(),
            Some(' ' | '>' | '/' | '\n' | '\t' | '\r')
        )
    })
}

// ---------------------------------------------------------------------------
// Zip
// ---------------------------------------------------------------------------

fn write_zip(package: &Package<'_>, path: &Path) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;

    for e in &package.entries {
        let opts = if e.stored { stored } else { deflated };
        zip.start_file(format!("OEBPS/{}", e.path), opts)
            .with_context(|| format!("adding OEBPS/{}", e.path))?;
        zip.write_all(&e.data)?;
    }
    let mut w = zip.finish()?;
    w.flush()?;
    let file = w
        .into_inner()
        .map_err(|e| anyhow::anyhow!("flushing {}: {}", path.display(), e.error()))?;
    file.sync_all().ok();
    Ok(())
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

// ---------------------------------------------------------------------------
// OPF
// ---------------------------------------------------------------------------

struct OpfInput<'a> {
    meta: &'a Metadata,
    title: &'a str,
    lang: &'a str,
    identifier: &'a str,
    modified: &'a str,
    manifest: &'a [Item],
    spine: &'a [String],
    cover_image_id: Option<&'a str>,
    first_chapter: Option<&'a str>,
}

fn opf_document(o: &OpfInput<'_>) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        s,
        "<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"bookid\" xml:lang=\"{}\">",
        escape(o.lang)
    );
    s.push_str("  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n");
    let _ = writeln!(
        s,
        "    <dc:identifier id=\"bookid\">{}</dc:identifier>",
        escape(o.identifier)
    );
    let _ = writeln!(s, "    <dc:title>{}</dc:title>", escape(o.title));
    let mut n = 0;
    for author in &o.meta.authors {
        let author = clean_text(author);
        if author.is_empty() {
            continue;
        }
        n += 1;
        let _ = writeln!(
            s,
            "    <dc:creator id=\"creator{n}\">{}</dc:creator>",
            escape(&author)
        );
        let _ = writeln!(
            s,
            "    <meta refines=\"#creator{n}\" property=\"role\" scheme=\"marc:relators\">aut</meta>"
        );
    }
    let _ = writeln!(s, "    <dc:language>{}</dc:language>", escape(o.lang));
    if let Some(date) = o.meta.date.as_deref().map(str::trim)
        && is_w3c_date(date)
    {
        let _ = writeln!(s, "    <dc:date>{}</dc:date>", escape(date));
    }
    let _ = writeln!(
        s,
        "    <meta property=\"dcterms:modified\">{}</meta>",
        o.modified
    );
    s.push_str("    <meta property=\"rendition:layout\">reflowable</meta>\n");
    if let Some(id) = o.cover_image_id {
        let _ = writeln!(s, "    <meta name=\"cover\" content=\"{}\"/>", escape(id));
    }
    s.push_str("  </metadata>\n  <manifest>\n");
    for item in o.manifest {
        let props = if item.properties.is_empty() {
            String::new()
        } else {
            format!(" properties=\"{}\"", item.properties.join(" "))
        };
        let _ = writeln!(
            s,
            "    <item id=\"{}\" href=\"{}\" media-type=\"{}\"{props}/>",
            escape(&item.id),
            escape(&href_encode(&item.href)),
            escape(&item.media_type)
        );
    }
    s.push_str("  </manifest>\n  <spine toc=\"ncx\">\n");
    for id in o.spine {
        let _ = writeln!(s, "    <itemref idref=\"{}\"/>", escape(id));
    }
    s.push_str("  </spine>\n  <guide>\n");
    s.push_str(
        "    <reference type=\"cover\" title=\"Cover\" href=\"text/cover.xhtml\"/>\n    <reference type=\"toc\" title=\"Contents\" href=\"nav.xhtml\"/>\n",
    );
    if let Some(first) = o.first_chapter {
        let _ = writeln!(
            s,
            "    <reference type=\"text\" title=\"Start\" href=\"{}\"/>",
            escape(&href_encode(first))
        );
    }
    s.push_str("  </guide>\n</package>\n");
    s
}

/// Accepts the W3C date forms `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, optionally
/// followed by a `T` time part.
fn is_w3c_date(s: &str) -> bool {
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    let lens_ok = match parts.len() {
        1 => parts[0].len() == 4,
        2 => parts[0].len() == 4 && parts[1].len() == 2,
        3 => parts[0].len() == 4 && parts[1].len() == 2 && parts[2].len() == 2,
        _ => false,
    };
    if !lens_ok || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let num = |i: usize| parts.get(i).and_then(|p| p.parse::<u32>().ok());
    if num(1).is_some_and(|m| !(1..=12).contains(&m))
        || num(2).is_some_and(|d| !(1..=31).contains(&d))
    {
        return false;
    }
    match time {
        None => true,
        Some(t) => {
            parts.len() == 3
                && !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b':' | b'.' | b'Z' | b'+' | b'-'))
        }
    }
}

/// Percent-encodes characters that are not allowed literally in a URL path.
fn href_encode(href: &str) -> String {
    let mut out = String::with_capacity(href.len());
    for b in href.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=@".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `href_encode` for a chapter path plus an optional `#fragment`.
fn target_href(path: &str, fragment: Option<&str>) -> String {
    match fragment {
        Some(f) if !f.is_empty() => format!("{}#{}", href_encode(path), fragment_encode(f)),
        _ => href_encode(path),
    }
}

/// Encodes characters that cannot appear in a URL fragment.
fn fragment_encode(f: &str) -> String {
    let mut out = String::with_capacity(f.len());
    for c in f.chars() {
        if c.is_ascii_control() || matches!(c, ' ' | '"' | '<' | '>' | '`' | '#' | '%') {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// UUID (version 5 style bits) derived from the book's content hash.
fn content_uuid(book: &Book) -> String {
    let mut h = Sha256::new();
    let mut field = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    field(book.meta.title.as_bytes());
    for a in &book.meta.authors {
        field(a.as_bytes());
    }
    field(book.meta.lang.as_bytes());
    for c in &book.chapters {
        field(c.file_name.as_bytes());
        field(c.body_xhtml.as_bytes());
    }
    for r in &book.resources {
        field(r.href.as_bytes());
        field(&r.data);
    }
    field(book.css.as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &x[0..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..32]
    )
}

/// `dcterms:modified` value: `SOURCE_DATE_EPOCH` if set, else now.
fn modified_timestamp() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|s| *s >= 0)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    format_utc(secs)
}

/// Unix seconds as `YYYY-MM-DDThh:mm:ssZ`.
fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------
// Table of contents
// ---------------------------------------------------------------------------

/// A flat TOC entry before nesting.
#[derive(Debug, Clone, PartialEq)]
struct TocEntry {
    level: u8,
    label: String,
    /// Path relative to `OEBPS/` (unencoded).
    path: String,
    fragment: Option<String>,
}

/// A nested TOC entry.
#[derive(Debug, Clone, PartialEq)]
struct TocNode {
    label: String,
    path: String,
    fragment: Option<String>,
    children: Vec<TocNode>,
}

impl TocNode {
    fn href(&self) -> String {
        target_href(&self.path, self.fragment.as_deref())
    }
}

/// Deepest heading level that appears in the TOC.
const TOC_MAX_LEVEL: u8 = 3;

/// Flattens the chapters into TOC entries: one per chapter (merged with its
/// first heading when that heading is the chapter title) plus its headings
/// down to level 3. Chapters with `in_toc: false` are skipped.
fn toc_entries(chapters: &[Chapter]) -> Vec<TocEntry> {
    let mut out = Vec::new();
    for ch in chapters.iter().filter(|c| c.in_toc) {
        let path = format!("text/{}", ch.file_name);
        let headings: Vec<_> = ch
            .headings
            .iter()
            .filter(|h| (1..=TOC_MAX_LEVEL).contains(&h.level))
            .map(|h| (h.level, clean_text(&h.text), h.id.trim()))
            .filter(|(_, text, _)| !text.is_empty())
            .collect();
        let title = clean_text(&ch.title);
        let merged = headings.first().is_some_and(|(_, text, _)| *text == title);
        if !merged {
            let label = if title.is_empty() {
                ch.file_name
                    .rsplit_once('.')
                    .map_or(ch.file_name.as_str(), |(stem, _)| stem)
                    .to_string()
            } else {
                title
            };
            // A chapter entry sits one level above its own headings.
            let level = headings
                .iter()
                .map(|(l, _, _)| *l)
                .min()
                .map_or(1, |l| l.saturating_sub(1).max(1));
            out.push(TocEntry {
                level,
                label,
                path: path.clone(),
                fragment: None,
            });
        }
        for (i, (level, text, id)) in headings.into_iter().enumerate() {
            let fragment = if (i == 0 && merged) || id.is_empty() {
                None
            } else {
                Some(id.to_string())
            };
            out.push(TocEntry {
                level,
                label: text,
                path: path.clone(),
                fragment,
            });
        }
    }
    out
}

/// Nests flat entries by level: each entry becomes a child of the nearest
/// preceding entry with a lower level. Skipped levels are tolerated.
fn build_tree(entries: Vec<TocEntry>) -> Vec<TocNode> {
    fn attach(stack: &mut [(u8, TocNode)], roots: &mut Vec<TocNode>, node: TocNode) {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(node),
            None => roots.push(node),
        }
    }
    let mut roots = Vec::new();
    let mut stack: Vec<(u8, TocNode)> = Vec::new();
    for e in entries {
        while stack.last().is_some_and(|(l, _)| *l >= e.level) {
            if let Some((_, node)) = stack.pop() {
                attach(&mut stack, &mut roots, node);
            }
        }
        stack.push((
            e.level,
            TocNode {
                label: e.label,
                path: e.path,
                fragment: e.fragment,
                children: Vec::new(),
            },
        ));
    }
    while let Some((_, node)) = stack.pop() {
        attach(&mut stack, &mut roots, node);
    }
    roots
}

fn tree_depth(nodes: &[TocNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + tree_depth(&n.children))
        .max()
        .unwrap_or(0)
}

fn nav_document(title: &str, lang: &str, toc: &[TocNode], first_chapter: Option<&str>) -> String {
    let mut items = String::new();
    if toc.is_empty() {
        // A nav `ol` must list at least one entry.
        let target = first_chapter.unwrap_or("text/cover.xhtml");
        let _ = writeln!(
            items,
            "      <li><a href=\"{}\">{}</a></li>",
            escape(&href_encode(target)),
            escape(title)
        );
    } else {
        nav_items(toc, 3, &mut items);
    }
    let mut landmarks = String::from(
        "      <li><a epub:type=\"cover\" href=\"text/cover.xhtml\">Cover</a></li>\n      <li><a epub:type=\"toc\" href=\"#toc\">Contents</a></li>\n",
    );
    if let Some(first) = first_chapter {
        let _ = writeln!(
            landmarks,
            "      <li><a epub:type=\"bodymatter\" href=\"{}\">Start</a></li>",
            escape(&href_encode(first))
        );
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{lang}" xml:lang="{lang}">
<head>
<title>{title}</title>
<link rel="stylesheet" type="text/css" href="css/base.css"/>
</head>
<body>
  <nav epub:type="toc" id="toc">
    <h1>Contents</h1>
    <ol>
{items}    </ol>
  </nav>
  <nav epub:type="landmarks" id="landmarks" hidden="hidden">
    <h2>Guide</h2>
    <ol>
{landmarks}    </ol>
  </nav>
</body>
</html>
"#,
        lang = escape(lang),
        title = escape(title),
    )
}

fn nav_items(nodes: &[TocNode], indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    for n in nodes {
        let _ = write!(
            out,
            "{pad}<li><a href=\"{}\">{}</a>",
            escape(&n.href()),
            escape(&n.label)
        );
        if n.children.is_empty() {
            out.push_str("</li>\n");
        } else {
            let _ = writeln!(out, "\n{pad}  <ol>");
            nav_items(&n.children, indent + 2, out);
            let _ = writeln!(out, "{pad}  </ol>\n{pad}</li>");
        }
    }
}

fn ncx_document(title: &str, authors: &[String], identifier: &str, toc: &[TocNode]) -> String {
    let mut points = String::new();
    let mut order = 0usize;
    if toc.is_empty() {
        let fallback = [TocNode {
            label: title.to_string(),
            path: "text/cover.xhtml".into(),
            fragment: None,
            children: Vec::new(),
        }];
        ncx_points(&fallback, 2, &mut order, &mut points);
    } else {
        ncx_points(toc, 2, &mut order, &mut points);
    }
    let authors: Vec<String> = authors
        .iter()
        .map(|a| clean_text(a))
        .filter(|a| !a.is_empty())
        .collect();
    let doc_author = if authors.is_empty() {
        String::new()
    } else {
        format!(
            "  <docAuthor><text>{}</text></docAuthor>\n",
            escape(&authors.join(", "))
        )
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="{id}"/>
    <meta name="dtb:depth" content="{depth}"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle><text>{title}</text></docTitle>
{doc_author}  <navMap>
{points}  </navMap>
</ncx>
"#,
        id = escape(identifier),
        depth = tree_depth(toc).max(1),
        title = escape(title),
    )
}

fn ncx_points(nodes: &[TocNode], indent: usize, order: &mut usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    for n in nodes {
        *order += 1;
        let _ = writeln!(
            out,
            "{pad}<navPoint id=\"np{o}\" playOrder=\"{o}\">\n{pad}  <navLabel><text>{}</text></navLabel>\n{pad}  <content src=\"{}\"/>",
            escape(&n.label),
            escape(&n.href()),
            o = *order
        );
        ncx_points(&n.children, indent + 1, order, out);
        let _ = writeln!(out, "{pad}</navPoint>");
    }
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// Wraps serialized body content in an EPUB 3 XHTML document. `<style>`
/// elements found in the body (the sanitizer keeps an input's own styles at
/// the start of each chapter) move into `<head>`, where XHTML allows them.
fn chapter_document(title: &str, lang: &str, body: &str) -> String {
    let (styles, body) = extract_styles(body);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{lang}" xml:lang="{lang}">
<head>
<title>{title}</title>
<link rel="stylesheet" type="text/css" href="../css/base.css"/>
{styles}</head>
<body>
{body}
</body>
</html>
"#,
        lang = escape(lang),
        title = escape(title),
    )
}

/// Splits serialized XHTML into its `<style>` elements (each on its own
/// line) and the remaining content. Style text is escaped by the serializer,
/// so a literal `</style>` can only be the element's end tag.
fn extract_styles(body: &str) -> (String, String) {
    let mut styles = String::new();
    let mut rest = String::with_capacity(body.len());
    let mut pos = 0;
    while let Some(off) = find_style_start(&body[pos..]) {
        let start = pos + off;
        let after_open = &body[start..];
        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        let end = if after_open[..tag_end].ends_with('/') {
            start + tag_end + 1
        } else {
            match after_open.find("</style>") {
                Some(e) => start + e + "</style>".len(),
                None => break,
            }
        };
        rest.push_str(&body[pos..start]);
        styles.push_str(&body[start..end]);
        styles.push('\n');
        pos = end;
    }
    rest.push_str(&body[pos..]);
    (styles, rest)
}

/// Offset of the next `<style` start tag (not e.g. `<styles`).
fn find_style_start(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find("<style") {
        let at = from + i;
        match s.as_bytes().get(at + "<style".len()) {
            Some(b'>' | b' ' | b'/' | b'\t' | b'\n' | b'\r') => return Some(at),
            _ => from = at + 1,
        }
    }
    None
}

fn cover_document(title: &str, authors: &[String], lang: &str, cover: &Cover<'_>) -> String {
    let body = match cover {
        Cover::Image { href, .. } => format!(
            "<section epub:type=\"cover\" class=\"cover\" style=\"text-align: center; margin: 0; padding: 0;\">\n<img src=\"../{}\" alt=\"{}\" style=\"max-width: 100%; max-height: 100%; height: auto;\"/>\n</section>",
            escape(&href_encode(href)),
            escape(title)
        ),
        Cover::Text => {
            let authors: Vec<String> = authors
                .iter()
                .map(|a| clean_text(a))
                .filter(|a| !a.is_empty())
                .collect();
            let mut s = format!(
                "<section epub:type=\"cover\" class=\"cover\" style=\"text-align: center; padding-top: 30%;\">\n<h1 class=\"cover-title\">{}</h1>\n",
                escape(title)
            );
            if !authors.is_empty() {
                let _ = writeln!(
                    s,
                    "<p class=\"cover-author\">{}</p>",
                    escape(&authors.join(", "))
                );
            }
            s.push_str("</section>");
            s
        }
    };
    chapter_document("Cover", lang, &body)
}

// ---------------------------------------------------------------------------
// Generated cover
// ---------------------------------------------------------------------------

/// Nominal size of the generated cover (3:4, like e-ink screens).
const COVER_W: u32 = 1200;
const COVER_H: u32 = 1600;
/// Render scale; 1.5 gives 1800×2400 pixels.
const COVER_SCALE: f32 = 1.5;

/// Renders a plain title/author cover to PNG.
fn generate_cover(meta: &Metadata, title: &str) -> Result<Vec<u8>> {
    let svg = cover_svg(title, &meta.authors);
    let max_w = (COVER_W as f32 * COVER_SCALE) as u32;
    let max_h = (COVER_H as f32 * COVER_SCALE) as u32;
    let raster = crate::svg::rasterize(svg.as_bytes(), None, max_w, max_h, COVER_SCALE)
        .context("rendering the generated cover")?;
    if raster.png.is_empty() {
        bail!("generated cover is empty");
    }
    Ok(raster.png)
}

/// The SVG source of the generated cover.
fn cover_svg(title: &str, authors: &[String]) -> String {
    let w = COVER_W as f32;
    let h = COVER_H as f32;
    let text_w = w - 240.0;

    // Largest title size whose wrapped lines fit the upper part of the page.
    let mut title_size = 56.0;
    let mut title_lines = wrap_text(title, chars_per_line(text_w, title_size));
    for size in [120.0, 104.0, 90.0, 78.0, 66.0, 56.0] {
        let lines = wrap_text(title, chars_per_line(text_w, size));
        if lines.len() as f32 * size * 1.2 <= h * 0.45 {
            title_size = size;
            title_lines = lines;
            break;
        }
    }
    title_lines.truncate(8);

    let author_text = authors
        .iter()
        .map(|a| clean_text(a))
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let author_size = 52.0;
    let mut author_lines = wrap_text(&author_text, chars_per_line(text_w, author_size));
    author_lines.truncate(3);

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{COVER_W}\" height=\"{COVER_H}\" viewBox=\"0 0 {COVER_W} {COVER_H}\">\n\
         <rect x=\"0\" y=\"0\" width=\"{COVER_W}\" height=\"{COVER_H}\" fill=\"#ffffff\"/>\n\
         <rect x=\"60\" y=\"60\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"#000000\" stroke-width=\"6\"/>\n\
         <rect x=\"84\" y=\"84\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"#000000\" stroke-width=\"2\"/>\n",
        COVER_W - 120,
        COVER_H - 120,
        COVER_W - 168,
        COVER_H - 168
    );
    let line_h = title_size * 1.2;
    let block_h = line_h * title_lines.len() as f32;
    let mut y = h * 0.38 - block_h / 2.0 + title_size;
    for line in &title_lines {
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"{y:.0}\" font-family=\"sans-serif\" font-size=\"{title_size:.0}\" font-weight=\"bold\" text-anchor=\"middle\" fill=\"#000000\">{}</text>",
            w / 2.0,
            escape(line)
        );
        y += line_h;
    }
    let rule_y = (y - line_h + title_size * 0.6 + 40.0).max(h * 0.5);
    let _ = writeln!(
        s,
        "<rect x=\"{}\" y=\"{rule_y:.0}\" width=\"200\" height=\"4\" fill=\"#000000\"/>",
        w / 2.0 - 100.0
    );
    let mut y = rule_y + 60.0 + author_size;
    for line in &author_lines {
        let _ = writeln!(
            s,
            "<text x=\"{}\" y=\"{y:.0}\" font-family=\"sans-serif\" font-size=\"{author_size:.0}\" text-anchor=\"middle\" fill=\"#000000\">{}</text>",
            w / 2.0,
            escape(line)
        );
        y += author_size * 1.3;
    }
    s.push_str("</svg>\n");
    s
}

/// Rough number of characters that fit in `width` at `font_size`.
fn chars_per_line(width: f32, font_size: f32) -> usize {
    ((width / (font_size * 0.58)) as usize).max(4)
}

/// Greedy word wrap by character count; over-long words are broken.
fn wrap_text(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut len = 0usize;
    for word in text.split_whitespace() {
        let mut chars: Vec<char> = word.chars().collect();
        while chars.len() > max {
            if len > 0 {
                lines.push(std::mem::take(&mut line));
                len = 0;
            }
            lines.push(chars.drain(..max).collect());
        }
        if chars.is_empty() {
            continue;
        }
        let wlen = chars.len();
        if len > 0 && len + 1 + wlen > max {
            lines.push(std::mem::take(&mut line));
            len = 0;
        }
        if len > 0 {
            line.push(' ');
            len += 1;
        }
        line.extend(chars);
        len += wlen;
    }
    if len > 0 {
        lines.push(line);
    }
    lines
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Collapses whitespace runs into single spaces and trims.
fn clean_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn non_empty<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Heading;
    use std::collections::HashMap;
    use std::io::Read;

    type Archive = zip::ZipArchive<std::fs::File>;

    fn heading(level: u8, text: &str, id: &str) -> Heading {
        Heading {
            level,
            text: text.into(),
            id: id.into(),
        }
    }

    fn chapter(file: &str, title: &str, body: &str, headings: Vec<Heading>) -> Chapter {
        Chapter {
            title: title.into(),
            file_name: file.into(),
            body_xhtml: body.into(),
            headings,
            in_toc: true,
            source: None,
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 10, 10]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn sample_book() -> Book {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>";
        Book {
            meta: Metadata {
                title: "A <Small> & Book".into(),
                authors: vec!["Ada Lovelace".into(), "Charles Babbage".into()],
                lang: "en-GB".into(),
                identifier: None,
                date: Some("2024-05-01".into()),
            },
            chapters: vec![
                chapter(
                    "ch001.xhtml",
                    "Intro",
                    "<h1 id=\"intro\">Intro</h1><p>Hi <img src=\"../images/aa.png\" alt=\"\"/></p>\
                     <h2 id=\"part-a\">Part A</h2><h3 id=\"deep\">Deep</h3><h4 id=\"deeper\">Deeper</h4>",
                    vec![
                        heading(1, "Intro", "intro"),
                        heading(2, "Part A", "part-a"),
                        heading(3, "Deep", "deep"),
                        heading(4, "Deeper", "deeper"),
                    ],
                ),
                chapter(
                    "ch002.xhtml",
                    "Pictures",
                    "<h1 id=\"pictures\">Pictures</h1><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"><rect width=\"4\" height=\"4\"/></svg>",
                    vec![heading(1, "Pictures", "pictures")],
                ),
                chapter(
                    "ch003.xhtml",
                    "Math",
                    "<h1 id=\"math\">Math</h1><p><math xmlns=\"http://www.w3.org/1998/Math/MathML\"><mi>x</mi></math> <img src=\"../images/bb.svg\" alt=\"\"/></p>",
                    vec![heading(1, "Math", "math")],
                ),
                Chapter {
                    in_toc: false,
                    ..chapter(
                        "ch004.xhtml",
                        "Hidden notes",
                        "<div class=\"notes-page\"/>",
                        vec![],
                    )
                },
            ],
            resources: vec![
                Resource {
                    href: "images/aa.png".into(),
                    media_type: "image/png".into(),
                    data: png(3, 2),
                },
                Resource {
                    href: "images/bb.svg".into(),
                    media_type: "image/svg+xml".into(),
                    data: svg.as_bytes().to_vec(),
                },
            ],
            css: "body { color: black; }".into(),
            cover: Some(Resource {
                href: "images/cc.jpg".into(),
                media_type: "image/jpeg".into(),
                data: vec![0xff, 0xd8, 0xff, 0xd9],
            }),
        }
    }

    fn write_book(book: &Book) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("book.epub");
        write(book, &out).unwrap();
        (dir, out)
    }

    fn open(out: &Path) -> Archive {
        zip::ZipArchive::new(std::fs::File::open(out).unwrap()).unwrap()
    }

    fn names(zip: &Archive) -> Vec<String> {
        zip.file_names().map(|n| n.unwrap().into_owned()).collect()
    }

    fn read(zip: &mut Archive, name: &str) -> String {
        let mut s = String::new();
        zip.by_name(name)
            .unwrap_or_else(|_| panic!("missing {name}"))
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    fn parse(xml: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse(xml).unwrap_or_else(|e| panic!("bad XML ({e}):\n{xml}"))
    }

    /// Manifest items of an OPF: id -> (href, media type, properties).
    fn manifest(opf: &str) -> HashMap<String, (String, String, String)> {
        let doc = parse(opf);
        doc.descendants()
            .filter(|n| n.has_tag_name("item"))
            .map(|n| {
                (
                    n.attribute("id").unwrap().to_string(),
                    (
                        n.attribute("href").unwrap().to_string(),
                        n.attribute("media-type").unwrap().to_string(),
                        n.attribute("properties").unwrap_or("").to_string(),
                    ),
                )
            })
            .collect()
    }

    fn spine(opf: &str) -> Vec<String> {
        parse(opf)
            .descendants()
            .filter(|n| n.has_tag_name("itemref"))
            .map(|n| n.attribute("idref").unwrap().to_string())
            .collect()
    }

    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// Asserts the structural invariants every EPUB we write must hold.
    fn check_package(out: &Path) -> Archive {
        let bytes = std::fs::read(out).unwrap();
        // Local header of the first entry: "mimetype", stored, no extra field.
        assert_eq!(&bytes[0..4], b"PK\x03\x04");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 0, "compression");
        assert_eq!(u16::from_le_bytes([bytes[26], bytes[27]]), 8, "name length");
        assert_eq!(u16::from_le_bytes([bytes[28], bytes[29]]), 0, "extra field");
        assert_eq!(&bytes[30..38], b"mimetype");
        assert_eq!(&bytes[38..58], b"application/epub+zip");

        let mut zip = open(out);
        let names = names(&zip);
        assert_eq!(names[0], "mimetype");
        assert_eq!(
            zip.by_index(0).unwrap().compression(),
            CompressionMethod::Stored
        );
        let unique: HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "duplicate entries: {names:?}");

        let container = read(&mut zip, "META-INF/container.xml");
        parse(&container);
        assert!(container.contains("full-path=\"OEBPS/content.opf\""));

        let opf = read(&mut zip, "OEBPS/content.opf");
        let items = manifest(&opf);
        for (id, (href, media_type, _)) in &items {
            let path = format!("OEBPS/{}", percent_decode(href));
            assert!(
                names.contains(&path),
                "manifest item {id} -> missing {path}"
            );
            if media_type == "application/xhtml+xml" {
                parse(&read(&mut zip, &path));
            }
        }
        // Every file except mimetype, container and the OPF is in the manifest.
        for n in &names {
            if n == "mimetype" || n.starts_with("META-INF/") || n == "OEBPS/content.opf" {
                continue;
            }
            let href = n.strip_prefix("OEBPS/").unwrap();
            assert!(
                items.values().any(|(h, _, _)| percent_decode(h) == href),
                "{n} is not in the manifest"
            );
        }
        let spine = spine(&opf);
        assert!(!spine.is_empty());
        for idref in &spine {
            assert!(items.contains_key(idref), "spine idref {idref} unresolved");
        }
        let navs: Vec<_> = items.values().filter(|i| i.2.contains("nav")).collect();
        assert_eq!(navs.len(), 1);
        parse(&read(&mut zip, "OEBPS/nav.xhtml"));
        parse(&read(&mut zip, "OEBPS/toc.ncx"));
        zip
    }

    /// Nav hrefs in document order, from the `toc` nav only.
    fn nav_hrefs(nav: &str) -> Vec<String> {
        let doc = parse(nav);
        let toc = doc
            .descendants()
            .find(|n| n.has_tag_name("nav") && n.attribute("id") == Some("toc"))
            .unwrap();
        toc.descendants()
            .filter(|n| n.has_tag_name("a"))
            .map(|n| n.attribute("href").unwrap().to_string())
            .collect()
    }

    fn ncx_srcs(ncx: &str) -> Vec<String> {
        parse(ncx)
            .descendants()
            .filter(|n| n.has_tag_name("content"))
            .map(|n| n.attribute("src").unwrap().to_string())
            .collect()
    }

    #[test]
    fn full_book_layout() {
        let book = sample_book();
        let (dir, out) = write_book(&book);
        let mut zip = check_package(&out);
        let names = names(&zip);

        // Layout and order: mimetype, container, OPF, nav, NCX, ...
        assert_eq!(
            &names[..6],
            &[
                "mimetype",
                "META-INF/container.xml",
                "OEBPS/content.opf",
                "OEBPS/nav.xhtml",
                "OEBPS/toc.ncx",
                "OEBPS/css/base.css",
            ]
        );
        for n in [
            "OEBPS/text/cover.xhtml",
            "OEBPS/text/ch001.xhtml",
            "OEBPS/text/ch004.xhtml",
            "OEBPS/images/aa.png",
            "OEBPS/images/bb.svg",
            "OEBPS/images/cc.jpg",
        ] {
            assert!(names.iter().any(|x| x == n), "missing {n}");
        }
        assert_eq!(
            zip.by_name("OEBPS/images/aa.png").unwrap().compression(),
            CompressionMethod::Stored
        );
        assert_eq!(
            zip.by_name("OEBPS/text/ch001.xhtml").unwrap().compression(),
            CompressionMethod::Deflated
        );
        assert_eq!(read(&mut zip, "OEBPS/css/base.css"), book.css);

        let opf = read(&mut zip, "OEBPS/content.opf");
        let items = manifest(&opf);
        assert!(opf.contains("<dc:title>A &lt;Small&gt; &amp; Book</dc:title>"));
        assert!(opf.contains(">Ada Lovelace</dc:creator>"));
        assert!(opf.contains(">Charles Babbage</dc:creator>"));
        assert!(opf.contains("<dc:language>en-GB</dc:language>"));
        assert!(opf.contains("<dc:date>2024-05-01</dc:date>"));
        assert!(opf.contains("<meta name=\"cover\" content=\"cover-image\"/>"));
        assert!(opf.contains("property=\"dcterms:modified\""));
        assert!(opf.contains("urn:uuid:"));
        assert_eq!(items["cover-image"].0, "images/cc.jpg");
        assert_eq!(items["cover-image"].1, "image/jpeg");
        assert_eq!(items["cover-image"].2, "cover-image");
        assert_eq!(items["ch1"].2, "");
        assert_eq!(items["ch2"].2, "svg");
        assert_eq!(items["ch3"].2, "mathml");
        assert!(
            items
                .values()
                .any(|(h, m, _)| h == "images/bb.svg" && m == "image/svg+xml")
        );
        assert_eq!(spine(&opf), ["cover", "ch1", "ch2", "ch3", "ch4"]);

        // Chapter wrapper.
        let ch1 = read(&mut zip, "OEBPS/text/ch001.xhtml");
        assert!(ch1.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(!ch1.contains("<!DOCTYPE"));
        assert!(ch1.contains("xmlns:epub=\"http://www.idpf.org/2007/ops\""));
        assert!(ch1.contains("lang=\"en-GB\""));
        assert!(ch1.contains("<title>Intro</title>"));
        assert!(ch1.contains("href=\"../css/base.css\""));
        let cover = read(&mut zip, "OEBPS/text/cover.xhtml");
        assert!(cover.contains("src=\"../images/cc.jpg\""));

        // Nested nav: chapters with their h2/h3 headings, no h4, no hidden chapter.
        let nav = read(&mut zip, "OEBPS/nav.xhtml");
        let hrefs = nav_hrefs(&nav);
        assert_eq!(
            hrefs,
            [
                "text/ch001.xhtml",
                "text/ch001.xhtml#part-a",
                "text/ch001.xhtml#deep",
                "text/ch002.xhtml",
                "text/ch003.xhtml",
            ]
        );
        let doc = parse(&nav);
        let deep = doc
            .descendants()
            .find(|n| n.attribute("href") == Some("text/ch001.xhtml#deep"))
            .unwrap();
        // a -> li -> ol -> li -> ol -> li -> ol (toc root)
        let depth = deep.ancestors().filter(|n| n.has_tag_name("ol")).count();
        assert_eq!(depth, 3);
        assert!(!nav.contains("Deeper"));
        assert!(!nav.contains("Hidden notes"));
        assert!(nav.contains("epub:type=\"landmarks\""));
        assert!(nav.contains("epub:type=\"bodymatter\" href=\"text/ch001.xhtml\""));

        // NCX mirrors the nav.
        let ncx = read(&mut zip, "OEBPS/toc.ncx");
        assert_eq!(ncx_srcs(&ncx), hrefs);
        assert!(ncx.contains("<meta name=\"dtb:depth\" content=\"3\"/>"));
        let orders: Vec<String> = parse(&ncx)
            .descendants()
            .filter_map(|n| n.attribute("playOrder").map(str::to_string))
            .collect();
        assert_eq!(orders, ["1", "2", "3", "4", "5"]);

        // No temp files left behind.
        assert!(
            !dir.path().read_dir().unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
    }

    #[test]
    fn generates_cover_when_missing() {
        let book = Book {
            cover: None,
            ..sample_book()
        };
        let (_dir, out) = write_book(&book);
        let mut zip = check_package(&out);
        let opf = read(&mut zip, "OEBPS/content.opf");
        let items = manifest(&opf);
        assert_eq!(items["cover-image"].0, "images/cover.png");
        assert_eq!(items["cover-image"].2, "cover-image");
        let mut data = Vec::new();
        zip.by_name("OEBPS/images/cover.png")
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        assert!(data.starts_with(b"\x89PNG"));
        let cover = read(&mut zip, "OEBPS/text/cover.xhtml");
        assert!(cover.contains("src=\"../images/cover.png\""));
    }

    #[test]
    fn cover_svg_is_well_formed_and_wraps() {
        let svg = cover_svg(
            "A Very Long Title <With> Markup & Ampersands That Must Wrap Over Several Lines",
            &["Someone".into(), "  ".into()],
        );
        let doc = parse(&svg);
        let texts: Vec<_> = doc
            .descendants()
            .filter(|n| n.has_tag_name("text"))
            .collect();
        assert!(texts.len() >= 3, "{svg}");
        assert!(svg.contains("&lt;With&gt;"));
        assert!(svg.contains(">Someone</text>"));
        assert!(svg.starts_with("<svg"));
        // Untitled/empty input still yields valid SVG.
        parse(&cover_svg("", &[]));
    }

    #[test]
    fn cover_shared_with_resource() {
        let mut book = sample_book();
        let shared = book.resources[0].clone();
        book.cover = Some(shared);
        let (_dir, out) = write_book(&book);
        let mut zip = check_package(&out);
        let items = manifest(&read(&mut zip, "OEBPS/content.opf"));
        assert_eq!(items["cover-image"].0, "images/aa.png");
        assert_eq!(items["cover-image"].2, "cover-image");
        assert_eq!(
            items
                .values()
                .filter(|(h, _, _)| h == "images/aa.png")
                .count(),
            1
        );
    }

    #[test]
    fn cover_clashing_with_different_resource_is_renamed() {
        let mut book = sample_book();
        book.cover = Some(Resource {
            href: "images/aa.png".into(),
            media_type: "image/png".into(),
            data: png(5, 5),
        });
        let (_dir, out) = write_book(&book);
        let mut zip = check_package(&out);
        let items = manifest(&read(&mut zip, "OEBPS/content.opf"));
        assert_eq!(items["cover-image"].0, "images/cover.png");
    }

    #[test]
    fn skips_bad_and_duplicate_resources() {
        let mut book = sample_book();
        let dup = book.resources[0].clone();
        book.resources.push(dup);
        for href in ["../evil.png", "/abs.png", "", "nav.xhtml", "a//b.png"] {
            book.resources.push(Resource {
                href: href.into(),
                media_type: "image/png".into(),
                data: vec![1],
            });
        }
        book.resources.push(Resource {
            href: "images/no type.gif".into(),
            media_type: String::new(),
            data: vec![1],
        });
        let (_dir, out) = write_book(&book);
        let mut zip = check_package(&out);
        let names = names(&zip);
        assert!(
            !names
                .iter()
                .any(|n| n.contains("evil") || n.contains("abs"))
        );
        let opf = read(&mut zip, "OEBPS/content.opf");
        let items = manifest(&opf);
        assert!(
            items
                .values()
                .any(|(h, m, _)| h == "images/no%20type.gif" && m == "image/gif")
        );
        assert!(names.iter().any(|n| n == "OEBPS/images/no type.gif"));
    }

    #[test]
    fn empty_book_is_valid() {
        let book = Book {
            meta: Metadata::default(),
            ..Book::default()
        };
        let (_dir, out) = write_book(&book);
        let mut zip = check_package(&out);
        let opf = read(&mut zip, "OEBPS/content.opf");
        assert!(opf.contains("<dc:title>Untitled</dc:title>"));
        assert!(opf.contains("<dc:language>en</dc:language>"));
        assert_eq!(spine(&opf), ["cover"]);
        let nav = read(&mut zip, "OEBPS/nav.xhtml");
        assert_eq!(nav_hrefs(&nav), ["text/cover.xhtml"]);
        assert_eq!(
            ncx_srcs(&read(&mut zip, "OEBPS/toc.ncx")),
            ["text/cover.xhtml"]
        );
    }

    #[test]
    fn identifier_supplied_or_derived() {
        let mut book = sample_book();
        book.meta.identifier = Some("isbn:9780000000000".into());
        let (_d, out) = write_book(&book);
        let mut zip = open(&out);
        let opf = read(&mut zip, "OEBPS/content.opf");
        assert!(opf.contains(">isbn:9780000000000</dc:identifier>"));
        let ncx = read(&mut zip, "OEBPS/toc.ncx");
        assert!(ncx.contains("content=\"isbn:9780000000000\""));

        let a = content_uuid(&sample_book());
        assert_eq!(a, content_uuid(&sample_book()));
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "5");
        let mut other = sample_book();
        other.chapters[0].body_xhtml.push_str("<p>more</p>");
        assert_ne!(a, content_uuid(&other));
    }

    #[test]
    fn failure_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.epub");
        std::fs::write(&out, b"previous").unwrap();
        let mut book = sample_book();
        book.chapters[1].file_name = "ch001.xhtml".into();
        assert!(write(&book, &out).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"previous");
        for bad in ["", "../x.xhtml", "a/b.xhtml", "cover.xhtml"] {
            let mut book = sample_book();
            book.chapters[0].file_name = bad.into();
            let err = write(&book, &out).unwrap_err().to_string();
            assert!(err.contains("file name"), "{err}");
        }
        // Writing into a missing directory creates it.
        let nested = dir.path().join("a/b/c.epub");
        write(&sample_book(), &nested).unwrap();
        assert!(nested.is_file());
        let leftovers: Vec<_> = dir
            .path()
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !leftovers.iter().any(|n| n.ends_with(".tmp")),
            "{leftovers:?}"
        );
    }

    #[test]
    fn toc_tree_nesting() {
        let chapters = vec![
            // Title from file stem, headings start at level 2: chapter is parent.
            chapter(
                "ch001.xhtml",
                "Preface",
                "",
                vec![heading(2, "One", "one"), heading(2, "Two", "two")],
            ),
            // Skipped level (h1 then h3) nests directly.
            chapter(
                "ch002.xhtml",
                "Big",
                "",
                vec![heading(1, "Big", "big"), heading(3, "Small", "small")],
            ),
            // Untitled chapter with no headings.
            chapter("ch003.xhtml", "  ", "", vec![]),
        ];
        let tree = build_tree(toc_entries(&chapters));
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].label, "Preface");
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[1].href(), "text/ch001.xhtml#two");
        assert_eq!(tree[1].href(), "text/ch002.xhtml");
        assert_eq!(tree[1].children[0].label, "Small");
        assert_eq!(tree[2].label, "ch003");
        assert_eq!(tree_depth(&tree), 2);
    }

    #[test]
    fn element_detection() {
        assert!(contains_element("<p><svg width=\"1\"/></p>", "svg"));
        assert!(contains_element("<svg>", "svg"));
        assert!(!contains_element("<svgx/>", "svg"));
        assert!(!contains_element("&lt;svg&gt;", "svg"));
        assert!(contains_element("<math\nxmlns=\"\"/>", "math"));
        assert!(!contains_element("<mathx/>", "math"));
    }

    #[test]
    fn helpers() {
        assert_eq!(wrap_text("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap_text("abcdefgh", 3), ["abc", "def", "gh"]);
        assert!(wrap_text("   ", 5).is_empty());
        assert_eq!(href_encode("images/a b%.png"), "images/a%20b%25.png");
        assert_eq!(href_encode("text/ch001.xhtml"), "text/ch001.xhtml");
        assert_eq!(
            target_href("text/c.xhtml", Some("a b")),
            "text/c.xhtml#a%20b"
        );
        assert!(is_w3c_date("2024"));
        assert!(is_w3c_date("2024-05"));
        assert!(is_w3c_date("2024-05-01T10:00:00Z"));
        assert!(!is_w3c_date("May 2024"));
        assert!(!is_w3c_date("2024-13-01"));
        assert!(!is_w3c_date("2024-05-01T"));
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(modified_timestamp().len(), 20);
        assert_eq!(clean_text("  a \n b "), "a b");
    }

    #[test]
    fn bad_date_is_omitted() {
        let mut book = sample_book();
        book.meta.date = Some("sometime".into());
        let (_d, out) = write_book(&book);
        let opf = read(&mut open(&out), "OEBPS/content.opf");
        assert!(!opf.contains("dc:date"));
    }

    #[test]
    fn body_styles_move_to_head() {
        let body = "<style>.a { color: red; }</style>\n<p>x</p><styles-not/><style type=\"text/css\">b &gt; i {}</style><p>y</p>";
        let doc = chapter_document("T", "en", body);
        let xml = parse(&doc);
        let head = xml
            .root_element()
            .children()
            .find(|n| n.has_tag_name("head"))
            .unwrap();
        let body_el = xml
            .root_element()
            .children()
            .find(|n| n.has_tag_name("body"))
            .unwrap();
        assert_eq!(
            head.children().filter(|n| n.has_tag_name("style")).count(),
            2
        );
        assert!(!body_el.descendants().any(|n| n.has_tag_name("style")));
        assert!(doc.contains("<styles-not/>"));
        assert!(doc.contains("b &gt; i {}"));
        let (s, rest) = extract_styles("<p>no styles</p><style>unterminated");
        assert!(s.is_empty());
        assert_eq!(rest, "<p>no styles</p><style>unterminated");
    }
}

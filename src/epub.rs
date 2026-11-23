//! EPUB 3 packaging (owned by T9).
//!
//! Contract for [`write`]: produce
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
//! `<meta name="cover">`), a nested `nav.xhtml` (headings down to level 3,
//! skipping `in_toc: false`, plus landmarks) and matching `toc.ncx`. When
//! `book.cover` is `None`, generate a title/author cover through
//! `svg::rasterize`. Writes to a temp file and renames, so a failed run never
//! leaves a broken EPUB.
//!
//! The T0 stub writes a minimal valid EPUB with a flat TOC and the given
//! cover only.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use crate::dom::escape;
use crate::model::Book;

/// Writes `book` as an EPUB file at `out`. See the module docs.
pub fn write(book: &Book, out: &Path) -> Result<()> {
    let tmp = temp_path(out);
    let result = write_to(book, &tmp);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, out).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

fn temp_path(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book.epub".into());
    out.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

fn write_to(book: &Book, path: &Path) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut zip = ZipWriter::new(std::io::BufWriter::new(file));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;

    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;

    let lang = if book.meta.lang.is_empty() {
        "en"
    } else {
        &book.meta.lang
    };
    let title = if book.meta.title.is_empty() {
        "Untitled"
    } else {
        &book.meta.title
    };

    let mut manifest = String::new();
    let mut spine = String::new();
    let mut nav_items = String::new();
    let mut ncx_points = String::new();

    manifest.push_str(
        "    <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n",
    );
    manifest.push_str(
        "    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
    );
    manifest.push_str("    <item id=\"css\" href=\"css/base.css\" media-type=\"text/css\"/>\n");
    zip.start_file("OEBPS/css/base.css", deflated)?;
    zip.write_all(book.css.as_bytes())?;

    let mut cover_meta = String::new();
    if let Some(cover) = &book.cover {
        let _ = writeln!(
            manifest,
            "    <item id=\"cover-image\" href=\"{}\" media-type=\"{}\" properties=\"cover-image\"/>",
            escape(&cover.href),
            escape(&cover.media_type)
        );
        manifest.push_str(
            "    <item id=\"cover\" href=\"text/cover.xhtml\" media-type=\"application/xhtml+xml\"/>\n",
        );
        spine.push_str("    <itemref idref=\"cover\" linear=\"no\"/>\n");
        cover_meta = "    <meta name=\"cover\" content=\"cover-image\"/>\n".into();
        zip.start_file(format!("OEBPS/{}", cover.href), stored)?;
        zip.write_all(&cover.data)?;
        let body = format!(
            "<div class=\"cover\"><img src=\"../{}\" alt=\"{}\"/></div>",
            escape(&cover.href),
            escape(title)
        );
        zip.start_file("OEBPS/text/cover.xhtml", deflated)?;
        zip.write_all(chapter_document(title, lang, &body).as_bytes())?;
    }

    for (i, res) in book.resources.iter().enumerate() {
        let _ = writeln!(
            manifest,
            "    <item id=\"res{}\" href=\"{}\" media-type=\"{}\"/>",
            i + 1,
            escape(&res.href),
            escape(&res.media_type)
        );
        let opts = if res.media_type.starts_with("image/") && res.media_type != "image/svg+xml" {
            stored
        } else {
            deflated
        };
        zip.start_file(format!("OEBPS/{}", res.href), opts)?;
        zip.write_all(&res.data)?;
    }

    for (i, ch) in book.chapters.iter().enumerate() {
        let id = format!("ch{}", i + 1);
        let mut props = Vec::new();
        if ch.body_xhtml.contains("<svg") {
            props.push("svg");
        }
        if ch.body_xhtml.contains("<math") {
            props.push("mathml");
        }
        let props = if props.is_empty() {
            String::new()
        } else {
            format!(" properties=\"{}\"", props.join(" "))
        };
        let _ = writeln!(
            manifest,
            "    <item id=\"{id}\" href=\"text/{}\" media-type=\"application/xhtml+xml\"{props}/>",
            escape(&ch.file_name)
        );
        let _ = writeln!(spine, "    <itemref idref=\"{id}\"/>");
        if ch.in_toc {
            let _ = writeln!(
                nav_items,
                "      <li><a href=\"text/{}\">{}</a></li>",
                escape(&ch.file_name),
                escape(&ch.title)
            );
            let n = i + 1;
            let _ = writeln!(
                ncx_points,
                "    <navPoint id=\"np{n}\" playOrder=\"{n}\"><navLabel><text>{}</text></navLabel><content src=\"text/{}\"/></navPoint>",
                escape(&ch.title),
                escape(&ch.file_name)
            );
        }
        zip.start_file(format!("OEBPS/text/{}", ch.file_name), deflated)?;
        zip.write_all(chapter_document(&ch.title, lang, &ch.body_xhtml).as_bytes())?;
    }
    if nav_items.is_empty() {
        // A nav must list at least one entry.
        nav_items = format!(
            "      <li><a href=\"nav.xhtml\">{}</a></li>\n",
            escape(title)
        );
    }

    let identifier = match &book.meta.identifier {
        Some(id) => id.clone(),
        None => format!("urn:uuid:{}", content_uuid(book)),
    };
    let mut creators = String::new();
    for a in &book.meta.authors {
        let _ = writeln!(creators, "    <dc:creator>{}</dc:creator>", escape(a));
    }
    let date = book
        .meta
        .date
        .as_ref()
        .map(|d| format!("    <dc:date>{}</dc:date>\n", escape(d)))
        .unwrap_or_default();
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid" xml:lang="{lang}">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">{id}</dc:identifier>
    <dc:title>{title}</dc:title>
{creators}    <dc:language>{lang}</dc:language>
{date}    <meta property="dcterms:modified">{modified}</meta>
{cover_meta}  </metadata>
  <manifest>
{manifest}  </manifest>
  <spine toc="ncx">
{spine}  </spine>
</package>
"#,
        lang = escape(lang),
        id = escape(&identifier),
        title = escape(title),
        modified = now_utc(),
    );
    zip.start_file("OEBPS/content.opf", deflated)?;
    zip.write_all(opf.as_bytes())?;

    let nav = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{lang}" xml:lang="{lang}">
<head><title>{title}</title></head>
<body>
  <nav epub:type="toc" id="toc">
    <h1>Contents</h1>
    <ol>
{nav_items}    </ol>
  </nav>
</body>
</html>
"#,
        lang = escape(lang),
        title = escape(title),
    );
    zip.start_file("OEBPS/nav.xhtml", deflated)?;
    zip.write_all(nav.as_bytes())?;

    let ncx = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head><meta name="dtb:uid" content="{id}"/></head>
  <docTitle><text>{title}</text></docTitle>
  <navMap>
{ncx_points}  </navMap>
</ncx>
"#,
        id = escape(&identifier),
        title = escape(title),
    );
    zip.start_file("OEBPS/toc.ncx", deflated)?;
    zip.write_all(ncx.as_bytes())?;

    let mut w = zip.finish()?;
    w.flush()?;
    Ok(())
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

/// Wraps serialized body content in an EPUB 3 XHTML document.
fn chapter_document(title: &str, lang: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{lang}" xml:lang="{lang}">
<head>
<title>{title}</title>
<link rel="stylesheet" type="text/css" href="../css/base.css"/>
</head>
<body>
{body}
</body>
</html>
"#,
        lang = escape(lang),
        title = escape(title),
    )
}

/// UUID (version 5 style bits) derived from the book's content hash.
fn content_uuid(book: &Book) -> String {
    let mut h = Sha256::new();
    h.update(book.meta.title.as_bytes());
    for a in &book.meta.authors {
        h.update(a.as_bytes());
    }
    for c in &book.chapters {
        h.update(c.body_xhtml.as_bytes());
    }
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

/// Current UTC time as `YYYY-MM-DDThh:mm:ssZ`.
fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chapter, Metadata, Resource};
    use std::io::Read;

    #[test]
    fn stub_writes_valid_layout() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.epub");
        let book = Book {
            meta: Metadata {
                title: "T & U".into(),
                authors: vec!["A".into()],
                lang: "en".into(),
                ..Metadata::default()
            },
            chapters: vec![Chapter {
                title: "One".into(),
                file_name: "ch001.xhtml".into(),
                body_xhtml: "<p>x</p>".into(),
                in_toc: true,
                ..Chapter::default()
            }],
            resources: vec![Resource {
                href: "images/a.png".into(),
                media_type: "image/png".into(),
                data: vec![1],
            }],
            css: "body{}".into(),
            cover: None,
        };
        write(&book, &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        // Local header of the first entry: name "mimetype", stored, no extra field.
        assert_eq!(&bytes[0..4], b"PK\x03\x04");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 0);
        assert_eq!(u16::from_le_bytes([bytes[28], bytes[29]]), 0);
        assert_eq!(&bytes[30..38], b"mimetype");
        assert_eq!(&bytes[38..58], b"application/epub+zip");

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        for n in [
            "META-INF/container.xml",
            "OEBPS/content.opf",
            "OEBPS/nav.xhtml",
            "OEBPS/toc.ncx",
            "OEBPS/text/ch001.xhtml",
            "OEBPS/images/a.png",
            "OEBPS/css/base.css",
        ] {
            assert!(names.iter().any(|x| x == n), "missing {n}");
        }
        let mut opf = String::new();
        zip.by_name("OEBPS/content.opf")
            .unwrap()
            .read_to_string(&mut opf)
            .unwrap();
        assert!(opf.contains("<dc:title>T &amp; U</dc:title>"));
        assert!(opf.contains("urn:uuid:"));
        assert!(
            !dir.path()
                .read_dir()
                .unwrap()
                .any(|e| { e.unwrap().file_name().to_string_lossy().ends_with(".tmp") })
        );
    }

    #[test]
    fn date_format() {
        let s = now_utc();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
    }
}

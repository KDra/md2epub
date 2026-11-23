//! End-to-end tests: run the built `md2epub` binary on the fixtures under
//! every profile and inspect the resulting EPUB.
//!
//! Every run passes `--offline`, so no test touches the network. Assertions
//! that depend on the optional `math` and `mermaid` features are gated with
//! `cfg`, so the suite also passes with `--no-default-features`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use zip::CompressionMethod;
use zip::ZipArchive;

const PROFILES: [&str; 4] = ["scribe", "remarkable", "remarkable-pro", "generic"];
/// Profiles under which no SVG, MathML or alpha channel may survive.
const STRICT_PROFILES: [&str; 3] = ["scribe", "remarkable", "remarkable-pro"];

const OPF_NS: &str = "http://www.idpf.org/2007/opf";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";
const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn fixture(name: &str) -> String {
    fixtures().join(name).to_string_lossy().into_owned()
}

/// Runs the binary with `args` and returns its output, whatever the status.
/// The user config points at a file that never exists, so the developer's
/// own defaults never leak into (or get changed by) the tests.
fn run_raw(args: &[&str]) -> Output {
    let no_config = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("md2epub-no-user-config")
        .join("config.toml");
    run_with_config(args, &no_config, None)
}

/// Runs the binary with `args`, the user config at `config`, and the
/// working directory `cwd` (default: inherited).
fn run_with_config(args: &[&str], config: &Path, cwd: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_md2epub"));
    cmd.args(args)
        .env("MD2EPUB_CONFIG", config)
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.output().expect("failed to start md2epub")
}

/// Result of a successful conversion.
struct Run {
    epub: Epub,
    stderr: String,
    _dir: TempDir,
}

/// Converts with `args` plus `--offline -o <tmp>/out.epub`, asserting success.
fn convert(args: &[&str]) -> Run {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out.epub");
    let out_str = out.to_string_lossy().into_owned();
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--offline", "-o", &out_str]);
    let output = run_raw(&all);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "md2epub {all:?} failed:\n{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(out.is_file(), "no EPUB written for {all:?}");
    Run {
        epub: Epub::open(&out),
        stderr,
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------
// Reading an EPUB
// ---------------------------------------------------------------------------

struct Entry {
    name: String,
    data: Vec<u8>,
    method: CompressionMethod,
    extra_len: usize,
}

struct ManifestItem {
    href: String,
    media_type: String,
    properties: String,
}

struct Epub {
    entries: Vec<Entry>,
    /// Manifest items by id.
    manifest: BTreeMap<String, ManifestItem>,
    /// Spine idrefs in order.
    spine: Vec<String>,
}

impl Epub {
    fn open(path: &Path) -> Epub {
        let mut zip = ZipArchive::new(File::open(path).unwrap()).expect("not a zip file");
        let mut entries = Vec::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            let extra_len = f.extra_data().map_or(0, |d| d.len());
            entries.push(Entry {
                name: f.name().expect("bad entry name").into_owned(),
                data,
                method: f.compression(),
                extra_len,
            });
        }
        let mut epub = Epub {
            entries,
            manifest: BTreeMap::new(),
            spine: Vec::new(),
        };
        let opf = epub.text("OEBPS/content.opf");
        let doc = parse_xml("OEBPS/content.opf", &opf);
        for n in doc.descendants() {
            if n.tag_name().namespace() != Some(OPF_NS) {
                continue;
            }
            match n.tag_name().name() {
                "item" => {
                    epub.manifest.insert(
                        n.attribute("id").expect("item without id").to_string(),
                        ManifestItem {
                            href: n.attribute("href").expect("item without href").to_string(),
                            media_type: n.attribute("media-type").unwrap_or_default().to_string(),
                            properties: n.attribute("properties").unwrap_or_default().to_string(),
                        },
                    );
                }
                "itemref" => epub
                    .spine
                    .push(n.attribute("idref").expect("itemref without idref").into()),
                _ => {}
            }
        }
        epub
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.names().any(|n| n == name)
    }

    fn bytes(&self, name: &str) -> &[u8] {
        &self
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("missing entry {name}"))
            .data
    }

    fn text(&self, name: &str) -> String {
        String::from_utf8(self.bytes(name).to_vec()).unwrap_or_else(|_| panic!("{name}: not UTF-8"))
    }

    /// Zip paths of the XHTML documents in spine order.
    fn spine_docs(&self) -> Vec<String> {
        self.spine
            .iter()
            .map(|id| format!("OEBPS/{}", self.manifest[id].href))
            .collect()
    }

    /// Spine documents other than the cover page.
    fn chapter_docs(&self) -> Vec<String> {
        self.spine_docs()
            .into_iter()
            .filter(|d| !d.ends_with("cover.xhtml"))
            .collect()
    }

    fn xhtml_names(&self) -> Vec<String> {
        self.names()
            .filter(|n| n.ends_with(".xhtml"))
            .map(str::to_string)
            .collect()
    }

    fn css(&self) -> String {
        self.text("OEBPS/css/base.css")
    }

    /// Whether a spine document is a generated note page.
    fn is_note_page(&self, doc: &str) -> bool {
        self.text(doc).contains("notes-page")
    }
}

fn parse_xml<'a>(name: &str, text: &'a str) -> roxmltree::Document<'a> {
    roxmltree::Document::parse(text)
        .unwrap_or_else(|e| panic!("{name} is not well-formed XML: {e}"))
}

/// Joins a relative URL path onto the directory of `base` (both zip paths)
/// and normalizes `.` and `..` segments.
fn resolve(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').collect();
    parts.pop();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Decodes `%XX` escapes in a URL path.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extracts the targets of `url(...)` in CSS text.
fn css_urls(css: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = css;
    while let Some(pos) = rest.find("url(") {
        rest = &rest[pos + 4..];
        let end = rest.find(')').unwrap_or(rest.len());
        let raw = rest[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        urls.push(raw.to_string());
        rest = &rest[end..];
    }
    urls
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

/// Zip layout: `mimetype` first, stored, no extra field; container points to
/// the OPF; manifest and zip contents agree; spine resolves.
fn assert_layout(epub: &Epub) {
    let first = &epub.entries[0];
    assert_eq!(first.name, "mimetype");
    assert_eq!(first.method, CompressionMethod::Stored);
    assert_eq!(first.extra_len, 0, "mimetype has an extra field");
    assert_eq!(first.data, b"application/epub+zip");

    for required in [
        "META-INF/container.xml",
        "OEBPS/content.opf",
        "OEBPS/nav.xhtml",
        "OEBPS/toc.ncx",
        "OEBPS/css/base.css",
        "OEBPS/text/cover.xhtml",
    ] {
        assert!(epub.has(required), "missing {required}");
    }
    let container = epub.text("META-INF/container.xml");
    parse_xml("container.xml", &container);
    assert!(container.contains("full-path=\"OEBPS/content.opf\""));

    let names: BTreeSet<&str> = epub.names().collect();
    assert_eq!(names.len(), epub.entries.len(), "duplicate zip entries");
    let mut manifest_paths = BTreeSet::new();
    for (id, item) in &epub.manifest {
        let path = format!("OEBPS/{}", percent_decode(&item.href));
        assert!(
            names.contains(path.as_str()),
            "manifest item {id} -> missing {path}"
        );
        assert!(!item.media_type.is_empty(), "item {id} has no media-type");
        manifest_paths.insert(path);
    }
    for n in &names {
        if n.starts_with("OEBPS/") && *n != "OEBPS/content.opf" {
            assert!(manifest_paths.contains(*n), "{n} is not in the manifest");
        }
    }
    assert!(
        epub.manifest.values().any(|i| i.properties.contains("nav")),
        "no nav document"
    );
    assert!(!epub.spine.is_empty(), "empty spine");
    for idref in &epub.spine {
        let item = epub
            .manifest
            .get(idref)
            .unwrap_or_else(|| panic!("spine idref {idref} unresolved"));
        assert_eq!(item.media_type, "application/xhtml+xml");
    }
    for n in &names {
        if n.ends_with(".xhtml")
            || n.ends_with(".opf")
            || n.ends_with(".ncx")
            || n.ends_with(".svg")
        {
            parse_xml(n, &epub.text(n));
        }
    }
}

/// Every `src`/`href` in XHTML, nav, NCX and CSS points into the book (with
/// a valid fragment id when it has one), or is an external `http(s)`/`mailto`
/// link (`href` only; images must be embedded).
fn assert_links_resolve(epub: &Epub) {
    let names: BTreeSet<&str> = epub.names().collect();
    let mut ids: HashMap<String, BTreeSet<String>> = HashMap::new();
    for doc in epub.xhtml_names() {
        let text = epub.text(&doc);
        let xml = parse_xml(&doc, &text);
        let set = xml
            .descendants()
            .filter_map(|n| n.attribute("id").map(str::to_string))
            .collect();
        ids.insert(doc, set);
    }

    let check = |from: &str, attr: &str, url: &str| {
        if url.is_empty() {
            panic!("empty {attr} in {from}");
        }
        if let Some((scheme, _)) = url.split_once(':')
            && !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+')
        {
            assert!(
                attr == "href" && ["http", "https", "mailto"].contains(&scheme),
                "external {attr}={url:?} in {from}"
            );
            return;
        }
        let (path, frag) = match url.split_once('#') {
            Some((p, f)) => (p, Some(f)),
            None => (url, None),
        };
        let target = if path.is_empty() {
            from.to_string()
        } else {
            resolve(from, &percent_decode(path))
        };
        assert!(
            names.contains(target.as_str()),
            "{attr}={url:?} in {from} does not resolve ({target})"
        );
        if let Some(frag) = frag
            && let Some(set) = ids.get(&target)
        {
            assert!(
                set.contains(frag),
                "{attr}={url:?} in {from}: no id {frag:?} in {target}"
            );
        }
    };

    for doc in epub.xhtml_names() {
        let text = epub.text(&doc);
        let xml = parse_xml(&doc, &text);
        for n in xml.descendants() {
            for a in n.attributes() {
                let name = a.name();
                if name == "src" || name == "href" {
                    check(&doc, name, a.value());
                }
                if name == "style" {
                    for u in css_urls(a.value()) {
                        check(&doc, "src", &u);
                    }
                }
            }
            if n.has_tag_name((XHTML_NS, "style")) {
                for u in css_urls(n.text().unwrap_or_default()) {
                    check(&doc, "src", &u);
                }
            }
        }
    }
    let ncx = epub.text("OEBPS/toc.ncx");
    for n in parse_xml("toc.ncx", &ncx).descendants() {
        if n.has_tag_name("content") {
            check("OEBPS/toc.ncx", "href", n.attribute("src").unwrap());
        }
    }
    for u in css_urls(&epub.css()) {
        check("OEBPS/css/base.css", "src", &u);
    }
}

/// Strict device profiles: no `<svg>`, no `<math>`, no SVG resource, and
/// every image is an opaque PNG or a JPEG.
fn assert_device_safe(epub: &Epub, label: &str) {
    for doc in epub.xhtml_names() {
        let text = epub.text(&doc);
        for n in parse_xml(&doc, &text).descendants() {
            let ns = n.tag_name().namespace();
            assert!(
                ns != Some(SVG_NS) && ns != Some(MATHML_NS),
                "{label}: <{}> left in {doc}",
                n.tag_name().name()
            );
        }
        assert!(
            !text.contains("<svg") && !text.contains("<math"),
            "{label}: {doc}"
        );
    }
    for item in epub.manifest.values() {
        assert!(
            !item
                .properties
                .split_whitespace()
                .any(|p| p == "svg" || p == "mathml"),
            "{label}: manifest property {:?} on {}",
            item.properties,
            item.href
        );
        if item.media_type.starts_with("image/") {
            let path = format!("OEBPS/{}", item.href);
            match item.media_type.as_str() {
                "image/png" => {
                    let img = image::load_from_memory(epub.bytes(&path))
                        .unwrap_or_else(|e| panic!("{label}: {path} does not decode: {e}"));
                    assert!(
                        !img.color().has_alpha(),
                        "{label}: {path} has an alpha channel"
                    );
                }
                "image/jpeg" => {
                    image::load_from_memory(epub.bytes(&path))
                        .unwrap_or_else(|e| panic!("{label}: {path} does not decode: {e}"));
                }
                other => panic!("{label}: {path} has media type {other}"),
            }
        }
    }
    assert!(
        !epub.names().any(|n| n.ends_with(".svg")),
        "{label}: SVG resource left"
    );
}

fn assert_valid(epub: &Epub) {
    assert_layout(epub);
    assert_links_resolve(epub);
}

// ---------------------------------------------------------------------------
// Profile matrix
// ---------------------------------------------------------------------------

fn profile_matrix(profile: &str) {
    let strict = STRICT_PROFILES.contains(&profile);
    let ks = fixture("kitchen-sink.md");
    let page = fixture("page.html");
    let book = fixture("book.toml");
    let runs = [
        ("kitchen-sink", convert(&[&ks, "--profile", profile])),
        ("page", convert(&[&page, "--profile", profile])),
        ("book", convert(&["build", &book, "--profile", profile])),
    ];
    for (name, run) in &runs {
        let label = format!("{name}/{profile}");
        assert_valid(&run.epub);
        if strict {
            assert_device_safe(&run.epub, &label);
        }
        assert!(!run.epub.chapter_docs().is_empty(), "{label}: no chapters");
    }
    let ks_epub = &runs[0].1.epub;
    let text: String = ks_epub
        .chapter_docs()
        .iter()
        .map(|d| ks_epub.text(d))
        .collect();
    assert!(text.contains("Kitchen Sink"));
    assert!(text.contains("<table"), "table missing");
    if profile == "generic" {
        // The generic profile keeps vector images.
        assert!(
            ks_epub.names().any(|n| n.ends_with(".svg")),
            "generic: no SVG kept"
        );
    }
}

#[test]
fn profile_scribe() {
    profile_matrix("scribe");
}

#[test]
fn profile_remarkable() {
    profile_matrix("remarkable");
}

#[test]
fn profile_remarkable_pro() {
    profile_matrix("remarkable-pro");
}

#[test]
fn profile_generic() {
    profile_matrix("generic");
}

#[test]
fn all_profiles_are_covered() {
    assert_eq!(PROFILES.len(), 4);
    for p in STRICT_PROFILES {
        assert!(PROFILES.contains(&p));
    }
}

// ---------------------------------------------------------------------------
// Feature-specific checks
// ---------------------------------------------------------------------------

#[test]
fn metadata_and_structure() {
    let run = convert(&[&fixture("kitchen-sink.md"), "--profile", "scribe"]);
    let epub = &run.epub;
    let opf = epub.text("OEBPS/content.opf");
    // Front matter feeds the metadata.
    assert!(opf.contains(">Kitchen Sink</dc:title>"), "{opf}");
    assert!(opf.contains(">Ada Lovelace</dc:creator>"), "{opf}");
    assert!(opf.contains(">en</dc:language>"), "{opf}");
    assert!(opf.contains("dcterms:modified"), "{opf}");
    assert!(opf.contains("properties=\"cover-image\""), "{opf}");
    assert!(opf.contains("<meta name=\"cover\""), "{opf}");
    assert!(epub.spine_docs()[0].ends_with("cover.xhtml"));
    let nav = epub.text("OEBPS/nav.xhtml");
    assert!(nav.contains("epub:type=\"toc\""));
    assert!(nav.contains("Tables"), "subheadings in the TOC");

    let ch = epub.text(&epub.chapter_docs()[0]);
    // Footnotes, task lists, highlighted code with inline styles.
    assert!(ch.contains("footnote"), "footnotes missing");
    assert!(ch.contains("type=\"checkbox\""), "task list missing");
    assert!(
        ch.contains("<pre") && ch.contains("style=\"color"),
        "no highlighted code"
    );
    // Entities come out as characters or numeric references, never named.
    assert!(!ch.contains("&nbsp;"));
}

#[test]
fn flags_override_metadata() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        "--title",
        "Ünïcödé & <Title>",
        "--author",
        "First",
        "--author",
        "Second",
        "--lang",
        "de",
    ]);
    let opf = run.epub.text("OEBPS/content.opf");
    parse_xml("content.opf", &opf);
    assert!(opf.contains("Ünïcödé &amp; &lt;Title&gt;"), "{opf}");
    assert!(opf.contains(">First</dc:creator>") && opf.contains(">Second</dc:creator>"));
    assert!(opf.contains(">de</dc:language>"));
}

#[test]
fn scribe_math_and_mermaid_become_images() {
    let run = convert(&[&fixture("kitchen-sink.md"), "--profile", "scribe"]);
    let epub = &run.epub;
    let text: String = epub.chapter_docs().iter().map(|d| epub.text(d)).collect();
    #[cfg(feature = "math")]
    {
        assert!(
            text.contains("class=\"math-inline\""),
            "no inline math image"
        );
        assert!(
            text.contains("class=\"math-display\""),
            "no display math image"
        );
        assert!(
            text.contains("vertical-align:"),
            "inline math lacks baseline shift"
        );
    }
    #[cfg(not(feature = "math"))]
    {
        assert!(
            text.contains("<code class=\"math-inline\">E = mc^2</code>"),
            "math source not kept as code"
        );
        assert!(run.stderr.contains("math"), "no math warning");
    }
    #[cfg(feature = "mermaid")]
    assert!(
        text.contains("<figure class=\"mermaid\"><img"),
        "mermaid diagram not rendered"
    );
    // Without the feature the diagram is either rendered by an `mmdc` found
    // on PATH or left as a code block; both are fine.
    #[cfg(not(feature = "mermaid"))]
    assert!(text.contains("mermaid") || text.contains("flowchart TD"));
    assert_device_safe(epub, "math/scribe");
}

#[test]
fn math_modes() {
    let ks = fixture("kitchen-sink.md");
    let mathml = convert(&[&ks, "--math", "mathml"]);
    assert_valid(&mathml.epub);
    let doc = &mathml.epub.chapter_docs()[0];
    assert!(mathml.epub.text(doc).contains("<math"), "no MathML");
    let href = doc.strip_prefix("OEBPS/").unwrap();
    assert!(
        mathml
            .epub
            .manifest
            .values()
            .any(|i| i.href == href && i.properties.contains("mathml")),
        "chapter lacks the mathml property"
    );

    let off = convert(&[&ks, "--math", "off"]);
    assert_valid(&off.epub);
    let text = off.epub.text(&off.epub.chapter_docs()[0]);
    assert!(!text.contains("<math"));
    assert!(!text.contains("<img src=\"../images/") || !text.contains("alt=\"E = mc^2\""));
    assert!(
        text.contains("<code class=\"math-inline\">E = mc^2</code>"),
        "LaTeX source not kept"
    );

    // MathML under the Scribe profile still builds but warns.
    let scribe = convert(&[&ks, "--profile", "scribe", "--math", "mathml"]);
    assert!(
        scribe.stderr.to_lowercase().contains("kindle"),
        "{}",
        scribe.stderr
    );
}

#[test]
fn svg_keep_under_device_profile() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        "--profile",
        "remarkable-pro",
        "--svg",
        "keep",
    ]);
    assert_valid(&run.epub);
    assert!(run.epub.names().any(|n| n.ends_with(".svg")));
    for item in run.epub.manifest.values() {
        if item.href.ends_with(".svg") {
            assert_eq!(item.media_type, "image/svg+xml");
        }
    }
}

#[test]
fn note_pages_after_each_chapter() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        "--profile",
        "scribe",
        "--split-level",
        "2",
        "--notes",
        "after-chapter",
        "--notes-style",
        "grid",
        "--notes-pages",
        "2",
    ]);
    let epub = &run.epub;
    assert_valid(epub);
    assert_device_safe(epub, "notes/scribe");
    let docs = epub.chapter_docs();
    let flags: Vec<bool> = docs.iter().map(|d| epub.is_note_page(d)).collect();
    let content = flags.iter().filter(|f| !**f).count();
    assert!(content > 1, "split-level 2 should give several chapters");
    assert_eq!(flags.iter().filter(|f| **f).count(), content * 2);
    // Pattern: content, note, note, content, note, note, ...
    for (i, chunk) in flags.chunks(3).enumerate() {
        assert_eq!(chunk, [false, true, true], "unexpected order at chunk {i}");
    }
    let note = epub.text(&docs[1]);
    assert!(note.contains("notes-grid"));
    // Note pages are not in the TOC in after-chapter mode.
    let nav = epub.text("OEBPS/nav.xhtml");
    let note_href = docs[1].strip_prefix("OEBPS/").unwrap();
    assert!(!nav.contains(note_href), "note page listed in the TOC");

    // The page image matches the Scribe resolution, and is shared.
    let srcs: BTreeSet<String> = docs
        .iter()
        .filter(|d| epub.is_note_page(d))
        .map(|d| {
            let text = epub.text(d);
            let xml = parse_xml(d, &text);
            let img = xml
                .descendants()
                .find(|n| n.has_tag_name((XHTML_NS, "img")))
                .expect("note page without image");
            resolve(d, img.attribute("src").unwrap())
        })
        .collect();
    assert_eq!(srcs.len(), 1, "note page image not reused");
    let img = image::load_from_memory(epub.bytes(srcs.iter().next().unwrap())).unwrap();
    assert_eq!((img.width(), img.height()), (1860, 2480));
}

#[test]
fn note_pages_at_end() {
    let run = convert(&[
        &fixture("page.html"),
        "--profile",
        "remarkable",
        "--notes",
        "end",
        "--notes-pages",
        "3",
    ]);
    let epub = &run.epub;
    assert_valid(epub);
    let docs = epub.chapter_docs();
    let flags: Vec<bool> = docs.iter().map(|d| epub.is_note_page(d)).collect();
    let n = flags.len();
    assert!(n >= 4);
    assert_eq!(&flags[n - 3..], [true, true, true]);
    assert!(flags[..n - 3].iter().all(|f| !f));
    let nav = epub.text("OEBPS/nav.xhtml");
    assert!(nav.contains(">Notes<"), "no Notes TOC entry");
    let img_href = epub
        .manifest
        .values()
        .filter(|i| i.media_type == "image/png")
        .map(|i| format!("OEBPS/{}", i.href))
        .find(|p| {
            image::load_from_memory(epub.bytes(p))
                .map(|i| (i.width(), i.height()) == (1404, 1872))
                .unwrap_or(false)
        });
    assert!(img_href.is_some(), "no note page at reMarkable resolution");
}

#[test]
fn no_note_pages_by_default() {
    let run = convert(&[&fixture("kitchen-sink.md")]);
    let epub = &run.epub;
    assert!(epub.chapter_docs().iter().all(|d| !epub.is_note_page(d)));
}

#[test]
fn margins_and_line_spacing_in_css() {
    let ks = fixture("kitchen-sink.md");
    let normal = convert(&[&ks]);
    assert!(!normal.epub.css().contains("padding-left: 8%"));
    let wide = convert(&[&ks, "--margin", "wide"]);
    assert!(wide.epub.css().contains("padding-left: 8%"));
    let extra = convert(&[
        &ks,
        "--margin",
        "extra",
        "--line-spacing",
        "1.75",
        "--css",
        &fixture("extra.css"),
    ]);
    let css = extra.epub.css();
    assert!(css.contains("padding-left: 15%"));
    assert!(css.contains("line-height: 1.75"));
    let user = std::fs::read_to_string(fixtures().join("extra.css")).unwrap();
    assert!(
        css.trim_end().ends_with(user.trim_end()),
        "user CSS not appended last"
    );
    // Every chapter links the stylesheet.
    for d in extra.epub.chapter_docs() {
        assert!(extra.epub.text(&d).contains("css/base.css"), "{d}");
    }
}

#[test]
fn offline_warns_about_remote_image() {
    let run = convert(&[&fixture("kitchen-sink.md")]);
    assert!(
        run.stderr.contains("rust-logo-blk.svg") && run.stderr.contains("offline"),
        "{}",
        run.stderr
    );
    // The alt text stays in place of the image.
    let text = run.epub.text(&run.epub.chapter_docs()[0]);
    assert!(text.contains("Remote image"));
    assert!(!text.contains("src=\"https://"));
}

#[test]
fn html_input_is_sanitized() {
    // page.html links to kitchen-sink.md, whose task-list checkboxes are
    // legitimate <input> elements; check page.html on its own.
    let run = convert(&[
        &fixture("page.html"),
        "--profile",
        "scribe",
        "--no-follow-links",
    ]);
    let epub = &run.epub;
    let text: String = epub.chapter_docs().iter().map(|d| epub.text(d)).collect();
    for bad in [
        "<script",
        "onclick",
        "javascript:",
        "<iframe",
        "<form",
        "<input",
    ] {
        assert!(!text.contains(bad), "{bad} survived sanitizing");
    }
    assert!(text.contains("©") || text.contains("&#169;"));
    // <style> blocks end up in the head.
    for d in epub.chapter_docs() {
        let t = epub.text(&d);
        if let Some((_, body)) = t.split_once("<body") {
            assert!(!body.contains("<style"), "<style> in body of {d}");
        }
    }
    assert!(run.stderr.contains("missing.css"));
    // Duplicate headings get unique ids.
    let mut seen = BTreeSet::new();
    for d in epub.chapter_docs() {
        let t = epub.text(&d);
        for n in parse_xml(&d, &t).descendants() {
            if let Some(id) = n.attribute("id") {
                assert!(seen.insert(id.to_string()), "duplicate id {id}");
            }
        }
    }
}

#[test]
fn cross_file_links_are_rewritten() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        &fixture("page.html"),
        "--split-level",
        "2",
    ]);
    let epub = &run.epub;
    assert_valid(epub);
    let all: String = epub.chapter_docs().iter().map(|d| epub.text(d)).collect();
    assert!(
        !all.contains("href=\"page.html"),
        "link to page.html not rewritten"
    );
    assert!(
        !all.contains("kitchen-sink.md"),
        "link to kitchen-sink.md not rewritten"
    );
    assert!(all.contains("#second-section\""));
}

#[test]
fn build_book_toml() {
    let run = convert(&["build", &fixture("book.toml")]);
    let epub = &run.epub;
    assert_valid(epub);
    assert_device_safe(epub, "book.toml");
    let opf = epub.text("OEBPS/content.opf");
    assert!(opf.contains(">Fixture Book</dc:title>"));
    assert!(
        opf.contains(">Ada Lovelace</dc:creator>") && opf.contains(">Charles Babbage</dc:creator>")
    );
    // Lined note pages after every chapter, wide margins, user CSS.
    let docs = epub.chapter_docs();
    assert!(docs.iter().any(|d| epub.text(d).contains("notes-lined")));
    let css = epub.css();
    assert!(css.contains("padding-left: 8%"));
    assert!(css.contains("line-height: 1.5"));
    // The cover is the supplied JPEG.
    assert!(
        epub.manifest
            .values()
            .any(|i| i.properties.contains("cover-image") && i.media_type == "image/jpeg"),
        "{opf}"
    );
}

#[test]
fn grayscale_and_cover_options() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        "--profile",
        "scribe",
        "--grayscale",
        "--cover",
        &fixture("images/diagram.svg"),
    ]);
    let epub = &run.epub;
    assert_valid(epub);
    assert_device_safe(epub, "grayscale");
    for item in epub.manifest.values() {
        if item.media_type == "image/png" || item.media_type == "image/jpeg" {
            let img = image::load_from_memory(epub.bytes(&format!("OEBPS/{}", item.href)))
                .unwrap()
                .to_rgb8();
            if item.properties.contains("cover-image") {
                continue;
            }
            assert!(
                img.pixels().all(|p| p[0] == p[1] && p[1] == p[2]),
                "{} is not grayscale",
                item.href
            );
        }
    }
}

#[test]
fn split_level_none_gives_one_chapter_per_file() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        &fixture("page.html"),
        "--split-level",
        "none",
    ]);
    assert_valid(&run.epub);
    assert_eq!(run.epub.chapter_docs().len(), 2);
}

#[test]
fn output_is_deterministic() {
    let a = convert(&[&fixture("kitchen-sink.md"), "--profile", "scribe"]);
    let b = convert(&[&fixture("kitchen-sink.md"), "--profile", "scribe"]);
    let names_a: Vec<&str> = a.epub.names().collect();
    let names_b: Vec<&str> = b.epub.names().collect();
    assert_eq!(names_a, names_b);
    for e in &a.epub.entries {
        if e.name.ends_with(".xhtml") || e.name.starts_with("OEBPS/images/") {
            assert_eq!(
                e.data,
                b.epub.bytes(&e.name),
                "{} differs between runs",
                e.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn assert_fails(args: &[&str], needle: &str) {
    let out = run_raw(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{args:?} should fail");
    assert!(!stderr.contains("panicked"), "{args:?} panicked:\n{stderr}");
    assert!(
        stderr.to_lowercase().contains(&needle.to_lowercase()),
        "{args:?}: expected {needle:?} in:\n{stderr}"
    );
}

#[test]
fn bad_inputs_fail_cleanly() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("x.epub");
    let out = out.to_str().unwrap();
    let missing = dir.path().join("nope.md");
    assert_fails(&[missing.to_str().unwrap(), "-o", out], "nope.md");

    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, "hello").unwrap();
    assert_fails(&[txt.to_str().unwrap(), "-o", out], "notes.txt");

    let toml = dir.path().join("book.toml");
    std::fs::write(&toml, "title = [unclosed").unwrap();
    assert_fails(&["build", toml.to_str().unwrap(), "-o", out], "book.toml");

    assert_fails(
        &[&fixture("kitchen-sink.md"), "--profile", "kobo", "-o", out],
        "kobo",
    );
    assert_fails(&["-o", out], "input");
    assert!(!Path::new(out).exists(), "a failed run left an EPUB behind");
}

#[test]
fn garbage_input_does_not_panic() {
    let dir = TempDir::new().unwrap();
    let md = dir.path().join("garbage.md");
    let mut src = String::from("---\ntitle: [unterminated\n---\n");
    src.push_str("<svg><svg><text>unclosed\n$$\\frac{$$\n```mermaid\n%%%%\n```\n");
    src.push_str("![x](data:image/png;base64,!!!!) ![y](missing.png) <img src=\"\">\n");
    src.push_str("[link](#nowhere) <math><mi>x</mi></math> \u{0}\u{1}\n");
    std::fs::write(&md, src).unwrap();
    let out = dir.path().join("g.epub");
    let res = run_raw(&[
        md.to_str().unwrap(),
        "--profile",
        "scribe",
        "--offline",
        "-o",
        out.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    if res.status.success() {
        let epub = Epub::open(&out);
        assert_valid(&epub);
        assert_device_safe(&epub, "garbage");
    } else {
        assert!(
            stderr.starts_with("error:") || stderr.contains("\nerror:"),
            "{stderr}"
        );
    }
}

#[test]
fn version_and_help() {
    let out = run_raw(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
    let out = run_raw(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--profile",
        "--notes",
        "--margin",
        "--math",
        "--svg",
        "--offline",
    ] {
        assert!(help.contains(flag), "--help lacks {flag}");
    }
}

// ---------------------------------------------------------------------------
// Regression tests for review findings
// ---------------------------------------------------------------------------

#[test]
fn user_css_resources_are_embedded() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("images")).unwrap();
    std::fs::copy(
        fixtures().join("images/chart.png"),
        dir.path().join("images/chart.png"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("images/diagram.svg"),
        dir.path().join("images/diagram.svg"),
    )
    .unwrap();
    std::fs::write(dir.path().join("font.woff2"), b"wOF2fakefontdata").unwrap();
    let css = dir.path().join("user.css");
    std::fs::write(
        &css,
        "@font-face { font-family: U; src: url(font.woff2) format(\"woff2\"); }\n\
         .a { background: url(images/chart.png); }\n\
         .b { list-style-image: url(\"images/diagram.svg\"); }\n\
         .c { background: url(missing.png); }\n",
    )
    .unwrap();
    for profile in ["generic", "scribe"] {
        let run = convert(&[
            &fixture("kitchen-sink.md"),
            "--profile",
            profile,
            "--css",
            css.to_str().unwrap(),
        ]);
        // assert_links_resolve also checks every url() in base.css.
        assert_valid(&run.epub);
        let base = run.epub.css();
        assert!(base.contains("url(\"../fonts/"), "{base}");
        assert!(base.contains("url(\"../images/"), "{base}");
        assert!(!base.contains("missing.png"), "{base}");
        assert!(run.stderr.contains("missing.png"), "{}", run.stderr);
        if profile == "scribe" {
            assert_device_safe(&run.epub, "user css/scribe");
        }
    }
}

#[test]
fn raw_mathml_is_removed_under_device_profiles() {
    let dir = TempDir::new().unwrap();
    let md = dir.path().join("m.md");
    std::fs::write(
        &md,
        "# M\n\nInline <math><mi>x</mi></math> and\n\n<math display=\"block\"><semantics><mrow><mi>z</mi></mrow><annotation encoding=\"application/x-tex\">z^2</annotation></semantics></math>\n",
    )
    .unwrap();
    for profile in STRICT_PROFILES {
        let run = convert(&[md.to_str().unwrap(), "--profile", profile]);
        assert_valid(&run.epub);
        assert_device_safe(&run.epub, profile);
        assert!(run.stderr.contains("TeX annotation"), "{}", run.stderr);
    }
    // The generic profile keeps MathML.
    let run = convert(&[md.to_str().unwrap()]);
    let doc = &run.epub.chapter_docs()[0];
    assert!(run.epub.text(doc).contains("<math"));
}

#[test]
fn output_path_cannot_overwrite_the_input() {
    let dir = TempDir::new().unwrap();
    let md = dir.path().join("a2.md");
    std::fs::write(&md, "# Keep me\n").unwrap();
    let md_s = md.to_str().unwrap();
    // `-o a2.md` gets `.epub` appended, so the input is never the target.
    let out = run_raw(&[md_s, "--offline", "-o", md_s]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("a2.md.epub").is_file());
    assert_eq!(std::fs::read_to_string(&md).unwrap(), "# Keep me\n");
    // The guard still protects the other sources.
    let css = dir.path().join("style.epub");
    std::fs::write(&css, "p { color: black }").unwrap();
    let css_s = css.to_str().unwrap();
    assert_fails(&[md_s, "--css", css_s, "-o", css_s], "same as the CSS file");
    assert_eq!(std::fs::read_to_string(&css).unwrap(), "p { color: black }");
}

#[test]
fn explicit_missing_cover_fails() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("x.epub");
    assert_fails(
        &[
            &fixture("kitchen-sink.md"),
            "--cover",
            "nope-cover.png",
            "--offline",
            "-o",
            out.to_str().unwrap(),
        ],
        "cover image not found",
    );
}

#[test]
fn verbose_from_book_toml_prints_each_warning_once() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n\n![m](missing.png)\n").unwrap();
    let toml = dir.path().join("v.toml");
    std::fs::write(
        &toml,
        "inputs = [\"a.md\"]\nverbose = true\noffline = true\noutput = \"v.epub\"\n",
    )
    .unwrap();
    let out = run_raw(&["build", toml.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("warning: image not embedded").count(),
        1,
        "{stderr}"
    );
    // The flag turns it off again.
    let out = run_raw(&["build", toml.to_str().unwrap(), "--no-verbose"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("processing"), "{stderr}");
    assert_eq!(
        stderr.matches("warning: image not embedded").count(),
        1,
        "{stderr}"
    );
}

#[test]
fn footnotes_have_epub_semantics() {
    let run = convert(&[&fixture("kitchen-sink.md"), "--split-level", "none"]);
    assert_valid(&run.epub);
    let text = run.epub.text(&run.epub.chapter_docs()[0]);
    assert!(
        text.contains("epub:type=\"noteref\" role=\"doc-noteref\""),
        "no noteref"
    );
    assert!(
        text.contains("epub:type=\"endnotes\" role=\"doc-endnotes\""),
        "no endnotes"
    );
    assert!(text.contains("epub:type=\"endnote\""), "no endnote");
    assert!(text.contains("role=\"doc-backlink\""), "no backlink");
}

#[test]
fn margin_presets_leave_note_pages_full_width() {
    let run = convert(&[
        &fixture("kitchen-sink.md"),
        "--profile",
        "scribe",
        "--margin",
        "extra",
        "--notes",
        "after-chapter",
    ]);
    assert_valid(&run.epub);
    let notes: Vec<String> = run
        .epub
        .chapter_docs()
        .into_iter()
        .filter(|d| run.epub.is_note_page(d))
        .collect();
    assert!(!notes.is_empty());
    for d in &notes {
        assert!(run.epub.text(d).contains("<body class=\"notes\">"), "{d}");
    }
    let css = run.epub.css();
    let preset = css.find("margin preset: extra").unwrap();
    let reset = css.find("body.notes").expect("no body.notes rule");
    assert!(reset > preset);
}

#[test]
fn html_lang_and_language_tags() {
    let dir = TempDir::new().unwrap();
    let html = dir.path().join("p.html");
    std::fs::write(
        &html,
        "<!DOCTYPE html><html lang=\"de\"><head><title>Seite</title></head><body><p>Hallo</p></body></html>",
    )
    .unwrap();
    let run = convert(&[html.to_str().unwrap()]);
    let opf = run.epub.text("OEBPS/content.opf");
    assert!(opf.contains("<dc:language>de</dc:language>"), "{opf}");

    let md = dir.path().join("l.md");
    std::fs::write(&md, "---\nlang: \"en_US\"\ndate: 2024-13-45\n---\n# L\n").unwrap();
    let run = convert(&[md.to_str().unwrap()]);
    let opf = run.epub.text("OEBPS/content.opf");
    assert!(opf.contains("<dc:language>en-US</dc:language>"), "{opf}");
    assert!(run.stderr.contains("2024-13-45"), "{}", run.stderr);

    let run = convert(&[md.to_str().unwrap(), "--lang", "xx-invalid-tag-!!"]);
    let opf = run.epub.text("OEBPS/content.opf");
    assert!(opf.contains("<dc:language>en</dc:language>"), "{opf}");
    assert!(
        run.stderr.contains("invalid language tag"),
        "{}",
        run.stderr
    );
}

#[test]
fn deeply_nested_input_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    let html = dir.path().join("deep.html");
    std::fs::write(&html, "<div>".repeat(20_000)).unwrap();
    let out = dir.path().join("d.epub");
    assert_fails(
        &[html.to_str().unwrap(), "-o", out.to_str().unwrap()],
        "nested",
    );
}

#[test]
fn build_after_an_input_is_the_subcommand() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n").unwrap();
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    let toml = dir.path().join("v.toml");
    std::fs::write(&toml, "inputs = [\"a.md\"]\noffline = true\n").unwrap();
    let out = dir.path().join("o.epub");
    let res = run_raw(&[
        dir.path().join("b.md").to_str().unwrap(),
        "build",
        toml.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(res.status.success(), "{stderr}");
    let epub = Epub::open(&out);
    assert!(
        epub.text("OEBPS/content.opf")
            .contains("<dc:title>B</dc:title>")
    );
}

// ---------------------------------------------------------------------------
// Linked local files
// ---------------------------------------------------------------------------

fn linked_fixture(name: &str) -> String {
    fixtures()
        .join("linked")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// The `<title>` of a spine document (the chapter title).
fn doc_title(epub: &Epub, doc: &str) -> String {
    let text = epub.text(doc);
    let start = text.find("<title>").expect("no <title>") + "<title>".len();
    let end = text[start..].find("</title>").expect("no </title>") + start;
    text[start..end].to_string()
}

/// The `src` of the first `<img>` in a document.
fn first_img_src(text: &str) -> Option<String> {
    let at = text.find("<img")?;
    let tail = &text[at..];
    let s = tail.find("src=\"")? + 5;
    let e = tail[s..].find('"')? + s;
    Some(tail[s..e].to_string())
}

#[test]
fn linked_local_files_are_added() {
    for profile in ["scribe", "generic"] {
        let run = convert(&[&linked_fixture("a.md"), "--profile", profile]);
        let epub = &run.epub;
        let label = format!("linked/{profile}");
        assert_valid(epub);
        if profile == "scribe" {
            assert_device_safe(epub, &label);
        }
        let docs = epub.chapter_docs();
        let titles: Vec<String> = docs.iter().map(|d| doc_title(epub, d)).collect();
        // Inputs first, then linked documents breadth-first (each once, even
        // through the a.md <-> b.md cycle), then the "Linked files" pages in
        // order of first reference.
        assert_eq!(
            titles,
            [
                "Linked Start",
                "Second Document",
                "More Notes",
                "HTML Page",
                "Linked files",
                "flow.svg",
                "flow.mmd",
                "example.rs",
                "notes.txt",
            ],
            "{label}"
        );
        let file = |i: usize| docs[i].rsplit('/').next().unwrap().to_string();
        let texts: Vec<String> = docs.iter().map(|d| epub.text(d)).collect();
        let start = &texts[0];
        // A fragment that exists is kept, an unknown one goes to the start.
        assert!(
            start.contains(&format!("href=\"{}#later\"", file(1))),
            "{label}: {start}"
        );
        for (i, text) in [
            (1, "unknown anchor"),
            (5, "diagram image"),
            (6, "Mermaid source"),
            (7, "Rust code"),
            (8, "plain notes"),
        ] {
            assert!(
                start.contains(&format!("href=\"{}\">{text}", file(i))),
                "{label}: {text}: {start}"
            );
        }
        // Files that cannot be added lose the link but keep the text.
        for text in ["binary blob", "missing file", "directory"] {
            assert!(start.contains(&format!("<a>{text}</a>")), "{label}: {text}");
        }
        // Web, mail and in-page links are untouched.
        assert!(start.contains("href=\"https://example.com/\""), "{label}");
        assert!(
            start.contains("href=\"mailto:someone@example.com\""),
            "{label}"
        );
        assert!(start.contains("href=\"#linked-start\""), "{label}");
        // Links between linked documents work too (back to the input,
        // percent-encoded names, fragments into HTML documents).
        assert!(
            texts[1].contains(&format!("href=\"{}\"", file(0))),
            "{label}"
        );
        assert!(
            texts[1].contains(&format!("href=\"{}\"", file(2))),
            "{label}"
        );
        assert!(
            texts[2].contains(&format!("href=\"{}#part\"", file(3))),
            "{label}"
        );
        assert!(
            texts[3].contains(&format!("href=\"{}\"", file(8))),
            "{label}"
        );

        // Each failure is explained once, with the reason.
        let err = &run.stderr;
        for (name, why) in [
            ("data.bin", "binary file"),
            ("missing.md", "does not exist"),
            ("diagrams/", "directory"),
        ] {
            let lines: Vec<&str> = err
                .lines()
                .filter(|l| l.contains(&format!("'{name}'")))
                .collect();
            assert_eq!(lines.len(), 1, "{label}: {name}: {err}");
            assert!(lines[0].contains(why), "{label}: {}", lines[0]);
        }
        for name in ["b.md", "flow.svg", "example.rs", "notes.txt", "c.html"] {
            assert!(
                !err.contains(name),
                "{label}: unexpected warning about {name}: {err}"
            );
        }

        // The image page and the image embedded in b.md share one resource.
        let page_img = first_img_src(&texts[5]).expect("no image on the flow.svg page");
        assert_eq!(
            first_img_src(&texts[1]).as_deref(),
            Some(page_img.as_str()),
            "{label}"
        );
        if profile == "scribe" {
            assert!(page_img.ends_with(".png"), "{label}: {page_img}");
        } else {
            assert!(page_img.ends_with(".svg"), "{label}: {page_img}");
        }
        // The Mermaid page shows the diagram (or the source with a warning).
        #[cfg(feature = "mermaid")]
        assert!(texts[6].contains("<img"), "{label}: {}", texts[6]);
        #[cfg(not(feature = "mermaid"))]
        assert!(
            texts[6].contains("<img") || (texts[6].contains("A[Start]") && err.contains("Mermaid")),
            "{label}: {}",
            texts[6]
        );
        // Source code is highlighted; plain text is escaped.
        assert!(texts[7].contains("language-rs") && texts[7].contains("<span style="));
        assert!(texts[8].contains("Plain notes &lt;with&gt; &amp; special"));

        // The non-document pages nest under "Linked files" in the nav.
        let nav = epub.text("OEBPS/nav.xhtml");
        let section = nav.split_once("Linked files</a>").expect("no section").1;
        let nested = section.split_once("<ol>").expect("no nested list").1;
        let nested = nested.split_once("</ol>").expect("unclosed list").0;
        for name in ["flow.svg", "flow.mmd", "example.rs", "notes.txt"] {
            assert!(
                nested.contains(&format!(">{name}</a>")),
                "{label}: {name} in {nav}"
            );
        }
        assert!(!nested.contains("More Notes"), "{label}");
        // Linked files never name the book.
        let opf = epub.text("OEBPS/content.opf");
        assert!(opf.contains(">Linked Start</dc:title>"), "{label}");
    }
}

#[test]
fn follow_links_can_be_turned_off() {
    let run = convert(&[&linked_fixture("a.md"), "--no-follow-links"]);
    assert_valid(&run.epub);
    assert_eq!(run.epub.chapter_docs().len(), 1);
    let text = run.epub.text(&run.epub.chapter_docs()[0]);
    assert!(text.contains("<a>Rust code</a>"), "{text}");
    assert!(run.stderr.contains("turned off"), "{}", run.stderr);

    // The same from book.toml, and --follow-links wins over it.
    let dir = TempDir::new().unwrap();
    let toml = dir.path().join("book.toml");
    let input = linked_fixture("a.md").replace('\\', "/");
    std::fs::write(
        &toml,
        format!("inputs = [\"{input}\"]\nfollow-links = false\n"),
    )
    .unwrap();
    let toml = toml.to_string_lossy().into_owned();
    let off = convert(&["build", &toml]);
    assert_eq!(off.epub.chapter_docs().len(), 1);
    let on = convert(&["build", &toml, "--follow-links"]);
    assert!(on.epub.chapter_docs().len() > 1);
}

#[test]
fn follow_links_from_the_user_config() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.toml");
    let a = linked_fixture("a.md");
    let out = dir.path().join("out.epub");
    let out_str = out.to_string_lossy().into_owned();
    let chapters = |extra: &[&str]| {
        let mut args = vec![a.as_str(), "--offline", "-o", out_str.as_str()];
        args.extend_from_slice(extra);
        let res = run_with_config(&args, &config, None);
        assert!(res.status.success(), "{}", stderr_of(&res));
        let epub = Epub::open(&out);
        assert_valid(&epub);
        epub.chapter_docs().len()
    };

    // Built-in default: on, shown in `config list` and in --help.
    let list = config_ok(&config, &["list"]);
    let line = list.lines().find(|l| l.contains("follow-links")).unwrap();
    assert!(line.contains("true") && line.contains("built-in"), "{line}");
    let help = stdout_of(&run_with_config(&["--help"], &config, None));
    let line = help.lines().find(|l| l.contains("--follow-links")).unwrap();
    assert!(line.contains("[default: on]"), "{help}");
    assert!(chapters(&[]) > 1);

    // `config set follow-links false` becomes the default ...
    config_ok(&config, &["set", "follow-links", "false"]);
    assert_eq!(
        config_ok(&config, &["get", "follow-links"]).trim_end(),
        "false"
    );
    let list = config_ok(&config, &["list"]);
    let line = list.lines().find(|l| l.contains("follow-links")).unwrap();
    assert!(
        line.contains("false") && line.contains("user config"),
        "{line}"
    );
    let help = stdout_of(&run_with_config(&["--help"], &config, None));
    let line = help.lines().find(|l| l.contains("--follow-links")).unwrap();
    assert!(line.contains("[default: off]"), "{help}");
    assert_eq!(chapters(&[]), 1);

    // ... and the flag and book.toml still win over it.
    assert!(chapters(&["--follow-links"]) > 1);
    let toml = dir.path().join("book.toml");
    std::fs::write(
        &toml,
        format!(
            "inputs = [\"{}\"]\nfollow-links = true\n",
            a.replace('\\', "/")
        ),
    )
    .unwrap();
    let res = run_with_config(
        &["build", toml.to_str().unwrap(), "--offline", "-o", &out_str],
        &config,
        None,
    );
    assert!(res.status.success(), "{}", stderr_of(&res));
    assert!(Epub::open(&out).chapter_docs().len() > 1);
}

#[test]
fn links_to_explicit_inputs_are_not_duplicated() {
    // b.md is an input too: it stays where it was given, and a.md's link
    // maps to it instead of adding it a second time.
    let run = convert(&[&linked_fixture("a.md"), &linked_fixture("b.md")]);
    let epub = &run.epub;
    assert_valid(epub);
    let titles: Vec<String> = epub
        .chapter_docs()
        .iter()
        .map(|d| doc_title(epub, d))
        .collect();
    assert_eq!(
        titles
            .iter()
            .filter(|t| t.as_str() == "Second Document")
            .count(),
        1
    );
    assert_eq!(titles[1], "Second Document");
    assert_eq!(titles[2], "More Notes");
}

// ---------------------------------------------------------------------------
// User config (`md2epub config`), output paths, --help defaults
// ---------------------------------------------------------------------------

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `md2epub config ...` against `config`, asserting success.
fn config_ok(config: &Path, args: &[&str]) -> String {
    let mut all = vec!["config"];
    all.extend_from_slice(args);
    let out = run_with_config(&all, config, None);
    assert!(out.status.success(), "{all:?}: {}", stderr_of(&out));
    stdout_of(&out)
}

#[test]
fn config_subcommand_round_trip() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("cfg").join("config.toml");
    assert_eq!(
        config_ok(&config, &["path"]).trim_end(),
        config.to_str().unwrap()
    );
    let list = config_ok(&config, &[]);
    assert!(list.contains("not created yet"), "{list}");
    assert!(
        list.contains("profile") && list.contains("generic"),
        "{list}"
    );

    config_ok(&config, &["set", "profile", "scribe"]);
    config_ok(
        &config,
        &["set", "author", "Config Author", "Second Author"],
    );
    config_ok(&config, &["set", "notes", "end"]);
    assert!(config.is_file());
    assert_eq!(config_ok(&config, &["get", "profile"]).trim_end(), "scribe");
    assert_eq!(config_ok(&config, &["get", "svg"]).trim_end(), "rasterize");
    let list = config_ok(&config, &["list"]);
    assert!(list.contains("user config"), "{list}");
    assert!(list.contains("built-in (profile scribe)"), "{list}");

    // --help shows the configured defaults (plain text: stdout is a pipe).
    let help = stdout_of(&run_with_config(&["--help"], &config, None));
    assert!(help.contains("generic [default: scribe]"), "{help}");
    assert!(
        help.contains("rasterize, keep [default: rasterize]"),
        "{help}"
    );
    assert!(
        help.contains("[default: Config Author, Second Author]"),
        "{help}"
    );
    assert!(
        !help.contains('\x1b'),
        "escape codes in piped help:\n{help}"
    );

    // A conversion picks the defaults up; flags still win.
    let out = dir.path().join("u.epub");
    let res = run_with_config(
        &[
            &fixture("page.html"),
            "--offline",
            "--notes",
            "none",
            "-o",
            out.to_str().unwrap(),
        ],
        &config,
        None,
    );
    assert!(res.status.success(), "{}", stderr_of(&res));
    let epub = Epub::open(&out);
    assert_valid(&epub);
    assert_device_safe(&epub, "user config scribe");
    let opf = epub.text("OEBPS/content.opf");
    assert!(opf.contains(">Config Author</dc:creator>"), "{opf}");
    assert!(opf.contains(">Second Author</dc:creator>"), "{opf}");
    assert!(
        !epub.chapter_docs().iter().any(|d| epub.is_note_page(d)),
        "--notes none must beat the user config"
    );

    let msg = config_ok(&config, &["unset", "profile"]);
    assert!(msg.contains("removed profile"), "{msg}");
    assert_eq!(
        config_ok(&config, &["get", "profile"]).trim_end(),
        "generic"
    );
    let msg = config_ok(&config, &["reset"]);
    assert!(msg.contains("removed") && msg.contains("author"), "{msg}");
    assert!(!config.exists());
}

#[test]
fn config_errors_are_clean() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.toml");
    let fails = |args: &[&str], needle: &str| {
        let out = run_with_config(args, &config, None);
        let stderr = stderr_of(&out);
        assert!(!out.status.success(), "{args:?} should fail");
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert!(
            stderr.contains(needle),
            "{args:?}: expected {needle:?} in {stderr}"
        );
    };
    fails(&["config", "set", "colour", "red"], "unknown key");
    fails(&["config", "set", "title", "x"], "single book");
    fails(&["config", "set", "math", "katex"], "math mode");
    fails(&["config", "set", "notes-pages", "lots"], "whole number");
    fails(&["config", "get", "nope"], "unknown key");
    fails(
        &["config", "list", "--profile", "scribe"],
        "takes no input files",
    );
    fails(&[&fixture("page.html"), "config"], "takes no input files");
    assert!(!config.exists());

    // A broken file: conversions fail naming the file and the key, but
    // path, reset and --help still work.
    std::fs::write(&config, "profile = \"scribe\"\nmargin = \"huge\"\n").unwrap();
    let out = dir.path().join("x.epub");
    let convert_args = [
        fixture("page.html"),
        "--offline".into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
    ];
    let convert_args: Vec<&str> = convert_args.iter().map(String::as_str).collect();
    fails(&convert_args, "config.toml");
    fails(&convert_args, "`margin`");
    assert!(!out.exists());
    fails(&["config", "list"], "`margin`");
    let help = run_with_config(&["--help"], &config, None);
    assert!(help.status.success());
    assert!(stdout_of(&help).contains("[default: generic]"));
    assert!(config_ok(&config, &["path"]).contains("config.toml"));
    std::fs::write(&config, "this is [not toml").unwrap();
    fails(&["config", "set", "profile", "scribe"], "not valid TOML");
    let msg = config_ok(&config, &["reset"]);
    assert!(msg.contains("removed"), "{msg}");
    assert!(!config.exists());
}

#[cfg(unix)]
#[test]
fn config_edit_runs_the_editor_and_validates() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("sub").join("config.toml");
    let editor = |name: &str, line: &str| {
        let script = dir.path().join(name);
        std::fs::write(&script, format!("#!/bin/sh\necho '{line}' >> \"$1\"\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    };
    let good = editor("good.sh", "margin = \"wide\"");
    let bad = editor("bad.sh", "notes = \"sometimes\"");
    let edit = |visual: &str| {
        Command::new(env!("CARGO_BIN_EXE_md2epub"))
            .args(["config", "edit"])
            .env("MD2EPUB_CONFIG", &config)
            .env("VISUAL", visual)
            .output()
            .unwrap()
    };
    let out = edit(good.to_str().unwrap());
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("valid"), "{}", stdout_of(&out));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("# md2epub user defaults."), "{text}");
    assert_eq!(config_ok(&config, &["get", "margin"]).trim_end(), "wide");
    let out = edit(bad.to_str().unwrap());
    assert!(!out.status.success());
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("`notes`") && stderr.contains("config.toml"),
        "{stderr}"
    );
    // A failing editor is reported.
    let out = edit("false");
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("editor"), "{}", stderr_of(&out));
}

#[test]
fn output_path_rules() {
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    std::fs::create_dir(d.join("src")).unwrap();
    let md = d.join("src").join("notes.md");
    std::fs::write(&md, "# Notes\n\nText.\n").unwrap();
    let md = md.to_str().unwrap();
    let no_config = d.join("none.toml");
    let ok = |args: &[&str]| {
        let out = run_with_config(args, &no_config, Some(d));
        let stderr = stderr_of(&out);
        assert!(out.status.success(), "{args:?}: {stderr}");
        stderr
    };
    // No extension: .epub is added.
    ok(&[md, "-o", "book"]);
    assert!(d.join("book.epub").is_file());
    // Another extension: .epub is appended, with a note.
    let stderr = ok(&[md, "-o", "out.txt"]);
    assert!(d.join("out.txt.epub").is_file());
    assert!(
        stderr.contains("note:") && stderr.contains("out.txt.epub"),
        "{stderr}"
    );
    // A trailing separator creates the directory.
    ok(&[md, "-o", "made/"]);
    let made = d.join("made").join("notes.epub");
    assert!(made.is_file());
    // An existing directory.
    std::fs::remove_file(&made).unwrap();
    ok(&[md, "-o", "made"]);
    assert!(made.is_file());
    // No -o: the current directory by default ...
    ok(&[md]);
    assert!(d.join("notes.epub").is_file());
    // ... or beside the input.
    ok(&[md, "--output-location", "input"]);
    let beside = d.join("src").join("notes.epub");
    assert!(beside.is_file());
    std::fs::remove_file(&beside).unwrap();
    // ... also when the user config says so.
    let config = d.join("cfg.toml");
    std::fs::write(&config, "output-location = \"input\"\n").unwrap();
    let out = run_with_config(&[md], &config, Some(d));
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_valid(&Epub::open(&beside));
}

#[test]
fn input_named_config_needs_a_path_prefix() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("config"), "# not markdown by name\n").unwrap();
    let no_config = dir.path().join("none.toml");
    // `./config` is an input file (refused for its missing extension), not
    // the subcommand.
    let out = run_with_config(&["./config"], &no_config, Some(dir.path()));
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("unsupported input"),
        "{}",
        stderr_of(&out)
    );
    // `config` alone is the subcommand.
    let out = run_with_config(&["config", "path"], &no_config, Some(dir.path()));
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("none.toml"));
}

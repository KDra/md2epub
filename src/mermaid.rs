//! Mermaid diagrams (owned by T7; best effort).
//!
//! Contract:
//! - [`render`]: Mermaid source to an SVG string. The in-process renderer
//!   (`mermaid-rs-renderer`, behind the `mermaid` cargo feature) is tried
//!   first; when it is not compiled in or fails on a diagram, `mmdc` is run if
//!   it is on `PATH` (temp files in a private temp dir, 30 s timeout).
//! - [`transform`]: replace each `<pre class="mermaid">SOURCE</pre>` with
//!   `<figure class="mermaid"><img data-res="1" ...></figure>` through
//!   `ctx.add_vector`. When nothing can render a diagram, it is left as a
//!   normal code block (`<pre><code class="language-mermaid">`) and a warning
//!   is recorded once per run.
//!
//! Both renderers use a high-contrast grayscale look that survives e-ink
//! screens and Send to Kindle's grayscale conversion.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::ctx::Ctx;
use crate::dom::{self, Handle};

/// Time limit for one diagram, per renderer.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(30);

/// Prefix of the once-per-run warning about diagrams left as code.
const WARN_PREFIX: &str = "Mermaid:";

/// Renders Mermaid source to SVG. See the module docs.
pub fn render(src: &str) -> Result<String> {
    render_with(src, find_mmdc().as_deref(), RENDER_TIMEOUT)
}

/// [`render`] with an explicit `mmdc` executable (or none) and time limit.
fn render_with(src: &str, mmdc: Option<&Path>, timeout: Duration) -> Result<String> {
    let src = normalize_source(src);
    if src.trim().is_empty() {
        bail!("empty Mermaid diagram");
    }
    let mut errors: Vec<String> = Vec::new();

    #[cfg(feature = "mermaid")]
    match builtin::render(&src, timeout).and_then(|svg| finish_svg(&svg)) {
        Ok(svg) => return Ok(svg),
        Err(e) => errors.push(format!("built-in renderer: {e:#}")),
    }
    #[cfg(not(feature = "mermaid"))]
    errors.push("built-in renderer: not compiled in (cargo feature `mermaid`)".into());

    match mmdc {
        Some(exe) => match run_mmdc(exe, &src, timeout).and_then(|svg| finish_svg(&svg)) {
            Ok(svg) => return Ok(svg),
            Err(e) => errors.push(format!("mmdc: {e:#}")),
        },
        None => errors.push("mmdc: not found on PATH".into()),
    }
    Err(anyhow!(errors.join("; ")))
}

/// Replaces Mermaid placeholders in the document. See the module docs.
pub fn transform(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    transform_with(doc, ctx, render)
}

/// [`transform`] with an injectable renderer (for tests).
fn transform_with(
    doc: &Handle,
    ctx: &mut Ctx,
    render: impl Fn(&str) -> Result<String>,
) -> Result<()> {
    let blocks: Vec<Handle> = dom::select_by_tag(doc, "pre")
        .into_iter()
        .filter(|p| dom::has_class(p, "mermaid"))
        .collect();
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;

    for pre in blocks {
        let src = dom::text_content(&pre);
        let result = render(&src).and_then(|svg| {
            // Mermaid output never references external files.
            ctx.add_vector_in(&svg, None).context("storing diagram")
        });
        match result {
            Ok(img) => {
                let (w, h) = (img.width_px.to_string(), img.height_px.to_string());
                let alt = alt_text(&src);
                let figure = dom::create_element("figure", &[("class", "mermaid")]);
                let img_el = dom::create_element(
                    "img",
                    &[
                        ("data-res", "1"),
                        ("src", &img.src),
                        ("alt", &alt),
                        ("width", &w),
                        ("height", &h),
                    ],
                );
                dom::append_child(&figure, &img_el);
                dom::replace_with(&pre, &figure);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                ctx.info(format!("Mermaid diagram left as code: {msg}"));
                demote_to_code(&pre, &src);
                failed += 1;
                first_error.get_or_insert(msg);
            }
        }
    }

    if failed > 0 && !ctx.warnings().iter().any(|w| w.starts_with(WARN_PREFIX)) {
        ctx.warn(format!(
            "{WARN_PREFIX} {failed} diagram(s) could not be rendered and were left as code \
             blocks (further failures are not reported; use -v for details). First error: {}",
            first_error.unwrap_or_default()
        ));
    }
    Ok(())
}

/// Turns a `<pre class="mermaid">` into `<pre><code class="language-mermaid">`.
fn demote_to_code(pre: &Handle, src: &str) {
    dom::remove_attr(pre, "class");
    dom::clear_children(pre);
    let code = dom::create_element("code", &[("class", "language-mermaid")]);
    dom::append_child(&code, &dom::create_text(src));
    dom::append_child(pre, &code);
}

/// Normalizes line endings and strips a leading BOM.
fn normalize_source(src: &str) -> String {
    src.trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Alt text: `accTitle`, else a `title`, else the diagram kind.
fn alt_text(src: &str) -> String {
    let lines: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"))
        .collect();
    for l in &lines {
        if let Some(rest) = l.strip_prefix("accTitle")
            && let Some(t) = rest.trim_start().strip_prefix(':')
            && !t.trim().is_empty()
        {
            return t.trim().to_string();
        }
    }
    let header = lines.first().copied().unwrap_or("");
    let mut header_words = header.split_whitespace();
    let kind_word = header_words
        .next()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("");
    // `pie title Pets` keeps its title on the header line.
    let header_rest: Vec<&str> = header_words.collect();
    if header_rest.first() == Some(&"title") && header_rest.len() > 1 {
        return header_rest[1..].join(" ");
    }
    for l in lines.iter().skip(1) {
        if let Some(t) = l.strip_prefix("title ")
            && !t.trim().is_empty()
        {
            return t.trim().to_string();
        }
    }
    let kind = match kind_word {
        "graph" | "flowchart" | "flowchart-elk" => "Flowchart",
        "sequenceDiagram" => "Sequence diagram",
        "classDiagram" | "classDiagram-v2" => "Class diagram",
        "stateDiagram" | "stateDiagram-v2" => "State diagram",
        "erDiagram" => "Entity relationship diagram",
        "gantt" => "Gantt chart",
        "pie" => "Pie chart",
        "journey" => "User journey diagram",
        "gitGraph" => "Git graph",
        "mindmap" => "Mind map",
        "timeline" => "Timeline",
        "quadrantChart" => "Quadrant chart",
        "xychart" | "xychart-beta" => "XY chart",
        "requirementDiagram" => "Requirement diagram",
        "block-beta" | "block" => "Block diagram",
        "sankey-beta" | "sankey" => "Sankey diagram",
        _ => "Diagram",
    };
    kind.to_string()
}

/// Validates renderer output and gives the root element absolute
/// `width`/`height` (taken from the `viewBox` when missing or relative).
fn finish_svg(svg: &str) -> Result<String> {
    let svg = fix_root_size(svg);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())
        .context("renderer produced invalid SVG")?;
    let size = tree.size();
    if size.width() < 1.0 || size.height() < 1.0 {
        bail!("renderer produced an empty SVG");
    }
    Ok(svg)
}

/// Rewrites the root `<svg>` start tag so it has absolute `width` and
/// `height` (mmdc writes `width="100%"`). Returns the input unchanged when
/// there is nothing to fix or no usable `viewBox`.
fn fix_root_size(svg: &str) -> String {
    let Some(start) = svg.find("<svg") else {
        return svg.to_string();
    };
    let Some(len) = start_tag_len(&svg[start..]) else {
        return svg.to_string();
    };
    let tag = &svg[start..start + len];
    let self_closing = tag.ends_with("/>");
    let inner = tag[4..tag.len() - if self_closing { 2 } else { 1 }].to_string();
    let mut attrs = parse_attrs(&inner);

    let absolute = |name: &str, attrs: &[(String, String)]| {
        attrs.iter().any(|(n, v)| {
            n == name && {
                let v = v.trim();
                !v.is_empty() && !v.ends_with('%') && v != "auto"
            }
        })
    };
    if absolute("width", &attrs) && absolute("height", &attrs) {
        return svg.to_string();
    }
    let Some((vw, vh)) = attrs
        .iter()
        .find(|(n, _)| n == "viewBox")
        .and_then(|(_, v)| {
            let nums: Vec<f32> = v
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            (nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0).then(|| (nums[2], nums[3]))
        })
    else {
        return svg.to_string();
    };
    attrs.retain(|(n, _)| n != "width" && n != "height");
    attrs.push(("width".into(), format!("{vw}")));
    attrs.push(("height".into(), format!("{vh}")));

    let mut out = String::with_capacity(svg.len() + 32);
    out.push_str(&svg[..start]);
    out.push_str("<svg");
    for (n, v) in &attrs {
        let q = if v.contains('"') { '\'' } else { '"' };
        out.push(' ');
        out.push_str(n);
        out.push('=');
        out.push(q);
        out.push_str(v);
        out.push(q);
    }
    out.push_str(if self_closing { "/>" } else { ">" });
    out.push_str(&svg[start + len..]);
    out
}

/// Length of the start tag at the beginning of `s` (through its `>`),
/// skipping `>` inside quoted attribute values.
fn start_tag_len(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// Parses `name="value"` pairs (raw values, entities left as written).
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' {
            i += 1;
        }
        let name: String = chars[name_start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let q = chars[i];
                i += 1;
                let vs = i;
                while i < chars.len() && chars[i] != q {
                    i += 1;
                }
                value = chars[vs..i].iter().collect();
                i += 1;
            } else {
                let vs = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                value = chars[vs..i].iter().collect();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// In-process rendering with `mermaid-rs-renderer`.
#[cfg(feature = "mermaid")]
mod builtin {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::{Result, anyhow, bail};
    use mermaid_rs_renderer::{RenderOptions, Theme, render_with_options};

    /// Stack for the render thread; deeply nested input recurses.
    const STACK_SIZE: usize = 32 * 1024 * 1024;

    /// Renders on a worker thread so a panic becomes an error and a runaway
    /// layout is abandoned after `timeout`.
    pub fn render(src: &str, timeout: Duration) -> Result<String> {
        let (tx, rx) = mpsc::channel();
        let src = src.to_string();
        std::thread::Builder::new()
            .name("mermaid".into())
            .stack_size(STACK_SIZE)
            .spawn(move || {
                let options = RenderOptions {
                    theme: eink_theme(),
                    ..RenderOptions::default()
                };
                let result = std::panic::catch_unwind(move || {
                    render_with_options(&src, options).map_err(|e| e.to_string())
                });
                let _ = tx.send(result);
            })
            .map_err(|e| anyhow!("cannot start render thread: {e}"))?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(Ok(svg))) => Ok(svg),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("renderer crashed on this diagram")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("timed out after {} s", timeout.as_secs())
            }
        }
    }

    /// Black on white with light gray fills: legible on e-ink and unchanged
    /// by grayscale conversion.
    pub fn eink_theme() -> Theme {
        let mut t = Theme::modern();
        let ink = "#000000".to_string();
        let dark = "#333333".to_string();
        let white = "#FFFFFF".to_string();
        t.primary_color = "#F2F2F2".into();
        t.secondary_color = "#E0E0E0".into();
        t.tertiary_color = white.clone();
        t.primary_text_color = ink.clone();
        t.primary_border_color = ink.clone();
        t.line_color = dark.clone();
        t.edge_label_background = white.clone();
        t.cluster_background = "#FAFAFA".into();
        t.cluster_border = "#666666".into();
        t.background = white.clone();
        t.sequence_actor_fill = "#F2F2F2".into();
        t.sequence_actor_border = ink.clone();
        t.sequence_actor_line = dark.clone();
        t.sequence_note_fill = white.clone();
        t.sequence_note_border = dark.clone();
        t.sequence_activation_fill = "#E0E0E0".into();
        t.sequence_activation_border = ink.clone();
        t.text_color = ink.clone();
        t.pie_colors = [
            "#F2F2F2", "#BDBDBD", "#E0E0E0", "#9E9E9E", "#D0D0D0", "#808080", "#FAFAFA", "#B0B0B0",
            "#C8C8C8", "#A8A8A8", "#EAEAEA", "#909090",
        ]
        .map(String::from);
        t.pie_title_text_color = ink.clone();
        t.pie_section_text_color = ink.clone();
        t.pie_legend_text_color = ink;
        t.pie_stroke_color = dark;
        t.pie_outer_stroke_color = "#666666".into();
        t.pie_opacity = 1.0;
        t
    }
}

/// Finds `mmdc` on `PATH` (`mmdc.cmd`/`.exe`/`.bat` on Windows).
fn find_mmdc() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["mmdc.cmd", "mmdc.exe", "mmdc.bat", "mmdc"]
    } else {
        &["mmdc"]
    };
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

/// mmdc configuration: SVG-native labels (resvg cannot draw the default
/// `foreignObject` HTML labels) and the grayscale `neutral` theme.
const MMDC_CONFIG: &str = r#"{"theme":"neutral","htmlLabels":false,"flowchart":{"htmlLabels":false},"class":{"htmlLabels":false},"state":{"htmlLabels":false}}"#;

/// A temp dir removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = base.join(format!(
                "md2epub-mermaid-{}-{}-{nanos}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(TempDir(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("creating temp dir"),
            }
        }
        bail!("cannot create a unique temp dir")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `mmdc` on `src` in a private temp dir and returns the SVG.
fn run_mmdc(exe: &Path, src: &str, timeout: Duration) -> Result<String> {
    let tmp = TempDir::new()?;
    let input = tmp.0.join("diagram.mmd");
    let output = tmp.0.join("diagram.svg");
    let config = tmp.0.join("config.json");
    let log = tmp.0.join("stderr.txt");
    std::fs::write(&input, src).context("writing diagram source")?;
    std::fs::write(&config, MMDC_CONFIG).context("writing mmdc config")?;
    let log_file = std::fs::File::create(&log).context("creating log file")?;

    let mut child = Command::new(exe)
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("-c")
        .arg(&config)
        .args(["-b", "white", "-q"])
        .current_dir(&tmp.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for mmdc")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {} s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    if !status.success() {
        let mut err = String::new();
        if let Ok(mut f) = std::fs::File::open(&log) {
            let _ = f.by_ref().take(64 * 1024).read_to_string(&mut err);
        }
        let first = err
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no error output");
        let first: String = first.chars().take(200).collect();
        bail!("exited with {status}: {first}");
    }
    let svg = std::fs::read_to_string(&output).context("reading mmdc output")?;
    if svg.contains("<foreignObject") {
        bail!("output uses HTML labels that cannot be rasterized");
    }
    Ok(svg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Options, SvgMode};

    const FLOW: &str = "flowchart TD\n    A[Markdown] --> B{Has math?}\n    B -->|yes| C[Render SVG]\n    B -->|no| D[Keep text]\n    C --> E[EPUB]\n    D --> E";
    #[cfg(feature = "mermaid")]
    const SEQ: &str = "sequenceDiagram\n    Alice->>Bob: Hello Bob\n    Bob-->>Alice: Hi Alice";
    const FAKE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60" viewBox="0 0 120 60"><rect width="120" height="60" fill="#fff"/><text x="10" y="30">A</text></svg>"##;

    fn ctx(svg: SvgMode) -> Ctx {
        let mut ctx = Ctx::new(Options::default());
        ctx.opts.svg = svg;
        ctx
    }

    fn mermaid_doc(src: &str) -> Handle {
        dom::parse(&format!(
            r#"<p>before</p><pre class="mermaid">{}</pre><p>after</p>"#,
            dom::escape(src)
        ))
    }

    #[test]
    fn transform_replaces_block_with_figure() {
        for mode in [SvgMode::Keep, SvgMode::Rasterize] {
            let mut ctx = ctx(mode);
            let doc = mermaid_doc("graph TD; A-->B");
            let seen = std::cell::RefCell::new(String::new());
            transform_with(&doc, &mut ctx, |s| {
                *seen.borrow_mut() = s.to_string();
                Ok(FAKE_SVG.to_string())
            })
            .unwrap();
            // The renderer sees the unescaped source.
            assert_eq!(*seen.borrow(), "graph TD; A-->B");
            assert!(dom::select_by_tag(&doc, "pre").is_empty());
            let fig = dom::first_by_tag(&doc, "figure").expect("figure");
            assert!(dom::has_class(&fig, "mermaid"));
            let img = dom::first_by_tag(&fig, "img").expect("img");
            assert_eq!(dom::get_attr(&img, "data-res").as_deref(), Some("1"));
            assert_eq!(dom::get_attr(&img, "alt").as_deref(), Some("Flowchart"));
            assert!(dom::get_attr(&img, "width").is_some());
            assert!(dom::get_attr(&img, "height").is_some());
            let src = dom::get_attr(&img, "src").unwrap();
            assert!(src.starts_with("../images/"));
            assert_eq!(ctx.resources().len(), 1);
            let want = if mode == SvgMode::Keep {
                ".svg"
            } else {
                ".png"
            };
            assert!(src.ends_with(want), "{src}");
            assert!(ctx.warnings().is_empty());
            let body = dom::body(&doc).unwrap();
            let xhtml = dom::serialize_inner_xhtml(&body);
            assert!(xhtml.contains("<p>before</p><figure"), "{xhtml}");
            dom::tests_support::assert_well_formed(&xhtml);
        }
    }

    #[test]
    fn transform_falls_back_to_code_and_warns_once() {
        let mut ctx = ctx(SvgMode::Keep);
        let doc = dom::parse(
            r#"<pre class="mermaid">graph TD; A--&gt;B</pre><pre class="mermaid">x &amp; y</pre><pre>plain</pre>"#,
        );
        transform_with(&doc, &mut ctx, |_| bail!("no renderer")).unwrap();
        let pres = dom::select_by_tag(&doc, "pre");
        assert_eq!(pres.len(), 3);
        for pre in &pres[..2] {
            assert!(!dom::has_class(pre, "mermaid"));
            let code = dom::first_by_tag(pre, "code").expect("code");
            assert!(dom::has_class(&code, "language-mermaid"));
        }
        assert_eq!(dom::text_content(&pres[0]), "graph TD; A-->B");
        assert_eq!(dom::text_content(&pres[1]), "x & y");
        assert!(ctx.resources().is_empty());
        assert_eq!(ctx.warnings().len(), 1);
        assert!(ctx.warnings()[0].contains("2 diagram(s)"));
        assert!(ctx.warnings()[0].contains("no renderer"));

        // A second input file in the same run does not warn again.
        let doc2 = mermaid_doc("graph LR; X-->Y");
        transform_with(&doc2, &mut ctx, |_| bail!("no renderer")).unwrap();
        assert_eq!(ctx.warnings().len(), 1);
        let body = dom::body(&doc).unwrap();
        dom::tests_support::assert_well_formed(&dom::serialize_inner_xhtml(&body));
    }

    #[test]
    fn transform_falls_back_when_svg_cannot_be_stored() {
        // The renderer "succeeds" but returns junk that add_vector rejects
        // (in Keep mode, intrinsic_size; in Rasterize mode, rasterize).
        let mut ctx = ctx(SvgMode::Keep);
        let doc = mermaid_doc("graph TD; A-->B");
        let _ = transform_with(&doc, &mut ctx, |_| Ok("not svg".into()));
        // Whatever the SVG stage decides, the document is never left broken.
        assert_eq!(
            dom::select_by_tag(&doc, "pre").len() + dom::select_by_tag(&doc, "figure").len(),
            1
        );
    }

    #[test]
    fn transform_without_blocks_is_noop() {
        let mut ctx = ctx(SvgMode::Keep);
        let doc = dom::parse("<pre>code</pre><pre class=\"rust\">fn x</pre>");
        transform_with(&doc, &mut ctx, |_| panic!("must not be called")).unwrap();
        assert_eq!(dom::select_by_tag(&doc, "pre").len(), 2);
        assert!(ctx.warnings().is_empty());
    }

    #[test]
    fn garbage_never_panics() {
        let inputs = [
            "",
            "   \n\t",
            "this is not mermaid }}}{{{",
            "flowchart TD\n A[[[[ --> \n -->> (((",
            "sequenceDiagram\n ->> :::",
            "graph",
            "\u{feff}graph TD;;;;;;",
            "%%{init: {broken}}%%\ngraph TD; A-->B",
            "\0\0\0",
            "classDiagram\n class {{{{",
            "pie\n \"a\" : -1e999",
            "gantt\n dateFormat nonsense\n section\n :",
        ];
        for src in inputs {
            let _ = render_with(src, None, RENDER_TIMEOUT);
            let mut ctx = ctx(SvgMode::Keep);
            let doc = mermaid_doc(src);
            transform_with(&doc, &mut ctx, |s| render_with(s, None, RENDER_TIMEOUT)).unwrap();
            let body = dom::body(&doc).unwrap();
            dom::tests_support::assert_well_formed(&dom::serialize_inner_xhtml(&body));
        }
        let long = "graph TD;".to_string() + &"A-->".repeat(2000);
        let _ = render_with(&long, None, RENDER_TIMEOUT);
        let nested = "flowchart TD\n".to_string()
            + &"subgraph s\n".repeat(200)
            + "A-->B\n"
            + &"end\n".repeat(200);
        let _ = render_with(&nested, None, RENDER_TIMEOUT);
    }

    #[test]
    fn empty_source_is_an_error() {
        assert!(render_with("  \n", None, RENDER_TIMEOUT).is_err());
    }

    #[cfg(feature = "mermaid")]
    #[test]
    fn builtin_renders_flowchart_and_sequence() {
        for src in [FLOW, SEQ, "graph LR; A[a & b < c]-->B"] {
            let svg = render_with(src, None, RENDER_TIMEOUT).unwrap();
            assert!(svg.contains("<svg"));
            let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).unwrap();
            assert!(tree.size().width() > 20.0 && tree.size().height() > 20.0);
        }
        let svg = render_with(FLOW, None, RENDER_TIMEOUT).unwrap();
        assert!(svg.contains("Render SVG"));
        let svg = render_with(SEQ, None, RENDER_TIMEOUT).unwrap();
        assert!(svg.contains("Alice") && svg.contains("Hello Bob"));
        // The e-ink theme is applied (black text, no colored fills).
        assert!(svg.contains("#000000"));
        assert!(!svg.contains("#FFF7ED"));
    }

    #[cfg(feature = "mermaid")]
    #[test]
    fn transform_end_to_end_with_builtin() {
        for mode in [SvgMode::Keep, SvgMode::Rasterize] {
            let mut ctx = ctx(mode);
            let doc = mermaid_doc(FLOW);
            transform(&doc, &mut ctx).unwrap();
            assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
            assert!(dom::select_by_tag(&doc, "pre").is_empty());
            let img = dom::first_by_tag(&doc, "img").unwrap();
            assert_eq!(dom::get_attr(&img, "data-res").as_deref(), Some("1"));
            assert_eq!(ctx.resources().len(), 1);
        }
    }

    #[cfg(feature = "mermaid")]
    #[test]
    fn builtin_failure_reports_both_renderers() {
        let err = render_with("not a diagram", None, RENDER_TIMEOUT).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("built-in renderer"), "{msg}");
        assert!(msg.contains("mmdc: not found"), "{msg}");
    }

    #[cfg(not(feature = "mermaid"))]
    #[test]
    fn without_feature_needs_mmdc() {
        let msg = format!("{:#}", render_with(FLOW, None, RENDER_TIMEOUT).unwrap_err());
        assert!(msg.contains("not compiled in"), "{msg}");
    }

    #[test]
    fn alt_text_sources() {
        assert_eq!(alt_text("graph TD; A-->B"), "Flowchart");
        assert_eq!(
            alt_text("%% c\nsequenceDiagram\n A->>B: x"),
            "Sequence diagram"
        );
        assert_eq!(
            alt_text("pie title Pets adopted\n \"Dogs\" : 3"),
            "Pets adopted"
        );
        assert_eq!(alt_text("gantt\n title Plan\n dateFormat X"), "Plan");
        assert_eq!(
            alt_text("flowchart LR\n accTitle: Build steps\n title Other\n A-->B"),
            "Build steps"
        );
        assert_eq!(alt_text("whatever"), "Diagram");
        assert_eq!(alt_text(""), "Diagram");
    }

    #[test]
    fn root_size_fix() {
        let svg = r#"<?xml version="1.0"?><svg id="m" width="100%" xmlns="http://www.w3.org/2000/svg" style="max-width: 200px;" viewBox="0 0 200 80" role="graphics-document document"><g/></svg>"#;
        let fixed = fix_root_size(svg);
        assert!(
            fixed.starts_with(r#"<?xml version="1.0"?><svg id="m""#),
            "{fixed}"
        );
        assert!(fixed.contains(r#"width="200""#) && fixed.contains(r#"height="80""#));
        assert!(!fixed.contains("100%"));
        assert!(fixed.contains(r#"role="graphics-document document""#));
        assert!(fixed.ends_with("<g/></svg>"));
        let tree = usvg::Tree::from_str(&fixed, &usvg::Options::default()).unwrap();
        assert_eq!(tree.size().width(), 200.0);

        // Already absolute, or nothing to go on: unchanged.
        assert_eq!(fix_root_size(FAKE_SVG), FAKE_SVG);
        let no_vb = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%"/>"#;
        assert_eq!(fix_root_size(no_vb), no_vb);
        assert_eq!(fix_root_size("garbage <svg"), "garbage <svg");
        assert_eq!(fix_root_size("x"), "x");
    }

    #[test]
    fn finish_svg_rejects_junk() {
        assert!(finish_svg("").is_err());
        assert!(finish_svg("<svg").is_err());
        assert!(finish_svg("<html/>").is_err());
        assert!(finish_svg(FAKE_SVG).is_ok());
    }

    /// Writes an executable shell script acting as a fake `mmdc`.
    #[cfg(unix)]
    fn fake_mmdc(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("mmdc");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn mmdc_fallback_success() {
        let dir = tempfile::tempdir().unwrap();
        // Find the value after -o and write a percentage-sized SVG there.
        let script = r#"out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
printf '<svg xmlns="http://www.w3.org/2000/svg" width="100%%" viewBox="0 0 300 150"><rect width="300" height="150"/></svg>' > "$out""#;
        let exe = fake_mmdc(dir.path(), script);
        // Garbage makes the built-in renderer (if compiled in) fail first.
        let svg = run_mmdc(&exe, "anything", RENDER_TIMEOUT).and_then(|s| finish_svg(&s));
        let svg = svg.unwrap();
        assert!(svg.contains(r#"width="300""#), "{svg}");
        let svg = render_with("not a diagram", Some(&exe), RENDER_TIMEOUT).unwrap();
        assert!(svg.contains(r#"height="150""#));
    }

    #[cfg(unix)]
    #[test]
    fn mmdc_fallback_errors() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_mmdc(
            dir.path(),
            "echo 'Error: Could not find chrome' >&2; exit 2",
        );
        let msg = format!(
            "{:#}",
            run_mmdc(&exe, "graph TD", RENDER_TIMEOUT).unwrap_err()
        );
        assert!(msg.contains("Could not find chrome"), "{msg}");

        let msg = format!(
            "{:#}",
            render_with("not a diagram", Some(&exe), RENDER_TIMEOUT).unwrap_err()
        );
        assert!(msg.contains("mmdc: exited"), "{msg}");

        // Exits 0 but writes nothing.
        let exe = fake_mmdc(dir.path(), "exit 0");
        assert!(run_mmdc(&exe, "graph TD", RENDER_TIMEOUT).is_err());

        // Hangs: killed at the timeout.
        let exe = fake_mmdc(dir.path(), "exec sleep 30");
        let start = Instant::now();
        let msg = format!(
            "{:#}",
            run_mmdc(&exe, "graph TD", Duration::from_millis(300)).unwrap_err()
        );
        assert!(msg.contains("timed out"), "{msg}");
        assert!(start.elapsed() < Duration::from_secs(10));

        // Missing executable.
        assert!(run_mmdc(&dir.path().join("nope"), "graph TD", RENDER_TIMEOUT).is_err());
    }

    #[test]
    fn temp_dir_is_removed() {
        let t = TempDir::new().unwrap();
        let p = t.0.clone();
        assert!(p.is_dir());
        drop(t);
        assert!(!p.exists());
    }
}

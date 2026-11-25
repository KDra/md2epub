//! Image and asset resolution (owned by T4).
//!
//! Contract:
//! - [`transform`]: for every `<img>` without `data-res`, load the source
//!   (path relative to `ctx.base_dir`, absolute path, `file://`, `data:` URI,
//!   or `http(s)` via `ureq` + rustls, skipped with a warning under
//!   `--offline`; 20 s timeout; 50 MB cap). On failure keep the alt text and
//!   warn. Raster images: convert non PNG/JPEG to PNG, downscale to the
//!   profile screen, flatten alpha when `profile.flatten_alpha`, grayscale
//!   when `opts.grayscale`, never re-encode compliant files; store with
//!   `ctx.add_image`. SVG files and inline `<svg>` go through
//!   `ctx.add_vector_in` (inline `<svg>` becomes an `<img>` with its
//!   `<title>`/`aria-label` as alt). Keep `width`/`height` sensible, add
//!   `alt=""` where missing, remove `data-res`, resolve `url(...)` in `style`
//!   attributes and `<style>` blocks.
//! - [`load_cover`]: same processing for the cover image, returning the
//!   resource (href `images/cover.<ext>`) without adding it to `ctx`.
//!
//! Details worth knowing:
//! - Image references inside SVGs (`<image href>`/`xlink:href`) are resolved
//!   against the SVG's own location (file directory or URL). When SVGs are
//!   rasterized they are inlined as PNG/JPEG data URIs for the rasterizer;
//!   when SVGs are kept they are stored as separate book resources next to
//!   the SVG (`images/`), so no base64 image ends up inside a kept SVG.
//! - Fonts referenced from CSS `url(...)` are stored under `fonts/`.
//! - A `url(...)` that cannot be resolved is replaced by `none` (with a
//!   warning), so the book never references a missing file; in an
//!   `@font-face` `src` descriptor the whole declaration is removed instead.
//!   `url(...)` inside comments and strings is left alone, and `@import`
//!   rules are removed with a warning (add such styles with `--css`).
//! - The `--css` stylesheet goes through the same `url(...)` resolution
//!   ([`rewrite_user_css`]), relative to its own directory.
//! - Kept SVGs get the same treatment for `url(...)` in their `<style>`
//!   elements and `style` attributes; external `<use>` references are
//!   removed with a warning.
//! - `<picture>` is reduced to its `<img>`. When `src` is missing or cannot be
//!   loaded, the `srcset` candidates (largest first) and those of the
//!   picture's `<source>` elements (kept by `html::sanitize`) are tried.
//! - EXIF orientation of JPEGs is applied (the image is re-encoded), and CMYK
//!   (detected from the JPEG header) or 16-bit images are converted to 8-bit
//!   RGB/gray. Images over [`MAX_DECODE_PIXELS`] are refused before decoding.

use std::io::{Cursor, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{
    STANDARD, STANDARD_PAD_INDIFFERENT, URL_SAFE_PAD_INDIFFERENT,
};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{
    ColorType, DynamicImage, ExtendedColorType, GrayImage, ImageDecoder, ImageFormat, ImageReader,
    RgbImage,
};
use percent_encoding::percent_decode_str;

use crate::ctx::{Ctx, ImgRef, SVG_RASTER_SCALE, content_hash, fit_within};
use crate::dom::{self, Handle};
use crate::html::{SOURCES_ATTR, SRCSET_ATTR};
use crate::model::{OutputFormat, Resource, SvgMode, media_type_for_ext};
use crate::svg;

/// Timeout for a whole remote download (connect, headers and body).
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest remote image that is downloaded.
pub const MAX_DOWNLOAD_BYTES: u64 = 50 * 1024 * 1024;
/// Largest local image file that is read.
const MAX_LOCAL_BYTES: u64 = 256 * 1024 * 1024;
/// Largest decompressed `.svgz`.
const MAX_SVGZ_BYTES: u64 = 64 * 1024 * 1024;
/// How deep SVGs referencing SVGs are followed.
const MAX_SVG_NESTING: u32 = 4;
/// Quality used when a JPEG has to be re-encoded.
const JPEG_QUALITY: u8 = 90;
/// Largest image (in pixels) that is decoded. A few KB of PNG can claim
/// gigapixels; checking the header first keeps memory bounded.
pub const MAX_DECODE_PIXELS: u64 = 100_000_000;
/// Largest single allocation the image decoders may make.
const MAX_DECODE_ALLOC: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Resolves and stores every image in the document. See the module docs.
///
/// Never fails on bad input: every problem becomes a warning in `ctx`.
pub fn transform(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    let base = Base::Dir(ctx.base_dir.clone());

    // Inline SVG first, so `<img>`/`<style>` inside them are handled as part
    // of the SVG and not rewritten to book paths the rasterizer cannot read.
    for svg_el in dom::select_by_tag(doc, "svg") {
        if dom::parent(&svg_el).is_none() || has_svg_ancestor(&svg_el) {
            continue;
        }
        replace_inline_svg(&svg_el, &base, ctx);
    }

    // `<picture>`: keep only the `<img>` fallback.
    for picture in dom::select_by_tag(doc, "picture") {
        for child in dom::children(&picture) {
            if dom::is_tag(&child, "source") {
                dom::remove(&child);
            }
        }
        dom::unwrap(&picture);
    }

    for img in dom::select_by_tag(doc, "img") {
        process_img(&img, &base, ctx);
    }

    for el in dom::select_by_tag(doc, "*") {
        if dom::is_tag(&el, "style") {
            let css = dom::text_content(&el);
            if contains_url(&css) {
                let new = rewrite_css(&css, &base, ctx);
                if new != css {
                    dom::set_text(&el, &new);
                }
            }
        }
        if let Some(style) = dom::get_attr(&el, "style")
            && contains_url(&style)
        {
            let new = rewrite_css(&style, &base, ctx);
            if new != style {
                dom::set_attr(&el, "style", &new);
            }
        }
    }
    Ok(())
}

/// Loads and processes the cover image. See the module docs.
///
/// `path` may also be a URL (`http(s)://`, `file://`, `data:`). Raster covers
/// get the same conversion as content images (and are always flattened onto
/// white); SVG covers are rasterized to fill the profile screen.
pub fn load_cover(path: &Path, ctx: &mut Ctx) -> Result<Resource> {
    let loaded = if path.is_file() {
        read_local(path)
    } else {
        load(&path.to_string_lossy(), &Base::Dir(PathBuf::from(".")), ctx)
    }
    .with_context(|| format!("loading cover image {}", path.display()))?;

    let (max_w, max_h) = (ctx.opts.profile.screen_w, ctx.opts.profile.screen_h);
    let opts = RasterOpts {
        max_w,
        max_h,
        flatten: true,
        grayscale: ctx.opts.grayscale,
    };
    let out = match sniff(&loaded) {
        Kind::Raster => process_raster(loaded.bytes, opts)?,
        Kind::Svg => {
            let text = svg_text(&loaded.bytes)?;
            let prepared = prepare_svg(&text, &loaded.base, SvgMode::Rasterize, ctx, 0)?;
            let (iw, ih) = svg::intrinsic_size(prepared.as_bytes()).unwrap_or((0.0, 0.0));
            let scale = if iw > 0.0 && ih > 0.0 {
                (max_w as f32 / iw).min(max_h as f32 / ih)
            } else {
                SVG_RASTER_SCALE
            };
            let scale = if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                SVG_RASTER_SCALE
            };
            let dir = loaded.base.dir();
            let raster = svg::rasterize(prepared.as_bytes(), dir, max_w, max_h, scale)
                .context("rasterizing SVG cover")?;
            process_raster(raster.png, opts)?
        }
        Kind::Font(_) | Kind::Unknown => {
            bail!("unsupported cover format: {}", path.display())
        }
    };
    Ok(Resource {
        href: format!("images/cover.{}", out.ext),
        media_type: media_type_for_ext(out.ext).to_string(),
        data: out.bytes,
    })
}

// ---------------------------------------------------------------------------
// <img> and inline <svg>
// ---------------------------------------------------------------------------

/// Where relative references resolve.
#[derive(Debug, Clone)]
enum Base {
    /// A local directory.
    Dir(PathBuf),
    /// A remote document URL.
    Url(String),
}

impl Base {
    fn dir(&self) -> Option<&Path> {
        match self {
            Base::Dir(d) => Some(d),
            Base::Url(_) => None,
        }
    }
}

/// Bytes of a loaded source.
struct Loaded {
    bytes: Vec<u8>,
    /// MIME type from a `data:` URI or the HTTP `Content-Type`.
    mime: Option<String>,
    /// File path or URL, used for the extension and messages.
    name: String,
    /// Where the source's own relative references resolve.
    base: Base,
}

/// What a stored image looks like on the page.
struct Stored {
    src: String,
    width: u32,
    height: u32,
    vector: bool,
}

fn process_img(img: &Handle, base: &Base, ctx: &mut Ctx) {
    let own_srcset = dom::get_attr(img, SRCSET_ATTR)
        .or_else(|| dom::get_attr(img, "srcset"))
        .unwrap_or_default();
    let sources = dom::get_attr(img, SOURCES_ATTR).unwrap_or_default();
    for a in ["srcset", "sizes", SRCSET_ATTR, SOURCES_ATTR] {
        dom::remove_attr(img, a);
    }
    if dom::has_attr(img, "data-res") {
        dom::remove_attr(img, "data-res");
        ensure_alt(img);
        return;
    }
    let alt = dom::get_attr(img, "alt").unwrap_or_default();
    let mut candidates: Vec<String> = ["src", "data-src", "data-original"]
        .iter()
        .filter_map(|a| dom::get_attr(img, a))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(1)
        .collect();
    candidates.extend(srcset_candidates(&own_srcset));
    for line in sources.lines() {
        candidates.extend(srcset_candidates(line));
    }
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|c| seen.insert(c.clone()));
    if candidates.is_empty() {
        ctx.warn(format!("image without a source dropped (alt: {alt:?})"));
        fallback(img, &alt);
        return;
    }
    let mut first_error = None;
    for src in &candidates {
        match load(src, base, ctx).and_then(|l| store(l, ctx)) {
            Ok(stored) => {
                dom::set_attr(img, "src", &stored.src);
                fix_dimensions(img, stored.width, stored.height, stored.vector, ctx);
                ensure_alt(img);
                return;
            }
            Err(e) => {
                first_error.get_or_insert((src.clone(), e));
            }
        }
    }
    // Confluence can show a remote image by its URL (`<ri:url>`), so one
    // that is not attached (offline, or the download failed) stays linked.
    if ctx.opts.format == OutputFormat::Confluence
        && let Some(url) = candidates.iter().find(|c| {
            let l = c.to_ascii_lowercase();
            l.starts_with("https://") || l.starts_with("http://")
        })
    {
        if ctx.opts.offline {
            ctx.info(format!(
                "remote image linked by URL (offline): {}",
                short(url)
            ));
        } else if let Some((src, e)) = &first_error {
            ctx.warn(format!(
                "image not attached, linked by URL instead: {} ({e:#})",
                short(src)
            ));
        }
        dom::set_attr(img, "src", url);
        dom::set_attr(img, REMOTE_ATTR, "1");
        ensure_alt(img);
        return;
    }
    if let Some((src, e)) = first_error {
        ctx.warn(format!("image not embedded: {} ({e:#})", short(&src)));
    }
    fallback(img, &alt);
}

/// Marks an `<img>` whose `src` is a remote URL that was deliberately not
/// downloaded (Confluence output only).
pub const REMOTE_ATTR: &str = "data-md2epub-remote";

/// The URLs of a `srcset` value, largest descriptor (`w` or `x`) first.
fn srcset_candidates(srcset: &str) -> Vec<String> {
    let mut found: Vec<(String, f32)> = Vec::new();
    let b = srcset.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut url = &srcset[start..i];
        let mut descriptor = String::new();
        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            let mut paren = 0usize;
            while i < b.len() {
                match b[i] {
                    b'(' => paren += 1,
                    b')' => paren = paren.saturating_sub(1),
                    b',' if paren == 0 => break,
                    _ => {}
                }
                descriptor.push(b[i] as char);
                i += 1;
            }
        }
        let value = descriptor
            .split_ascii_whitespace()
            .find_map(|d| {
                d.strip_suffix(['w', 'x'])
                    .and_then(|n| n.parse::<f32>().ok())
            })
            .filter(|v| v.is_finite())
            .unwrap_or(1.0);
        if !url.is_empty() {
            found.push((url.to_string(), value));
        }
    }
    found.sort_by(|a, b| b.1.total_cmp(&a.1));
    found.into_iter().map(|(u, _)| u).collect()
}

fn replace_inline_svg(svg_el: &Handle, base: &Base, ctx: &mut Ctx) {
    let alt = svg_alt(svg_el);
    let markup = dom::serialize_xhtml(svg_el);
    let mode = ctx.opts.svg;
    let result = prepare_svg(&markup, base, mode, ctx, 0).and_then(|s| add_vector(ctx, &s, base));
    match result {
        Ok(r) => {
            let (w, h) = (r.width_px.to_string(), r.height_px.to_string());
            let img = dom::create_element(
                "img",
                &[
                    ("src", &r.src),
                    ("alt", &alt),
                    ("width", &w),
                    ("height", &h),
                    ("data-res", "1"),
                ],
            );
            for name in ["id", "class", "style"] {
                if let Some(v) = dom::get_attr(svg_el, name) {
                    dom::set_attr(&img, name, &v);
                }
            }
            dom::replace_with(svg_el, &img);
        }
        Err(e) => {
            let label = if alt.is_empty() { "untitled" } else { &alt };
            ctx.warn(format!("inline SVG ({label}) not converted: {e:#}"));
            fallback(svg_el, &alt);
        }
    }
}

/// Alt text for an inline SVG: `aria-label`, else its `<title>`.
fn svg_alt(svg_el: &Handle) -> String {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(label) = dom::get_attr(svg_el, "aria-label") {
        let label = collapse(&label);
        if !label.is_empty() {
            return label;
        }
    }
    let title = dom::children(svg_el)
        .into_iter()
        .find(|c| dom::is_tag(c, "title"))
        .or_else(|| dom::first_by_tag(svg_el, "title"));
    title
        .map(|t| collapse(&dom::text_content(&t)))
        .unwrap_or_default()
}

fn has_svg_ancestor(node: &Handle) -> bool {
    let mut cur = dom::parent(node);
    while let Some(p) = cur {
        if dom::is_tag(&p, "svg") {
            return true;
        }
        cur = dom::parent(&p);
    }
    false
}

fn ensure_alt(img: &Handle) {
    if !dom::has_attr(img, "alt") {
        dom::set_attr(img, "alt", "");
    }
}

/// Replaces an unresolvable image by its alt text (or removes it).
fn fallback(node: &Handle, alt: &str) {
    let alt = alt.trim();
    if alt.is_empty() {
        dom::remove(node);
    } else {
        let span = dom::create_element("span", &[("class", "img-missing")]);
        dom::append_child(&span, &dom::create_text(alt));
        dom::replace_with(node, &span);
    }
}

/// Shortens a source for messages (data URIs can be huge).
fn short(src: &str) -> String {
    const MAX: usize = 80;
    if src.chars().count() <= MAX {
        src.to_string()
    } else {
        let s: String = src.chars().take(MAX).collect();
        format!("{s}...")
    }
}

/// A parsed `width`/`height` attribute.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Dim {
    Px(f32),
    Percent(f32),
    Invalid,
}

fn parse_dim(s: &str) -> Dim {
    let t = s.trim().to_ascii_lowercase();
    if let Some(p) = t.strip_suffix('%') {
        return match p.trim().parse::<f32>() {
            Ok(v) if v.is_finite() && v > 0.0 => Dim::Percent(v.min(100.0)),
            _ => Dim::Invalid,
        };
    }
    let n = t.strip_suffix("px").unwrap_or(&t).trim();
    match n.parse::<f32>() {
        Ok(v) if v.is_finite() && v >= 1.0 => Dim::Px(v),
        _ => Dim::Invalid,
    }
}

/// Normalizes `width`/`height` to positive integers (EPUB XHTML requires
/// that), moves a percentage width into `style`, fills in a missing
/// dimension for vector images (whose stored PNG has more pixels than the
/// display size) and keeps everything within the profile screen.
fn fix_dimensions(img: &Handle, natural_w: u32, natural_h: u32, fill: bool, ctx: &Ctx) {
    let mut w = None;
    let mut h = None;
    let mut percent = false;
    if let Some(v) = dom::get_attr(img, "width") {
        match parse_dim(&v) {
            Dim::Px(p) => w = Some(p),
            Dim::Percent(p) => {
                percent = true;
                let style = dom::get_attr(img, "style").unwrap_or_default();
                let style = style.trim().trim_end_matches(';').trim();
                let sep = if style.is_empty() { "" } else { "; " };
                dom::set_attr(img, "style", &format!("{style}{sep}width: {p}%"));
            }
            Dim::Invalid => {}
        }
    }
    if let Some(v) = dom::get_attr(img, "height")
        && let Dim::Px(p) = parse_dim(&v)
    {
        h = Some(p);
    }
    if percent {
        // A fixed height would distort a percentage width.
        h = None;
    }

    let (nw, nh) = (natural_w.max(1) as f32, natural_h.max(1) as f32);
    if fill && !percent {
        match (w, h) {
            (None, None) => {
                w = Some(nw);
                h = Some(nh);
            }
            (Some(a), None) => h = Some(a * nh / nw),
            (None, Some(b)) => w = Some(b * nw / nh),
            _ => {}
        }
    }

    let (max_w, max_h) = (
        ctx.opts.profile.screen_w as f32,
        ctx.opts.profile.screen_h as f32,
    );
    let (w, h) = match (w, h) {
        (Some(a), Some(b)) => {
            let (a, b) = fit_within(a, b, max_w, max_h);
            (Some(a), Some(b))
        }
        (Some(a), None) => (Some((a.min(max_w).round() as u32).max(1)), None),
        (None, Some(b)) => (None, Some((b.min(max_h).round() as u32).max(1))),
        (None, None) => (None, None),
    };
    match w {
        Some(v) => dom::set_attr(img, "width", &v.to_string()),
        None => dom::remove_attr(img, "width"),
    }
    match h {
        Some(v) => dom::set_attr(img, "height", &v.to_string()),
        None => dom::remove_attr(img, "height"),
    }
}

// ---------------------------------------------------------------------------
// Storing
// ---------------------------------------------------------------------------

/// What a loaded source contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Raster,
    Svg,
    /// A font, with its canonical extension.
    Font(&'static str),
    Unknown,
}

fn extension_of(name: &str) -> String {
    let path = name.split(['?', '#']).next().unwrap_or(name);
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match file.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => String::new(),
    }
}

fn sniff(l: &Loaded) -> Kind {
    let bytes = &l.bytes;
    if image::guess_format(bytes).is_ok() {
        return Kind::Raster;
    }
    let ext = extension_of(&l.name);
    let mime = l.mime.as_deref().unwrap_or("");
    let svg_hint = ext == "svg" || ext == "svgz" || mime.contains("svg");
    if is_gzip(bytes) && svg_hint {
        return Kind::Svg;
    }
    if looks_like_svg(bytes) {
        return Kind::Svg;
    }
    if let Some(f) = font_kind(bytes, &ext, mime) {
        return Kind::Font(f);
    }
    if svg_hint {
        return Kind::Svg;
    }
    Kind::Unknown
}

fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    let head = String::from_utf8_lossy(head);
    let t = head.trim_start_matches('\u{feff}').trim_start();
    t.starts_with('<') && t.contains("<svg")
}

fn font_kind(bytes: &[u8], ext: &str, mime: &str) -> Option<&'static str> {
    if bytes.starts_with(b"wOF2") {
        return Some("woff2");
    }
    if bytes.starts_with(b"wOFF") {
        return Some("woff");
    }
    if bytes.starts_with(b"OTTO") {
        return Some("otf");
    }
    if bytes.starts_with(&[0, 1, 0, 0]) || bytes.starts_with(b"true") {
        return Some("ttf");
    }
    match ext {
        "woff2" => Some("woff2"),
        "woff" => Some("woff"),
        "otf" => Some("otf"),
        "ttf" => Some("ttf"),
        _ if mime.starts_with("font/") => mime.strip_prefix("font/").and_then(|m| {
            ["woff2", "woff", "otf", "ttf"]
                .into_iter()
                .find(|f| *f == m)
        }),
        _ => None,
    }
}

/// Decodes SVG bytes (plain or gzip-compressed) to text.
fn svg_text(bytes: &[u8]) -> Result<String> {
    let bytes = if is_gzip(bytes) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .take(MAX_SVGZ_BYTES + 1)
            .read_to_end(&mut out)
            .context("decompressing SVGZ")?;
        if out.len() as u64 > MAX_SVGZ_BYTES {
            bail!("decompressed SVG is too large");
        }
        out
    } else {
        bytes.to_vec()
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

fn add_vector(ctx: &mut Ctx, svg_src: &str, base: &Base) -> Result<ImgRef> {
    ctx.add_vector_in(svg_src, base.dir())
}

fn store(l: Loaded, ctx: &mut Ctx) -> Result<Stored> {
    match sniff(&l) {
        Kind::Raster => {
            let out = process_raster(l.bytes, RasterOpts::for_ctx(ctx))?;
            let (width, height) = (out.width, out.height);
            let src = ctx.add_image(out.bytes, out.ext);
            Ok(Stored {
                src,
                width,
                height,
                vector: false,
            })
        }
        Kind::Svg => {
            let text = svg_text(&l.bytes)?;
            let mode = ctx.opts.svg;
            let prepared = prepare_svg(&text, &l.base, mode, ctx, 0)?;
            let r = add_vector(ctx, &prepared, &l.base)?;
            Ok(Stored {
                src: r.src,
                width: r.width_px,
                height: r.height_px,
                vector: true,
            })
        }
        Kind::Font(_) | Kind::Unknown => bail!("not a supported image format"),
    }
}

// ---------------------------------------------------------------------------
// Raster processing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct RasterOpts {
    max_w: u32,
    max_h: u32,
    flatten: bool,
    grayscale: bool,
}

impl RasterOpts {
    fn for_ctx(ctx: &Ctx) -> Self {
        RasterOpts {
            max_w: ctx.opts.profile.screen_w,
            max_h: ctx.opts.profile.screen_h,
            flatten: ctx.opts.profile.flatten_alpha,
            grayscale: ctx.opts.grayscale,
        }
    }
}

/// A processed raster image, PNG or JPEG.
#[derive(Debug)]
struct RasterOut {
    bytes: Vec<u8>,
    ext: &'static str,
    width: u32,
    height: u32,
}

/// Decodes an image and makes it EPUB-friendly: PNG or JPEG, within
/// `max_w × max_h`, upright, 8-bit, flattened/grayscale as requested.
/// Compliant PNG/JPEG files are returned byte for byte.
fn process_raster(bytes: Vec<u8>, o: RasterOpts) -> Result<RasterOut> {
    let format = image::guess_format(&bytes).context("unrecognized image data")?;
    let (img, orientation, color, original) = {
        let reader = ImageReader::with_format(Cursor::new(bytes.as_slice()), format);
        let mut decoder = reader
            .into_decoder()
            .with_context(|| format!("decoding {format:?} image"))?;
        let (dw, dh) = decoder.dimensions();
        if u64::from(dw) * u64::from(dh) > MAX_DECODE_PIXELS {
            bail!(
                "image is too large to embed ({dw}×{dh} pixels; the limit is {} megapixels)",
                MAX_DECODE_PIXELS / 1_000_000
            );
        }
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(MAX_DECODE_ALLOC);
        decoder
            .set_limits(limits)
            .with_context(|| format!("decoding {format:?} image"))?;
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let color = decoder.color_type();
        let original = decoder.original_color_type();
        let img = DynamicImage::from_decoder(decoder)
            .with_context(|| format!("decoding {format:?} image"))?;
        (img, orientation, color, original)
    };
    let mut img = img;
    img.apply_orientation(orientation);
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        bail!("image has no pixels");
    }
    let (tw, th) = fit_within(w as f32, h as f32, o.max_w as f32, o.max_h as f32);

    let native = matches!(format, ImageFormat::Png | ImageFormat::Jpeg);
    let has_alpha = color.has_alpha();
    let is_gray = matches!(
        color,
        ColorType::L8 | ColorType::La8 | ColorType::L16 | ColorType::La16
    );
    let eight_bit = matches!(
        color,
        ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8
    );
    let cmyk = matches!(
        original,
        ExtendedColorType::Cmyk8 | ExtendedColorType::Cmyk16
    ) || (format == ImageFormat::Jpeg && jpeg_is_cmyk(&bytes));
    let resize = (tw, th) != (w, h);
    let flatten = o.flatten && has_alpha;
    let to_gray = o.grayscale && !is_gray;
    let upright = orientation == Orientation::NoTransforms;

    if native && eight_bit && !cmyk && upright && !resize && !flatten && !to_gray {
        let ext = if format == ImageFormat::Png {
            "png"
        } else {
            "jpg"
        };
        return Ok(RasterOut {
            bytes,
            ext,
            width: w,
            height: h,
        });
    }
    drop(bytes);

    if resize {
        img = img.resize_exact(tw, th, FilterType::Lanczos3);
    }
    let gray_out = is_gray || o.grayscale;
    let keep_alpha = has_alpha && !o.flatten;
    let img = match (gray_out, keep_alpha, has_alpha) {
        (true, true, _) => DynamicImage::ImageLumaA8(img.to_luma_alpha8()),
        (false, true, _) => DynamicImage::ImageRgba8(img.to_rgba8()),
        (true, false, true) => DynamicImage::ImageLuma8(flatten_gray(&img)),
        (false, false, true) => DynamicImage::ImageRgb8(flatten_rgb(&img)),
        (true, false, false) => DynamicImage::ImageLuma8(img.to_luma8()),
        (false, false, false) => DynamicImage::ImageRgb8(img.to_rgb8()),
    };
    let jpeg = format == ImageFormat::Jpeg && !keep_alpha;
    let mut out = Vec::new();
    if jpeg {
        img.write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))
            .context("encoding JPEG")?;
    } else {
        img.write_with_encoder(PngEncoder::new(&mut out))
            .context("encoding PNG")?;
    }
    Ok(RasterOut {
        bytes: out,
        ext: if jpeg { "jpg" } else { "png" },
        width: img.width(),
        height: img.height(),
    })
}

/// Whether a JPEG stores four color components (CMYK or YCCK), read from
/// its start-of-frame header. Decoders convert such images to RGB, but the
/// original bytes must not be kept: many readers show CMYK JPEGs inverted
/// or not at all.
fn jpeg_is_cmyk(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return false;
    }
    let mut i = 2;
    while i + 3 < bytes.len() {
        if bytes[i] != 0xFF {
            return false;
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            // Fill byte.
            i += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return false;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            return bytes.get(i + 9) == Some(&4);
        }
        if len < 2 {
            return false;
        }
        i += 2 + len;
    }
    false
}

fn blend(c: u8, a: u8) -> u8 {
    let (c, a) = (c as u32, a as u32);
    ((c * a + 255 * (255 - a) + 127) / 255) as u8
}

/// Composites an image onto white, dropping the alpha channel.
fn flatten_rgb(img: &DynamicImage) -> RgbImage {
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        image::Rgb([blend(p[0], p[3]), blend(p[1], p[3]), blend(p[2], p[3])])
    })
}

/// Composites a grayscale image onto white, dropping the alpha channel.
fn flatten_gray(img: &DynamicImage) -> GrayImage {
    let la = img.to_luma_alpha8();
    GrayImage::from_fn(la.width(), la.height(), |x, y| {
        let p = la.get_pixel(x, y).0;
        image::Luma([blend(p[0], p[1])])
    })
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Loads `src` (path, `file://`, `data:` or `http(s)` URL) relative to `base`.
fn load(src: &str, base: &Base, ctx: &mut Ctx) -> Result<Loaded> {
    let src = src.trim();
    if src.is_empty() {
        bail!("empty source");
    }
    if let Some(rest) = strip_prefix_ci(src, "data:") {
        let (bytes, mime) = decode_data_uri(rest)?;
        return Ok(Loaded {
            bytes,
            mime,
            name: "data URI".into(),
            base: base.clone(),
        });
    }
    if strip_prefix_ci(src, "http://").is_some() || strip_prefix_ci(src, "https://").is_some() {
        return fetch_remote(src, ctx);
    }
    if src.starts_with("//") {
        let scheme = match base {
            Base::Url(u) if strip_prefix_ci(u, "http:").is_some() => "http:",
            _ => "https:",
        };
        return fetch_remote(&format!("{scheme}{src}"), ctx);
    }
    if strip_prefix_ci(src, "file:").is_some() {
        return read_local(&file_url_to_path(src)?);
    }
    if let Some(scheme) = url_scheme(src) {
        bail!("unsupported URL scheme '{scheme}:'");
    }
    match base {
        Base::Url(u) => fetch_remote(&join_url(u, src), ctx),
        Base::Dir(d) => read_local_relative(src, d),
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// The URL scheme of `s`, if it has one (`ftp`, `javascript`, ...). A single
/// letter followed by `:` is a Windows drive, not a scheme.
fn url_scheme(s: &str) -> Option<&str> {
    let (scheme, _) = s.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (scheme.len() >= 2
        && first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(scheme)
}

fn percent_decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn read_local_relative(src: &str, dir: &Path) -> Result<Loaded> {
    let stripped = src.split(['?', '#']).next().unwrap_or(src);
    let mut candidates = vec![
        src.to_string(),
        stripped.to_string(),
        percent_decode(stripped),
    ];
    // Windows-style separators (`sub\img.png`, or `sub%5Cimg.png` once
    // encoded) work on every OS.
    for c in candidates.clone() {
        if c.contains('\\') {
            candidates.push(c.replace('\\', "/"));
        }
    }
    candidates.dedup();
    for c in &candidates {
        if c.is_empty() {
            continue;
        }
        let p = Path::new(c);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            dir.join(p)
        };
        if full.is_file() {
            return read_local(&full);
        }
    }
    let shown = candidates.last().map(String::as_str).unwrap_or(src);
    bail!("file not found: {}", dir.join(shown).display())
}

fn read_local(path: &Path) -> Result<Loaded> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !meta.is_file() {
        bail!("not a file: {}", path.display());
    }
    if meta.len() > MAX_LOCAL_BYTES {
        bail!(
            "{} is larger than {} MB",
            path.display(),
            MAX_LOCAL_BYTES / 1024 / 1024
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok(Loaded {
        bytes,
        mime: None,
        name: path.to_string_lossy().into_owned(),
        base: Base::Dir(dir),
    })
}

/// Converts a `file:` URL to a local path.
fn file_url_to_path(url: &str) -> Result<PathBuf> {
    let rest = strip_prefix_ci(url, "file:").unwrap_or(url);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = if let Some(after) = rest.strip_prefix("//") {
        // file://host/path: only an empty host or localhost is local.
        let (host, path) = match after.find('/') {
            Some(i) => (&after[..i], &after[i..]),
            None => (after, ""),
        };
        let drive = host.len() == 2
            && host.as_bytes()[0].is_ascii_alphabetic()
            && host.as_bytes()[1] == b':';
        if drive {
            // file://C:/dir (missing third slash)
            return Ok(PathBuf::from(percent_decode(after)));
        }
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            bail!("file URL with a remote host is not supported: {url}");
        }
        path
    } else {
        rest
    };
    let mut decoded = percent_decode(path);
    // file:///C:/dir -> C:/dir
    let b = decoded.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        decoded.remove(0);
    }
    if decoded.is_empty() {
        bail!("empty file URL");
    }
    Ok(PathBuf::from(decoded))
}

/// Decodes the part of a `data:` URI after `data:`.
fn decode_data_uri(rest: &str) -> Result<(Vec<u8>, Option<String>)> {
    let (meta, data) = rest.split_once(',').context("malformed data URI")?;
    let mut params = meta.split(';').map(str::trim);
    let mime = params
        .next()
        .map(|m| m.to_ascii_lowercase())
        .filter(|m| !m.is_empty());
    let base64 = params.any(|p| p.eq_ignore_ascii_case("base64"));
    let bytes = if base64 {
        let cleaned: String = percent_decode(data)
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        STANDARD_PAD_INDIFFERENT
            .decode(&cleaned)
            .or_else(|_| URL_SAFE_PAD_INDIFFERENT.decode(&cleaned))
            .context("invalid base64 in data URI")?
    } else {
        percent_decode_str(data).collect()
    };
    if bytes.is_empty() {
        bail!("empty data URI");
    }
    Ok((bytes, mime))
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// Resolves `rel` against an absolute `http(s)` URL.
fn join_url(base: &str, rel: &str) -> String {
    if url_scheme(rel).is_some() {
        return rel.to_string();
    }
    let (scheme, after) = match base.split_once("://") {
        Some(x) => x,
        None => return rel.to_string(),
    };
    if rel.starts_with("//") {
        return format!("{scheme}:{rel}");
    }
    let host_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let origin = format!("{scheme}://{}", &after[..host_end]);
    let base_path = after[host_end..].split(['?', '#']).next().unwrap_or("");
    let joined = if rel.starts_with('/') {
        rel.to_string()
    } else if rel.starts_with('?') || rel.starts_with('#') || rel.is_empty() {
        format!(
            "{}{rel}",
            if base_path.is_empty() { "/" } else { base_path }
        )
    } else {
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        format!("{dir}{rel}")
    };
    // Normalize dot segments in the path part.
    let (path, tail) = match joined.find(['?', '#']) {
        Some(i) => (&joined[..i], &joined[i..]),
        None => (joined.as_str(), ""),
    };
    let mut segs: Vec<&str> = Vec::new();
    let parts: Vec<&str> = path.split('/').collect();
    for (i, seg) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match *seg {
            "." => {
                if last {
                    segs.push("");
                }
            }
            ".." => {
                if segs.len() > 1 {
                    segs.pop();
                }
                if last {
                    segs.push("");
                }
            }
            s => segs.push(s),
        }
    }
    let mut path = segs.join("/");
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    format!("{origin}{path}{tail}")
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .user_agent(concat!("md2epub/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

fn fetch_remote(url: &str, ctx: &mut Ctx) -> Result<Loaded> {
    if ctx.opts.offline {
        bail!("remote image skipped in offline mode (--offline)");
    }
    ctx.info(format!("downloading {url}"));
    let (bytes, mime, final_url) = fetch(url, MAX_DOWNLOAD_BYTES)?;
    Ok(Loaded {
        bytes,
        mime,
        name: final_url.clone(),
        base: Base::Url(final_url),
    })
}

/// Downloads `url` (following redirects) with the global timeout and a size
/// cap. Returns the body, its MIME type and the final URL.
fn fetch(url: &str, limit: u64) -> Result<(Vec<u8>, Option<String>, String)> {
    use ureq::ResponseExt as _;
    let mut resp = http_agent()
        .get(url)
        .call()
        .map_err(|e| anyhow!("downloading {url}: {e}"))?;
    let final_url = resp.get_uri().to_string();
    let mime = resp.body().mime_type().map(|m| m.to_ascii_lowercase());
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| match e {
            ureq::Error::BodyExceedsLimit(_) => {
                anyhow!("{url} is larger than {} MB", limit / 1024 / 1024)
            }
            e => anyhow!("downloading {url}: {e}"),
        })?;
    Ok((bytes, mime, final_url))
}

// ---------------------------------------------------------------------------
// Image references inside SVG
// ---------------------------------------------------------------------------

/// An `<image>` start tag inside SVG source.
struct ImageTag {
    range: Range<usize>,
    self_closing: bool,
    /// Byte ranges of `href`/`xlink:href` values (without quotes).
    hrefs: Vec<Range<usize>>,
}

/// A start tag inside SVG source.
struct SvgTag {
    range: Range<usize>,
    self_closing: bool,
    /// Lowercase local name (`image`, `style`, `use`, ...).
    local: String,
    attrs: Vec<SvgAttr>,
}

/// An attribute of an [`SvgTag`].
struct SvgAttr {
    /// Lowercase local name (`href` for `xlink:href`).
    local: String,
    /// The whole attribute, `name="value"`.
    full: Range<usize>,
    /// The value without quotes.
    value: Range<usize>,
}

/// Finds `<image>` tags (any prefix) and their href value ranges. Comments
/// and CDATA sections are skipped.
fn find_image_tags(text: &str) -> Vec<ImageTag> {
    scan_svg_tags(text)
        .into_iter()
        .filter(|t| t.local == "image")
        .map(|t| ImageTag {
            range: t.range,
            self_closing: t.self_closing,
            hrefs: t
                .attrs
                .into_iter()
                .filter(|a| a.local == "href")
                .map(|a| a.value)
                .collect(),
        })
        .collect()
}

/// Finds every start tag (any prefix) with its attributes. Comments and
/// CDATA sections are skipped.
fn scan_svg_tags(text: &str) -> Vec<SvgTag> {
    let b = text.as_bytes();
    let len = b.len();
    let is_name = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b':' | b'-' | b'_' | b'.');
    let mut tags = Vec::new();
    let mut i = 0;
    while i < len {
        let Some(off) = text[i..].find('<') else {
            break;
        };
        let start = i + off;
        let rest = &text[start..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(len, |e| start + e + 3);
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i = rest.find("]]>").map_or(len, |e| start + e + 3);
            continue;
        }
        let mut j = start + 1;
        while j < len && is_name(b[j]) {
            j += 1;
        }
        let name = &text[start + 1..j];
        let local = name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase();
        if j == start + 1 {
            i = j.max(start + 1);
            continue;
        }
        // Parse attributes up to the end of the tag.
        let mut attrs = Vec::new();
        let mut self_closing = false;
        let mut k = j;
        let end = loop {
            while k < len && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if k >= len {
                break len;
            }
            if b[k] == b'>' {
                break k + 1;
            }
            if b[k] == b'/' {
                if k + 1 < len && b[k + 1] == b'>' {
                    self_closing = true;
                    break k + 2;
                }
                k += 1;
                continue;
            }
            let name_start = k;
            while k < len && !b[k].is_ascii_whitespace() && !matches!(b[k], b'=' | b'>' | b'/') {
                k += 1;
            }
            let attr = &text[name_start..k];
            if k == name_start {
                k += 1;
                continue;
            }
            while k < len && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < len && b[k] == b'=' {
                k += 1;
                while k < len && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                let value = if k < len && (b[k] == b'"' || b[k] == b'\'') {
                    let q = b[k];
                    let vs = k + 1;
                    let ve = text[vs..].find(q as char).map_or(len, |e| vs + e);
                    k = (ve + 1).min(len);
                    vs..ve
                } else {
                    let vs = k;
                    while k < len && !b[k].is_ascii_whitespace() && b[k] != b'>' {
                        k += 1;
                    }
                    vs..k
                };
                let attr_local = attr.rsplit(':').next().unwrap_or(attr);
                attrs.push(SvgAttr {
                    local: attr_local.to_ascii_lowercase(),
                    full: name_start..k,
                    value,
                });
            }
        };
        tags.push(SvgTag {
            range: start..end,
            self_closing,
            local,
            attrs,
        });
        i = end.max(start + 1);
    }
    tags
}

fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(semi) = after.find(';').filter(|&n| n <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };
        let ent = &after[..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn xml_escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Rewrites the `<image>` references of an SVG document so it is
/// self-contained for `mode`:
///
/// - `Rasterize`: every reference that is not already a PNG/JPEG data URI is
///   loaded, converted and inlined as a data URI (the rasterizer sees it, the
///   book never does).
/// - `Keep`: every reference (data URIs included) is stored as a separate
///   book resource and referenced by its file name, which is a sibling of the
///   stored SVG in `images/`.
///
/// References that cannot be resolved are removed with a warning.
fn prepare_svg(
    text: &str,
    base: &Base,
    mode: SvgMode,
    ctx: &mut Ctx,
    depth: u32,
) -> Result<String> {
    if depth > MAX_SVG_NESTING {
        bail!("SVG images nested too deeply");
    }
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for tag in find_image_tags(text) {
        let mut tag_edits = Vec::new();
        let mut drop_tag = false;
        for vr in &tag.hrefs {
            let href = xml_unescape(&text[vr.clone()]);
            let href = href.trim();
            if href.is_empty() || href.starts_with('#') {
                continue;
            }
            if mode == SvgMode::Rasterize && is_inline_raster(href) {
                continue;
            }
            match resolve_svg_ref(href, base, mode, ctx, depth) {
                Ok(new) => tag_edits.push((vr.clone(), xml_escape_attr(&new))),
                Err(e) => {
                    ctx.warn(format!(
                        "image inside SVG not embedded: {} ({e:#})",
                        short(href)
                    ));
                    if tag.self_closing {
                        drop_tag = true;
                        break;
                    }
                    tag_edits.push((vr.clone(), String::new()));
                }
            }
        }
        if drop_tag {
            edits.push((tag.range.clone(), String::new()));
        } else {
            edits.extend(tag_edits);
        }
    }
    let tags = scan_svg_tags(text);
    // External `<use>` targets are not part of the book (and renderers
    // do not load them): remove the reference.
    for tag in tags.iter().filter(|t| t.local == "use") {
        for a in tag.attrs.iter().filter(|a| a.local == "href") {
            let href = xml_unescape(&text[a.value.clone()]);
            let href = href.trim();
            if !href.is_empty() && !href.starts_with('#') {
                ctx.warn(format!(
                    "external <use> reference in SVG removed: {} (only references inside the same SVG work)",
                    short(href)
                ));
                edits.push((a.full.clone(), String::new()));
            }
        }
    }
    if mode == SvgMode::Keep {
        // A kept SVG is a book resource, so it must not be scripted (EPUB
        // would require the `scripted` property, and readers do not run
        // scripts anyway).
        let mut removed_script = false;
        for tag in &tags {
            if tag.local == "script" {
                let end = if tag.self_closing {
                    tag.range.end
                } else {
                    let close = find_end_tag(text, tag.range.end, "script");
                    text[close..]
                        .find('>')
                        .map_or(text.len(), |e| close + e + 1)
                };
                edits.push((tag.range.start..end, String::new()));
                removed_script = true;
                continue;
            }
            for a in &tag.attrs {
                let value = xml_unescape(&text[a.value.clone()]);
                let scripted_url = matches!(a.local.as_str(), "href" | "src" | "action")
                    && value
                        .trim()
                        .get(..11)
                        .is_some_and(|p| p.eq_ignore_ascii_case("javascript:"));
                if a.local.starts_with("on") || scripted_url {
                    edits.push((a.full.clone(), String::new()));
                    removed_script = true;
                }
            }
        }
        if removed_script {
            ctx.info("scripts and event handlers removed from an SVG image");
        }
        // Its CSS references must be book resources too.
        for tag in &tags {
            for a in tag.attrs.iter().filter(|a| a.local == "style") {
                let css = xml_unescape(&text[a.value.clone()]);
                if contains_url(&css) {
                    let new = rewrite_css(&css, base, ctx);
                    edits.push((a.value.clone(), xml_escape_attr(&new)));
                }
            }
            if tag.local == "style" && !tag.self_closing {
                let start = tag.range.end;
                let end = find_end_tag(text, start, "style");
                let content = &text[start..end];
                if contains_url(content) {
                    edits.push((start..end, rewrite_svg_style(content, base, ctx)));
                }
            }
        }
    }
    // Edits inside a removed tag go with it.
    let outer: Vec<Range<usize>> = edits.iter().map(|(r, _)| r.clone()).collect();
    edits.retain(|(r, _)| {
        !outer
            .iter()
            .any(|o| o != r && o.start <= r.start && r.end <= o.end)
    });
    let mut out = text.to_string();
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    for (r, new) in edits {
        out.replace_range(r, &new);
    }
    Ok(out)
}

/// Byte offset of the end tag `</prefix:name>` (any prefix) at or after
/// `from`, skipping CDATA sections and comments; the text length if none.
fn find_end_tag(text: &str, from: usize, name: &str) -> usize {
    let mut i = from;
    while let Some(off) = text[i..].find('<') {
        let at = i + off;
        let rest = &text[at..];
        if rest.starts_with("<![CDATA[") {
            i = rest.find("]]>").map_or(text.len(), |e| at + e + 3);
            continue;
        }
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(text.len(), |e| at + e + 3);
            continue;
        }
        if let Some(after) = rest.strip_prefix("</") {
            let tag: String = after
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '>')
                .collect();
            if tag
                .rsplit(':')
                .next()
                .unwrap_or(&tag)
                .eq_ignore_ascii_case(name)
            {
                return at;
            }
        }
        i = at + 1;
    }
    text.len()
}

/// Rewrites the CSS inside an SVG `<style>` element (CDATA sections are
/// rewritten as they are; plain text is unescaped and escaped again).
fn rewrite_svg_style(content: &str, base: &Base, ctx: &mut Ctx) -> String {
    if !content.contains("<![CDATA[") {
        let css = xml_unescape(content);
        return rewrite_css(&css, base, ctx)
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(i) = rest.find("<![CDATA[") {
        out.push_str(&rest[..i + 9]);
        let after = &rest[i + 9..];
        let end = after.find("]]>").unwrap_or(after.len());
        out.push_str(&rewrite_css(&after[..end], base, ctx));
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn is_inline_raster(href: &str) -> bool {
    let Some(rest) = strip_prefix_ci(href, "data:") else {
        return false;
    };
    let mime = rest.split([';', ',']).next().unwrap_or("").trim();
    ["image/png", "image/jpeg", "image/jpg"]
        .iter()
        .any(|m| mime.eq_ignore_ascii_case(m))
}

fn resolve_svg_ref(
    href: &str,
    base: &Base,
    mode: SvgMode,
    ctx: &mut Ctx,
    depth: u32,
) -> Result<String> {
    let l = load(href, base, ctx)?;
    let rasterize = mode == SvgMode::Rasterize;
    let sibling = |src: String| src.trim_start_matches("../images/").to_string();
    match sniff(&l) {
        Kind::Raster => {
            let opts = RasterOpts {
                // The rasterizer composites onto white itself.
                flatten: !rasterize && ctx.opts.profile.flatten_alpha,
                ..RasterOpts::for_ctx(ctx)
            };
            let out = process_raster(l.bytes, opts)?;
            if rasterize {
                Ok(data_uri(media_type_for_ext(out.ext), &out.bytes))
            } else {
                Ok(sibling(ctx.add_image(out.bytes, out.ext)))
            }
        }
        Kind::Svg => {
            let text = svg_text(&l.bytes)?;
            let nested = prepare_svg(&text, &l.base, mode, ctx, depth + 1)?;
            if rasterize {
                Ok(data_uri("image/svg+xml", nested.as_bytes()))
            } else {
                Ok(sibling(ctx.add_image(nested.into_bytes(), "svg")))
            }
        }
        Kind::Font(_) | Kind::Unknown => bail!("not a supported image format"),
    }
}

// ---------------------------------------------------------------------------
// CSS url(...)
// ---------------------------------------------------------------------------

/// Whether CSS text has anything [`rewrite_css`] would change.
fn contains_url(css: &str) -> bool {
    let lower = css.to_ascii_lowercase();
    lower.contains("url(") || lower.contains("@import")
}

/// Parses the argument of a `url(` starting at byte `open` (just after the
/// parenthesis). Returns the raw value and the index after `)`.
fn parse_css_url(css: &str, open: usize) -> Option<(String, usize)> {
    let b = css.as_bytes();
    let len = b.len();
    let mut k = open;
    while k < len && b[k].is_ascii_whitespace() {
        k += 1;
    }
    let mut value = String::new();
    if k < len && (b[k] == b'"' || b[k] == b'\'') {
        let q = b[k];
        k += 1;
        let start = k;
        while k < len && b[k] != q {
            if b[k] == b'\\' {
                k += 1;
            }
            k += 1;
        }
        if k >= len {
            return None;
        }
        value.push_str(&css_unescape(&css[start..k]));
        k += 1;
        while k < len && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if k >= len || b[k] != b')' {
            return None;
        }
        Some((value, k + 1))
    } else {
        let start = k;
        while k < len && b[k] != b')' {
            if b[k] == b'\\' {
                k += 1;
            }
            k += 1;
        }
        if k >= len {
            return None;
        }
        value.push_str(css_unescape(&css[start..k]).trim());
        Some((value, k + 1))
    }
}

fn css_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 {
            match chars.peek() {
                Some(h) if h.is_ascii_hexdigit() => {
                    hex.push(*h);
                    chars.next();
                }
                _ => break,
            }
        }
        if hex.is_empty() {
            if let Some(n) = chars.next()
                && n != '\n'
            {
                out.push(n);
            }
        } else {
            if matches!(chars.peek(), Some(' ')) {
                chars.next();
            }
            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(ch);
            }
        }
    }
    out
}

fn css_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\a "),
            c => out.push(c),
        }
    }
    out
}

/// Rewrites the `url(...)` references of the user's `--css` stylesheet
/// (read from `dir`) to stored book resources, exactly as for `<style>`
/// blocks. The resulting paths (`../images/...`, `../fonts/...`) are valid
/// from `OEBPS/css/` as well as from `OEBPS/text/`.
pub fn rewrite_user_css(css: &str, dir: &Path, ctx: &mut Ctx) -> String {
    let base = Base::Dir(dir.to_path_buf());
    rewrite_css(css, &base, ctx)
}

/// Index just past the end of the `@import` rule (or declaration) starting
/// at `from`: after the next `;` outside strings and parentheses, or at a
/// `}` (not consumed), or at the end of the text.
fn statement_end(css: &str, from: usize) -> (usize, bool) {
    let b = css.as_bytes();
    let mut i = from;
    let mut paren = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => i = skip_css_string(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_css_comment(css, i),
            b'(' => {
                paren += 1;
                i += 1;
            }
            b')' => {
                paren = paren.saturating_sub(1);
                i += 1;
            }
            b';' if paren == 0 => return (i + 1, true),
            b'}' if paren == 0 => return (i, false),
            _ => i += 1,
        }
    }
    (b.len(), false)
}

/// Index just past a quoted CSS string starting at `start`.
fn skip_css_string(b: &[u8], start: usize) -> usize {
    let q = b[start];
    let mut i = start + 1;
    while i < b.len() && b[i] != q {
        if b[i] == b'\\' {
            i += 1;
        } else if b[i] == b'\n' {
            // An unterminated string ends at the line end.
            return i;
        }
        i += 1;
    }
    (i + 1).min(b.len())
}

/// Index just past a `/* ... */` comment starting at `start`.
fn skip_css_comment(css: &str, start: usize) -> usize {
    css[start + 2..]
        .find("*/")
        .map_or(css.len(), |e| start + 2 + e + 2)
}

/// The stylesheet an `@import` prelude names (for messages).
fn import_target(prelude: &str) -> String {
    let p = prelude.trim();
    let lower = p.to_ascii_lowercase();
    if lower.starts_with("url(")
        && let Some((v, _)) = parse_css_url(p, 4)
    {
        return v;
    }
    let p = p.trim_start_matches(['"', '\'']);
    p.split(['"', '\'', ';', ' '])
        .next()
        .unwrap_or("")
        .to_string()
}

/// Rewrites every `url(...)` in CSS text to a stored book resource, or to
/// `none` (with a warning) when it cannot be resolved; an unresolvable
/// `src` descriptor (`@font-face`) is removed with its declaration, since
/// `src: none` is invalid. Fragment-only URLs (`url(#id)`) are left alone,
/// comments and strings are copied untouched, and `@import` rules are
/// removed with a warning.
fn rewrite_css(css: &str, base: &Base, ctx: &mut Ctx) -> String {
    let b = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    let mut copied = 0;
    // Where the current declaration starts in `out`.
    let mut decl_start = 0;
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_';
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_css_comment(css, i),
            b'"' | b'\'' => i = skip_css_string(b, i),
            b';' | b'{' | b'}' => {
                i += 1;
                out.push_str(&css[copied..i]);
                copied = i;
                decl_start = out.len();
            }
            b'@' if css[i..]
                .get(..7)
                .is_some_and(|h| h.eq_ignore_ascii_case("@import"))
                && !b.get(i + 7).copied().is_some_and(is_ident) =>
            {
                let (end, _) = statement_end(css, i + 7);
                let target = import_target(css[i + 7..end].trim_end_matches(';'));
                out.push_str(&css[copied..i]);
                ctx.warn(format!(
                    "imported stylesheet '{}' ignored (add it with --css)",
                    short(&target)
                ));
                i = end;
                copied = end;
            }
            b'u' | b'U'
                if css[i..]
                    .get(..4)
                    .is_some_and(|h| h.eq_ignore_ascii_case("url("))
                    && !(i > 0 && is_ident(b[i - 1])) =>
            {
                let Some((value, end)) = parse_css_url(css, i + 4) else {
                    i += 4;
                    continue;
                };
                out.push_str(&css[copied..i]);
                let value = value.trim();
                if value.is_empty() || value.starts_with('#') {
                    out.push_str(&css[i..end]);
                    i = end;
                    copied = end;
                    continue;
                }
                match resolve_css_url(value, base, ctx) {
                    Ok(url) => {
                        out.push_str("url(\"");
                        out.push_str(&css_escape_string(&url));
                        out.push_str("\")");
                        i = end;
                    }
                    Err(e) => {
                        ctx.warn(format!(
                            "CSS resource not embedded: {} ({e:#})",
                            short(value)
                        ));
                        let prop = out[decl_start..]
                            .split(':')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_ascii_lowercase();
                        if out[decl_start..].contains(':') && prop == "src" {
                            out.truncate(decl_start);
                            let (decl_end, _) = statement_end(css, end);
                            i = decl_end;
                        } else {
                            out.push_str("none");
                            i = end;
                        }
                    }
                }
                copied = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(&css[copied.min(css.len())..]);
    out
}

fn resolve_css_url(value: &str, base: &Base, ctx: &mut Ctx) -> Result<String> {
    let l = load(value, base, ctx)?;
    match sniff(&l) {
        Kind::Font(ext) => {
            let href = format!("fonts/{}.{ext}", content_hash(&l.bytes));
            ctx.add_resource(&href, media_type_for_ext(ext), l.bytes);
            Ok(format!("../{href}"))
        }
        Kind::Raster | Kind::Svg => store(l, ctx).map(|s| s.src),
        Kind::Unknown => bail!("not a supported image or font"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::TcpListener;

    use super::*;
    use crate::model::Options;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn ctx_with(f: impl FnOnce(&mut Options)) -> Ctx {
        let mut opts = Options::default();
        f(&mut opts);
        let mut ctx = Ctx::new(opts);
        ctx.base_dir = fixtures();
        ctx
    }

    fn ctx() -> Ctx {
        ctx_with(|_| {})
    }

    fn scribe(o: &mut Options) {
        o.profile.name = "scribe".into();
        o.profile.screen_w = 1860;
        o.profile.screen_h = 2480;
        o.profile.flatten_alpha = true;
        o.profile.svg = SvgMode::Rasterize;
        o.svg = SvgMode::Rasterize;
    }

    fn run(html: &str, ctx: &mut Ctx) -> String {
        let doc = dom::parse(html);
        transform(&doc, ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        dom::tests_support::assert_well_formed(&out);
        out
    }

    fn png_bytes(img: DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    fn encode(img: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), format).unwrap();
        out
    }

    #[test]
    fn local_png_and_jpeg_are_stored_untouched() {
        let mut ctx = ctx();
        let out = run(
            r#"<p><img src="images/chart.png" alt="Chart"><img src="photo.jpg" title="t"></p>"#,
            &mut ctx,
        );
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert_eq!(ctx.resources().len(), 2);
        let png = std::fs::read(fixtures().join("images/chart.png")).unwrap();
        let jpg = std::fs::read(fixtures().join("photo.jpg")).unwrap();
        assert_eq!(ctx.resources()[0].data, png);
        assert_eq!(ctx.resources()[0].media_type, "image/png");
        assert_eq!(ctx.resources()[1].data, jpg);
        assert_eq!(ctx.resources()[1].media_type, "image/jpeg");
        assert!(out.contains(r#"alt="Chart""#));
        assert!(out.contains(r#"alt="""#), "alt added: {out}");
        assert!(out.contains("../images/") && !out.contains("chart.png"));
    }

    #[test]
    fn dedupes_and_handles_encoded_paths_and_queries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(
            fixtures().join("images/chart.png"),
            dir.path().join("my chart.png"),
        )
        .unwrap();
        let mut ctx = ctx();
        ctx.base_dir = dir.path().to_path_buf();
        let abs = dir.path().join("my chart.png");
        let html = format!(
            r#"<img src="my%20chart.png?v=2#x" alt="a"><img src="{}" alt="b"><img src="file:///{}" alt="c">"#,
            abs.display(),
            abs.to_string_lossy()
                .replace(' ', "%20")
                .replace('\\', "/")
                .trim_start_matches('/')
        );
        let out = run(&html, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert_eq!(ctx.resources().len(), 1);
        assert_eq!(out.matches("<img").count(), 3);
    }

    #[test]
    fn data_uris() {
        let png = std::fs::read(fixtures().join("images/chart.png")).unwrap();
        let mut ctx = ctx();
        let html = format!(
            r#"<img src="data:image/png;base64,{}" alt="d"><img src="data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20width%3D%2210%22%20height%3D%2210%22%2F%3E" alt="s">"#,
            STANDARD.encode(&png)
        );
        let out = run(&html, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert_eq!(ctx.resources().len(), 2);
        assert_eq!(ctx.resources()[0].data, png);
        assert_eq!(ctx.resources()[1].media_type, "image/svg+xml");
        assert!(!out.contains("data:"));

        // Garbage data URIs fall back to the alt text.
        let mut ctx = ctx_with(|_| {});
        let out = run(
            r#"<img src="data:image/png;base64,!!!" alt="bad"><img src="data:," alt="empty"><img src="data:text/plain,hello" alt="text">"#,
            &mut ctx,
        );
        assert_eq!(ctx.warnings().len(), 3, "{:?}", ctx.warnings());
        assert!(!out.contains("<img"));
        assert!(out.contains(">bad<") && out.contains(">text<"));
    }

    #[test]
    fn missing_and_unsupported_sources_keep_alt_text() {
        let mut ctx = ctx();
        let out = run(
            r#"<p>A <img src="nope.png" alt="Missing one"> B <img src="nope2.png"> C <img alt="no src"> <img src="ftp://x/y.png" alt="ftp"> <img src="../fixtures/extra.css" alt="css"></p>"#,
            &mut ctx,
        );
        assert_eq!(ctx.warnings().len(), 5, "{:?}", ctx.warnings());
        assert!(ctx.warnings()[0].contains("nope.png"));
        assert!(ctx.warnings()[3].contains("ftp"));
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains(r#"<span class="img-missing">Missing one</span>"#));
        assert!(out.contains(">ftp<") && out.contains(">css<"));
        assert!(ctx.resources().is_empty());
    }

    #[test]
    fn oversized_images_are_downscaled() {
        let big = DynamicImage::ImageRgb8(RgbImage::from_pixel(3200, 400, image::Rgb([9, 9, 9])));
        let mut ctx = ctx();
        let html = format!(
            r#"<img src="data:image/png;base64,{}"><img src="data:image/jpeg;base64,{}" width="3200" height="400">"#,
            STANDARD.encode(png_bytes(big.clone())),
            STANDARD.encode(encode(big, ImageFormat::Jpeg))
        );
        let out = run(&html, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        let png = image::load_from_memory(&ctx.resources()[0].data).unwrap();
        assert_eq!((png.width(), png.height()), (1600, 200));
        assert_eq!(ctx.resources()[1].media_type, "image/jpeg");
        let jpg = image::load_from_memory(&ctx.resources()[1].data).unwrap();
        assert_eq!((jpg.width(), jpg.height()), (1600, 200));
        // Attributes are clamped to the screen too.
        assert!(out.contains(r#"width="1600""#) && out.contains(r#"height="200""#));
    }

    #[test]
    fn transparent_images_flattened_only_when_profile_asks() {
        let original = std::fs::read(fixtures().join("images/transparent.png")).unwrap();
        let mut ctx = ctx();
        run(r#"<img src="images/transparent.png">"#, &mut ctx);
        assert_eq!(ctx.resources()[0].data, original);

        let mut ctx = ctx_with(scribe);
        run(r#"<img src="images/transparent.png">"#, &mut ctx);
        let r = &ctx.resources()[0];
        assert_eq!(r.media_type, "image/png");
        let img = image::load_from_memory(&r.data).unwrap();
        assert!(!img.color().has_alpha());
        let src = image::load_from_memory(&original).unwrap().to_rgba8();
        // A fully transparent source pixel becomes white.
        let (x, y) = src
            .enumerate_pixels()
            .find(|(_, _, p)| p.0[3] == 0)
            .map(|(x, y, _)| (x, y))
            .expect("fixture has transparent pixels");
        assert_eq!(img.to_rgb8().get_pixel(x, y).0, [255, 255, 255]);
    }

    #[test]
    fn webp_gif_bmp_tiff_become_png() {
        let small = DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 6, image::Rgb([200, 10, 10])));
        let mut html = String::from(r#"<img src="images/sample.webp" alt="w">"#);
        for f in [ImageFormat::Gif, ImageFormat::Bmp, ImageFormat::Tiff] {
            html.push_str(&format!(
                r#"<img src="data:image/x;base64,{}">"#,
                STANDARD.encode(encode(small.clone(), f))
            ));
        }
        let mut ctx = ctx();
        let out = run(&html, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert!(!out.contains(".webp"));
        // BMP and TIFF of the same pixels dedupe to one PNG (GIF quantizes).
        assert_eq!(ctx.resources().len(), 3);
        for r in ctx.resources() {
            assert_eq!(r.media_type, "image/png");
            assert!(r.href.ends_with(".png"));
            assert_eq!(
                image::guess_format(&r.data).unwrap(),
                ImageFormat::Png,
                "{}",
                r.href
            );
        }
    }

    #[test]
    fn grayscale_option() {
        let mut ctx = ctx_with(|o| o.grayscale = true);
        run(
            r#"<img src="images/chart.png"><img src="photo.jpg">"#,
            &mut ctx,
        );
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        let png = image::load_from_memory(&ctx.resources()[0].data).unwrap();
        assert_eq!(png.color(), ColorType::L8);
        assert_eq!(ctx.resources()[1].media_type, "image/jpeg");
        let jpg = image::load_from_memory(&ctx.resources()[1].data).unwrap();
        assert_eq!(jpg.color(), ColorType::L8);
    }

    #[test]
    fn inline_svg_is_replaced_by_img() {
        let html = r##"<p>x <svg xmlns="http://www.w3.org/2000/svg" id="s1" width="120" height="60" viewBox="0 0 120 60"><title>Inline shapes</title><rect width="50" height="50" fill="#888"/><svg width="5" height="5"/></svg></p>
            <svg width="10" height="10" aria-label="Labelled"><circle r="4"/></svg>
            <svg width="10" height="10"><text>t</text></svg>"##;
        let mut ctx = ctx();
        let out = run(html, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert!(!out.contains("<svg"), "{out}");
        assert_eq!(out.matches("<img").count(), 3);
        assert!(out.contains(r#"alt="Inline shapes""#));
        assert!(out.contains(r#"alt="Labelled""#));
        assert!(out.contains(r#"alt="""#));
        assert!(out.contains(r#"id="s1""#));
        assert!(!out.contains("data-res"));
        assert_eq!(ctx.resources().len(), 3);
        let svg = String::from_utf8(ctx.resources()[0].data.clone()).unwrap();
        assert!(
            svg.contains(r#"xmlns="http://www.w3.org/2000/svg""#),
            "{svg}"
        );
        assert!(svg.contains("<rect") && svg.contains("<title>"));
        assert!(
            ctx.resources()
                .iter()
                .all(|r| r.media_type == "image/svg+xml")
        );

        let mut ctx = ctx_with(scribe);
        let out = run(html, &mut ctx);
        assert!(!out.contains("<svg"));
        assert!(ctx.resources().iter().all(|r| r.media_type == "image/png"));
        assert!(out.contains(r#"width=""#) && out.contains(r#"height=""#));
    }

    #[test]
    fn referenced_svgs() {
        let mut ctx = ctx();
        let out = run(
            r#"<img src="images/diagram.svg" alt="d"><img src="images/viewbox-only.svg" alt="v">"#,
            &mut ctx,
        );
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert_eq!(ctx.resources().len(), 2);
        assert!(out.contains(".svg\"") && out.contains("width=\""));
        let original = std::fs::read(fixtures().join("images/diagram.svg")).unwrap();
        assert_eq!(ctx.resources()[0].data, original);

        let mut ctx = ctx_with(scribe);
        let out = run(r#"<img src="images/diagram.svg" alt="d">"#, &mut ctx);
        assert!(out.contains(".png\""));
        assert_eq!(ctx.resources()[0].media_type, "image/png");
    }

    #[test]
    fn svg_image_references_are_resolved_against_the_svg() {
        // Keep: linked and embedded images become sibling resources.
        let mut ctx = ctx();
        run(
            r#"<img src="images/linked-image.svg"><img src="images/embedded-image.svg">"#,
            &mut ctx,
        );
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        let svgs: Vec<String> = ctx
            .resources()
            .iter()
            .filter(|r| r.media_type == "image/svg+xml")
            .map(|r| String::from_utf8(r.data.clone()).unwrap())
            .collect();
        assert_eq!(svgs.len(), 2);
        let pngs: Vec<&Resource> = ctx
            .resources()
            .iter()
            .filter(|r| r.media_type == "image/png")
            .collect();
        assert_eq!(pngs.len(), 2);
        for (svg, png) in svgs.iter().zip(&pngs) {
            let name = png.href.trim_start_matches("images/");
            assert!(svg.contains(&format!("\"{name}\"")), "{svg}");
            assert!(!svg.contains("base64") && !svg.contains("chart.png"));
        }

        // Rasterize: references are inlined for the rasterizer.
        let mut ctx = ctx_with(scribe);
        let text = std::fs::read_to_string(fixtures().join("images/linked-image.svg")).unwrap();
        let base = Base::Dir(fixtures().join("images"));
        let out = prepare_svg(&text, &base, SvgMode::Rasterize, &mut ctx, 0).unwrap();
        assert!(out.contains("data:image/png;base64,"));
        assert!(ctx.resources().is_empty());

        // Broken references are dropped with a warning.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image href="missing.png"/><image xlink:href="#ok"/></svg>"##;
        let out = prepare_svg(svg, &base, SvgMode::Keep, &mut ctx, 0).unwrap();
        assert!(!out.contains("missing.png") && out.contains("#ok"));
        assert_eq!(ctx.warnings().len(), 1);
    }

    #[test]
    fn kept_svgs_lose_scripts_and_event_handlers() {
        let dir = tempfile::tempdir().unwrap();
        let svg = concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10" onload="x()">"#,
            r#"<script type="text/javascript"><![CDATA[ if (a < b) { alert(1) } ]]></script>"#,
            r#"<svg:script xmlns:svg="http://www.w3.org/2000/svg">alert(2)</svg:script><script href="x.js"/>"#,
            r#"<a xlink:href=" JavaScript:alert(3)"><rect width="5" height="5" onclick="y()" ONMOUSEOVER="z()"/></a>"#,
            r##"<a href="#ok"><circle r="1" opacity="0.5"/></a></svg>"##
        );
        std::fs::write(dir.path().join("s.svg"), svg).unwrap();
        let mut ctx = ctx();
        ctx.base_dir = dir.path().to_path_buf();
        run(r#"<img src="s.svg" alt="s">"#, &mut ctx);
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        let kept = String::from_utf8(ctx.resources()[0].data.clone()).unwrap();
        for gone in [
            "script",
            "alert",
            "onload",
            "onclick",
            "ONMOUSEOVER",
            "JavaScript",
        ] {
            assert!(!kept.contains(gone), "{gone} in {kept}");
        }
        for stays in [r##"href="#ok""##, r#"opacity="0.5""#, "<rect", "</svg>"] {
            assert!(kept.contains(stays), "{stays} missing from {kept}");
        }
        assert!(roxmltree::Document::parse(&kept).is_ok(), "{kept}");
    }

    #[test]
    fn offline_skips_remote_images() {
        let mut ctx = ctx_with(|o| o.offline = true);
        let out = run(
            r#"<p><img src="https://example.invalid/logo.png" alt="Remote"><img src="//example.invalid/x.png"></p><div style="background: url(http://example.invalid/bg.png)"></div>"#,
            &mut ctx,
        );
        assert_eq!(ctx.warnings().len(), 3, "{:?}", ctx.warnings());
        assert!(ctx.warnings().iter().all(|w| w.contains("offline")));
        assert!(out.contains(">Remote<") && !out.contains("<img"));
        assert!(out.contains("background: none"));
    }

    /// Serves canned responses on a loopback port (no external network).
    fn serve(responses: Vec<(&'static str, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn remote_images_via_loopback() {
        let png = std::fs::read(fixtures().join("images/chart.png")).unwrap();
        let base = serve(vec![("200 OK", png.clone()), ("404 Not Found", Vec::new())]);
        let mut ctx = ctx();
        let out = run(
            &format!(r#"<img src="{base}/a.png" alt="ok"><img src="{base}/b.png" alt="gone">"#),
            &mut ctx,
        );
        assert_eq!(ctx.resources().len(), 1);
        assert_eq!(ctx.resources()[0].data, png);
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        assert!(ctx.warnings()[0].contains("b.png"));
        assert!(out.contains(">gone<"));

        // The size cap applies to the body.
        let base = serve(vec![("200 OK", vec![0u8; 4096])]);
        let err = fetch(&format!("{base}/big"), 1024).unwrap_err().to_string();
        assert!(err.contains("larger than"), "{err}");
    }

    #[test]
    fn css_urls_in_style_attributes_and_blocks() {
        let mut ctx = ctx();
        let out = run(
            r##"<p>x</p><style>.hero { background-image: url("images/chart.png"); } .x { fill: url(#grad); } .y { background: url(  'nope.png'  ) }</style>
            <div style="background:URL(images/chart.png) no-repeat"><span style="color: red">t</span></div>"##,
            &mut ctx,
        );
        assert_eq!(ctx.resources().len(), 1);
        let src = format!("../{}", ctx.resources()[0].href);
        assert_eq!(out.matches(&src).count(), 2, "{out}");
        assert!(out.contains("url(#grad)"));
        assert!(out.contains("background: none"));
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        assert!(!out.contains("chart.png"));
    }

    #[test]
    fn css_fonts_are_stored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.woff2"), b"wOF2fakefontdata").unwrap();
        let mut ctx = ctx();
        ctx.base_dir = dir.path().to_path_buf();
        let out = run(
            r#"<p>x</p><style>@font-face { font-family: X; src: url(f.woff2) format("woff2"); }</style>"#,
            &mut ctx,
        );
        assert_eq!(ctx.resources()[0].media_type, "font/woff2");
        assert!(ctx.resources()[0].href.starts_with("fonts/"));
        assert!(out.contains("../fonts/"));
    }

    #[test]
    fn finished_images_and_dimensions() {
        let mut ctx = ctx();
        let out = run(
            r#"<img src="../images/done.png" data-res="1"><img src="images/chart.png" width="50%" height="20" style="border:0"><img src="images/chart.png" width="auto" height="10px" srcset="a.png 2x"><picture><source srcset="x.webp"><img src="images/chart.png" alt="p"></picture>"#,
            &mut ctx,
        );
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
        assert!(out.contains(r#"src="../images/done.png""#));
        assert!(!out.contains("data-res") && !out.contains("srcset"));
        assert!(!out.contains("<picture") && !out.contains("<source"));
        assert!(out.contains("border:0; width: 50%"), "{out}");
        assert!(!out.contains(r#"width="50%""#) && !out.contains(r#"height="20""#));
        assert!(!out.contains(r#"width="auto""#) && out.contains(r#"height="10""#));
        assert_eq!(out.matches(r#"alt="""#).count(), 3);
    }

    #[test]
    fn vector_dimensions_are_filled_from_one_side() {
        let img = dom::create_element("img", &[("width", "200")]);
        let ctx = ctx();
        fix_dimensions(&img, 100, 50, true, &ctx);
        assert_eq!(dom::get_attr(&img, "height").as_deref(), Some("100"));
        let img = dom::create_element("img", &[]);
        fix_dimensions(&img, 4000, 1000, true, &ctx);
        assert_eq!(dom::get_attr(&img, "width").as_deref(), Some("1600"));
        assert_eq!(dom::get_attr(&img, "height").as_deref(), Some("400"));
        let img = dom::create_element("img", &[]);
        fix_dimensions(&img, 40, 10, false, &ctx);
        assert!(!dom::has_attr(&img, "width"));
    }

    #[test]
    fn cover_processing() {
        let dir = fixtures();
        let mut ctx = ctx();
        let r = load_cover(&dir.join("photo.jpg"), &mut ctx).unwrap();
        assert_eq!(r.media_type, "image/jpeg");
        assert_eq!(r.href, "images/cover.jpg");
        assert_eq!(r.data, std::fs::read(dir.join("photo.jpg")).unwrap());

        let r = load_cover(&dir.join("images/transparent.png"), &mut ctx).unwrap();
        assert_eq!(r.href, "images/cover.png");
        assert!(
            !image::load_from_memory(&r.data)
                .unwrap()
                .color()
                .has_alpha()
        );

        let r = load_cover(&dir.join("images/sample.webp"), &mut ctx).unwrap();
        assert_eq!(r.media_type, "image/png");

        let r = load_cover(&dir.join("images/diagram.svg"), &mut ctx).unwrap();
        assert_eq!(r.media_type, "image/png");
        assert!(r.data.starts_with(b"\x89PNG"));

        assert!(load_cover(&dir.join("missing.png"), &mut ctx).is_err());
        assert!(load_cover(&dir.join("extra.css"), &mut ctx).is_err());
        assert!(ctx.resources().is_empty(), "cover is not added to ctx");
    }

    #[test]
    fn helpers() {
        assert_eq!(
            join_url("https://a.org/x/y/z.svg?q=1", "img/p.png"),
            "https://a.org/x/y/img/p.png"
        );
        assert_eq!(
            join_url("https://a.org/x/y.svg", "../p.png"),
            "https://a.org/p.png"
        );
        assert_eq!(
            join_url("https://a.org/x/y.svg", "/p.png"),
            "https://a.org/p.png"
        );
        assert_eq!(join_url("https://a.org", "p.png"), "https://a.org/p.png");
        assert_eq!(join_url("http://a.org/x", "//b.org/p"), "http://b.org/p");
        assert_eq!(url_scheme("C:\\dir\\a.png"), None);
        assert_eq!(url_scheme("ftp://x"), Some("ftp"));
        assert_eq!(url_scheme("a/b:c.png"), None);
        assert_eq!(
            file_url_to_path("file:///tmp/a%20b.png").unwrap(),
            PathBuf::from("/tmp/a b.png")
        );
        assert_eq!(
            file_url_to_path("file:///C:/x.png").unwrap(),
            PathBuf::from("C:/x.png")
        );
        assert!(file_url_to_path("file://remote/x.png").is_err());
        assert_eq!(
            file_url_to_path("file://c:/x.png").unwrap(),
            PathBuf::from("c:/x.png")
        );
        let (b, m) = decode_data_uri("text/plain;charset=utf-8,a%20b").unwrap();
        assert_eq!(
            (b.as_slice(), m.as_deref()),
            (&b"a b"[..], Some("text/plain"))
        );
        let (b, _) = decode_data_uri(";base64,aGk").unwrap();
        assert_eq!(b, b"hi");
        assert_eq!(xml_unescape("a&amp;b&#x41;&#66;&bogus"), "a&bAB&bogus");
        assert_eq!(css_unescape(r"a\(b\29 c"), "a(b)c");
        assert_eq!(parse_dim("12px"), Dim::Px(12.0));
        assert_eq!(parse_dim("40%"), Dim::Percent(40.0));
        assert_eq!(parse_dim("0"), Dim::Invalid);
        let tags = find_image_tags(
            r#"<!-- <image href="c"/> --><svg:image x='1' xlink:href='a.png'/><image href="b.png" ><g/>"#,
        );
        assert_eq!(tags.len(), 2);
        assert!(tags[0].self_closing && !tags[1].self_closing);
        assert_eq!(tags[0].hrefs.len(), 1);
    }

    #[test]
    fn never_panics_on_garbage() {
        let mut ctx = ctx();
        for html in [
            "<img src=\"\u{0}\">",
            "<img src=\"data:\">",
            "<img src=\"data:image/png;base64,iVBORw0KGgo=\">",
            "<svg><image href=\"",
            "<div style=\"background:url(\"></div>",
            "<style>url( url(' url(\\</style>",
            "<img src=\"images/\" width=\"-5\" height=\"1e40\">",
            "<svg width=\"0\" height=\"0\"></svg>",
        ] {
            let _ = run(html, &mut ctx);
        }
        assert!(find_image_tags("<image href='x").len() == 1);
        assert!(
            prepare_svg(
                "<image href=\"&#xFFFFFFFF;\"/>",
                &Base::Dir(fixtures()),
                SvgMode::Keep,
                &mut ctx,
                0
            )
            .is_ok()
        );
    }

    /// A PNG whose header claims `w`×`h` pixels (the pixel data is junk:
    /// the size check must fire before anything is decoded).
    fn png_header_only(w: u32, h: u32) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(data);
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&crc.sum().to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &[0x78, 0x9c, 0x03, 0x00]);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn decompression_bombs_are_refused_before_decoding() {
        let bomb = png_header_only(60_000, 60_000);
        let err = process_raster(bomb.clone(), RasterOpts::for_ctx(&ctx()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("too large"), "{err}");
        let mut ctx = ctx();
        let uri = data_uri("image/png", &bomb);
        let out = run(&format!(r#"<img src="{uri}" alt="bomb">"#), &mut ctx);
        assert!(out.contains(">bomb<"), "{out}");
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        assert!(ctx.warnings()[0].contains("too large"));
    }

    #[test]
    fn cmyk_jpegs_are_converted_to_rgb() {
        let bytes = std::fs::read(fixtures().join("images/cmyk.jpg")).unwrap();
        assert!(jpeg_is_cmyk(&bytes));
        let rgb = encode(DynamicImage::new_rgb8(4, 4), ImageFormat::Jpeg);
        assert!(!jpeg_is_cmyk(&rgb));
        assert!(!jpeg_is_cmyk(b"\xFF\xD8\xFF"));
        assert!(!jpeg_is_cmyk(b"not a jpeg"));
        let out = process_raster(bytes.clone(), RasterOpts::for_ctx(&ctx())).unwrap();
        assert_ne!(out.bytes, bytes, "CMYK JPEG kept byte for byte");
        assert_eq!(out.ext, "jpg");
        assert!(!jpeg_is_cmyk(&out.bytes));
        let img = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!(img.color(), ColorType::Rgb8);
        // (0, 200, 200, 0) CMYK is red-ish.
        let p = img.to_rgb8().get_pixel(8, 6).0;
        assert!(p[0] > 200 && p[1] < 120 && p[2] < 120, "{p:?}");
    }

    #[test]
    fn srcset_and_picture_sources_are_fallbacks() {
        let mut ctx = ctx();
        let doc = dom::parse(
            r#"<img alt="only srcset" srcset="images/missing.png 1x, images/chart.png 2x">
            <picture><source srcset="images/sample.webp"><img src="gone.png" alt="pic"></picture>
            <img alt="none at all">"#,
        );
        crate::html::sanitize(&doc, &mut ctx).unwrap();
        transform(&doc, &mut ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        assert_eq!(out.matches("<img").count(), 2, "{out}");
        assert!(!out.contains("md2epub"), "{out}");
        assert!(
            out.contains(r#"alt="only srcset""#) && out.contains(r#"alt="pic""#),
            "{out}"
        );
        assert!(out.contains(">none at all<"), "{out}");
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        assert_eq!(
            srcset_candidates("a.png 1x, b.png 3x,c.png 2x, data:image/png;base64,AA 640w"),
            ["data:image/png;base64,AA", "b.png", "c.png", "a.png"]
        );
        assert_eq!(srcset_candidates("x.png, y.png"), ["x.png", "y.png"]);
        assert!(srcset_candidates("  , ").is_empty());
    }

    #[test]
    fn backslash_paths_resolve() {
        let mut ctx = ctx();
        let doc = dom::parse(
            r#"<img src="images\chart.png" alt="a"><p>![x](images%5Cchart.png)</p><img src="images%5Cchart.png" alt="b">"#,
        );
        crate::html::sanitize(&doc, &mut ctx).unwrap();
        transform(&doc, &mut ctx).unwrap();
        let out = dom::serialize_inner_xhtml(&dom::body(&doc).unwrap());
        assert_eq!(out.matches("../images/").count(), 2, "{out}");
        assert!(ctx.warnings().is_empty(), "{:?}", ctx.warnings());
    }

    #[test]
    fn css_comments_strings_imports_and_font_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(
            fixtures().join("images/chart.png"),
            dir.path().join("p.png"),
        )
        .unwrap();
        std::fs::write(dir.path().join("other.css"), "p{}").unwrap();
        let mut ctx = ctx();
        ctx.base_dir = dir.path().to_path_buf();
        let out = run(
            r#"<p>x</p><style>.d{content:"url(p.png)"} /* url(p.png) in comment */ @import url(other.css); @IMPORT "x.css" screen;
            @font-face{font-family:G;src:url(g.woff2) format("woff2");font-weight:bold} .q{display:flex} .b{background:url(p.png)}</style>"#,
            &mut ctx,
        );
        assert!(out.contains(r#".d{content:"url(p.png)"}"#), "{out}");
        assert!(out.contains("/* url(p.png) in comment */"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("@import"), "{out}");
        assert!(!out.contains("none"), "{out}");
        assert!(
            out.contains("@font-face{font-family:G;font-weight:bold}"),
            "{out}"
        );
        assert!(out.contains(".q{display:flex}"), "{out}");
        assert_eq!(ctx.resources().len(), 1);
        assert!(
            out.contains(&format!("url(\"../{}\")", ctx.resources()[0].href)),
            "{out}"
        );
        let w = ctx.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(
            w[0].contains("other.css") && w[0].contains("--css"),
            "{w:?}"
        );
        assert!(w[1].contains("x.css"), "{w:?}");
        assert!(w[2].contains("g.woff2"), "{w:?}");
    }

    #[test]
    fn kept_svgs_have_no_dangling_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(
            fixtures().join("images/chart.png"),
            dir.path().join("pat.png"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.svg"),
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><style>@import url(foo.css); rect{fill:url(pat.png)} .m{fill:url(missing.png)} .g{fill:url(#grad)}</style><use href="b.svg#shape"/><use xlink:href="#local"/><rect style="fill:url(pat.png)" width="5" height="5"/><style><![CDATA[ .c{background:url(pat.png)} ]]></style></svg>"##,
        )
        .unwrap();
        let mut ctx = ctx();
        ctx.base_dir = dir.path().to_path_buf();
        let out = run(r#"<img src="a.svg" alt="s">"#, &mut ctx);
        assert!(out.contains(".svg"), "{out}");
        let svg = ctx
            .resources()
            .iter()
            .find(|r| r.media_type == "image/svg+xml")
            .unwrap();
        let text = String::from_utf8(svg.data.clone()).unwrap();
        roxmltree::Document::parse(&text).unwrap();
        let png = ctx
            .resources()
            .iter()
            .find(|r| r.media_type == "image/png")
            .unwrap();
        let rel = format!("../{}", png.href);
        assert!(
            !text.contains("foo.css") && !text.contains("@import"),
            "{text}"
        );
        assert!(!text.contains("b.svg"), "{text}");
        assert!(text.contains("#local"), "{text}");
        assert!(text.contains("url(#grad)"), "{text}");
        assert!(
            !text.contains("pat.png") && !text.contains("missing.png"),
            "{text}"
        );
        assert_eq!(text.matches(&rel).count(), 3, "{text}");
        assert!(text.contains("<![CDATA["), "{text}");
        let w = ctx.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
    }
}

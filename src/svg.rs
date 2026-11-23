//! SVG rasterizing (owned by T5).
//!
//! Contract:
//! - [`rasterize`] renders an SVG document with `resvg`/`usvg`/`tiny-skia`
//!   onto an opaque white background, at `scale` times the SVG's nominal size
//!   (`width`/`height`, else the `viewBox`), shrunk to fit within
//!   `max_w × max_h` pixels, and returns PNG bytes plus the pixel size.
//!   Relative image references inside the SVG resolve against
//!   `resources_dir`. Malformed input returns an error, never panics.
//! - [`intrinsic_size`] returns the nominal size in CSS pixels, for callers
//!   that keep the SVG as-is.
//! - Text uses system fonts plus an embedded open-licensed fallback sans font
//!   ([`FALLBACK_FAMILY`], Source Sans 3 under the SIL OFL 1.1, see
//!   `assets/fonts/SourceSans3-OFL.md`) set as the default family. The
//!   generic `sans-serif` family maps to it, and any family that cannot be
//!   found falls back to it, so text renders the same on every OS.
//!
//! The output PNG is 8-bit RGB (no alpha channel), which is what the strict
//! device profiles need.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use usvg::fontdb;

/// Embedded fallback font (Source Sans 3 Regular, SIL Open Font License 1.1).
static FALLBACK_FONT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/SourceSans3-Regular.ttf"
));

/// License text of the embedded fallback font, shipped in the binary with it.
pub static FALLBACK_FONT_LICENSE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/SourceSans3-OFL.md"
));

/// Family name of the embedded fallback font.
pub const FALLBACK_FAMILY: &str = "Source Sans 3";

/// Largest raster edge we are willing to allocate, whatever the caller asks.
const MAX_EDGE: u32 = 8_192;

/// Largest raster area (pixels) we are willing to allocate (about 128 MB of
/// RGBA canvas).
const MAX_PIXELS: f32 = 32_000_000.0;

/// Scale used when the caller passes a non-finite or non-positive one.
const DEFAULT_SCALE: f32 = 2.0;

/// A rendered PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raster {
    /// PNG bytes (opaque, white background).
    pub png: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Renders `svg` to an opaque PNG. See the module docs for the contract.
pub fn rasterize(
    svg: &[u8],
    resources_dir: Option<&Path>,
    max_w: u32,
    max_h: u32,
    scale: f32,
) -> Result<Raster> {
    let opts = options(resources_dir, Some(font_db()));
    let tree = parse(svg, &opts)?;

    let size = tree.size();
    let (nw, nh) = (size.width(), size.height());
    if !(nw.is_finite() && nh.is_finite() && nw > 0.0 && nh > 0.0) {
        bail!("SVG has an invalid size ({nw}×{nh})");
    }
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        DEFAULT_SCALE
    };
    let (pw, ph) = target_size(
        nw,
        nh,
        scale,
        max_w.clamp(1, MAX_EDGE),
        max_h.clamp(1, MAX_EDGE),
    );

    let mut pixmap = tiny_skia::Pixmap::new(pw, ph)
        .ok_or_else(|| anyhow!("cannot allocate a {pw}×{ph} canvas"))?;
    pixmap.fill(tiny_skia::Color::WHITE);
    let transform = tiny_skia::Transform::from_scale(pw as f32 / nw, ph as f32 / nh);
    catch_unwind(AssertUnwindSafe(|| {
        resvg::render(&tree, transform, &mut pixmap.as_mut())
    }))
    .map_err(|_| anyhow!("SVG renderer failed on this document"))?;

    // The canvas is opaque white underneath, so every pixel is fully opaque
    // and premultiplied RGB equals straight RGB: just drop the alpha byte.
    let rgb: Vec<u8> = pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, _]| [r, g, b])
        .collect();
    let img = image::RgbImage::from_raw(pw, ph, rgb)
        .ok_or_else(|| anyhow!("raster buffer has an unexpected size"))?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding rasterized SVG as PNG")?;
    Ok(Raster {
        png,
        width: pw,
        height: ph,
    })
}

/// Nominal `(width, height)` of an SVG in CSS pixels: `width`/`height` when
/// present, else the `viewBox` size, else usvg's 100×100 default.
pub fn intrinsic_size(svg: &[u8]) -> Result<(f32, f32)> {
    // No fonts needed to read the size; skip the (slow) system font scan.
    let opts = options(None, None);
    let tree = parse(svg, &opts)?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        bail!("SVG has an invalid size ({w}×{h})");
    }
    Ok((w, h))
}

/// Pixel size for a nominal size at `scale`, shrunk (keeping the aspect
/// ratio) to fit within `max_w × max_h`. Never returns a zero edge.
fn target_size(nw: f32, nh: f32, scale: f32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (mut w, mut h) = (nw * scale, nh * scale);
    let fit = (max_w as f32 / w)
        .min(max_h as f32 / h)
        .min((MAX_PIXELS / (w * h)).sqrt());
    if fit < 1.0 {
        w *= fit;
        h *= fit;
    }
    let px = |v: f32, max: u32| (v.round().clamp(1.0, max as f32)) as u32;
    (px(w, max_w), px(h, max_h))
}

/// Parses with usvg; if that fails and the root `<svg>` lacks its namespace
/// declarations (common for inline SVG copied from HTML), retries once with
/// them added.
fn parse(svg: &[u8], opts: &usvg::Options) -> Result<usvg::Tree> {
    let first = match catch_unwind(AssertUnwindSafe(|| usvg::Tree::from_data(svg, opts))) {
        Ok(r) => r,
        Err(_) => bail!("SVG parser failed on this document"),
    };
    match first {
        Ok(tree) => Ok(tree),
        Err(err) => {
            let fixed = std::str::from_utf8(svg).ok().and_then(add_namespaces);
            if let Some(fixed) = fixed
                && let Ok(Ok(tree)) =
                    catch_unwind(AssertUnwindSafe(|| usvg::Tree::from_str(&fixed, opts)))
            {
                return Ok(tree);
            }
            Err(anyhow!("invalid SVG: {err}"))
        }
    }
}

/// Adds missing `xmlns` / `xmlns:xlink` declarations to the root `<svg>`
/// start tag. Returns `None` when there is nothing to add.
fn add_namespaces(text: &str) -> Option<String> {
    let start = find_svg_start(text)?;
    let tag_end = start + text[start..].find('>')?;
    let tag = &text[start..tag_end];
    let mut extra = String::new();
    if !tag.contains("xmlns=") {
        extra.push_str(r#" xmlns="http://www.w3.org/2000/svg""#);
    }
    if text.contains("xlink:") && !tag.contains("xmlns:xlink") {
        extra.push_str(r#" xmlns:xlink="http://www.w3.org/1999/xlink""#);
    }
    if extra.is_empty() {
        return None;
    }
    let insert_at = start + "<svg".len();
    let mut out = String::with_capacity(text.len() + extra.len());
    out.push_str(&text[..insert_at]);
    out.push_str(&extra);
    out.push_str(&text[insert_at..]);
    Some(out)
}

/// Byte offset of the first `<svg` start tag (followed by whitespace, `>` or
/// `/`), so `<svgfoo` is not mistaken for it.
fn find_svg_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = text[from..].find("<svg") {
        let at = from + i;
        match text[at + 4..].chars().next() {
            Some(c) if c.is_whitespace() || c == '>' || c == '/' => return Some(at),
            None => return None,
            _ => from = at + 4,
        }
    }
    None
}

/// usvg options with our font setup. `fonts: None` uses an empty database
/// (enough for size queries).
fn options(
    resources_dir: Option<&Path>,
    fonts: Option<Arc<fontdb::Database>>,
) -> usvg::Options<'static> {
    let mut opts = usvg::Options {
        resources_dir: resources_dir.map(Path::to_path_buf),
        font_family: FALLBACK_FAMILY.to_string(),
        ..usvg::Options::default()
    };
    if let Some(db) = fonts {
        opts.fontdb = db;
        opts.font_resolver = usvg::FontResolver {
            select_font: Box::new(select_font),
            select_fallback: usvg::FontResolver::default_fallback_selector(),
        };
    }
    opts
}

/// Font selection: the requested families first; when none of them exists
/// on this machine, the embedded fallback (instead of usvg's default of the
/// generic serif family, which may be missing and would drop the text).
fn select_font(font: &usvg::Font, db: &mut Arc<fontdb::Database>) -> Option<fontdb::ID> {
    use usvg::FontFamily;
    let mut families: Vec<fontdb::Family<'_>> = font
        .families()
        .iter()
        .map(|f| match f {
            FontFamily::Serif => fontdb::Family::Serif,
            FontFamily::SansSerif => fontdb::Family::SansSerif,
            FontFamily::Cursive => fontdb::Family::Cursive,
            FontFamily::Fantasy => fontdb::Family::Fantasy,
            FontFamily::Monospace => fontdb::Family::Monospace,
            FontFamily::Named(s) => fontdb::Family::Name(s),
        })
        .collect();
    families.push(fontdb::Family::Name(FALLBACK_FAMILY));

    let stretch = match font.stretch() {
        usvg::FontStretch::UltraCondensed => fontdb::Stretch::UltraCondensed,
        usvg::FontStretch::ExtraCondensed => fontdb::Stretch::ExtraCondensed,
        usvg::FontStretch::Condensed => fontdb::Stretch::Condensed,
        usvg::FontStretch::SemiCondensed => fontdb::Stretch::SemiCondensed,
        usvg::FontStretch::Normal => fontdb::Stretch::Normal,
        usvg::FontStretch::SemiExpanded => fontdb::Stretch::SemiExpanded,
        usvg::FontStretch::Expanded => fontdb::Stretch::Expanded,
        usvg::FontStretch::ExtraExpanded => fontdb::Stretch::ExtraExpanded,
        usvg::FontStretch::UltraExpanded => fontdb::Stretch::UltraExpanded,
    };
    let style = match font.style() {
        usvg::FontStyle::Normal => fontdb::Style::Normal,
        usvg::FontStyle::Italic => fontdb::Style::Italic,
        usvg::FontStyle::Oblique => fontdb::Style::Oblique,
    };
    db.query(&fontdb::Query {
        families: &families,
        weight: fontdb::Weight(font.weight()),
        stretch,
        style,
    })
}

/// The shared font database: system fonts plus the embedded fallback, with
/// the generic `sans-serif` family mapped to the fallback. Built once per
/// process (scanning system fonts is slow).
fn font_db() -> Arc<fontdb::Database> {
    static DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db.load_font_data(FALLBACK_FONT.to_vec());
        db.set_sans_serif_family(FALLBACK_FAMILY);
        Arc::new(db)
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> (Vec<u8>, PathBuf) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images");
        (std::fs::read(dir.join(name)).unwrap(), dir)
    }

    fn decode(r: &Raster) -> image::DynamicImage {
        let img = image::load_from_memory_with_format(&r.png, image::ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), (r.width, r.height));
        img
    }

    fn non_white(img: &image::DynamicImage) -> usize {
        img.to_rgb8()
            .pixels()
            .filter(|p| p.0 != [255, 255, 255])
            .count()
    }

    fn dark(img: &image::DynamicImage) -> usize {
        img.to_rgb8()
            .pixels()
            .filter(|p| p.0.iter().all(|&c| c < 128))
            .count()
    }

    #[test]
    fn rasterizes_fixture_with_text_at_2x() {
        let (svg, dir) = fixture("diagram.svg");
        let r = rasterize(&svg, Some(&dir), 1860, 2480, 2.0).unwrap();
        assert_eq!((r.width, r.height), (600, 240));
        let img = decode(&r);
        assert_eq!(img.color(), image::ColorType::Rgb8, "PNG must be opaque");
        assert!(non_white(&img) > 1000);
    }

    #[test]
    fn text_renders_with_embedded_font() {
        // A family that exists nowhere, plus the default (no font-family).
        for attrs in [
            r#"font-family="NoSuchFontAnywhere""#,
            "",
            "font-family=\"sans-serif\"",
        ] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="50"><text x="5" y="35" font-size="30" {attrs}>Hello</text></svg>"#
            );
            let r = rasterize(svg.as_bytes(), None, 1000, 1000, 1.0).unwrap();
            assert_eq!((r.width, r.height), (200, 50));
            assert!(dark(&decode(&r)) > 100, "text did not render for {attrs:?}");
        }
    }

    #[test]
    fn embedded_font_family_name_matches() {
        let mut db = fontdb::Database::new();
        db.load_font_data(FALLBACK_FONT.to_vec());
        let face = db.faces().next().expect("embedded font parses");
        assert!(face.families.iter().any(|(n, _)| n == FALLBACK_FAMILY));
        assert!(FALLBACK_FONT_LICENSE.contains("SIL OPEN FONT LICENSE"));
        let db = font_db();
        let id = db.query(&fontdb::Query {
            families: &[fontdb::Family::SansSerif],
            ..Default::default()
        });
        let face = db.face(id.unwrap()).unwrap();
        assert!(face.families.iter().any(|(n, _)| n == FALLBACK_FAMILY));
    }

    #[test]
    fn viewbox_only_uses_viewbox_size() {
        let (svg, dir) = fixture("viewbox-only.svg");
        assert_eq!(intrinsic_size(&svg).unwrap(), (200.0, 100.0));
        let r = rasterize(&svg, Some(&dir), 1860, 2480, 2.0).unwrap();
        assert_eq!((r.width, r.height), (400, 200));
        let img = decode(&r).to_rgb8();
        // Center of the blue circle.
        let p = img.get_pixel(200, 100).0;
        assert!(p[2] > p[0] + 50, "expected blue, got {p:?}");
    }

    #[test]
    fn embedded_base64_image_is_drawn() {
        let (svg, dir) = fixture("embedded-image.svg");
        let r = rasterize(&svg, Some(&dir), 1860, 2480, 2.0).unwrap();
        assert_eq!((r.width, r.height), (128, 96));
        let img = decode(&r).to_rgb8();
        // Background is #eee; the embedded PNG covers (8..40, 8..40) * 2.
        assert_eq!(img.get_pixel(4, 4).0, [0xee, 0xee, 0xee]);
        assert_ne!(img.get_pixel(48, 48).0, [0xee, 0xee, 0xee]);
        assert!(non_white(&decode(&r)) > 0);
    }

    #[test]
    fn linked_image_resolves_against_resources_dir() {
        let (svg, dir) = fixture("linked-image.svg");
        let with = rasterize(&svg, Some(&dir), 1860, 2480, 1.0).unwrap();
        let missing = std::env::temp_dir().join("md2epub-no-such-dir-for-svg-test");
        let without = rasterize(&svg, Some(&missing), 1860, 2480, 1.0).unwrap();
        assert_eq!((with.width, with.height), (120, 80));
        assert_ne!(
            decode(&with).to_rgb8().get_pixel(60, 40),
            decode(&without).to_rgb8().get_pixel(60, 40),
            "linked chart.png was not drawn"
        );
    }

    #[test]
    fn fits_within_max_box() {
        let (svg, dir) = fixture("diagram.svg");
        let r = rasterize(&svg, Some(&dir), 100, 100, 2.0).unwrap();
        assert_eq!((r.width, r.height), (100, 40));
        let r = rasterize(&svg, Some(&dir), 1000, 30, 2.0).unwrap();
        assert_eq!((r.width, r.height), (75, 30));

        let huge = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1e9" height="5e8"><rect width="10" height="10"/></svg>"#;
        let r = rasterize(huge, None, 1860, 2480, 2.0).unwrap();
        assert_eq!((r.width, r.height), (1860, 930));
    }

    #[test]
    fn odd_arguments_do_not_panic() {
        let (svg, _) = fixture("viewbox-only.svg");
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let r = rasterize(&svg, None, 1000, 1000, scale).unwrap();
            assert_eq!((r.width, r.height), (400, 200));
        }
        let r = rasterize(&svg, None, 0, 0, 2.0).unwrap();
        assert_eq!((r.width, r.height), (1, 1));
        let r = rasterize(&svg, None, u32::MAX, u32::MAX, 2.0).unwrap();
        assert_eq!((r.width, r.height), (400, 200));
        let tiny = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0.01" height="0.01"/>"#;
        let r = rasterize(tiny, None, 100, 100, 2.0).unwrap();
        assert_eq!((r.width, r.height), (1, 1));
    }

    #[test]
    fn target_size_caps_edges_and_area() {
        assert_eq!(target_size(300.0, 120.0, 2.0, 1860, 2480), (600, 240));
        let (w, h) = target_size(400.0, 200.0, 1000.0, MAX_EDGE, MAX_EDGE);
        assert!(w <= MAX_EDGE && h <= MAX_EDGE);
        assert!((w as f32) * (h as f32) <= MAX_PIXELS * 1.001);
        assert_eq!(w / h, 2);
        let (w, h) = target_size(10.0, 1e7, 2.0, MAX_EDGE, MAX_EDGE);
        assert!(w >= 1 && h == MAX_EDGE);
    }

    #[test]
    fn malformed_input_is_an_error() {
        let cases: [&[u8]; 7] = [
            b"",
            b"not svg at all",
            b"<svg",
            b"<html><body/></html>",
            b"\xff\xfe\x00garbage",
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect></svg>"#,
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="-5" height="10"/>"#,
        ];
        for svg in cases {
            assert!(rasterize(svg, None, 100, 100, 2.0).is_err(), "{svg:?}");
            assert!(intrinsic_size(svg).is_err(), "{svg:?}");
        }
    }

    #[test]
    fn missing_namespaces_are_added() {
        let svg = br##"<svg width="40" height="20"><rect width="40" height="20" fill="#000"/><use xlink:href="#x"/></svg>"##;
        assert_eq!(intrinsic_size(svg).unwrap(), (40.0, 20.0));
        let r = rasterize(svg, None, 100, 100, 1.0).unwrap();
        assert_eq!(dark(&decode(&r)), 800);
        assert_eq!(add_namespaces("<svgx/>"), None);
        assert_eq!(
            add_namespaces(r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#),
            None
        );
    }

    #[test]
    fn intrinsic_size_reads_width_and_height() {
        let (svg, _) = fixture("diagram.svg");
        assert_eq!(intrinsic_size(&svg).unwrap(), (300.0, 120.0));
        let (svg, _) = fixture("embedded-image.svg");
        assert_eq!(intrinsic_size(&svg).unwrap(), (64.0, 48.0));
    }
}

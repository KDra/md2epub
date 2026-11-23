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
//!   set as the default family.
//!
//! The T0 stub returns a 1×1 white PNG and a 1×1 size.

use std::path::Path;

use anyhow::{Context as _, Result};

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
    let _ = (svg, resources_dir, max_w, max_h, scale);
    let img = image::RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255]));
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding placeholder PNG")?;
    Ok(Raster {
        png,
        width: 1,
        height: 1,
    })
}

/// Nominal `(width, height)` of an SVG in CSS pixels.
pub fn intrinsic_size(svg: &[u8]) -> Result<(f32, f32)> {
    let _ = svg;
    Ok((1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_png() {
        let r = rasterize(b"<svg/>", None, 100, 100, 2.0).unwrap();
        assert!(r.png.starts_with(b"\x89PNG"));
        assert!(r.width >= 1 && r.height >= 1);
        assert!(intrinsic_size(b"<svg/>").is_ok());
    }
}

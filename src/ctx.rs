//! Per-run context shared by all pipeline stages: resolved options, the
//! resource store (content-hash deduplicated), book-wide id bookkeeping and
//! the warning list.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};

pub use crate::model::ImgRef;
use crate::model::{Options, Resource, SvgMode, media_type_for_ext};
use crate::svg;

/// Default render scale for rasterized SVGs (2x nominal size, crisp at 300 ppi).
pub const SVG_RASTER_SCALE: f32 = 2.0;

/// Re-encodes a PNG as 8-bit grayscale (used for `--grayscale`).
fn to_grayscale_png(png: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .context("decoding rasterized SVG")?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.to_luma8()
        .write_to(&mut out, image::ImageFormat::Png)
        .context("encoding grayscale PNG")?;
    Ok(out.into_inner())
}

/// Shared state for one conversion run.
#[derive(Debug)]
pub struct Ctx {
    /// Fully resolved options.
    pub opts: Options,
    /// Directory of the input currently being processed; relative asset
    /// paths resolve against it.
    pub base_dir: PathBuf,
    /// The input file currently being processed.
    pub current_input: PathBuf,
    /// Title hint found while processing (HTML `<title>` or first `<h1>`),
    /// used when no title was configured. First writer wins.
    pub title_hint: Option<String>,
    /// Element ids already used anywhere in the book (for unique heading ids).
    pub used_ids: HashSet<String>,
    /// Number of chapter files allocated so far (see [`Ctx::next_chapter_file`]).
    pub chapter_count: usize,
    resources: Vec<Resource>,
    by_href: HashMap<String, usize>,
    warnings: Vec<String>,
}

impl Ctx {
    /// Creates a context with an empty resource store.
    pub fn new(opts: Options) -> Self {
        Ctx {
            opts,
            base_dir: PathBuf::from("."),
            current_input: PathBuf::new(),
            title_hint: None,
            used_ids: HashSet::new(),
            chapter_count: 0,
            resources: Vec::new(),
            by_href: HashMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Stores image bytes under `images/<hash>.<ext>` (deduplicated by content
    /// hash) and returns the chapter-relative `src`, `../images/<hash>.<ext>`.
    ///
    /// `ext` is lowercased; `jpeg` is normalized to `jpg`. The bytes are stored
    /// as-is: conversion, downscaling and alpha flattening are the caller's job.
    pub fn add_image(&mut self, bytes: Vec<u8>, ext: &str) -> String {
        let mut ext = ext.trim_start_matches('.').to_ascii_lowercase();
        if ext == "jpeg" {
            ext = "jpg".into();
        }
        let href = format!("images/{}.{}", content_hash(&bytes), ext);
        if !self.by_href.contains_key(&href) {
            self.by_href.insert(href.clone(), self.resources.len());
            self.resources.push(Resource {
                href: href.clone(),
                media_type: media_type_for_ext(&ext).to_string(),
                data: bytes,
            });
        }
        format!("../{href}")
    }

    /// Stores an SVG document according to `opts.svg`, resolving its relative
    /// references against `base_dir`. See [`Ctx::add_vector_in`].
    pub fn add_vector(&mut self, svg: &str) -> Result<ImgRef> {
        let dir = self.base_dir.clone();
        self.add_vector_in(svg, Some(&dir))
    }

    /// Stores an SVG document according to `opts.svg`:
    ///
    /// - `SvgMode::Rasterize`: renders through [`svg::rasterize`] at
    ///   [`SVG_RASTER_SCALE`] within the profile screen and stores an opaque PNG.
    /// - `SvgMode::Keep`: stores the SVG as `image/svg+xml`, sized with
    ///   [`svg::intrinsic_size`].
    ///
    /// `resources_dir` is where the SVG's own relative image references
    /// resolve. The returned size is the display size in CSS pixels.
    pub fn add_vector_in(&mut self, svg: &str, resources_dir: Option<&Path>) -> Result<ImgRef> {
        let (w, h) = (self.opts.profile.screen_w, self.opts.profile.screen_h);
        match self.opts.svg {
            SvgMode::Rasterize => {
                let raster = svg::rasterize(svg.as_bytes(), resources_dir, w, h, SVG_RASTER_SCALE)
                    .context("rasterizing SVG")?;
                let png = if self.opts.grayscale {
                    to_grayscale_png(&raster.png)?
                } else {
                    raster.png
                };
                let src = self.add_image(png, "png");
                Ok(ImgRef {
                    src,
                    width_px: ((raster.width as f32 / SVG_RASTER_SCALE).round() as u32).max(1),
                    height_px: ((raster.height as f32 / SVG_RASTER_SCALE).round() as u32).max(1),
                })
            }
            SvgMode::Keep => {
                let (iw, ih) = svg::intrinsic_size(svg.as_bytes()).context("reading SVG size")?;
                let (dw, dh) = fit_within(iw, ih, w as f32, h as f32);
                let src = self.add_image(svg.as_bytes().to_vec(), "svg");
                Ok(ImgRef {
                    src,
                    width_px: dw,
                    height_px: dh,
                })
            }
        }
    }

    /// Adds an arbitrary resource at an explicit `href` (relative to
    /// `OEBPS/`). Replaces an existing resource with the same `href`.
    pub fn add_resource(&mut self, href: &str, media_type: &str, data: Vec<u8>) {
        let res = Resource {
            href: href.to_string(),
            media_type: media_type.to_string(),
            data,
        };
        if let Some(&i) = self.by_href.get(href) {
            self.resources[i] = res;
        } else {
            self.by_href.insert(href.to_string(), self.resources.len());
            self.resources.push(res);
        }
    }

    /// Resources stored so far, in insertion order.
    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// Moves the stored resources out (used when building the `Book`).
    pub fn take_resources(&mut self) -> Vec<Resource> {
        self.by_href.clear();
        std::mem::take(&mut self.resources)
    }

    /// Records a non-fatal problem. Printed by the pipeline at the end of the
    /// run (and immediately in verbose mode).
    pub fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if self.opts.verbose {
            eprintln!("warning: {msg}");
        }
        self.warnings.push(msg);
    }

    /// Prints a progress message to stderr in verbose mode.
    pub fn info(&self, msg: impl AsRef<str>) {
        if self.opts.verbose {
            eprintln!("{}", msg.as_ref());
        }
    }

    /// Warnings recorded so far.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Allocates the next chapter file name: `ch001.xhtml`, `ch002.xhtml`, ...
    /// numbered across the whole book.
    pub fn next_chapter_file(&mut self) -> String {
        self.chapter_count += 1;
        format!("ch{:03}.xhtml", self.chapter_count)
    }

    /// Returns `base` if unused in the book, else `base-2`, `base-3`, ...
    /// and records the result in [`Ctx::used_ids`].
    pub fn unique_id(&mut self, base: &str) -> String {
        let base = if base.is_empty() { "id" } else { base };
        let mut id = base.to_string();
        let mut n = 1;
        while self.used_ids.contains(&id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        self.used_ids.insert(id.clone());
        id
    }
}

/// Short, stable content hash (first 16 hex chars of SHA-256).
pub fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(&digest[..8])
}

/// Scales `(w, h)` down (never up) to fit in `(max_w, max_h)`, keeping the
/// aspect ratio. Returns integer pixels, at least 1.
pub fn fit_within(w: f32, h: f32, max_w: f32, max_h: f32) -> (u32, u32) {
    let (w, h) = (w.max(1.0), h.max(1.0));
    let scale = (max_w / w).min(max_h / h).min(1.0);
    (
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_image_dedupes() {
        let mut ctx = Ctx::new(Options::default());
        let a = ctx.add_image(vec![1, 2, 3], "PNG");
        let b = ctx.add_image(vec![1, 2, 3], "png");
        let c = ctx.add_image(vec![4], "jpeg");
        assert_eq!(a, b);
        assert!(a.starts_with("../images/") && a.ends_with(".png"));
        assert!(c.ends_with(".jpg"));
        assert_eq!(ctx.resources().len(), 2);
        assert_eq!(ctx.resources()[1].media_type, "image/jpeg");
        assert_eq!(ctx.take_resources().len(), 2);
        assert!(ctx.resources().is_empty());
    }

    #[test]
    fn add_vector_modes() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20"/></svg>"#;
        let mut ctx = Ctx::new(Options::default());
        ctx.opts.svg = SvgMode::Keep;
        let r = ctx.add_vector(svg).unwrap();
        assert!(r.src.ends_with(".svg"));
        assert_eq!(ctx.resources()[0].media_type, "image/svg+xml");

        ctx.opts.svg = SvgMode::Rasterize;
        let r = ctx.add_vector(svg).unwrap();
        assert!(r.src.ends_with(".png"));
        assert!(r.width_px >= 1 && r.height_px >= 1);
        assert_eq!(ctx.resources()[1].media_type, "image/png");

        ctx.opts.grayscale = true;
        let color = r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="red"/></svg>"#;
        let r = ctx.add_vector(color).unwrap();
        let res = ctx.resources().last().unwrap();
        assert!(r.src.ends_with(".png"));
        let img = image::load_from_memory(&res.data).unwrap();
        assert_eq!(img.color(), image::ColorType::L8);
    }

    #[test]
    fn ids_chapters_warnings() {
        let mut ctx = Ctx::new(Options::default());
        assert_eq!(ctx.unique_id("intro"), "intro");
        assert_eq!(ctx.unique_id("intro"), "intro-2");
        assert_eq!(ctx.unique_id(""), "id");
        assert_eq!(ctx.next_chapter_file(), "ch001.xhtml");
        assert_eq!(ctx.next_chapter_file(), "ch002.xhtml");
        ctx.warn("x");
        assert_eq!(ctx.warnings(), ["x".to_string()]);
        ctx.add_resource("css/a.css", "text/css", vec![1]);
        ctx.add_resource("css/a.css", "text/css", vec![2]);
        assert_eq!(ctx.resources().len(), 1);
        assert_eq!(ctx.resources()[0].data, vec![2]);
    }

    #[test]
    fn fit() {
        assert_eq!(fit_within(100.0, 50.0, 1000.0, 1000.0), (100, 50));
        assert_eq!(fit_within(4000.0, 2000.0, 1000.0, 1000.0), (1000, 500));
        assert_eq!(fit_within(0.0, 0.0, 10.0, 10.0), (1, 1));
    }
}

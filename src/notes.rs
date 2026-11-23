//! Note pages for handwriting (owned by T8).
//!
//! [`insert`]: for `NotesPlacement::AfterChapter` or `End`, adds
//! `opts.notes_pages` note pages (after each content chapter, or at the end of
//! the book). Each page is its own chapter (`in_toc: false`, except one
//! "Notes" TOC entry in `End` mode, `source: None`, file name from
//! `ctx.next_chapter_file()`) whose body is a single full-page PNG generated
//! at the profile resolution in the chosen `NotesStyle`:
//!
//! - `blank`: white with a faint corner mark, so readers do not collapse the
//!   page as empty;
//! - `lined`: horizontal ruled lines;
//! - `dotted`: a dot grid;
//! - `grid`: a square grid.
//!
//! Patterns are drawn in light gray on an opaque 8-bit grayscale PNG (no
//! alpha, so it is safe under every profile). The image is generated once per
//! run and every note page references the same resource (`ctx.add_image`
//! also dedupes by content). Page breaks before and after come from the
//! `.notes-page` rules in `assets/base.css`.

use std::io::Cursor;

use anyhow::{Context as _, Result};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, GrayImage, ImageEncoder, Luma};

use crate::ctx::Ctx;
use crate::model::{Chapter, NotesPlacement, NotesStyle};

/// Title used for note page chapters (and the single TOC entry in `End` mode).
pub const NOTES_TITLE: &str = "Notes";

/// Upper bound on the note image side, to keep memory sane on bad profiles.
const MAX_SIDE: u32 = 8000;

/// Upper bound on pages per insertion point.
const MAX_PAGES: u32 = 200;

/// Page background.
const WHITE: u8 = 255;
/// Ruled lines and grid lines: light gray, still visible on 16-level e-ink.
const LINE_GRAY: u8 = 190;
/// Dots are small, so slightly darker than lines.
const DOT_GRAY: u8 = 160;
/// The blank page's corner mark: barely visible.
const MARK_GRAY: u8 = 225;

/// Inserts note pages into the chapter list. See the module docs.
pub fn insert(chapters: Vec<Chapter>, ctx: &mut Ctx) -> Result<Vec<Chapter>> {
    let placement = ctx.opts.notes;
    if placement == NotesPlacement::None {
        return Ok(chapters);
    }
    let mut pages = ctx.opts.notes_pages;
    if pages == 0 {
        return Ok(chapters);
    }
    if pages > MAX_PAGES {
        ctx.warn(format!(
            "--notes-pages {pages} is too many; using {MAX_PAGES} per insertion point"
        ));
        pages = MAX_PAGES;
    }

    let (w, h) = page_size(ctx.opts.profile.screen_w, ctx.opts.profile.screen_h);
    let png = render_png(ctx.opts.notes_style, w, h).context("generating note page image")?;
    let page = NotePage {
        src: ctx.add_image(png, "png"),
        width: w,
        height: h,
        style: ctx.opts.notes_style,
    };
    ctx.info(format!(
        "adding {pages} {} note page(s) {}",
        page.style,
        match placement {
            NotesPlacement::End => "at the end",
            _ => "after each chapter",
        }
    ));

    let mut out = Vec::with_capacity(chapters.len() * (pages as usize + 1));
    match placement {
        // Returned early above; kept so the match stays exhaustive.
        NotesPlacement::None => out.extend(chapters),
        NotesPlacement::AfterChapter => {
            for ch in chapters {
                out.push(ch);
                for _ in 0..pages {
                    out.push(page.chapter(ctx, false));
                }
            }
        }
        NotesPlacement::End => {
            out.extend(chapters);
            for i in 0..pages {
                out.push(page.chapter(ctx, i == 0));
            }
        }
    }
    Ok(out)
}

/// The stored note image shared by every note page.
struct NotePage {
    src: String,
    width: u32,
    height: u32,
    style: NotesStyle,
}

impl NotePage {
    /// Builds one note page chapter with a fresh file name.
    fn chapter(&self, ctx: &mut Ctx, in_toc: bool) -> Chapter {
        let file_name = ctx.next_chapter_file();
        let body_xhtml = format!(
            "<div class=\"notes-page notes-{style}\">\
             <img src=\"{src}\" alt=\"{alt}\" width=\"{w}\" height=\"{h}\"/>\
             </div>",
            style = self.style,
            src = self.src,
            alt = alt_text(self.style),
            w = self.width,
            h = self.height,
        );
        Chapter {
            title: NOTES_TITLE.to_string(),
            file_name,
            body_xhtml,
            headings: Vec::new(),
            in_toc,
            source: None,
        }
    }
}

/// Alt text for a note page image.
fn alt_text(style: NotesStyle) -> &'static str {
    match style {
        NotesStyle::Blank => "Blank note page",
        NotesStyle::Lined => "Lined note page",
        NotesStyle::Dotted => "Dotted note page",
        NotesStyle::Grid => "Grid note page",
    }
}

/// Note image size: the profile screen, clamped to `1..=MAX_SIDE`.
fn page_size(w: u32, h: u32) -> (u32, u32) {
    (w.clamp(1, MAX_SIDE), h.clamp(1, MAX_SIDE))
}

/// Renders a note page pattern as an opaque 8-bit grayscale PNG.
pub fn render_png(style: NotesStyle, width: u32, height: u32) -> Result<Vec<u8>> {
    let (width, height) = page_size(width, height);
    let img = render(style, width, height);
    let mut buf = Cursor::new(Vec::new());
    PngEncoder::new_with_quality(&mut buf, CompressionType::Best, FilterType::Adaptive)
        .write_image(img.as_raw(), width, height, ExtendedColorType::L8)
        .context("encoding PNG")?;
    Ok(buf.into_inner())
}

/// Pattern geometry, scaled from the page width so every profile gets
/// roughly the same physical spacing (about 7 mm rules at 300 ppi).
struct Geometry {
    /// Distance between ruled lines.
    line_step: u32,
    /// Distance between grid lines and dots (a little tighter than rules).
    grid_step: u32,
    /// Line thickness in pixels.
    stroke: u32,
    /// Outer margin left free of the pattern.
    margin_x: u32,
    margin_top: u32,
    margin_bottom: u32,
}

impl Geometry {
    fn new(width: u32, height: u32) -> Self {
        let base = width.min(height.saturating_mul(3) / 4).max(1) as f32;
        let line_step = ((base / 22.0).round() as u32).max(4);
        let grid_step = ((base / 30.0).round() as u32).max(3);
        let stroke = ((base / 900.0).round() as u32).clamp(1, 6);
        let margin_x = ((width as f32) * 0.05).round() as u32;
        let margin_top = ((height as f32) * 0.06).round() as u32;
        let margin_bottom = ((height as f32) * 0.04).round() as u32;
        Geometry {
            line_step,
            grid_step,
            stroke,
            margin_x,
            margin_top,
            margin_bottom,
        }
    }
}

/// Draws the pattern into a grayscale image.
fn render(style: NotesStyle, width: u32, height: u32) -> GrayImage {
    let mut img = GrayImage::from_pixel(width, height, Luma([WHITE]));
    let g = Geometry::new(width, height);
    let x0 = g.margin_x;
    let x1 = width.saturating_sub(g.margin_x);
    let y0 = g.margin_top;
    let y1 = height.saturating_sub(g.margin_bottom);

    match style {
        NotesStyle::Blank => draw_corner_mark(&mut img, &g),
        NotesStyle::Lined => {
            let mut y = y0 + g.line_step;
            while y + g.stroke <= y1 {
                fill_rect(&mut img, x0, y, x1, y + g.stroke, LINE_GRAY);
                y += g.line_step;
            }
        }
        NotesStyle::Grid => {
            // Snap the grid so it closes on a full square in both directions.
            let right = last_line_end(x0, x1, g.grid_step, g.stroke);
            let bottom = last_line_end(y0, y1, g.grid_step, g.stroke);
            let mut y = y0;
            while y < bottom {
                fill_rect(&mut img, x0, y, right, y + g.stroke, LINE_GRAY);
                y += g.grid_step;
            }
            let mut x = x0;
            while x < right {
                fill_rect(&mut img, x, y0, x + g.stroke, bottom, LINE_GRAY);
                x += g.grid_step;
            }
        }
        NotesStyle::Dotted => {
            let r = (g.stroke * 2).max(2);
            let mut y = y0;
            while y + r <= y1 {
                let mut x = x0;
                while x + r <= x1 {
                    fill_rect(&mut img, x, y, x + r, y + r, DOT_GRAY);
                    x += g.grid_step;
                }
                y += g.grid_step;
            }
        }
    }
    img
}

/// End coordinate (exclusive) of the last `stroke`-wide line of a series
/// starting at `start` every `step` pixels that still fits before `limit`.
/// Returns `start` when not even one line fits.
fn last_line_end(start: u32, limit: u32, step: u32, stroke: u32) -> u32 {
    if start + stroke > limit {
        return start;
    }
    let n = (limit - start - stroke) / step.max(1);
    start + n * step.max(1) + stroke
}

/// A small, faint L-shaped mark in the bottom-right corner, so a blank page is
/// not treated as empty and collapsed by a reader.
fn draw_corner_mark(img: &mut GrayImage, g: &Geometry) {
    let (w, h) = img.dimensions();
    let len = (g.line_step / 2).max(2);
    let s = g.stroke.max(1);
    let right = w.saturating_sub(g.margin_x / 2).max(1);
    let bottom = h.saturating_sub(g.margin_bottom / 2).max(1);
    let left = right.saturating_sub(len);
    let top = bottom.saturating_sub(len);
    // Horizontal arm along the bottom, vertical arm up the right side.
    fill_rect(
        img,
        left,
        bottom.saturating_sub(s),
        right,
        bottom,
        MARK_GRAY,
    );
    fill_rect(img, right.saturating_sub(s), top, right, bottom, MARK_GRAY);
}

/// Fills `[x0, x1) × [y0, y1)` (clipped to the image) with `value`.
fn fill_rect(img: &mut GrayImage, x0: u32, y0: u32, x1: u32, y1: u32, value: u8) {
    let (w, h) = img.dimensions();
    for y in y0.min(h)..y1.min(h) {
        for x in x0.min(w)..x1.min(w) {
            img.put_pixel(x, y, Luma([value]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Heading, Options};
    use crate::profile;

    fn content(n: usize, ctx: &mut Ctx) -> Vec<Chapter> {
        (0..n)
            .map(|i| Chapter {
                title: format!("Chapter {i}"),
                file_name: ctx.next_chapter_file(),
                body_xhtml: format!("<h1 id=\"c{i}\">Chapter {i}</h1><p>text</p>"),
                headings: vec![Heading {
                    level: 1,
                    text: format!("Chapter {i}"),
                    id: format!("c{i}"),
                }],
                in_toc: true,
                source: Some(format!("in{i}.md").into()),
            })
            .collect()
    }

    fn ctx_with(notes: NotesPlacement, style: NotesStyle, pages: u32) -> Ctx {
        let opts = Options {
            notes,
            notes_style: style,
            notes_pages: pages,
            ..Options::default()
        };
        Ctx::new(opts)
    }

    fn is_note(ch: &Chapter) -> bool {
        ch.source.is_none() && ch.body_xhtml.contains("notes-page")
    }

    #[test]
    fn none_mode_keeps_chapters() {
        let mut ctx = ctx_with(NotesPlacement::None, NotesStyle::Lined, 3);
        let chapters = content(3, &mut ctx);
        let out = insert(chapters.clone(), &mut ctx).unwrap();
        assert_eq!(out, chapters);
        assert!(ctx.resources().is_empty());
    }

    #[test]
    fn zero_pages_adds_nothing() {
        for mode in [NotesPlacement::AfterChapter, NotesPlacement::End] {
            let mut ctx = ctx_with(mode, NotesStyle::Lined, 0);
            let chapters = content(2, &mut ctx);
            let out = insert(chapters.clone(), &mut ctx).unwrap();
            assert_eq!(out, chapters);
            assert!(ctx.resources().is_empty());
        }
    }

    #[test]
    fn after_chapter_placement() {
        let mut ctx = ctx_with(NotesPlacement::AfterChapter, NotesStyle::Lined, 2);
        let chapters = content(3, &mut ctx);
        let out = insert(chapters, &mut ctx).unwrap();
        assert_eq!(out.len(), 3 + 3 * 2);
        let pattern: Vec<bool> = out.iter().map(is_note).collect();
        assert_eq!(
            pattern,
            [false, true, true, false, true, true, false, true, true]
        );
        assert!(out.iter().filter(|c| is_note(c)).all(|c| !c.in_toc));
        assert!(out.iter().filter(|c| !is_note(c)).all(|c| c.in_toc));
        // One shared image resource.
        assert_eq!(ctx.resources().len(), 1);
        // File names are unique and continue the book numbering.
        let mut names: Vec<&str> = out.iter().map(|c| c.file_name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), out.len());
        assert_eq!(out[1].file_name, "ch004.xhtml");
    }

    #[test]
    fn end_placement_has_one_toc_entry() {
        let mut ctx = ctx_with(NotesPlacement::End, NotesStyle::Grid, 3);
        let chapters = content(2, &mut ctx);
        let out = insert(chapters, &mut ctx).unwrap();
        assert_eq!(out.len(), 5);
        assert!(!is_note(&out[0]) && !is_note(&out[1]));
        assert!(out[2..].iter().all(is_note));
        let toc: Vec<&Chapter> = out[2..].iter().filter(|c| c.in_toc).collect();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].title, NOTES_TITLE);
        assert!(out[2].in_toc);
        assert_eq!(ctx.resources().len(), 1);
    }

    #[test]
    fn end_placement_with_no_chapters() {
        let mut ctx = ctx_with(NotesPlacement::End, NotesStyle::Blank, 1);
        let out = insert(Vec::new(), &mut ctx).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].in_toc);
        let mut ctx = ctx_with(NotesPlacement::AfterChapter, NotesStyle::Blank, 1);
        assert!(insert(Vec::new(), &mut ctx).unwrap().is_empty());
    }

    #[test]
    fn page_count_is_capped() {
        let mut ctx = ctx_with(NotesPlacement::End, NotesStyle::Blank, MAX_PAGES + 50);
        let out = insert(Vec::new(), &mut ctx).unwrap();
        assert_eq!(out.len(), MAX_PAGES as usize);
        assert_eq!(ctx.warnings().len(), 1);
    }

    #[test]
    fn note_body_is_well_formed_and_references_image() {
        let mut ctx = ctx_with(NotesPlacement::End, NotesStyle::Dotted, 1);
        let out = insert(Vec::new(), &mut ctx).unwrap();
        let body = &out[0].body_xhtml;
        crate::dom::tests_support::assert_well_formed(body);
        let href = &ctx.resources()[0].href;
        assert!(body.contains(&format!("src=\"../{href}\"")), "{body}");
        assert!(body.contains("class=\"notes-page notes-dotted\""));
        assert_eq!(ctx.resources()[0].media_type, "image/png");
    }

    #[test]
    fn image_matches_each_profile() {
        for name in ["scribe", "remarkable", "remarkable-pro", "generic"] {
            let profile = profile::by_name(name).unwrap();
            let (pw, ph) = (profile.screen_w, profile.screen_h);
            let opts = Options {
                profile,
                notes: NotesPlacement::AfterChapter,
                notes_style: NotesStyle::Lined,
                notes_pages: 1,
                ..Options::default()
            };
            let mut ctx = Ctx::new(opts);
            let chapters = content(2, &mut ctx);
            let out = insert(chapters, &mut ctx).unwrap();
            assert!(out[1].body_xhtml.contains(&format!("width=\"{pw}\"")));
            assert!(out[1].body_xhtml.contains(&format!("height=\"{ph}\"")));
            let img = image::load_from_memory(&ctx.resources()[0].data).unwrap();
            assert_eq!((img.width(), img.height()), (pw, ph), "{name}");
            assert!(!img.color().has_alpha(), "{name} note image has alpha");
        }
    }

    #[test]
    fn each_style_draws_light_gray_pattern() {
        let (w, h) = (600, 800);
        let mut pngs = Vec::new();
        for style in [
            NotesStyle::Blank,
            NotesStyle::Lined,
            NotesStyle::Dotted,
            NotesStyle::Grid,
        ] {
            let png = render_png(style, w, h).unwrap();
            let img = image::load_from_memory(&png).unwrap().to_luma8();
            assert_eq!(img.dimensions(), (w, h));
            let marked = img.pixels().filter(|p| p.0[0] != WHITE).count();
            assert!(marked > 0, "{style} page is empty");
            // Light gray only: nothing darker than the dots.
            assert!(img.pixels().all(|p| p.0[0] >= DOT_GRAY), "{style}");
            let share = marked as f64 / (w * h) as f64;
            match style {
                NotesStyle::Blank => assert!(share < 0.001, "blank too busy: {share}"),
                _ => assert!(share < 0.2, "{style} too dark: {share}"),
            }
            pngs.push(png);
        }
        // Every style looks different.
        for i in 0..pngs.len() {
            for j in i + 1..pngs.len() {
                assert_ne!(pngs[i], pngs[j]);
            }
        }
    }

    #[test]
    fn lined_has_many_rules() {
        let img = render(NotesStyle::Lined, 1860, 2480);
        let x = 930;
        let mut rules = 0;
        let mut prev_white = true;
        for y in 0..2480 {
            let white = img.get_pixel(x, y).0[0] == WHITE;
            if prev_white && !white {
                rules += 1;
            }
            prev_white = white;
        }
        assert!((20..=40).contains(&rules), "{rules} rules");
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (3, 2), (2, 50), (50, 2)] {
            for style in [
                NotesStyle::Blank,
                NotesStyle::Lined,
                NotesStyle::Dotted,
                NotesStyle::Grid,
            ] {
                let png = render_png(style, w, h).unwrap();
                let img = image::load_from_memory(&png).unwrap();
                assert_eq!(img.width(), w.max(1));
                assert_eq!(img.height(), h.max(1));
            }
        }
    }

    #[test]
    fn same_style_dedupes_across_runs() {
        let mut ctx = ctx_with(NotesPlacement::End, NotesStyle::Lined, 1);
        let a = insert(Vec::new(), &mut ctx).unwrap();
        let b = insert(Vec::new(), &mut ctx).unwrap();
        assert_eq!(ctx.resources().len(), 1);
        assert_ne!(a[0].file_name, b[0].file_name);
    }
}

//! Shared data types passed between pipeline stages.
//!
//! This module is the source of truth for the contracts described in
//! `TASKS.md` ("Types"). It has no dependencies on other modules so that
//! every stage can use it freely.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Result, bail};

/// How SVG images (referenced, inline, math, Mermaid) end up in the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SvgMode {
    /// Rasterize every SVG to an opaque PNG through `svg::rasterize`.
    Rasterize,
    /// Keep SVG files as `image/svg+xml` resources.
    #[default]
    Keep,
}

/// How LaTeX math placeholders are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MathMode {
    /// Render to an image (SVG, rasterized under `SvgMode::Rasterize`).
    #[default]
    Image,
    /// Convert to inline MathML.
    Mathml,
    /// Leave the LaTeX source in a `<code>` element.
    Off,
}

/// Where blank note pages are inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotesPlacement {
    /// No note pages.
    #[default]
    None,
    /// `notes_pages` pages after every content chapter.
    AfterChapter,
    /// `notes_pages` pages at the end of the book.
    End,
}

/// Background pattern of a note page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotesStyle {
    /// White page with a faint corner mark.
    Blank,
    /// Horizontal ruled lines.
    #[default]
    Lined,
    /// Dot grid.
    Dotted,
    /// Square grid.
    Grid,
}

/// Side margin preset applied by `style::build_css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Margin {
    /// No extra margin (the reader's own margins apply).
    #[default]
    Normal,
    /// About 8% side padding.
    Wide,
    /// About 15% side padding plus larger paragraph spacing.
    Extra,
}

/// Where the EPUB goes when no output path is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputLocation {
    /// `<first-input-stem>.epub` in the current directory.
    #[default]
    Cwd,
    /// `<first-input-stem>.epub` beside the first input file.
    Input,
}

macro_rules! str_enum {
    ($ty:ident, $what:literal, { $($variant:ident => [$($name:literal),+]),+ $(,)? }) => {
        impl $ty {
            /// Canonical lowercase name, as used on the command line.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($ty::$variant => str_enum!(@first $($name),+),)+
                }
            }

            /// All accepted canonical names, for help and error messages.
            pub fn names() -> &'static [&'static str] {
                &[$(str_enum!(@first $($name),+)),+]
            }
        }

        impl FromStr for $ty {
            type Err = anyhow::Error;

            fn from_str(s: &str) -> Result<Self> {
                let lower = s.trim().to_ascii_lowercase();
                $(
                    if [$($name),+].contains(&lower.as_str()) {
                        return Ok($ty::$variant);
                    }
                )+
                bail!(
                    "invalid {} '{}': expected one of {}",
                    $what,
                    s,
                    $ty::names().join(", ")
                )
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
    (@first $first:literal $(, $rest:literal)*) => { $first };
}

str_enum!(SvgMode, "svg mode", {
    Rasterize => ["rasterize", "raster", "png"],
    Keep => ["keep", "svg"],
});

str_enum!(MathMode, "math mode", {
    Image => ["image", "img", "svg"],
    Mathml => ["mathml"],
    Off => ["off", "none", "latex"],
});

str_enum!(NotesPlacement, "notes placement", {
    None => ["none", "off"],
    AfterChapter => ["after-chapter", "after_chapter", "chapter"],
    End => ["end"],
});

str_enum!(NotesStyle, "notes style", {
    Blank => ["blank"],
    Lined => ["lined", "lines", "ruled"],
    Dotted => ["dotted", "dots"],
    Grid => ["grid"],
});

str_enum!(Margin, "margin preset", {
    Normal => ["normal"],
    Wide => ["wide"],
    Extra => ["extra"],
});

str_enum!(OutputLocation, "output location", {
    Cwd => ["cwd", "current", "current-dir", "."],
    Input => ["input", "beside-input", "source"],
});

/// Parses a `--split-level` value: `1`, `2` or `none`.
///
/// Returns `Ok(None)` for `none`, `Ok(Some(n))` for 1 to 6.
pub fn parse_split_level(s: &str) -> Result<Option<u8>> {
    let s = s.trim().to_ascii_lowercase();
    if s == "none" || s == "off" || s == "0" {
        return Ok(None);
    }
    match s.parse::<u8>() {
        Ok(n @ 1..=6) => Ok(Some(n)),
        _ => bail!("invalid split level '{s}': expected 1, 2 (up to 6) or none"),
    }
}

/// A target device profile. Built by `profile::by_name`.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Profile name as given on the command line (`scribe`, `generic`, ...).
    pub name: String,
    /// Screen width in device pixels. Images are downscaled to fit.
    pub screen_w: u32,
    /// Screen height in device pixels.
    pub screen_h: u32,
    /// Default SVG handling for this device.
    pub svg: SvgMode,
    /// Default math handling for this device.
    pub math: MathMode,
    /// Whether images with alpha must be flattened onto white.
    pub flatten_alpha: bool,
}

impl Default for Profile {
    /// The `generic` profile.
    fn default() -> Self {
        Profile {
            name: "generic".into(),
            screen_w: 1600,
            screen_h: 2133,
            svg: SvgMode::Keep,
            math: MathMode::Image,
            flatten_alpha: false,
        }
    }
}

/// Book metadata written to the OPF.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    /// Book title. `config::resolve` leaves this empty when no flag, `book.toml`
    /// or front matter provides one; the pipeline then falls back to the first
    /// `<h1>` (or HTML `<title>`), and finally to the first input's file stem.
    pub title: String,
    /// Authors, in order. One `dc:creator` each.
    pub authors: Vec<String>,
    /// BCP 47 language tag, e.g. `en`. Defaults to `en`.
    pub lang: String,
    /// Explicit `dc:identifier`. When `None`, `epub::write` derives a UUID from
    /// the content hash.
    pub identifier: Option<String>,
    /// Publication date (free-form, ideally `YYYY-MM-DD`).
    pub date: Option<String>,
}

/// Fully resolved options for one run (see `config::resolve`).
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub profile: Profile,
    pub meta: Metadata,
    /// Cover image path (already resolved to be usable from the CWD).
    pub cover: Option<PathBuf>,
    /// Split chapters before headings of this level; `None` = one chapter per input.
    pub split_level: Option<u8>,
    pub notes: NotesPlacement,
    pub notes_style: NotesStyle,
    /// Number of note pages per insertion point.
    pub notes_pages: u32,
    pub margin: Margin,
    /// CSS `line-height` override.
    pub line_spacing: Option<f32>,
    pub math: MathMode,
    pub svg: SvgMode,
    /// Convert raster images to grayscale.
    pub grayscale: bool,
    /// Never touch the network (remote images are skipped with a warning).
    pub offline: bool,
    /// User stylesheet appended after the built-in CSS.
    pub extra_css: Option<PathBuf>,
    pub verbose: bool,
    /// Output path as given (`-o` or `book.toml`). `None` means "first
    /// input's stem + `.epub`" in the place `output_location` names.
    pub output: Option<PathBuf>,
    /// Where the EPUB goes when `output` is `None`.
    pub output_location: OutputLocation,
    /// Whether `meta.lang` was given explicitly (flag, `book.toml` or front
    /// matter). When it was not, the `<html lang>` of the first HTML input
    /// may set the book language.
    pub lang_given: bool,
    /// Add local files that chapters link to (documents, images, diagrams,
    /// text) to the book. See `linked`.
    pub follow_links: bool,
}

impl Default for Options {
    fn default() -> Self {
        let profile = Profile::default();
        Options {
            svg: profile.svg,
            math: profile.math,
            profile,
            meta: Metadata {
                lang: "en".into(),
                ..Metadata::default()
            },
            cover: None,
            split_level: Some(1),
            notes: NotesPlacement::None,
            notes_style: NotesStyle::Lined,
            notes_pages: 1,
            margin: Margin::Normal,
            line_spacing: None,
            grayscale: false,
            offline: false,
            extra_css: None,
            verbose: false,
            output: None,
            output_location: OutputLocation::Cwd,
            lang_given: false,
            follow_links: true,
        }
    }
}

/// A heading inside a chapter, used for the table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// 1 to 6.
    pub level: u8,
    pub text: String,
    /// The heading's `id` attribute (unique across the book).
    pub id: String,
}

/// One XHTML file in the spine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chapter {
    pub title: String,
    /// File name inside `OEBPS/text/`, e.g. `ch001.xhtml`.
    pub file_name: String,
    /// Serialized body content (children of `<body>`, no wrapper), well-formed XML.
    pub body_xhtml: String,
    pub headings: Vec<Heading>,
    /// Whether this chapter appears in the nav/NCX.
    pub in_toc: bool,
    /// The input file this chapter came from (`None` for generated chapters
    /// such as note pages). Used by `html::fix_links` to map cross-file links.
    pub source: Option<PathBuf>,
}

/// A binary resource stored in the book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Path relative to `OEBPS/`, e.g. `images/ab12cd.png`.
    pub href: String,
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    pub data: Vec<u8>,
}

/// A reference to an image stored through `Ctx::add_vector`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgRef {
    /// Chapter-relative `src`, e.g. `../images/ab12cd.png`.
    pub src: String,
    /// Display width in CSS pixels (the SVG's nominal size, fitted to the
    /// profile screen). For rasterized SVGs the PNG itself has more pixels.
    pub width_px: u32,
    /// Display height in CSS pixels.
    pub height_px: u32,
}

/// Everything `epub::write` needs.
#[derive(Debug, Clone, Default)]
pub struct Book {
    pub meta: Metadata,
    /// Chapters in spine order (cover page excluded; `epub::write` adds it).
    pub chapters: Vec<Chapter>,
    /// Images and other resources referenced from chapters.
    pub resources: Vec<Resource>,
    /// Full stylesheet, written to `OEBPS/css/base.css`.
    pub css: String,
    /// Cover image, if any. `epub::write` generates one when `None`.
    pub cover: Option<Resource>,
}

/// Returns the MIME type for a file extension (without the dot), or
/// `application/octet-stream` when unknown.
pub fn media_type_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "css" => "text/css",
        "xhtml" => "application/xhtml+xml",
        "ncx" => "application/x-dtbncx+xml",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_parse_and_display() {
        assert_eq!("rasterize".parse::<SvgMode>().unwrap(), SvgMode::Rasterize);
        assert_eq!("KEEP".parse::<SvgMode>().unwrap(), SvgMode::Keep);
        assert_eq!("mathml".parse::<MathMode>().unwrap(), MathMode::Mathml);
        assert_eq!(
            "after-chapter".parse::<NotesPlacement>().unwrap(),
            NotesPlacement::AfterChapter
        );
        assert_eq!("grid".parse::<NotesStyle>().unwrap(), NotesStyle::Grid);
        assert_eq!("extra".parse::<Margin>().unwrap(), Margin::Extra);
        assert_eq!(NotesPlacement::AfterChapter.to_string(), "after-chapter");
        let err = "bogus".parse::<Margin>().unwrap_err().to_string();
        assert!(err.contains("normal, wide, extra"), "{err}");
    }

    #[test]
    fn split_level() {
        assert_eq!(parse_split_level("1").unwrap(), Some(1));
        assert_eq!(parse_split_level("2").unwrap(), Some(2));
        assert_eq!(parse_split_level("none").unwrap(), None);
        assert!(parse_split_level("9").is_err());
        assert!(parse_split_level("x").is_err());
    }

    #[test]
    fn defaults_are_generic() {
        let o = Options::default();
        assert_eq!(o.profile.name, "generic");
        assert_eq!(o.svg, SvgMode::Keep);
        assert_eq!(o.meta.lang, "en");
        assert_eq!(media_type_for_ext("JPG"), "image/jpeg");
    }
}

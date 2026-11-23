//! Stylesheet generation (owned by T8).
//!
//! [`build_css`] concatenates, in order:
//!
//! 1. `assets/base.css` (embedded with `include_str!`),
//! 2. the margin preset (`normal` nothing, `wide` about 8% side padding,
//!    `extra` about 15% side padding plus larger paragraph spacing),
//! 3. `line_spacing` as `line-height`,
//! 4. the user's `--css` file, appended last so it wins the cascade.
//!
//! [`build_book_css`] (used by the pipeline) resolves the `url(...)`
//! references of the user file against its directory and stores the fonts
//! and images it names in the book (see `assets::rewrite_user_css`);
//! [`build_css`] appends it verbatim.
//!
//! The margin presets pad `body`; note pages (`<body class="notes">`) are
//! exempt so their page image keeps the full width.
//!
//! Errors if the user CSS file cannot be read or `line_spacing` is not a
//! positive number.

use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::assets;
use crate::ctx::Ctx;
use crate::model::{Margin, Options};

/// Built-in stylesheet.
pub const BASE_CSS: &str = include_str!("../assets/base.css");

/// Side padding (percent of the page width) for the `wide` preset.
pub const WIDE_SIDE_PERCENT: u32 = 8;
/// Side padding (percent of the page width) for the `extra` preset.
pub const EXTRA_SIDE_PERCENT: u32 = 15;

/// Builds the book stylesheet with the user CSS appended verbatim. See the
/// module docs.
pub fn build_css(opts: &Options) -> Result<String> {
    build(opts, |css, _| css.to_string())
}

/// Builds the book stylesheet, resolving the user CSS's `url(...)`
/// references into book resources (warnings go to `ctx`).
pub fn build_book_css(ctx: &mut Ctx) -> Result<String> {
    let opts = ctx.opts.clone();
    build(&opts, |css, path| {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        assets::rewrite_user_css(css, &dir, ctx)
    })
}

fn build(opts: &Options, mut user_css: impl FnMut(&str, &Path) -> String) -> Result<String> {
    let mut css = String::with_capacity(BASE_CSS.len() + 512);
    css.push_str(BASE_CSS);
    if !css.ends_with('\n') {
        css.push('\n');
    }

    if let Some(margin_css) = margin_css(opts.margin) {
        css.push_str(&margin_css);
    }

    if let Some(ls) = opts.line_spacing {
        css.push_str(&line_spacing_css(ls)?);
    }

    if let Some(path) = &opts.extra_css {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading CSS file {}", path.display()))?;
        let user = String::from_utf8_lossy(&bytes);
        // A UTF-8 BOM in the middle of the stylesheet would break the first rule.
        let user = user.trim_start_matches('\u{feff}');
        css.push_str(&format!("\n/* user stylesheet: {} */\n", file_label(path)));
        css.push_str(&user_css(user, path));
        if !css.ends_with('\n') {
            css.push('\n');
        }
    }
    Ok(css)
}

/// Keeps note pages full width under a margin preset.
const NOTES_RESET: &str = "body.notes {\n  margin: 0;\n  padding: 0;\n}\n";

/// CSS for a margin preset, or `None` for `normal`.
fn margin_css(margin: Margin) -> Option<String> {
    match margin {
        Margin::Normal => None,
        Margin::Wide => Some(format!(
            "\n/* margin preset: wide */\n\
             body {{\n  padding-left: {p}%;\n  padding-right: {p}%;\n}}\n{NOTES_RESET}",
            p = WIDE_SIDE_PERCENT
        )),
        Margin::Extra => Some(format!(
            "\n/* margin preset: extra */\n\
             body {{\n  padding-left: {p}%;\n  padding-right: {p}%;\n}}\n\
             p {{\n  margin-bottom: 1.4em;\n}}\n\
             li {{\n  margin-bottom: 0.6em;\n}}\n\
             h1,\nh2,\nh3,\nh4,\nh5,\nh6 {{\n  margin-top: 1.6em;\n  margin-bottom: 0.8em;\n}}\n{NOTES_RESET}",
            p = EXTRA_SIDE_PERCENT
        )),
    }
}

/// CSS for a `line-height` override. Rejects non-finite or non-positive values.
fn line_spacing_css(ls: f32) -> Result<String> {
    if !ls.is_finite() || ls <= 0.0 {
        bail!("invalid line spacing {ls}: expected a positive number such as 1.5");
    }
    // Clamp to something a reader can still lay out.
    let ls = ls.clamp(0.5, 5.0);
    let value = format_number(ls);
    Ok(format!(
        "\n/* line spacing */\n\
         body,\np,\nli,\ndd,\nblockquote,\ntd,\nth {{\n  line-height: {value};\n}}\n"
    ))
}

/// Formats a float compactly (`1.5`, `2`, `1.25`), never in exponent form.
fn format_number(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// File name used in the user-stylesheet comment, with `*/` neutralized.
fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .replace("*/", "*_/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn opts_with_margin(margin: Margin) -> Options {
        Options {
            margin,
            ..Options::default()
        }
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn base_css_is_eink_friendly() {
        let css = build_css(&Options::default()).unwrap();
        assert!(css.starts_with(BASE_CSS));
        for needle in [
            "body",
            "max-width: 100%",
            "pre-wrap",
            ".notes-page",
            "page-break-before: always",
            "page-break-after: always",
            ".math-display",
            ".footnotes",
            "figure",
            "table",
        ] {
            assert!(css.contains(needle), "missing {needle}");
        }
        for banned in [
            "display: flex",
            "display: grid",
            "position: fixed",
            "vw",
            "vh",
        ] {
            assert!(!BASE_CSS.contains(banned), "base.css uses {banned}");
        }
    }

    #[test]
    fn margin_normal_adds_nothing() {
        let css = build_css(&opts_with_margin(Margin::Normal)).unwrap();
        assert_eq!(css.trim_end(), BASE_CSS.trim_end());
        assert!(!css.contains("margin preset"));
    }

    #[test]
    fn margin_wide() {
        let css = build_css(&opts_with_margin(Margin::Wide)).unwrap();
        assert!(css.contains("margin preset: wide"));
        assert!(css.contains("padding-left: 8%"));
        assert!(css.contains("padding-right: 8%"));
        assert!(!css.contains("15%"));
    }

    #[test]
    fn margin_extra() {
        let css = build_css(&opts_with_margin(Margin::Extra)).unwrap();
        assert!(css.contains("margin preset: extra"));
        assert!(css.contains("padding-left: 15%"));
        assert!(css.contains("padding-right: 15%"));
        // Larger paragraph spacing than the base 0.8em.
        let extra = &css[css.find("margin preset: extra").unwrap()..];
        assert!(extra.contains("p {\n  margin-bottom: 1.4em;"));
    }

    #[test]
    fn line_spacing() {
        let opts = Options {
            line_spacing: Some(1.5),
            ..Options::default()
        };
        let css = build_css(&opts).unwrap();
        assert!(css.contains("line-height: 1.5;"));
        let opts = Options {
            line_spacing: Some(2.0),
            ..Options::default()
        };
        assert!(build_css(&opts).unwrap().contains("line-height: 2;"));
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let opts = Options {
                line_spacing: Some(bad),
                ..Options::default()
            };
            assert!(build_css(&opts).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn order_is_base_margin_spacing_user() {
        let opts = Options {
            margin: Margin::Wide,
            line_spacing: Some(1.8),
            extra_css: Some(fixture("extra.css")),
            ..Options::default()
        };
        let css = build_css(&opts).unwrap();
        let base = css.find(".notes-page").unwrap();
        let margin = css.find("margin preset: wide").unwrap();
        let spacing = css.find("line-height: 1.8").unwrap();
        let user = css.find("user stylesheet").unwrap();
        assert!(base < margin && margin < spacing && spacing < user);
        assert!(css.trim_end().ends_with(".callout { font-size: 0.9em; }"));
        assert!(css.contains("h1 { text-align: center; }"));
    }

    #[test]
    fn user_css_with_bom_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.css");
        std::fs::write(&path, "\u{feff}p { color: red; }").unwrap();
        let opts = Options {
            extra_css: Some(path),
            ..Options::default()
        };
        let css = build_css(&opts).unwrap();
        assert!(!css.contains('\u{feff}'));
        assert!(css.contains("p { color: red; }\n"));

        let opts = Options {
            extra_css: Some("does-not-exist.css".into()),
            ..Options::default()
        };
        let err = build_css(&opts).unwrap_err().to_string();
        assert!(err.contains("does-not-exist.css"), "{err}");
    }

    #[test]
    fn margin_presets_exempt_note_pages() {
        for m in [Margin::Wide, Margin::Extra] {
            let css = build_css(&opts_with_margin(m)).unwrap();
            let preset = css.find("margin preset").unwrap();
            let reset = css
                .find("body.notes {\n  margin: 0;\n  padding: 0;")
                .unwrap();
            assert!(preset < reset, "{m:?}");
        }
    }

    #[test]
    fn user_css_urls_become_book_resources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixture("images/chart.png"), dir.path().join("p.png")).unwrap();
        std::fs::create_dir(dir.path().join("fonts")).unwrap();
        std::fs::write(dir.path().join("fonts/f.woff2"), b"wOF2fakefontdata").unwrap();
        let path = dir.path().join("user.css");
        std::fs::write(
            &path,
            "@font-face { font-family: F; src: url(fonts/f.woff2) format(\"woff2\"); }\n\
             body { background: url(p.png); }\n\
             .b { list-style-image: url(\"missing.png\"); }\n\
             /* url(p.png) in a comment */\n",
        )
        .unwrap();
        let mut ctx = Ctx::new(Options {
            extra_css: Some(path),
            ..Options::default()
        });
        let css = build_book_css(&mut ctx).unwrap();
        let hrefs: Vec<&str> = ctx.resources().iter().map(|r| r.href.as_str()).collect();
        assert_eq!(hrefs.len(), 2, "{hrefs:?}");
        assert!(
            hrefs
                .iter()
                .any(|h| h.starts_with("fonts/") && h.ends_with(".woff2"))
        );
        assert!(
            hrefs
                .iter()
                .any(|h| h.starts_with("images/") && h.ends_with(".png"))
        );
        for h in &hrefs {
            assert!(
                css.contains(&format!("url(\"../{h}\")")),
                "{h} not referenced:\n{css}"
            );
        }
        assert!(css.contains("list-style-image: none"), "{css}");
        assert!(css.contains("/* url(p.png) in a comment */"), "{css}");
        assert_eq!(ctx.warnings().len(), 1, "{:?}", ctx.warnings());
        assert!(ctx.warnings()[0].contains("missing.png"));
        // The plain variant keeps the text verbatim.
        let plain = build_css(&ctx.opts).unwrap();
        assert!(plain.contains("url(p.png)"));
    }

    #[test]
    fn number_format() {
        assert_eq!(format_number(1.5), "1.5");
        assert_eq!(format_number(2.0), "2");
        assert_eq!(format_number(1.25), "1.25");
    }
}

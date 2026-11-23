//! Stylesheet generation (owned by T8).
//!
//! Contract for [`build_css`]: `assets/base.css` (embedded with
//! `include_str!`), then the margin preset (`normal` nothing, `wide` about 8%
//! side padding, `extra` about 15% plus larger paragraph spacing), then
//! `line_spacing` as `line-height`, then the user's `--css` file appended
//! verbatim. Errors if the user CSS file cannot be read.
//!
//! The T0 stub returns the base CSS plus the user file.

use anyhow::{Context as _, Result};

use crate::model::Options;

/// Built-in stylesheet.
pub const BASE_CSS: &str = include_str!("../assets/base.css");

/// Builds the book stylesheet. See the module docs.
pub fn build_css(opts: &Options) -> Result<String> {
    let mut css = String::from(BASE_CSS);
    if let Some(path) = &opts.extra_css {
        let user = std::fs::read_to_string(path)
            .with_context(|| format!("reading CSS file {}", path.display()))?;
        css.push_str("\n/* user stylesheet */\n");
        css.push_str(&user);
    }
    Ok(css)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_base_css() {
        let css = build_css(&Options::default()).unwrap();
        assert!(css.contains("body"));
        let opts = Options {
            extra_css: Some("does-not-exist.css".into()),
            ..Options::default()
        };
        assert!(build_css(&opts).is_err());
    }
}

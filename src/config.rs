//! Option resolution (owned by T1).
//!
//! Contract: [`resolve`] merges, in order of precedence,
//! command-line flags > `book.toml` (for `md2epub build book.toml`) >
//! YAML front matter of the first input (`title`, `author`/`authors`, `lang`,
//! `date`, `cover`) > profile defaults, and returns the options plus the
//! input files in book order.
//!
//! - Paths from `book.toml` (inputs, cover, css, output) are relative to the
//!   TOML file's directory; paths from front matter are relative to the
//!   first input's directory; flag paths are relative to the CWD.
//! - `opts.svg` and `opts.math` default to the profile's values.
//! - `opts.meta.title` stays empty when nothing provides a title; the
//!   pipeline then falls back to the first `<h1>`, then the file stem.
//! - Errors clearly on missing inputs and unknown extensions (accepted:
//!   `.md`, `.markdown`, `.html`, `.htm`, `.xhtml`).
//!
//! The pipeline calls `resolve(cli, None)` first to learn the inputs, then
//! again with the first Markdown input's front matter.
//!
//! The T0 stub applies flags over the profile defaults only.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::cli::{Cli, Command};
use crate::model::{Options, parse_split_level};
use crate::profile;

/// Resolves the final options and input list. See the module docs.
pub fn resolve(cli: &Cli, front_matter: Option<&str>) -> Result<(Options, Vec<PathBuf>)> {
    let _ = front_matter;
    if let Some(Command::Build { config }) = &cli.command {
        bail!(
            "'build {}': book.toml support is not available in this build",
            config.display()
        );
    }
    let f = &cli.flags;
    let profile = match &f.profile {
        Some(name) => profile::by_name(name)?,
        None => profile::generic(),
    };
    let mut opts = Options {
        svg: profile.svg,
        math: profile.math,
        profile,
        ..Options::default()
    };
    if let Some(t) = &f.title {
        opts.meta.title = t.clone();
    }
    if !f.authors.is_empty() {
        opts.meta.authors = f.authors.clone();
    }
    if let Some(l) = &f.lang {
        opts.meta.lang = l.clone();
    }
    opts.cover = f.cover.clone();
    if let Some(s) = &f.split_level {
        opts.split_level = parse_split_level(s)?;
    }
    if let Some(v) = f.notes {
        opts.notes = v;
    }
    if let Some(v) = f.notes_style {
        opts.notes_style = v;
    }
    if let Some(v) = f.notes_pages {
        opts.notes_pages = v;
    }
    if let Some(v) = f.margin {
        opts.margin = v;
    }
    opts.line_spacing = f.line_spacing;
    if let Some(v) = f.math {
        opts.math = v;
    }
    if let Some(v) = f.svg {
        opts.svg = v;
    }
    opts.grayscale = f.grayscale;
    opts.offline = f.offline;
    opts.extra_css = f.css.clone();
    opts.verbose = f.verbose;
    opts.output = f.output.clone();

    if cli.inputs.is_empty() {
        bail!("no input files given (usage: md2epub <inputs...> -o book.epub)");
    }
    for input in &cli.inputs {
        if !input.is_file() {
            bail!("input file not found: {}", input.display());
        }
    }
    Ok((opts, cli.inputs.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn flags_override_profile() {
        let cli = Cli::try_parse_from([
            "md2epub",
            "Cargo.toml",
            "--profile",
            "scribe",
            "--svg",
            "keep",
        ])
        .unwrap();
        let (opts, inputs) = resolve(&cli, None).unwrap();
        assert_eq!(opts.profile.name, "scribe");
        assert_eq!(opts.svg, crate::model::SvgMode::Keep);
        assert_eq!(inputs.len(), 1);
    }

    #[test]
    fn missing_input_errors() {
        let cli = Cli::try_parse_from(["md2epub", "does-not-exist.md"]).unwrap();
        assert!(resolve(&cli, None).is_err());
    }
}

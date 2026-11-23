//! Command-line interface (owned by T1).
//!
//! ```text
//! md2epub <inputs...> -o book.epub
//! md2epub build book.toml
//! ```
//!
//! Every option is an `Option`/flag so that `config::resolve` can tell
//! "not given" apart from "given" and apply the precedence rule
//! (flags > `book.toml` > front matter > profile defaults). Options are
//! `global`, so they work before or after the `build` subcommand.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::model::{Margin, MathMode, NotesPlacement, NotesStyle, SvgMode};

/// Convert Markdown or HTML into a reflowable EPUB 3 for e-ink devices.
#[derive(Debug, Clone, Parser, PartialEq)]
#[command(
    name = "md2epub",
    version,
    about,
    args_conflicts_with_subcommands = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Input Markdown (.md) or HTML (.html) files, in book order.
    pub inputs: Vec<PathBuf>,

    #[command(flatten)]
    pub flags: Flags,
}

/// Subcommands.
#[derive(Debug, Clone, Subcommand, PartialEq)]
pub enum Command {
    /// Build a book described by a TOML file (same keys as the flags plus `inputs = [...]`).
    Build {
        /// Path to `book.toml`.
        config: PathBuf,
    },
}

/// Options shared by both forms of the command.
#[derive(Debug, Clone, Args, PartialEq, Default)]
pub struct Flags {
    /// Output EPUB path.
    #[arg(short = 'o', long = "output", global = true, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Device profile: scribe, remarkable, remarkable-pro, generic.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Book title.
    #[arg(long, global = true)]
    pub title: Option<String>,

    /// Author (repeat for several authors).
    #[arg(long = "author", global = true, value_name = "NAME")]
    pub authors: Vec<String>,

    /// Language tag, e.g. en, de, fr.
    #[arg(long, global = true)]
    pub lang: Option<String>,

    /// Cover image (PNG, JPEG, SVG, ...).
    #[arg(long, global = true, value_name = "IMG")]
    pub cover: Option<PathBuf>,

    /// Split chapters at heading level 1, 2, or none.
    #[arg(long = "split-level", global = true, value_name = "1|2|none")]
    pub split_level: Option<String>,

    /// Note pages: none, after-chapter, end.
    #[arg(long, global = true, value_name = "PLACEMENT")]
    pub notes: Option<NotesPlacement>,

    /// Note page style: blank, lined, dotted, grid.
    #[arg(long = "notes-style", global = true, value_name = "STYLE")]
    pub notes_style: Option<NotesStyle>,

    /// Number of note pages per insertion point.
    #[arg(long = "notes-pages", global = true, value_name = "N")]
    pub notes_pages: Option<u32>,

    /// Margin preset: normal, wide, extra.
    #[arg(long, global = true, value_name = "PRESET")]
    pub margin: Option<Margin>,

    /// Line height, e.g. 1.6.
    #[arg(long = "line-spacing", global = true, value_name = "FLOAT")]
    pub line_spacing: Option<f32>,

    /// Math rendering: image, mathml, off.
    #[arg(long, global = true, value_name = "MODE")]
    pub math: Option<MathMode>,

    /// SVG handling: rasterize, keep.
    #[arg(long, global = true, value_name = "MODE")]
    pub svg: Option<SvgMode>,

    /// Convert images to grayscale.
    #[arg(long, global = true)]
    pub grayscale: bool,

    /// Extra CSS file appended to the built-in stylesheet.
    #[arg(long, global = true, value_name = "FILE")]
    pub css: Option<PathBuf>,

    /// Do not fetch remote images.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Verbose output.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_inputs_and_build() {
        let cli =
            Cli::try_parse_from(["md2epub", "a.md", "b.html", "-o", "x.epub", "--math", "off"])
                .unwrap();
        assert_eq!(cli.inputs.len(), 2);
        assert_eq!(cli.flags.math, Some(MathMode::Off));
        let cli =
            Cli::try_parse_from(["md2epub", "build", "book.toml", "--profile", "scribe"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Build {
                config: "book.toml".into()
            })
        );
        assert_eq!(cli.flags.profile.as_deref(), Some("scribe"));
    }
}

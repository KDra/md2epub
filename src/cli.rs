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
    args_conflicts_with_subcommands = false,
    subcommand_precedence_over_arg = true
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
    #[arg(long, global = true, overrides_with = "no_grayscale")]
    pub grayscale: bool,

    /// Keep colors (overrides `grayscale = true` in book.toml).
    #[arg(long = "no-grayscale", global = true, overrides_with = "grayscale")]
    pub no_grayscale: bool,

    /// Extra CSS file appended to the built-in stylesheet.
    #[arg(long, global = true, value_name = "FILE")]
    pub css: Option<PathBuf>,

    /// Do not fetch remote images.
    #[arg(long, global = true, overrides_with = "no_offline")]
    pub offline: bool,

    /// Fetch remote images (overrides `offline = true` in book.toml).
    #[arg(long = "no-offline", global = true, overrides_with = "offline")]
    pub no_offline: bool,

    /// Verbose output.
    #[arg(short = 'v', long, global = true, overrides_with = "no_verbose")]
    pub verbose: bool,

    /// Quiet output (overrides `verbose = true` in book.toml).
    #[arg(long = "no-verbose", global = true, overrides_with = "verbose")]
    pub no_verbose: bool,

    // --- Linked local files (feature: follow-links) ---
    /// Add local files that the inputs link to (default).
    #[arg(
        long = "follow-links",
        global = true,
        overrides_with = "no_follow_links"
    )]
    pub follow_links: bool,

    /// Do not add linked local files; such links are removed.
    #[arg(
        long = "no-follow-links",
        global = true,
        overrides_with = "follow_links"
    )]
    pub no_follow_links: bool,
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

    #[test]
    fn global_flags_before_build_and_repeated_authors() {
        let cli = Cli::try_parse_from([
            "md2epub",
            "--grayscale",
            "--author",
            "A",
            "--author",
            "B",
            "build",
            "book.toml",
            "-v",
            "--notes",
            "after-chapter",
            "--notes-style",
            "dotted",
            "--notes-pages",
            "2",
            "--margin",
            "wide",
            "--line-spacing",
            "1.4",
            "--svg",
            "rasterize",
            "--split-level",
            "none",
        ])
        .unwrap();
        assert!(cli.inputs.is_empty());
        assert!(cli.flags.grayscale && cli.flags.verbose && !cli.flags.offline);
        assert_eq!(cli.flags.authors, vec!["A", "B"]);
        assert_eq!(cli.flags.notes, Some(NotesPlacement::AfterChapter));
        assert_eq!(cli.flags.notes_style, Some(NotesStyle::Dotted));
        assert_eq!(cli.flags.notes_pages, Some(2));
        assert_eq!(cli.flags.margin, Some(Margin::Wide));
        assert_eq!(cli.flags.line_spacing, Some(1.4));
        assert_eq!(cli.flags.svg, Some(SvgMode::Rasterize));
        assert_eq!(cli.flags.split_level.as_deref(), Some("none"));
    }

    #[test]
    fn negative_boolean_flags() {
        let cli =
            Cli::try_parse_from(["md2epub", "a.md", "--no-offline", "--no-grayscale"]).unwrap();
        assert!(cli.flags.no_offline && !cli.flags.offline);
        assert!(cli.flags.no_grayscale && !cli.flags.grayscale);
        // The last of a flag and its negation wins.
        let cli =
            Cli::try_parse_from(["md2epub", "a.md", "--no-offline", "--offline", "-v"]).unwrap();
        assert!(cli.flags.offline && !cli.flags.no_offline && cli.flags.verbose);
        let cli = Cli::try_parse_from(["md2epub", "a.md", "-v", "--no-verbose"]).unwrap();
        assert!(!cli.flags.verbose && cli.flags.no_verbose);
    }

    #[test]
    fn follow_links_flags() {
        let cli = Cli::try_parse_from(["md2epub", "a.md", "--no-follow-links"]).unwrap();
        assert!(cli.flags.no_follow_links && !cli.flags.follow_links);
        let cli = Cli::try_parse_from(["md2epub", "a.md", "--no-follow-links", "--follow-links"])
            .unwrap();
        assert!(cli.flags.follow_links && !cli.flags.no_follow_links);
    }

    #[test]
    fn build_after_positional_inputs_is_the_subcommand() {
        let cli = Cli::try_parse_from(["md2epub", "a.md", "build", "v.toml"]).unwrap();
        assert_eq!(cli.inputs, vec![PathBuf::from("a.md")]);
        assert_eq!(
            cli.command,
            Some(Command::Build {
                config: "v.toml".into()
            })
        );
    }

    #[test]
    fn unset_options_are_none() {
        let cli = Cli::try_parse_from(["md2epub", "a.md"]).unwrap();
        assert_eq!(cli.command, None);
        assert_eq!(cli.flags, Flags::default());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for args in [
            &["md2epub", "a.md", "--math", "katex"][..],
            &["md2epub", "a.md", "--notes", "sometimes"],
            &["md2epub", "a.md", "--notes-pages", "many"],
            &["md2epub", "a.md", "--line-spacing", "wide"],
            &["md2epub", "a.md", "--margin", "huge"],
            &["md2epub", "a.md", "--no-such-flag"],
            &["md2epub", "build"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
        let err = Cli::try_parse_from(["md2epub", "a.md", "--svg", "pdf"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("rasterize, keep"), "{err}");
    }
}

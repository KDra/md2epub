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

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::builder::StyledStr;
use clap::builder::styling::Style;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::model::{Margin, MathMode, NotesPlacement, NotesStyle, OutputLocation, SvgMode};

/// The clap command with `[default: ...]` appended to each listed option's
/// help. `defaults` maps an argument id (the `Flags` field name, e.g.
/// `split_level`) to the text shown in bold; ids that do not exist are
/// skipped. Bold is plain text when stdout is not a terminal or `NO_COLOR`
/// is set (clap strips the styling).
///
/// The global options are hidden from the help of `md2epub config`, which
/// does not use them.
pub fn command_with_defaults(
    defaults: &[(&str, String)],
    after_help: Option<&str>,
) -> clap::Command {
    let mut cmd = Cli::command();
    let bold = Style::new().bold();
    for (id, text) in defaults {
        let Some(arg) = cmd.get_arguments().find(|a| a.get_id() == id) else {
            continue;
        };
        let with_default = |help: Option<&StyledStr>| {
            let mut s = StyledStr::new();
            let base = help.map(|h| h.to_string()).unwrap_or_default();
            let base = base.trim_end();
            let _ = write!(
                s,
                "{base}{}[default: {}{text}{}]",
                if base.is_empty() { "" } else { " " },
                bold.render(),
                bold.render_reset()
            );
            s
        };
        let help = with_default(arg.get_help());
        let long_help = arg.get_long_help().map(|h| with_default(Some(h)));
        cmd = cmd.mut_arg(*id, |a| {
            let a = a.help(help);
            match long_help {
                Some(l) => a.long_help(l),
                None => a,
            }
        });
    }
    if let Some(text) = after_help {
        cmd = cmd.after_help(text.to_string());
    }
    // clap copies global arguments into every subcommand unless it already
    // has one with the same id, so hidden copies keep them out of the help.
    let globals: Vec<clap::Arg> = cmd
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(|a| a.clone().hide(true))
        .collect();
    cmd.mut_subcommand("config", |c| add_args(c, &globals))
}

/// Adds `args` to `cmd` and all its subcommands.
fn add_args(mut cmd: clap::Command, args: &[clap::Arg]) -> clap::Command {
    let subs: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in subs {
        cmd = cmd.mut_subcommand(name, |s| add_args(s, args));
    }
    cmd.args(args.iter().cloned())
}

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
    /// Show or change your default settings (run `md2epub config list` to see them).
    #[command(
        long_about = "Show or change your default settings.\n\n\
            Defaults live in a TOML file (`md2epub config path` prints where; the \
            MD2EPUB_CONFIG environment variable overrides the location). They apply to \
            every conversion; front matter, book.toml and command-line flags still win.\n\n\
            To convert a file literally named `config`, write it as `./config`.",
        after_help = CONFIG_KEYS_HELP
    )]
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
}

/// Settable keys, shown under `md2epub config --help`.
const CONFIG_KEYS_HELP: &str = "\
Keys: profile, author, lang, split-level, notes, notes-style, notes-pages, margin,
      line-spacing, math, svg, grayscale, offline, verbose, css, output-location,
      follow-links (the same values as the flags of the same name)

Examples:
  md2epub config set profile scribe
  md2epub config set author \"Ada Lovelace\" \"Charles Babbage\"
  md2epub config set grayscale true
  md2epub config unset profile";

/// Actions of `md2epub config`. Without one, `list` runs.
#[derive(Debug, Clone, Subcommand, PartialEq)]
pub enum ConfigAction {
    /// List every setting with its effective default and where it comes from (the default action).
    List,
    /// Print the effective default of one setting.
    Get {
        /// Setting name, e.g. profile or notes-style.
        key: String,
    },
    /// Save a default in the user config.
    #[command(after_help = CONFIG_KEYS_HELP)]
    Set {
        /// Setting name, e.g. profile or notes-style.
        key: String,
        /// New value, e.g. scribe, true or 1.5. `author` takes one or more names.
        #[arg(required = true, num_args = 1..)]
        values: Vec<String>,
    },
    /// Remove a setting from the user config, so the built-in default applies again.
    Unset {
        /// Setting name, e.g. profile or notes-style.
        key: String,
    },
    /// Delete the user config file, restoring every built-in default.
    Reset,
    /// Print the location of the user config file.
    Path,
    /// Open the user config in $VISUAL or $EDITOR, then check it.
    Edit,
}

/// Options shared by both forms of the command.
#[derive(Debug, Clone, Args, PartialEq, Default)]
pub struct Flags {
    /// Output EPUB path (a directory, or `.epub` is added when missing).
    #[arg(short = 'o', long = "output", global = true, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Where the EPUB goes without -o: cwd (current directory) or input (beside the first input).
    #[arg(long = "output-location", global = true, value_name = "WHERE")]
    pub output_location: Option<OutputLocation>,

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
        assert!(Cli::try_parse_from(["md2epub", "a.md", "--output-location", "desk"]).is_err());
    }

    #[test]
    fn config_subcommand_parses() {
        let cli = Cli::try_parse_from(["md2epub", "config"]).unwrap();
        assert_eq!(cli.command, Some(Command::Config { action: None }));
        let cli = Cli::try_parse_from(["md2epub", "config", "set", "author", "A B", "C"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Config {
                action: Some(ConfigAction::Set {
                    key: "author".into(),
                    values: vec!["A B".into(), "C".into()],
                })
            })
        );
        assert!(Cli::try_parse_from(["md2epub", "config", "set", "profile"]).is_err());
        assert!(Cli::try_parse_from(["md2epub", "config", "frobnicate"]).is_err());
        // `./config` is an input file, not the subcommand.
        let cli = Cli::try_parse_from(["md2epub", "./config"]).unwrap();
        assert_eq!(cli.command, None);
        assert_eq!(cli.inputs, vec![PathBuf::from("./config")]);
        let cli = Cli::try_parse_from(["md2epub", "a.md", "--output-location", "input"]).unwrap();
        assert_eq!(cli.flags.output_location, Some(OutputLocation::Input));
    }

    #[test]
    fn help_shows_bold_defaults() {
        let defaults = [
            ("profile", "scribe".to_string()),
            ("grayscale", "off".to_string()),
            ("no_such_arg", "ignored".to_string()),
        ];
        let mut cmd = command_with_defaults(&defaults, Some("Footer text."));
        cmd.clone().debug_assert();
        let help = cmd.render_help();
        let plain = help.to_string();
        assert!(plain.contains("generic [default: scribe]"), "{plain}");
        assert!(plain.contains("grayscale [default: off]"), "{plain}");
        assert!(plain.contains("Footer text."), "{plain}");
        assert!(!plain.contains('\x1b'), "{plain}");
        let styled = help.ansi().to_string();
        let bold = Style::new().bold();
        assert!(
            styled.contains(&format!("{}scribe{}", bold.render(), bold.render_reset())),
            "{styled}"
        );
        // Conversion options are hidden from `md2epub config --help` but
        // stay in `md2epub build --help`.
        let config_help = cmd
            .find_subcommand_mut("config")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(!config_help.contains("--profile"), "{config_help}");
        assert!(config_help.contains("md2epub config set"), "{config_help}");
        let build_help = cmd
            .find_subcommand_mut("build")
            .unwrap()
            .render_help()
            .to_string();
        assert!(build_help.contains("[default: scribe]"), "{build_help}");
        // Hidden copies still parse (and are then refused by main).
        let m = command_with_defaults(&defaults, None)
            .try_get_matches_from(["md2epub", "config", "list", "--profile", "scribe"])
            .unwrap();
        let cli = <Cli as clap::FromArgMatches>::from_arg_matches(&m).unwrap();
        assert_eq!(cli.flags.profile.as_deref(), Some("scribe"));
    }
}

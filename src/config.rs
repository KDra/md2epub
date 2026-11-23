//! Option resolution (owned by T1).
//!
//! Contract: [`resolve_with`] merges, in order of precedence,
//! command-line flags > `book.toml` (for `md2epub build book.toml`) >
//! YAML front matter of the first input (`title`, `author`/`authors`, `lang`,
//! `date`, `cover`) > the user config ([`UserConfig`], edited with
//! `md2epub config`) > profile defaults, and returns the options plus the
//! input files in book order. [`resolve`] is the same without a user config.
//!
//! The user config may set `profile`, so the profile-derived defaults (`svg`,
//! `math`) follow the profile it names unless a higher layer sets them.
//!
//! - Paths from `book.toml` (inputs, cover, css, output) are relative to the
//!   TOML file's directory; paths from front matter are relative to the
//!   first input's directory; flag paths are relative to the CWD; the user
//!   config's `css` is relative to the config file's directory (a leading
//!   `~/` is the home directory).
//! - `opts.svg` and `opts.math` default to the profile's values.
//! - `opts.meta.title` stays empty when nothing provides a title; the
//!   pipeline then falls back to the first `<h1>`, then the file stem.
//! - Errors clearly on missing inputs and unknown extensions (accepted:
//!   `.md`, `.markdown`, `.mdown`, `.mkd`, `.html`, `.htm`, `.xhtml`).
//!
//! The pipeline calls `resolve(cli, None)` first to learn the inputs, then
//! again with the first Markdown input's front matter.
//!
//! ## `book.toml`
//!
//! Holds the same keys as the long flags (kebab-case; `snake_case` is
//! accepted too) plus `inputs = [...]`:
//!
//! ```toml
//! title = "My Book"
//! authors = ["Ada Lovelace"]        # or author = "Ada Lovelace"
//! lang = "en"
//! date = 2026-01-15                 # TOML date or string
//! identifier = "urn:isbn:..."       # optional
//! inputs = ["intro.md", "part1.md"]
//! output = "my-book.epub"
//! cover = "cover.jpg"
//! profile = "scribe"
//! split-level = 2                   # 1, 2 or "none"
//! notes = "after-chapter"
//! notes-style = "lined"
//! notes-pages = 1
//! margin = "wide"
//! line-spacing = 1.5
//! math = "image"
//! svg = "rasterize"
//! grayscale = false
//! css = "extra.css"
//! offline = true
//! verbose = false
//! follow-links = true               # add linked local files
//! output-location = "cwd"           # or "input"; used when there is no output
//! ```
//!
//! Unknown keys are an error so that typos do not go unnoticed. As a
//! convenience, `md2epub book.toml` (a single `.toml` input and no
//! subcommand) behaves like `md2epub build book.toml`.
//!
//! Boolean flags (`--grayscale`, `--offline`, `-v`) switch a setting on and
//! their negations (`--no-grayscale`, `--no-offline`, `--no-verbose`) switch
//! it off; `book.toml` decides when neither is given. If a [`Cli`] carries
//! both `build` and positional inputs (`md2epub a.md build book.toml`), the
//! positional inputs replace the TOML `inputs`.
//!
//! A cover given by `--cover` or `book.toml` must exist (a missing one is an
//! error); a cover from front matter that cannot be loaded is only a warning.
//!
//! [`finalize`] runs once per conversion on the resolved options: it
//! normalizes and validates the language tag and the date and returns
//! warnings for values it had to fix or drop, and for explicit `svg = keep`
//! or `math = mathml` under a device profile.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_yaml::Value as Yaml;

use crate::cli::{Cli, Command, Flags};
use crate::model::{
    Margin, MathMode, Metadata, NotesPlacement, NotesStyle, Options, OutputLocation, SvgMode,
    parse_split_level,
};
use crate::pipeline::InputKind;
use crate::profile;

/// Input file extensions accepted by the pipeline (lowercase, no dot).
pub const INPUT_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "html", "htm", "xhtml"];

/// Keys accepted in `book.toml` (canonical kebab-case spelling).
const TOML_KEYS: &[&str] = &[
    "title",
    "author",
    "authors",
    "lang",
    "date",
    "identifier",
    "inputs",
    "output",
    "cover",
    "profile",
    "split-level",
    "notes",
    "notes-style",
    "notes-pages",
    "margin",
    "line-spacing",
    "math",
    "svg",
    "grayscale",
    "css",
    "offline",
    "verbose",
    "output-location",
    "follow-links",
];

/// Keys accepted in the user config, in `md2epub config list` order.
pub const USER_KEYS: &[&str] = &[
    "profile",
    "author",
    "lang",
    "split-level",
    "notes",
    "notes-style",
    "notes-pages",
    "margin",
    "line-spacing",
    "math",
    "svg",
    "grayscale",
    "offline",
    "verbose",
    "css",
    "output-location",
    "follow-links",
];

/// `book.toml` keys that describe one book and so make no sense as user
/// defaults.
const PER_BOOK_KEYS: &[&str] = &[
    "title",
    "identifier",
    "date",
    "cover",
    "inputs",
    "input",
    "output",
];

/// Which file a TOML table comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TomlSource {
    Book,
    User,
}

/// Canonical spelling of a TOML key: trimmed, lowercase, `_` as `-`, and
/// the aliases `authors`/`language` folded into `author`/`lang`.
pub fn canonical_key(raw: &str) -> String {
    let key = raw.trim().to_ascii_lowercase().replace('_', "-");
    match key.as_str() {
        "authors" => "author".into(),
        "language" => "lang".into(),
        _ => key,
    }
}

/// User defaults read from the user config file (see `md2epub config`).
/// `Default` is "no user config".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserConfig {
    layer: Layer,
    /// Canonical keys present in the file, in file order.
    keys: Vec<String>,
}

impl UserConfig {
    /// Parses user config text. Relative paths are joined onto `base` (the
    /// config file's directory). Unknown and per-book keys are errors.
    pub fn from_toml_str(text: &str, base: &Path) -> Result<UserConfig> {
        let table: toml::Table = toml::from_str(text).map_err(|e| anyhow!("invalid TOML: {e}"))?;
        let layer = Layer::from_table(&table, base, TomlSource::User)?;
        let mut keys: Vec<String> = Vec::new();
        for k in table.keys() {
            let k = canonical_key(k);
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        Ok(UserConfig { layer, keys })
    }

    /// Reads the user config at `path`; a missing file is an empty config.
    /// Errors name the file and the offending key.
    pub fn load(path: &Path) -> Result<UserConfig> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(UserConfig::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading user config {}", path.display()));
            }
        };
        UserConfig::from_toml_str(&text, &parent_dir(path)).with_context(|| {
            format!(
                "in user config {} (fix it with `md2epub config edit`, or remove it with `md2epub config reset`)",
                path.display()
            )
        })
    }

    /// Whether the file sets `key` (canonical spelling, see [`USER_KEYS`]).
    pub fn is_set(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }

    /// Canonical keys the file sets, in file order.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// The `follow-links` setting, if the file sets it.
    pub fn follow_links(&self) -> Option<bool> {
        self.layer.follow_links
    }
}

/// The options a conversion starts from before flags, `book.toml` and
/// front matter apply: the built-in defaults overridden by the user config,
/// including the defaults of the profile it names.
pub fn defaults(user: &UserConfig) -> Result<Options> {
    user.layer.clone().into_options()
}

/// Upper bound for `notes-pages`, to catch typos such as `notes-pages = 1000`.
const MAX_NOTES_PAGES: u32 = 100;

/// Resolves the final options and input list without a user config. See
/// the module docs.
pub fn resolve(cli: &Cli, front_matter: Option<&str>) -> Result<(Options, Vec<PathBuf>)> {
    resolve_with(cli, front_matter, &UserConfig::default())
}

/// Resolves the final options and input list, with `user` as the layer
/// between front matter and the profile defaults. See the module docs.
pub fn resolve_with(
    cli: &Cli,
    front_matter: Option<&str>,
    user: &UserConfig,
) -> Result<(Options, Vec<PathBuf>)> {
    let mut flags = Layer::from_flags(&cli.flags)?;

    let shortcut = cli.command.is_none() && is_toml_shortcut(&cli.inputs);
    let toml_path = match &cli.command {
        Some(Command::Build { config }) => Some(config.clone()),
        Some(Command::Config { .. }) => {
            bail!("`md2epub config` manages user defaults and converts nothing")
        }
        None if shortcut => Some(cli.inputs[0].clone()),
        None => None,
    };
    if !shortcut && !cli.inputs.is_empty() {
        flags.inputs = Some(cli.inputs.clone());
    }
    let book = match &toml_path {
        Some(path) => Layer::from_toml_file(path)?,
        None => Layer::default(),
    };

    let mut merged = flags.over(book);
    let inputs = merged.inputs.take().unwrap_or_default();
    check_inputs(&inputs, toml_path.as_deref())?;
    if let Some(cover) = &merged.cover {
        check_cover(cover)?;
    }

    if let Some(fm) = front_matter {
        let base = parent_dir(&inputs[0]);
        let fm_layer = Layer::from_front_matter(fm, &base)
            .with_context(|| format!("front matter of {}", inputs[0].display()))?;
        merged = merged.over(fm_layer);
    }
    merged = merged.over(user.layer.clone());

    let opts = merged.into_options()?;
    Ok((opts, inputs))
}

/// `md2epub book.toml` without the `build` subcommand.
fn is_toml_shortcut(inputs: &[PathBuf]) -> bool {
    inputs.len() == 1
        && inputs[0]
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

/// Errors clearly when there are no inputs, a file is missing or not a
/// regular file, or its extension is not supported.
fn check_inputs(inputs: &[PathBuf], toml: Option<&Path>) -> Result<()> {
    if inputs.is_empty() {
        match toml {
            Some(t) => bail!(
                "no input files: add `inputs = [\"chapter1.md\", ...]` to {}",
                t.display()
            ),
            None => bail!(
                "no input files given (usage: md2epub <inputs...> -o book.epub, md2epub build book.toml, or md2epub config)"
            ),
        }
    }
    for input in inputs {
        if input.is_dir() {
            bail!(
                "input '{}' is a directory, not a file (list the .md/.html files instead)",
                input.display()
            );
        }
        if !input.is_file() {
            if input.as_os_str() == "build" {
                bail!(
                    "input file not found: build (to build from a book file, run `md2epub build <book.toml>`)"
                );
            }
            if input.as_os_str() == "config" {
                bail!(
                    "input file not found: config (to change your defaults, run `md2epub config`)"
                );
            }
            bail!("input file not found: {}", input.display());
        }
        if InputKind::of(input).is_err() {
            let ext = input
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_else(|| "no extension".into());
            bail!(
                "unsupported input '{}' ({ext}): expected one of {}",
                input.display(),
                INPUT_EXTENSIONS
                    .iter()
                    .map(|e| format!(".{e}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

/// Errors when an explicitly configured cover file does not exist. URLs
/// (`http(s)://`, `file://`, `data:`) are checked when they are loaded.
fn check_cover(cover: &Path) -> Result<()> {
    let s = cover.to_string_lossy();
    let lower = s.to_ascii_lowercase();
    let is_url = ["http://", "https://", "file:", "data:"]
        .iter()
        .any(|p| lower.starts_with(p));
    if is_url {
        return Ok(());
    }
    if cover.is_dir() {
        bail!("cover '{}' is a directory, not an image", cover.display());
    }
    if !cover.is_file() {
        bail!("cover image not found: {}", cover.display());
    }
    Ok(())
}

/// Directory of `path`, or `.` when it has none.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Joins `p` onto `base` unless it is absolute.
fn rel_to(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() || base == Path::new(".") {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// One source of settings. `None` means "not set here".
#[derive(Debug, Clone, Default, PartialEq)]
struct Layer {
    title: Option<String>,
    authors: Option<Vec<String>>,
    lang: Option<String>,
    date: Option<String>,
    identifier: Option<String>,
    inputs: Option<Vec<PathBuf>>,
    output: Option<PathBuf>,
    cover: Option<PathBuf>,
    profile: Option<String>,
    /// `Some(None)` is an explicit `none`.
    split_level: Option<Option<u8>>,
    notes: Option<NotesPlacement>,
    notes_style: Option<NotesStyle>,
    notes_pages: Option<u32>,
    margin: Option<Margin>,
    line_spacing: Option<f32>,
    math: Option<MathMode>,
    svg: Option<SvgMode>,
    grayscale: Option<bool>,
    css: Option<PathBuf>,
    offline: Option<bool>,
    verbose: Option<bool>,
    output_location: Option<OutputLocation>,
    /// Linked local files (`--follow-links`, `follow-links`).
    follow_links: Option<bool>,
}

impl Layer {
    /// Settings given on the command line. Inputs are filled by the caller.
    fn from_flags(f: &Flags) -> Result<Layer> {
        let split_level = match &f.split_level {
            Some(s) => Some(parse_split_level(s).context("--split-level")?),
            None => None,
        };
        if let Some(p) = &f.profile {
            profile::by_name(p).context("--profile")?;
        }
        if let Some(ls) = f.line_spacing {
            check_line_spacing(ls).context("--line-spacing")?;
        }
        if let Some(n) = f.notes_pages {
            check_notes_pages(n).context("--notes-pages")?;
        }
        Ok(Layer {
            title: f.title.clone(),
            authors: (!f.authors.is_empty()).then(|| f.authors.clone()),
            lang: f.lang.clone(),
            date: None,
            identifier: None,
            inputs: None,
            output: f.output.clone(),
            cover: f.cover.clone(),
            profile: f.profile.clone(),
            split_level,
            notes: f.notes,
            notes_style: f.notes_style,
            notes_pages: f.notes_pages,
            margin: f.margin,
            line_spacing: f.line_spacing,
            math: f.math,
            svg: f.svg,
            grayscale: switch(f.grayscale, f.no_grayscale),
            css: f.css.clone(),
            offline: switch(f.offline, f.no_offline),
            verbose: switch(f.verbose, f.no_verbose),
            output_location: f.output_location,
            follow_links: switch(f.follow_links, f.no_follow_links),
        })
    }

    /// Reads and parses a `book.toml` file.
    fn from_toml_file(path: &Path) -> Result<Layer> {
        if path.is_dir() {
            bail!("book file '{}' is a directory", path.display());
        }
        if !path.is_file() {
            bail!("book file not found: {}", path.display());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Layer::from_toml_str(&text, &parent_dir(path))
            .with_context(|| format!("in {}", path.display()))
    }

    /// Parses `book.toml` text; relative paths are joined onto `base`.
    fn from_toml_str(text: &str, base: &Path) -> Result<Layer> {
        let table: toml::Table = toml::from_str(text).map_err(|e| anyhow!("invalid TOML: {e}"))?;
        Layer::from_table(&table, base, TomlSource::Book)
    }

    /// Reads the keys of a `book.toml` or user config table.
    fn from_table(table: &toml::Table, base: &Path, source: TomlSource) -> Result<Layer> {
        let mut l = Layer::default();
        for (raw_key, value) in table {
            let key = raw_key.trim().to_ascii_lowercase().replace('_', "-");
            let ctx = || format!("key `{raw_key}`");
            if source == TomlSource::User {
                if PER_BOOK_KEYS.contains(&key.as_str()) {
                    bail!(
                        "key `{raw_key}` cannot be a user default because it describes a single book; \
                         give it on the command line or in book.toml"
                    );
                }
                if !USER_KEYS.contains(&canonical_key(&key).as_str()) {
                    bail!(
                        "unknown key `{raw_key}`: expected one of {}",
                        USER_KEYS.join(", ")
                    );
                }
            }
            let path_of = |s: String| -> PathBuf {
                if source == TomlSource::User
                    && let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
                    && let Some(home) = dirs::home_dir()
                {
                    return home.join(rest);
                }
                rel_to(base, &s)
            };
            match key.as_str() {
                "title" => l.title = Some(toml_string(value).with_context(ctx)?),
                "author" | "authors" => {
                    let list = toml_string_list(value).with_context(ctx)?;
                    l.authors = Some(clean_authors(list));
                }
                "lang" | "language" => l.lang = Some(toml_string(value).with_context(ctx)?),
                "date" => l.date = Some(toml_scalar(value).with_context(ctx)?),
                "identifier" => l.identifier = Some(toml_string(value).with_context(ctx)?),
                "inputs" | "input" => {
                    let list = toml_string_list(value).with_context(ctx)?;
                    l.inputs = Some(list.iter().map(|p| rel_to(base, p)).collect());
                }
                "output" => l.output = Some(rel_to(base, &toml_string(value).with_context(ctx)?)),
                "cover" => l.cover = Some(rel_to(base, &toml_string(value).with_context(ctx)?)),
                "css" => l.css = Some(path_of(toml_string(value).with_context(ctx)?)),
                "profile" => {
                    let name = toml_string(value).with_context(ctx)?;
                    profile::by_name(&name).with_context(ctx)?;
                    l.profile = Some(name);
                }
                "split-level" => {
                    let s = toml_scalar(value).with_context(ctx)?;
                    l.split_level = Some(parse_split_level(&s).with_context(ctx)?);
                }
                "notes" => {
                    // `notes = false` reads naturally as "no note pages".
                    l.notes = Some(match value {
                        toml::Value::Boolean(false) => NotesPlacement::None,
                        _ => toml_parse(value).with_context(ctx)?,
                    })
                }
                "notes-style" => l.notes_style = Some(toml_parse(value).with_context(ctx)?),
                "margin" => l.margin = Some(toml_parse(value).with_context(ctx)?),
                "math" => l.math = Some(toml_parse(value).with_context(ctx)?),
                "svg" => l.svg = Some(toml_parse(value).with_context(ctx)?),
                "notes-pages" => {
                    let n = match value {
                        toml::Value::Integer(n) => u32::try_from(*n)
                            .map_err(|_| anyhow!("expected a non-negative whole number, got {n}")),
                        other => Err(type_error("a whole number", other)),
                    }
                    .with_context(ctx)?;
                    check_notes_pages(n).with_context(ctx)?;
                    l.notes_pages = Some(n);
                }
                "line-spacing" => {
                    let f = match value {
                        toml::Value::Float(f) => Ok(*f as f32),
                        toml::Value::Integer(i) => Ok(*i as f32),
                        other => Err(type_error("a number such as 1.5", other)),
                    }
                    .with_context(ctx)?;
                    check_line_spacing(f).with_context(ctx)?;
                    l.line_spacing = Some(f);
                }
                "grayscale" => l.grayscale = Some(toml_bool(value).with_context(ctx)?),
                "offline" => l.offline = Some(toml_bool(value).with_context(ctx)?),
                "verbose" => l.verbose = Some(toml_bool(value).with_context(ctx)?),
                "output-location" => l.output_location = Some(toml_parse(value).with_context(ctx)?),
                // --- Linked local files ---
                "follow-links" => l.follow_links = Some(toml_bool(value).with_context(ctx)?),
                _ => bail!(
                    "unknown key `{raw_key}`: expected one of {}",
                    TOML_KEYS.join(", ")
                ),
            }
            // A bad language in book.toml is fixed with a warning at
            // conversion time; a user default should simply be right.
            if source == TomlSource::User
                && matches!(key.as_str(), "lang" | "language")
                && let Some(lang) = &l.lang
                && normalize_lang(lang).is_none()
            {
                return Err(anyhow!(
                    "invalid language tag '{lang}': expected a BCP 47 tag such as en, de or pt-BR"
                ))
                .with_context(ctx);
            }
        }
        Ok(l)
    }

    /// Parses YAML front matter (with or without its `---` delimiters).
    /// Only metadata keys are read; other keys are ignored.
    fn from_front_matter(text: &str, base: &Path) -> Result<Layer> {
        let body = strip_front_matter_delimiters(text);
        let value: Yaml =
            serde_yaml::from_str(body).map_err(|e| anyhow!("invalid YAML front matter: {e}"))?;
        let map = match value {
            Yaml::Null => return Ok(Layer::default()),
            Yaml::Mapping(m) => m,
            _ => bail!("YAML front matter must be a mapping of `key: value` pairs"),
        };
        let mut l = Layer::default();
        for (k, v) in &map {
            let Some(raw_key) = k.as_str() else { continue };
            let key = raw_key.trim().to_ascii_lowercase();
            let ctx = || format!("key `{raw_key}`");
            if v.is_null() {
                continue;
            }
            match key.as_str() {
                "title" => l.title = Some(yaml_scalar(v).with_context(ctx)?),
                "author" | "authors" | "creator" => {
                    l.authors = Some(clean_authors(yaml_authors(v).with_context(ctx)?))
                }
                "lang" | "language" => l.lang = Some(yaml_scalar(v).with_context(ctx)?),
                "date" => l.date = Some(yaml_scalar(v).with_context(ctx)?),
                "identifier" => l.identifier = Some(yaml_scalar(v).with_context(ctx)?),
                "cover" | "cover-image" | "cover_image" => {
                    l.cover = Some(rel_to(base, &yaml_scalar(v).with_context(ctx)?))
                }
                _ => {}
            }
        }
        Ok(l)
    }

    /// Fills every unset field of `self` from `lower`.
    fn over(self, lower: Layer) -> Layer {
        Layer {
            title: self.title.or(lower.title),
            authors: self.authors.or(lower.authors),
            lang: self.lang.or(lower.lang),
            date: self.date.or(lower.date),
            identifier: self.identifier.or(lower.identifier),
            inputs: self.inputs.or(lower.inputs),
            output: self.output.or(lower.output),
            cover: self.cover.or(lower.cover),
            profile: self.profile.or(lower.profile),
            split_level: self.split_level.or(lower.split_level),
            notes: self.notes.or(lower.notes),
            notes_style: self.notes_style.or(lower.notes_style),
            notes_pages: self.notes_pages.or(lower.notes_pages),
            margin: self.margin.or(lower.margin),
            line_spacing: self.line_spacing.or(lower.line_spacing),
            math: self.math.or(lower.math),
            svg: self.svg.or(lower.svg),
            grayscale: self.grayscale.or(lower.grayscale),
            css: self.css.or(lower.css),
            offline: self.offline.or(lower.offline),
            verbose: self.verbose.or(lower.verbose),
            output_location: self.output_location.or(lower.output_location),
            follow_links: self.follow_links.or(lower.follow_links),
        }
    }

    /// Applies the merged layer over the profile defaults.
    fn into_options(self) -> Result<Options> {
        let profile = match &self.profile {
            Some(name) => profile::by_name(name)?,
            None => profile::generic(),
        };
        let d = Options::default();
        let non_empty =
            |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let lang = non_empty(self.lang);
        let lang_given = lang.is_some();
        let meta = Metadata {
            title: self.title.map(|t| t.trim().to_string()).unwrap_or_default(),
            authors: self.authors.map(clean_authors).unwrap_or_default(),
            lang: lang.unwrap_or_else(|| d.meta.lang.clone()),
            identifier: non_empty(self.identifier),
            date: non_empty(self.date),
        };
        Ok(Options {
            svg: self.svg.unwrap_or(profile.svg),
            math: self.math.unwrap_or(profile.math),
            profile,
            meta,
            cover: self.cover,
            split_level: self.split_level.unwrap_or(d.split_level),
            notes: self.notes.unwrap_or(d.notes),
            notes_style: self.notes_style.unwrap_or(d.notes_style),
            notes_pages: self.notes_pages.unwrap_or(d.notes_pages),
            margin: self.margin.unwrap_or(d.margin),
            line_spacing: self.line_spacing,
            grayscale: self.grayscale.unwrap_or(d.grayscale),
            offline: self.offline.unwrap_or(d.offline),
            extra_css: self.css,
            verbose: self.verbose.unwrap_or(d.verbose),
            output: self.output,
            output_location: self.output_location.unwrap_or(d.output_location),
            lang_given,
            follow_links: self.follow_links.unwrap_or(d.follow_links),
        })
    }
}

/// A boolean flag and its `--no-` negation (clap keeps only the last one).
fn switch(on: bool, off: bool) -> Option<bool> {
    if off {
        Some(false)
    } else if on {
        Some(true)
    } else {
        None
    }
}

/// Normalizes and checks resolved options. Returns warnings for values that
/// were fixed or dropped. See the module docs.
pub fn finalize(opts: &mut Options) -> Vec<String> {
    let mut warnings = Vec::new();

    let raw_lang = opts.meta.lang.trim().to_string();
    match normalize_lang(&raw_lang) {
        Some(tag) => opts.meta.lang = tag,
        None => {
            warnings.push(format!(
                "invalid language tag '{raw_lang}' (expected a BCP 47 tag such as en, de or pt-BR); using 'en'"
            ));
            opts.meta.lang = "en".into();
        }
    }

    if let Some(raw) = opts.meta.date.take() {
        let raw = raw.trim().to_string();
        match normalize_date(&raw) {
            Some(d) => opts.meta.date = Some(d),
            None => warnings.push(format!(
                "date '{raw}' is not a valid or unambiguous date (expected YYYY, YYYY-MM or YYYY-MM-DD); it is left out"
            )),
        }
    }

    if opts.profile.flatten_alpha {
        let name = &opts.profile.name;
        if opts.svg == SvgMode::Keep {
            warnings.push(format!(
                "SVG kept under the {name} profile (--svg keep); Send to Kindle or the reMarkable may drop or reject it"
            ));
        }
        if opts.math == MathMode::Mathml {
            warnings.push(format!(
                "math: MathML output under the {name} profile (--math mathml); Send to Kindle or the reMarkable may drop or reject it (use --math image)"
            ));
        }
    }
    warnings
}

/// Normalizes a language tag (`en_us` -> `en-US`, `EN` -> `en`,
/// `zh_hant_tw` -> `zh-Hant-TW`) and checks it against the BCP 47 syntax.
/// Returns `None` when it is not a well-formed tag.
pub fn normalize_lang(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c == '_' || c.is_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let subtags: Vec<&str> = cleaned.split('-').collect();
    if subtags.iter().any(|t| t.is_empty() || t.len() > 8)
        || !subtags
            .iter()
            .all(|t| t.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        return None;
    }
    let first = subtags[0].to_ascii_lowercase();
    let private = first == "x";
    if !private && !(first.len() >= 2 && first.chars().all(|c| c.is_ascii_alphabetic())) {
        return None;
    }
    if private && subtags.len() < 2 {
        return None;
    }
    let mut out = vec![first];
    // After a singleton (extension or private use) the subtags are opaque.
    let mut after_singleton = private;
    for (i, t) in subtags.iter().enumerate().skip(1) {
        let lower = t.to_ascii_lowercase();
        if after_singleton {
            out.push(lower);
            continue;
        }
        if t.len() == 1 {
            // A singleton must be followed by at least one subtag.
            if i + 1 == subtags.len() {
                return None;
            }
            after_singleton = true;
            out.push(lower);
        } else if t.len() == 4 && t.chars().all(|c| c.is_ascii_alphabetic()) && i == 1 {
            // Script: titlecase (`Hant`).
            let mut s = lower;
            s[..1].make_ascii_uppercase();
            out.push(s);
        } else if t.len() == 2 && t.chars().all(|c| c.is_ascii_alphabetic()) {
            // Region: uppercase (`US`).
            out.push(t.to_ascii_uppercase());
        } else {
            out.push(lower);
        }
    }
    Some(out.join("-"))
}

/// Normalizes a date to W3CDTF (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`, optionally
/// with a `T` time). Also accepts `YYYY/MM/DD`, `YYYY.MM.DD`, single-digit
/// months and days, a space before the time, and `DD/MM/YYYY` or
/// `MM/DD/YYYY` when only one reading is possible. Returns `None` for
/// anything else (including impossible dates such as `2024-13-45`).
pub fn normalize_date(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (date, time) = match raw.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t.trim())),
        None => (raw, None),
    };
    let parts: Vec<&str> = date.split(['-', '/', '.']).collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.len() > 4)
    {
        return None;
    }
    let num = |s: &str| s.parse::<u32>().ok();
    let (y, m, d) = match parts.as_slice() {
        [y] if y.len() == 4 => (num(y)?, None, None),
        [y, m] if y.len() == 4 => (num(y)?, Some(num(m)?), None),
        [y, m, d] if y.len() == 4 => (num(y)?, Some(num(m)?), Some(num(d)?)),
        [a, b, y] if y.len() == 4 && a.len() <= 2 && b.len() <= 2 => {
            let (a, b) = (num(a)?, num(b)?);
            match (a > 12, b > 12) {
                (true, false) => (num(y)?, Some(b), Some(a)),
                (false, true) => (num(y)?, Some(a), Some(b)),
                // `01/02/2024` could be either; `13/14/2024` is neither.
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut out = format!("{y:04}");
    if let Some(m) = m {
        if !(1..=12).contains(&m) {
            return None;
        }
        out.push_str(&format!("-{m:02}"));
    }
    if let (Some(m), Some(d)) = (m, d) {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let days = match m {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        if !(1..=days).contains(&d) {
            return None;
        }
        out.push_str(&format!("-{d:02}"));
    }
    if let Some(t) = time {
        let valid = d.is_some()
            && !t.is_empty()
            && t.starts_with(|c: char| c.is_ascii_digit())
            && t.bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b':' | b'.' | b'Z' | b'+' | b'-'));
        if !valid {
            return None;
        }
        out.push('T');
        out.push_str(t);
    }
    Some(out)
}

fn check_line_spacing(v: f32) -> Result<()> {
    if !v.is_finite() || v <= 0.0 || v > 10.0 {
        bail!("invalid line spacing {v}: expected a number between 0 and 10, such as 1.5");
    }
    Ok(())
}

fn check_notes_pages(n: u32) -> Result<()> {
    if n > MAX_NOTES_PAGES {
        bail!("invalid number of note pages {n}: expected 0 to {MAX_NOTES_PAGES}");
    }
    Ok(())
}

/// Trims names and drops empty ones.
fn clean_authors(list: Vec<String>) -> Vec<String> {
    list.into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect()
}

fn type_error(expected: &str, got: &toml::Value) -> anyhow::Error {
    anyhow!("expected {expected}, got {} `{got}`", got.type_str())
}

fn toml_string(v: &toml::Value) -> Result<String> {
    match v {
        toml::Value::String(s) => Ok(s.clone()),
        other => Err(type_error("a string", other)),
    }
}

/// A string, number or date rendered as a string.
fn toml_scalar(v: &toml::Value) -> Result<String> {
    match v {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        toml::Value::Datetime(d) => Ok(d.to_string()),
        other => Err(type_error("a string, number or date", other)),
    }
}

fn toml_string_list(v: &toml::Value) -> Result<Vec<String>> {
    match v {
        toml::Value::String(s) => Ok(vec![s.clone()]),
        toml::Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| toml_string(item).with_context(|| format!("item {}", i + 1)))
            .collect(),
        other => Err(type_error("a string or a list of strings", other)),
    }
}

fn toml_bool(v: &toml::Value) -> Result<bool> {
    match v {
        toml::Value::Boolean(b) => Ok(*b),
        other => Err(type_error("true or false", other)),
    }
}

/// A string value parsed through `FromStr` (the mode enums).
fn toml_parse<T>(v: &toml::Value) -> Result<T>
where
    T: std::str::FromStr<Err = anyhow::Error>,
{
    toml_string(v)?.parse()
}

/// Removes a leading `---` line and a trailing `---`/`...` line, if present.
fn strip_front_matter_delimiters(text: &str) -> &str {
    let mut s = text.trim_start_matches('\u{feff}');
    let first_line_end = s.find('\n').map_or(s.len(), |i| i + 1);
    if s[..first_line_end].trim() == "---" {
        s = &s[first_line_end..];
    }
    let trimmed = s.trim_end();
    let last_line_start = trimmed.rfind('\n').map_or(0, |i| i + 1);
    let last = trimmed[last_line_start..].trim();
    if last == "---" || last == "..." {
        &trimmed[..last_line_start]
    } else {
        s
    }
}

/// A YAML scalar (string, number, bool) as a string.
fn yaml_scalar(v: &Yaml) -> Result<String> {
    match v {
        Yaml::String(s) => Ok(s.clone()),
        Yaml::Number(n) => Ok(n.to_string()),
        Yaml::Bool(b) => Ok(b.to_string()),
        Yaml::Tagged(t) => yaml_scalar(&t.value),
        Yaml::Null => Ok(String::new()),
        Yaml::Sequence(_) => bail!("expected a single value, got a list"),
        Yaml::Mapping(_) => bail!("expected a single value, got a mapping"),
    }
}

/// `author: Name`, `authors: [A, B]`, or Pandoc-style `- name: A` entries.
fn yaml_authors(v: &Yaml) -> Result<Vec<String>> {
    match v {
        Yaml::Sequence(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| yaml_author(item).with_context(|| format!("item {}", i + 1)))
            .collect(),
        other => Ok(vec![yaml_author(other)?]),
    }
}

fn yaml_author(v: &Yaml) -> Result<String> {
    match v {
        Yaml::Mapping(m) => match m.get("name") {
            Some(name) => yaml_scalar(name),
            None => bail!("an author given as a mapping needs a `name` key"),
        },
        other => yaml_scalar(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn cli(args: &[&str]) -> Cli {
        let mut v = vec!["md2epub"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v).unwrap()
    }

    fn path_str(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// A temp dir with `a.md`, `b.html` and a `book.toml` built from `toml`.
    fn book_dir(toml: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A\n").unwrap();
        fs::write(dir.path().join("b.html"), "<p>b</p>").unwrap();
        let book = dir.path().join("book.toml");
        fs::write(&book, toml).unwrap();
        (dir, book)
    }

    fn err_of(r: Result<(Options, Vec<PathBuf>)>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    // ---- defaults and flags -------------------------------------------------

    #[test]
    fn defaults_without_flags() {
        let md = fixture("kitchen-sink.md");
        let (opts, inputs) = resolve(&cli(&[&path_str(&md)]), None).unwrap();
        assert_eq!(inputs, vec![md]);
        assert_eq!(opts, Options::default());
        assert!(opts.meta.title.is_empty(), "title left for the pipeline");
    }

    #[test]
    fn all_flags_apply() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let cover = path_str(&fixture("photo.jpg"));
        let c = cli(&[
            &md,
            "-o",
            "out.epub",
            "--profile",
            "remarkable",
            "--title",
            "T",
            "--author",
            "A",
            "--author",
            "B",
            "--lang",
            "de",
            "--cover",
            &cover,
            "--split-level",
            "none",
            "--notes",
            "end",
            "--notes-style",
            "grid",
            "--notes-pages",
            "3",
            "--margin",
            "extra",
            "--line-spacing",
            "1.8",
            "--math",
            "mathml",
            "--svg",
            "keep",
            "--grayscale",
            "--css",
            "x.css",
            "--offline",
            "-v",
        ]);
        let (o, _) = resolve(&c, None).unwrap();
        assert_eq!(o.output, Some(PathBuf::from("out.epub")));
        assert_eq!(o.profile.name, "remarkable");
        assert_eq!(o.meta.title, "T");
        assert_eq!(o.meta.authors, vec!["A", "B"]);
        assert_eq!(o.meta.lang, "de");
        assert_eq!(o.cover, Some(PathBuf::from(&cover)));
        assert_eq!(o.split_level, None);
        assert_eq!(o.notes, NotesPlacement::End);
        assert_eq!(o.notes_style, NotesStyle::Grid);
        assert_eq!(o.notes_pages, 3);
        assert_eq!(o.margin, Margin::Extra);
        assert_eq!(o.line_spacing, Some(1.8));
        assert_eq!(o.math, MathMode::Mathml);
        assert_eq!(o.svg, SvgMode::Keep);
        assert!(o.grayscale && o.offline && o.verbose);
        assert_eq!(o.extra_css, Some(PathBuf::from("x.css")));
    }

    // ---- profiles -----------------------------------------------------------

    #[test]
    fn each_profile_sets_defaults() {
        let md = path_str(&fixture("kitchen-sink.md"));
        for (name, w, h, svg, flatten) in [
            ("scribe", 1860, 2480, SvgMode::Rasterize, true),
            ("remarkable", 1404, 1872, SvgMode::Rasterize, true),
            ("remarkable-pro", 1620, 2160, SvgMode::Rasterize, true),
            ("generic", 1600, 2133, SvgMode::Keep, false),
        ] {
            let (o, _) = resolve(&cli(&[&md, "--profile", name]), None).unwrap();
            assert_eq!(o.profile.name, name);
            assert_eq!((o.profile.screen_w, o.profile.screen_h), (w, h));
            assert_eq!(o.svg, svg, "{name}");
            assert_eq!(o.math, MathMode::Image, "{name}");
            assert_eq!(o.profile.flatten_alpha, flatten, "{name}");
        }
    }

    #[test]
    fn flags_override_profile_defaults() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let (o, _) = resolve(
            &cli(&[&md, "--profile", "scribe", "--svg", "keep", "--math", "off"]),
            None,
        )
        .unwrap();
        assert_eq!(o.profile.name, "scribe");
        assert_eq!(o.svg, SvgMode::Keep);
        assert_eq!(o.math, MathMode::Off);
        assert!(o.profile.flatten_alpha);
    }

    // ---- precedence ---------------------------------------------------------

    const FM: &str = "---\ntitle: FM Title\nauthor: FM Author\nlang: fr\ndate: 2020-02-02\ncover: fm-cover.png\n---\n";

    #[test]
    fn front_matter_over_defaults() {
        let md = fixture("kitchen-sink.md");
        let (o, _) = resolve(&cli(&[&path_str(&md)]), Some(FM)).unwrap();
        assert_eq!(o.meta.title, "FM Title");
        assert_eq!(o.meta.authors, vec!["FM Author"]);
        assert_eq!(o.meta.lang, "fr");
        assert_eq!(o.meta.date.as_deref(), Some("2020-02-02"));
        // Front matter paths are relative to the first input's directory.
        assert_eq!(o.cover, Some(fixture("fm-cover.png")));
    }

    #[test]
    fn toml_over_front_matter() {
        let (dir, book) = book_dir(
            "title = \"Toml Title\"\nauthor = \"Toml Author\"\ninputs = [\"a.md\"]\nprofile = \"scribe\"\nmath = \"off\"\n",
        );
        let (o, inputs) = resolve(&cli(&["build", &path_str(&book)]), Some(FM)).unwrap();
        assert_eq!(inputs, vec![dir.path().join("a.md")]);
        assert_eq!(o.meta.title, "Toml Title");
        assert_eq!(o.meta.authors, vec!["Toml Author"]);
        // Not in the TOML: front matter still fills these.
        assert_eq!(o.meta.lang, "fr");
        assert_eq!(o.meta.date.as_deref(), Some("2020-02-02"));
        assert_eq!(o.cover, Some(dir.path().join("fm-cover.png")));
        // TOML over profile defaults; untouched settings keep the profile's.
        assert_eq!(o.profile.name, "scribe");
        assert_eq!(o.math, MathMode::Off);
        assert_eq!(o.svg, SvgMode::Rasterize);
    }

    #[test]
    fn flags_over_toml_and_front_matter() {
        let (_dir, book) = book_dir(
            "title = \"Toml\"\ninputs = [\"a.md\"]\nprofile = \"scribe\"\nsvg = \"rasterize\"\nmargin = \"wide\"\nnotes-pages = 2\ngrayscale = true\n",
        );
        let c = cli(&[
            "build",
            &path_str(&book),
            "--title",
            "Flag",
            "--lang",
            "it",
            "--profile",
            "generic",
            "--margin",
            "extra",
        ]);
        let (o, _) = resolve(&c, Some(FM)).unwrap();
        assert_eq!(o.meta.title, "Flag");
        assert_eq!(o.meta.lang, "it");
        assert_eq!(o.meta.authors, vec!["FM Author"]);
        assert_eq!(o.profile.name, "generic");
        // svg set explicitly in TOML beats the (flag-chosen) profile default.
        assert_eq!(o.svg, SvgMode::Rasterize);
        assert_eq!(o.margin, Margin::Extra);
        assert_eq!(o.notes_pages, 2);
        // A bool flag that is absent does not reset the TOML value.
        assert!(o.grayscale);
    }

    #[test]
    fn toml_bool_false_holds_without_flag_and_flag_turns_on() {
        let (_dir, book) = book_dir("inputs = [\"a.md\"]\noffline = false\n");
        let (o, _) = resolve(&cli(&["build", &path_str(&book)]), None).unwrap();
        assert!(!o.offline);
        let (o, _) = resolve(&cli(&["build", &path_str(&book), "--offline"]), None).unwrap();
        assert!(o.offline);
    }

    #[test]
    fn cli_inputs_replace_toml_inputs() {
        let (dir, book) = book_dir("inputs = [\"a.md\"]\n");
        let b = dir.path().join("b.html");
        // clap cannot place positionals before `build`, but the library API can.
        let c = Cli {
            command: Some(Command::Build { config: book }),
            inputs: vec![b.clone()],
            flags: Flags::default(),
        };
        let (_, inputs) = resolve(&c, None).unwrap();
        assert_eq!(inputs, vec![b]);
    }

    // ---- book.toml ----------------------------------------------------------

    #[test]
    fn parses_fixture_book_toml() {
        let book = fixture("book.toml");
        let (o, inputs) = resolve(&cli(&["build", &path_str(&book)]), None).unwrap();
        assert_eq!(
            inputs,
            vec![fixture("kitchen-sink.md"), fixture("page.html")]
        );
        assert_eq!(o.meta.title, "Fixture Book");
        assert_eq!(o.meta.authors, vec!["Ada Lovelace", "Charles Babbage"]);
        assert_eq!(o.meta.lang, "en");
        assert_eq!(o.output, Some(fixture("fixture-book.epub")));
        assert_eq!(o.cover, Some(fixture("photo.jpg")));
        assert_eq!(o.profile.name, "scribe");
        assert_eq!(o.split_level, Some(2));
        assert_eq!(o.notes, NotesPlacement::AfterChapter);
        assert_eq!(o.notes_style, NotesStyle::Lined);
        assert_eq!(o.notes_pages, 1);
        assert_eq!(o.margin, Margin::Wide);
        assert_eq!(o.line_spacing, Some(1.5));
        assert_eq!(o.math, MathMode::Image);
        assert_eq!(o.svg, SvgMode::Rasterize);
        assert!(!o.grayscale);
        assert_eq!(o.extra_css, Some(fixture("extra.css")));
        assert!(o.offline);
    }

    #[test]
    fn toml_shortcut_without_build() {
        let book = fixture("book.toml");
        let (o, inputs) = resolve(&cli(&[&path_str(&book)]), None).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(o.meta.title, "Fixture Book");
    }

    #[test]
    fn toml_value_forms() {
        let base = Path::new("/books");
        let l = Layer::from_toml_str(
            "author = \"Solo\"\ninput = \"one.md\"\nsplit_level = \"none\"\ndate = 2026-01-15\nline-spacing = 2\nnotes = false\nidentifier = \"urn:x\"\n",
            base,
        )
        .unwrap();
        assert_eq!(l.authors, Some(vec!["Solo".to_string()]));
        assert_eq!(l.inputs, Some(vec![PathBuf::from("/books/one.md")]));
        assert_eq!(l.split_level, Some(None));
        assert_eq!(l.date.as_deref(), Some("2026-01-15"));
        assert_eq!(l.line_spacing, Some(2.0));
        assert_eq!(l.notes, Some(NotesPlacement::None));
        assert_eq!(l.identifier.as_deref(), Some("urn:x"));
        let l = Layer::from_toml_str("split-level = 1\n", base).unwrap();
        assert_eq!(l.split_level, Some(Some(1)));
        let l = Layer::from_toml_str("", base).unwrap();
        assert_eq!(l, Layer::default());
    }

    #[test]
    fn toml_absolute_paths_kept() {
        let abs = std::env::temp_dir().join("abs-cover.png");
        let text = format!("cover = {:?}\n", path_str(&abs));
        let l = Layer::from_toml_str(&text, Path::new("/elsewhere")).unwrap();
        assert_eq!(l.cover, Some(abs));
    }

    #[test]
    fn toml_errors() {
        let base = Path::new(".");
        let cases = [
            ("titel = \"x\"", "unknown key `titel`"),
            ("title = 3", "expected a string"),
            ("authors = [\"a\", 2]", "item 2"),
            ("profile = \"kobo\"", "unknown profile 'kobo'"),
            ("split-level = 9", "invalid split level"),
            ("notes = \"sometimes\"", "invalid notes placement"),
            ("notes-style = \"wavy\"", "invalid notes style"),
            ("margin = \"huge\"", "invalid margin preset"),
            ("math = \"katex\"", "invalid math mode"),
            ("svg = \"pdf\"", "invalid svg mode"),
            ("notes-pages = -1", "non-negative"),
            ("notes-pages = 5000", "0 to 100"),
            ("notes-pages = \"2\"", "whole number"),
            ("line-spacing = \"wide\"", "number"),
            ("line-spacing = 0.0", "invalid line spacing"),
            ("grayscale = \"yes\"", "true or false"),
            ("inputs = 3", "list of strings"),
            ("title = ", "invalid TOML"),
        ];
        for (text, want) in cases {
            let err = format!("{:#}", Layer::from_toml_str(text, base).unwrap_err());
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn toml_file_errors() {
        let err = err_of(resolve(&cli(&["build", "no/such/book.toml"]), None));
        assert!(err.contains("book file not found"), "{err}");

        let (_dir, book) = book_dir("title = \"x\"\n");
        let err = err_of(resolve(&cli(&["build", &path_str(&book)]), None));
        assert!(
            err.contains("no input files") && err.contains("inputs ="),
            "{err}"
        );

        let (_dir, book) = book_dir("inputs = [\"missing.md\"]\n");
        let err = err_of(resolve(&cli(&["build", &path_str(&book)]), None));
        assert!(err.contains("input file not found"), "{err}");

        let (_dir, book) = book_dir("inputs = [\"a.md\"]\nbogus = 1\n");
        let err = err_of(resolve(&cli(&["build", &path_str(&book)]), None));
        assert!(err.contains("book.toml") && err.contains("bogus"), "{err}");
    }

    // ---- front matter -------------------------------------------------------

    #[test]
    fn front_matter_forms() {
        let base = Path::new(".");
        // Without delimiters, list of authors, numeric title, unknown keys.
        let l = Layer::from_front_matter(
            "title: 1984\nauthors:\n  - A\n  - name: B\n    affiliation: X\ntags: [x]\n",
            base,
        )
        .unwrap();
        assert_eq!(l.title.as_deref(), Some("1984"));
        assert_eq!(l.authors, Some(vec!["A".to_string(), "B".to_string()]));
        // With `...` terminator and BOM.
        let l = Layer::from_front_matter("\u{feff}---\nlanguage: de\n...\n", base).unwrap();
        assert_eq!(l.lang.as_deref(), Some("de"));
        // Empty front matter and null values.
        assert_eq!(
            Layer::from_front_matter("---\n---\n", base).unwrap(),
            Layer::default()
        );
        let l = Layer::from_front_matter("title:\nlang: en\n", base).unwrap();
        assert_eq!(l.title, None);
        // Front matter never sets non-metadata options.
        let l = Layer::from_front_matter("profile: scribe\nmath: off\n", base).unwrap();
        assert_eq!(l, Layer::default());
    }

    #[test]
    fn front_matter_errors() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let err = err_of(resolve(&cli(&[&md]), Some("---\ntitle: [unclosed\n---\n")));
        assert!(err.contains("invalid YAML front matter"), "{err}");
        assert!(err.contains("kitchen-sink.md"), "{err}");
        let err = err_of(resolve(&cli(&[&md]), Some("- just\n- a list\n")));
        assert!(err.contains("mapping"), "{err}");
        let err = err_of(resolve(&cli(&[&md]), Some("title: {a: b}\n")));
        assert!(err.contains("key `title`"), "{err}");
        let err = err_of(resolve(&cli(&[&md]), Some("author:\n  - role: x\n")));
        assert!(err.contains("`name`"), "{err}");
    }

    #[test]
    fn blank_values_fall_back() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let (o, _) = resolve(
            &cli(&[&md, "--lang", " ", "--author", " "]),
            Some("date: ''\n"),
        )
        .unwrap();
        assert_eq!(o.meta.lang, "en");
        assert!(o.meta.authors.is_empty());
        assert_eq!(o.meta.date, None);
    }

    // ---- bad inputs ---------------------------------------------------------

    #[test]
    fn no_inputs_errors() {
        let err = err_of(resolve(&cli(&[]), None));
        assert!(err.contains("no input files given"), "{err}");
    }

    #[test]
    fn missing_input_errors() {
        let err = err_of(resolve(&cli(&["does-not-exist.md"]), None));
        assert!(
            err.contains("input file not found: does-not-exist.md"),
            "{err}"
        );
    }

    #[test]
    fn unknown_extension_errors() {
        let toml = path_str(&Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock"));
        let err = err_of(resolve(&cli(&[&toml]), None));
        assert!(err.contains("unsupported input"), "{err}");
        assert!(err.contains(".lock") && err.contains(".md"), "{err}");
        let jpg = path_str(&fixture("photo.jpg"));
        let md = path_str(&fixture("kitchen-sink.md"));
        let err = err_of(resolve(&cli(&[&md, &jpg]), None));
        assert!(err.contains("photo.jpg"), "{err}");
    }

    #[test]
    fn directory_input_errors() {
        let dir = path_str(&fixture("images"));
        let err = err_of(resolve(&cli(&[&dir]), None));
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn bad_flag_values_error() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let err = err_of(resolve(&cli(&[&md, "--profile", "kobo"]), None));
        assert!(err.contains("--profile") && err.contains("kobo"), "{err}");
        let err = err_of(resolve(&cli(&[&md, "--split-level", "7"]), None));
        assert!(err.contains("--split-level"), "{err}");
        let err = err_of(resolve(&cli(&[&md, "--line-spacing", "0"]), None));
        assert!(err.contains("--line-spacing"), "{err}");
        let err = err_of(resolve(&cli(&[&md, "--notes-pages", "1000"]), None));
        assert!(err.contains("--notes-pages"), "{err}");
    }

    #[test]
    fn strip_delimiters() {
        assert_eq!(strip_front_matter_delimiters("---\na: 1\n---\n"), "a: 1\n");
        assert_eq!(strip_front_matter_delimiters("a: 1\n"), "a: 1\n");
        assert_eq!(strip_front_matter_delimiters("---\na: 1\n...\n"), "a: 1\n");
        assert_eq!(strip_front_matter_delimiters(""), "");
        assert_eq!(strip_front_matter_delimiters("---"), "");
    }

    // --- Linked local files ---
    #[test]
    fn follow_links_from_toml_and_flags() {
        let (_dir, plain) = book_dir("inputs = [\"a.md\"]\n");
        let (o, _) = resolve(&cli(&["build", &path_str(&plain)]), None).unwrap();
        assert!(o.follow_links, "on by default");
        let (_dir, book) = book_dir("inputs = [\"a.md\"]\nfollow-links = false\n");
        let (o, _) = resolve(&cli(&["build", &path_str(&book)]), None).unwrap();
        assert!(!o.follow_links);
        let (o, _) = resolve(&cli(&["build", &path_str(&book), "--follow-links"]), None).unwrap();
        assert!(o.follow_links);
        let (_dir, on) = book_dir("inputs = [\"a.md\"]\nfollow_links = true\n");
        let (o, _) = resolve(&cli(&["build", &path_str(&on), "--no-follow-links"]), None).unwrap();
        assert!(!o.follow_links);
        let (_dir, bad) = book_dir("inputs = [\"a.md\"]\nfollow-links = \"yes please\"\n");
        assert!(resolve(&cli(&["build", &path_str(&bad)]), None).is_err());
    }

    #[test]
    fn negated_flags_turn_toml_booleans_off() {
        let (_dir, book) =
            book_dir("inputs = [\"a.md\"]\noffline = true\ngrayscale = true\nverbose = true\n");
        let (o, _) = resolve(&cli(&["build", &path_str(&book)]), None).unwrap();
        assert!(o.offline && o.grayscale && o.verbose);
        let (o, _) = resolve(
            &cli(&[
                "build",
                &path_str(&book),
                "--no-offline",
                "--no-grayscale",
                "--no-verbose",
            ]),
            None,
        )
        .unwrap();
        assert!(!o.offline && !o.grayscale && !o.verbose);
    }

    #[test]
    fn explicit_missing_cover_is_an_error() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let err = err_of(resolve(&cli(&[&md, "--cover", "nope-cover.png"]), None));
        assert!(err.contains("cover image not found"), "{err}");
        let (_dir, book) = book_dir("inputs = [\"a.md\"]\ncover = \"missing.png\"\n");
        let err = err_of(resolve(&cli(&["build", &path_str(&book)]), None));
        assert!(err.contains("cover image not found"), "{err}");
        // URLs are checked when loaded; front matter covers only warn later.
        assert!(
            resolve(
                &cli(&[&md, "--cover", "https://example.invalid/c.png"]),
                None
            )
            .is_ok()
        );
        let (o, _) = resolve(&cli(&[&md]), Some("cover: nope.png\n")).unwrap();
        assert!(o.cover.is_some());
    }

    #[test]
    fn missing_build_input_hints_at_subcommand() {
        let c = Cli {
            command: None,
            inputs: vec![PathBuf::from("build")],
            flags: Flags::default(),
        };
        let err = err_of(resolve(&c, None));
        assert!(err.contains("md2epub build"), "{err}");
    }

    #[test]
    fn lang_given_tracks_explicit_language() {
        let md = path_str(&fixture("kitchen-sink.md"));
        let (o, _) = resolve(&cli(&[&md]), None).unwrap();
        assert!(!o.lang_given);
        assert_eq!(o.meta.lang, "en");
        let (o, _) = resolve(&cli(&[&md, "--lang", "de"]), None).unwrap();
        assert!(o.lang_given);
        let (o, _) = resolve(&cli(&[&md]), Some("lang: fr\n")).unwrap();
        assert!(o.lang_given);
    }

    #[test]
    fn language_tags_are_normalized_and_validated() {
        for (raw, want) in [
            ("en", "en"),
            ("EN", "en"),
            ("en_US", "en-US"),
            ("en us", "en-US"),
            ("en-us", "en-US"),
            ("zh_hant_tw", "zh-Hant-TW"),
            ("es-419", "es-419"),
            ("de-ch-1996", "de-CH-1996"),
            ("x-private", "x-private"),
            ("sl-rozaj-biske", "sl-rozaj-biske"),
        ] {
            let got = normalize_lang(raw);
            assert_eq!(got.as_deref(), Some(want), "{raw}");
        }
        for bad in [
            "",
            "7",
            "e",
            "xx-invalid-tag-!!",
            "en--US",
            "toolongtag",
            "en-",
            "a-b-",
        ] {
            assert_eq!(normalize_lang(bad), None, "{bad}");
        }
    }

    #[test]
    fn finalize_fixes_or_drops_bad_metadata() {
        let mut o = Options::default();
        o.meta.lang = "en_US".into();
        o.meta.date = Some("2024/1/5".into());
        assert!(finalize(&mut o).is_empty());
        assert_eq!(o.meta.lang, "en-US");
        assert_eq!(o.meta.date.as_deref(), Some("2024-01-05"));

        let mut o = Options::default();
        o.meta.lang = "xx-invalid-tag-!!".into();
        o.meta.date = Some("2024-13-45".into());
        let w = finalize(&mut o);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("language tag"), "{w:?}");
        assert!(w[1].contains("2024-13-45"), "{w:?}");
        assert_eq!(o.meta.lang, "en");
        assert_eq!(o.meta.date, None);

        // Front matter `lang: 7` reaches finalize as "7".
        let md = path_str(&fixture("kitchen-sink.md"));
        let (mut o, _) = resolve(&cli(&[&md]), Some("lang: 7\n")).unwrap();
        assert_eq!(finalize(&mut o).len(), 1);
        assert_eq!(o.meta.lang, "en");
    }

    #[test]
    fn dates_are_normalized() {
        for (raw, want) in [
            ("2024", Some("2024")),
            ("2024-01", Some("2024-01")),
            ("2024-01-15", Some("2024-01-15")),
            ("2024.1.15", Some("2024-01-15")),
            ("15/01/2024", Some("2024-01-15")),
            ("01/15/2024", Some("2024-01-15")),
            ("2024-01-15 10:30:00", Some("2024-01-15T10:30:00")),
            ("2024-01-15T10:30:00Z", Some("2024-01-15T10:30:00Z")),
            ("2024-02-29", Some("2024-02-29")),
            ("2023-02-29", None),
            ("01/02/2024", None),
            ("2024-13-45", None),
            ("yesterday", None),
            ("2024-01T10:00", None),
        ] {
            assert_eq!(normalize_date(raw).as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn finalize_warns_about_svg_and_mathml_on_device_profiles() {
        let md = path_str(&fixture("kitchen-sink.md"));
        for profile in ["scribe", "remarkable", "remarkable-pro"] {
            let (mut o, _) = resolve(
                &cli(&[
                    &md,
                    "--profile",
                    profile,
                    "--svg",
                    "keep",
                    "--math",
                    "mathml",
                ]),
                None,
            )
            .unwrap();
            let w = finalize(&mut o);
            assert_eq!(w.len(), 2, "{w:?}");
            assert!(w[0].contains("SVG kept") && w[0].contains(profile), "{w:?}");
            assert!(
                w[1].contains("MathML") && w[1].contains("Send to Kindle"),
                "{w:?}"
            );
        }
        let (mut o, _) = resolve(&cli(&[&md, "--profile", "scribe"]), None).unwrap();
        assert!(finalize(&mut o).is_empty());
        let (mut o, _) = resolve(&cli(&[&md, "--svg", "keep", "--math", "mathml"]), None).unwrap();
        assert!(finalize(&mut o).is_empty());
    }

    // ---- user config --------------------------------------------------------

    fn user(text: &str) -> UserConfig {
        UserConfig::from_toml_str(text, Path::new("/cfg")).unwrap()
    }

    #[test]
    fn user_config_sits_below_front_matter_toml_and_flags() {
        let u = user(
            "profile = \"scribe\"\nauthor = \"User\"\nlang = \"de\"\nnotes = \"end\"\nmargin = \"wide\"\ngrayscale = true\noutput-location = \"input\"\n",
        );
        let md = path_str(&fixture("kitchen-sink.md"));
        // Alone, the user config replaces the built-in defaults, and the
        // profile it names drives the profile defaults.
        let (o, _) = resolve_with(&cli(&[&md]), None, &u).unwrap();
        assert_eq!(o.profile.name, "scribe");
        assert_eq!(o.svg, SvgMode::Rasterize);
        assert_eq!(o.meta.authors, vec!["User"]);
        assert_eq!(o.meta.lang, "de");
        assert!(o.lang_given);
        assert_eq!(o.notes, NotesPlacement::End);
        assert_eq!(o.margin, Margin::Wide);
        assert!(o.grayscale);
        assert_eq!(o.output_location, OutputLocation::Input);
        // Front matter beats the user config.
        let (o, _) = resolve_with(&cli(&[&md]), Some(FM), &u).unwrap();
        assert_eq!(o.meta.authors, vec!["FM Author"]);
        assert_eq!(o.meta.lang, "fr");
        assert_eq!(o.notes, NotesPlacement::End);
        // book.toml beats the user config.
        let (_dir, book) =
            book_dir("inputs = [\"a.md\"]\nprofile = \"generic\"\nmargin = \"extra\"\n");
        let (o, _) = resolve_with(&cli(&["build", &path_str(&book)]), None, &u).unwrap();
        assert_eq!(o.profile.name, "generic");
        assert_eq!(
            o.svg,
            SvgMode::Keep,
            "profile defaults follow the winning profile"
        );
        assert_eq!(o.margin, Margin::Extra);
        assert_eq!(o.notes, NotesPlacement::End);
        // Flags beat everything.
        let (o, _) = resolve_with(
            &cli(&[
                &md,
                "--profile",
                "remarkable",
                "--no-grayscale",
                "--output-location",
                "cwd",
                "--author",
                "Flag",
            ]),
            Some(FM),
            &u,
        )
        .unwrap();
        assert_eq!(o.profile.name, "remarkable");
        assert!(!o.grayscale);
        assert_eq!(o.output_location, OutputLocation::Cwd);
        assert_eq!(o.meta.authors, vec!["Flag"]);
    }

    #[test]
    fn user_config_explicit_svg_survives_profile_change() {
        let u = user("profile = \"scribe\"\nsvg = \"keep\"\nmath = \"off\"\n");
        let md = path_str(&fixture("kitchen-sink.md"));
        let (o, _) = resolve_with(&cli(&[&md, "--profile", "generic"]), None, &u).unwrap();
        assert_eq!(o.svg, SvgMode::Keep);
        assert_eq!(o.math, MathMode::Off);
        let d = defaults(&u).unwrap();
        assert_eq!((d.profile.name.as_str(), d.svg), ("scribe", SvgMode::Keep));
    }

    #[test]
    fn user_config_rejects_bad_keys_and_values() {
        for (text, needle) in [
            ("title = \"x\"", "single book"),
            ("output = \"x.epub\"", "single book"),
            ("inputs = [\"a.md\"]", "single book"),
            ("cover = \"c.png\"", "single book"),
            ("colour = \"red\"", "unknown key `colour`"),
            ("math = \"katex\"", "key `math`"),
            ("notes-pages = -1", "key `notes-pages`"),
            ("lang = \"not a tag!\"", "key `lang`"),
            ("follow-links = \"maybe\"", "key `follow-links`"),
            ("profile = [", "invalid TOML"),
        ] {
            let err = format!(
                "{:#}",
                UserConfig::from_toml_str(text, Path::new(".")).unwrap_err()
            );
            assert!(err.contains(needle), "{text}: {err}");
        }
        // book.toml takes output-location and follow-links too.
        assert!(Layer::from_toml_str("output-location = \"input\"", Path::new(".")).is_ok());
        let book = Layer::from_toml_str("follow-links = false", Path::new(".")).unwrap();
        assert_eq!(book.follow_links, Some(false));
        let u = user("follow_links = false\nAuthors = [\"A\", \"B\"]\n");
        assert_eq!(u.follow_links(), Some(false));
        assert!(u.is_set("author") && u.is_set("follow-links"));
    }

    #[test]
    fn missing_user_config_is_empty_and_errors_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(UserConfig::load(&path).unwrap(), UserConfig::default());
        fs::write(&path, "margin = \"huge\"\n").unwrap();
        let err = format!("{:#}", UserConfig::load(&path).unwrap_err());
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("`margin`"), "{err}");
        assert!(err.contains("md2epub config"), "{err}");
    }
}

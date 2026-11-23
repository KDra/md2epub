//! User defaults: where the user config file lives, the values it gives,
//! and the `md2epub config` subcommand.
//!
//! The file is TOML with the keys in [`USER_KEYS`] (the long flag names),
//! for example:
//!
//! ```toml
//! profile = "scribe"
//! author = ["Ada Lovelace"]
//! notes = "after-chapter"
//! grayscale = true
//! ```
//!
//! It lives at `<config dir>/md2epub/config.toml` (`dirs::config_dir()`:
//! `~/.config` on Linux, `~/Library/Application Support` on macOS,
//! `%APPDATA%` on Windows); the `MD2EPUB_CONFIG` environment variable names
//! another file. Its values sit between front matter and the profile
//! defaults (see [`crate::config`]). `config set` and `config unset` edit
//! the file with `toml_edit`, so comments and the other keys are kept.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::cli::ConfigAction;
use crate::config::{self, USER_KEYS, UserConfig, canonical_key, normalize_lang};
use crate::model::{Margin, MathMode, NotesPlacement, NotesStyle, OutputLocation, SvgMode};
use crate::profile;

/// Environment variable that overrides the user config path.
pub const ENV_VAR: &str = "MD2EPUB_CONFIG";

/// Built-in default of `follow-links` (the option itself belongs to the
/// link-following feature).
const FOLLOW_LINKS_DEFAULT: bool = true;

/// First lines of a config file created by `config set` or `config edit`.
const TEMPLATE: &str = "\
# md2epub user defaults. Each key sets the default of the flag with the
# same name; flags, book.toml and front matter still win. Examples:
#
#   profile = \"scribe\"
#   author = [\"Ada Lovelace\"]
#   notes = \"after-chapter\"
#   grayscale = true
#
# Keys: profile, author, lang, split-level, notes, notes-style, notes-pages,
# margin, line-spacing, math, svg, grayscale, offline, verbose, css,
# output-location, follow-links.
# `md2epub config list` shows the effective values.
";

/// Path of the user config file: `$MD2EPUB_CONFIG` when set and not empty,
/// else `<config dir>/md2epub/config.toml`.
pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(ENV_VAR).filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    match dirs::config_dir() {
        Some(dir) => Ok(dir.join("md2epub").join("config.toml")),
        None => bail!(
            "cannot find the user configuration directory; set {ENV_VAR} to the path of a config file"
        ),
    }
}

/// Loads the user config from [`config_path`]. Without a config directory
/// there is simply no user config.
pub fn load() -> Result<UserConfig> {
    match config_path() {
        Ok(p) => UserConfig::load(&p),
        Err(_) => Ok(UserConfig::default()),
    }
}

/// Where an effective default comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// md2epub's own default.
    BuiltIn,
    /// The default of the named profile (`svg`, `math`).
    Profile(String),
    /// The user config file.
    User,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::BuiltIn => f.write_str("built-in"),
            Source::Profile(p) => write!(f, "built-in (profile {p})"),
            Source::User => f.write_str("user config"),
        }
    }
}

/// One settable key with its effective default.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    /// Canonical key, one of [`USER_KEYS`].
    pub key: &'static str,
    /// Value in `config set` syntax (`true`, `scribe`, `1.5`, ...);
    /// `none` or a description when unset.
    pub value: String,
    pub source: Source,
}

/// Every settable key with its effective default under `user`.
pub fn settings(user: &UserConfig) -> Result<Vec<Setting>> {
    let o = config::defaults(user)?;
    let profile_name = o.profile.name.clone();
    let mut out = Vec::new();
    for &key in USER_KEYS {
        let value = match key {
            "profile" => o.profile.name.clone(),
            "author" if o.meta.authors.is_empty() => "none".into(),
            "author" => o.meta.authors.join(", "),
            "lang" => o.meta.lang.clone(),
            "split-level" => o
                .split_level
                .map_or_else(|| "none".into(), |n| n.to_string()),
            "notes" => o.notes.as_str().into(),
            "notes-style" => o.notes_style.as_str().into(),
            "notes-pages" => o.notes_pages.to_string(),
            "margin" => o.margin.as_str().into(),
            "line-spacing" => o
                .line_spacing
                .map_or_else(|| "reader default".into(), |f| f.to_string()),
            "math" => o.math.as_str().into(),
            "svg" => o.svg.as_str().into(),
            "grayscale" => o.grayscale.to_string(),
            "offline" => o.offline.to_string(),
            "verbose" => o.verbose.to_string(),
            "css" => o
                .extra_css
                .as_ref()
                .map_or_else(|| "none".into(), |p| p.display().to_string()),
            "output-location" => o.output_location.as_str().into(),
            "follow-links" => user
                .follow_links()
                .unwrap_or(FOLLOW_LINKS_DEFAULT)
                .to_string(),
            _ => continue,
        };
        let source = if user.is_set(key) {
            Source::User
        } else if matches!(key, "math" | "svg") {
            Source::Profile(profile_name.clone())
        } else {
            Source::BuiltIn
        };
        out.push(Setting { key, value, source });
    }
    Ok(out)
}

/// `[default: ...]` texts for `--help`, keyed by clap argument id (see
/// [`crate::cli::command_with_defaults`]).
pub fn help_defaults(user: &UserConfig) -> Vec<(&'static str, String)> {
    let settings = settings(user)
        .or_else(|_| settings(&UserConfig::default()))
        .unwrap_or_default();
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let output_location = settings
        .iter()
        .find(|s| s.key == "output-location")
        .map_or("cwd", |s| s.value.as_str());
    out.push((
        "output",
        if output_location == "input" {
            "<first input>.epub beside the first input".into()
        } else {
            "<first input>.epub in the current directory".into()
        },
    ));
    out.push((
        "title",
        "front matter, then first heading, then file name".into(),
    ));
    out.push(("cover", "none (a title page is generated)".into()));
    for s in settings {
        let id = match s.key {
            "author" => "authors",
            "split-level" => "split_level",
            "notes-style" => "notes_style",
            "notes-pages" => "notes_pages",
            "line-spacing" => "line_spacing",
            "output-location" => "output_location",
            "follow-links" => "follow_links",
            other => other,
        };
        let value = match s.value.as_str() {
            "true" => "on".to_string(),
            "false" => "off".to_string(),
            _ => s.value,
        };
        out.push((id, value));
    }
    out
}

/// The note shown at the end of `--help`.
pub fn help_footer() -> String {
    let where_ = config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "not available".into());
    format!(
        "Defaults in [default: ...] include your own defaults from `md2epub config` ({where_}).\n\
         Precedence: flags > book.toml > front matter > user config > profile defaults."
    )
}

/// Runs a `md2epub config` action on the config file at `path`, writing
/// its normal output to `out`.
pub fn run(action: Option<&ConfigAction>, path: &Path, out: &mut dyn Write) -> Result<()> {
    match action.unwrap_or(&ConfigAction::List) {
        ConfigAction::List => list(path, out),
        ConfigAction::Get { key } => get(path, key, out),
        ConfigAction::Set { key, values } => set(path, key, values, out),
        ConfigAction::Unset { key } => unset(path, key, out),
        ConfigAction::Reset => reset(path, out),
        ConfigAction::Path => {
            writeln!(out, "{}", path.display())?;
            Ok(())
        }
        ConfigAction::Edit => edit(path, out),
    }
}

fn list(path: &Path, out: &mut dyn Write) -> Result<()> {
    let user = UserConfig::load(path)?;
    let state = if path.is_file() {
        ""
    } else {
        " (not created yet)"
    };
    writeln!(out, "# user config: {}{state}", path.display())?;
    let settings = settings(&user)?;
    let width = settings
        .iter()
        .map(|s| s.value.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    for s in settings {
        writeln!(out, "{:<16} {:<width$}  {}", s.key, s.value, s.source)?;
    }
    Ok(())
}

fn get(path: &Path, key: &str, out: &mut dyn Write) -> Result<()> {
    let key = user_key(key)?;
    let user = UserConfig::load(path)?;
    let setting = settings(&user)?
        .into_iter()
        .find(|s| s.key == key)
        .ok_or_else(|| anyhow!("unknown key `{key}`"))?;
    writeln!(out, "{}", setting.value)?;
    Ok(())
}

/// Checks a key name given to `get`/`set`/`unset` and returns its
/// canonical spelling.
fn user_key(raw: &str) -> Result<&'static str> {
    let key = canonical_key(raw);
    if let Some(k) = USER_KEYS.iter().find(|k| **k == key) {
        return Ok(k);
    }
    if [
        "title",
        "identifier",
        "date",
        "cover",
        "inputs",
        "input",
        "output",
    ]
    .contains(&key.as_str())
    {
        bail!(
            "`{raw}` cannot be a user default because it describes a single book; give it on the command line or in book.toml"
        );
    }
    bail!(
        "unknown key `{raw}`: expected one of {}",
        USER_KEYS.join(", ")
    )
}

/// Parses a boolean the way people type it.
fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => bail!("invalid value '{s}': expected true or false"),
    }
}

/// Converts the command-line values of `config set` into a TOML value,
/// checking them with the same parsers as the flags.
fn typed_value(key: &str, values: &[String]) -> Result<toml_edit::Value> {
    if key == "author" {
        let names: Vec<String> = values
            .iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        return match names.as_slice() {
            [] => bail!("expected at least one author name"),
            [one] => Ok(one.as_str().into()),
            many => Ok(toml_edit::Value::Array(many.iter().collect())),
        };
    }
    let [v] = values else {
        bail!(
            "`{key}` takes exactly one value, got {} (quote a value that contains spaces)",
            values.len()
        );
    };
    let v = v.trim();
    Ok(match key {
        "profile" => profile::by_name(v)?.name.into(),
        "lang" => normalize_lang(v)
            .ok_or_else(|| {
                anyhow!("invalid language tag '{v}': expected a BCP 47 tag such as en, de or pt-BR")
            })?
            .into(),
        "split-level" => match crate::model::parse_split_level(v)? {
            Some(n) => i64::from(n).into(),
            None => "none".into(),
        },
        "notes" => v.parse::<NotesPlacement>()?.as_str().into(),
        "notes-style" => v.parse::<NotesStyle>()?.as_str().into(),
        "margin" => v.parse::<Margin>()?.as_str().into(),
        "math" => v.parse::<MathMode>()?.as_str().into(),
        "svg" => v.parse::<SvgMode>()?.as_str().into(),
        "output-location" => v.parse::<OutputLocation>()?.as_str().into(),
        "notes-pages" => v
            .parse::<u32>()
            .map(i64::from)
            .map_err(|_| anyhow!("invalid number of note pages '{v}': expected a whole number"))?
            .into(),
        "line-spacing" => v
            .parse::<f64>()
            .map_err(|_| anyhow!("invalid line spacing '{v}': expected a number such as 1.5"))?
            .into(),
        "grayscale" | "offline" | "verbose" | "follow-links" => parse_bool(v)?.into(),
        "css" => {
            let p = std::path::absolute(v).with_context(|| format!("invalid path '{v}'"))?;
            if !p.is_file() {
                bail!("CSS file not found: {}", p.display());
            }
            p.to_string_lossy().into_owned().into()
        }
        _ => bail!("unknown key `{key}`"),
    })
}

/// Reads the config file as an editable document (an empty one when the
/// file does not exist).
fn read_document(path: &Path) -> Result<Option<toml_edit::DocumentMut>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| {
        anyhow!(
            "{} is not valid TOML ({}); fix it with `md2epub config edit` or remove it with `md2epub config reset`",
            path.display(),
            e.to_string().trim()
        )
    })?;
    Ok(Some(doc))
}

fn write_document(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

/// Raw spellings in `doc` of the canonical `key` (`author`, `authors`,
/// `split_level`, ...).
fn spellings(doc: &toml_edit::DocumentMut, key: &str) -> Vec<String> {
    doc.iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| canonical_key(k) == key)
        .collect()
}

fn set(path: &Path, raw_key: &str, values: &[String], out: &mut dyn Write) -> Result<()> {
    let key = user_key(raw_key)?;
    let value = typed_value(key, values).with_context(|| format!("config set {key}"))?;
    // Check the value exactly as the file will hold it.
    let mut single = toml_edit::DocumentMut::new();
    single[key] = toml_edit::Item::Value(value.clone());
    let base = path.parent().unwrap_or(Path::new("."));
    UserConfig::from_toml_str(&single.to_string(), base)
        .with_context(|| format!("config set {key}"))?;

    let mut doc = match read_document(path)? {
        Some(d) => d,
        None => TEMPLATE.parse::<toml_edit::DocumentMut>()?,
    };
    let existing = spellings(&doc, key);
    // Keep the key where it was, under the spelling the user chose.
    let target = existing.first().cloned().unwrap_or_else(|| key.to_string());
    for other in existing.iter().skip(1) {
        doc.remove(other);
    }
    let was_empty = doc.is_empty();
    match doc.get_mut(&target).and_then(|i| i.as_value_mut()) {
        Some(slot) => {
            let decor = slot.decor().clone();
            *slot = value;
            *slot.decor_mut() = decor;
        }
        None => doc[target.as_str()] = toml_edit::Item::Value(value),
    }
    // In a file with only comments (such as the template), toml_edit keeps
    // them after the last key; keep them above the first one instead.
    if was_empty {
        let comments = doc.trailing().as_str().unwrap_or("").to_string();
        if !comments.trim().is_empty() {
            doc.set_trailing("");
            if let Some(mut k) = doc.as_table_mut().key_mut(&target) {
                k.leaf_decor_mut()
                    .set_prefix(format!("{}\n\n", comments.trim_end()));
            }
        }
    }
    write_document(path, &doc)?;
    let shown = settings(&UserConfig::from_toml_str(&single.to_string(), base)?)?
        .into_iter()
        .find(|s| s.key == key)
        .map(|s| s.value)
        .unwrap_or_default();
    writeln!(out, "{key} = {shown} (saved in {})", path.display())?;
    Ok(())
}

fn unset(path: &Path, raw_key: &str, out: &mut dyn Write) -> Result<()> {
    let key = user_key(raw_key)?;
    let Some(mut doc) = read_document(path)? else {
        writeln!(
            out,
            "{key} was not set (no user config at {})",
            path.display()
        )?;
        return Ok(());
    };
    let found = spellings(&doc, key);
    if found.is_empty() {
        writeln!(out, "{key} was not set in {}", path.display())?;
        return Ok(());
    }
    for k in &found {
        doc.remove(k);
    }
    write_document(path, &doc)?;
    let builtin = settings(&UserConfig::default())?
        .into_iter()
        .find(|s| s.key == key)
        .map(|s| s.value)
        .unwrap_or_default();
    writeln!(
        out,
        "removed {key} from {}; the built-in default applies ({builtin})",
        path.display()
    )?;
    Ok(())
}

fn reset(path: &Path, out: &mut dyn Write) -> Result<()> {
    if !path.exists() {
        writeln!(
            out,
            "no user config at {}; nothing to reset",
            path.display()
        )?;
        return Ok(());
    }
    if path.is_dir() {
        bail!("{} is a directory, not a config file", path.display());
    }
    // Say what is being dropped, even when the file no longer parses.
    let keys: Vec<String> = fs::read_to_string(path)
        .ok()
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .map(|d| d.iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default();
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    if keys.is_empty() {
        writeln!(out, "removed {}", path.display())?;
    } else {
        writeln!(
            out,
            "removed {} (it set: {})",
            path.display(),
            keys.join(", ")
        )?;
    }
    writeln!(out, "all settings are back to their built-in defaults")?;
    Ok(())
}

/// The editor command: `$VISUAL`, then `$EDITOR`, then `notepad` on
/// Windows and `vi` elsewhere.
fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.into())
}

fn edit(path: &Path, out: &mut dyn Write) -> Result<()> {
    if path.is_dir() {
        bail!("{} is a directory, not a config file", path.display());
    }
    if !path.exists() {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(path, TEMPLATE).with_context(|| format!("creating {}", path.display()))?;
    }
    let editor = editor();
    // The editor may carry arguments (`code --wait`), so let a shell split it.
    let status = if cfg!(windows) {
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or("notepad");
        std::process::Command::new(program)
            .args(parts)
            .arg(path)
            .status()
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(path)
            .status()
    }
    .with_context(|| format!("starting the editor `{editor}` (set VISUAL or EDITOR)"))?;
    if !status.success() {
        bail!("the editor `{editor}` exited with {status}");
    }
    let user = UserConfig::load(path)?;
    let n = user.keys().len();
    writeln!(
        out,
        "{} is valid ({n} setting{} set)",
        path.display(),
        if n == 1 { "" } else { "s" }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_ok(path: &Path, action: ConfigAction) -> String {
        let mut out = Vec::new();
        run(Some(&action), path, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_err(path: &Path, action: ConfigAction) -> String {
        let mut out = Vec::new();
        format!("{:#}", run(Some(&action), path, &mut out).unwrap_err())
    }

    fn set_action(key: &str, values: &[&str]) -> ConfigAction {
        ConfigAction::Set {
            key: key.into(),
            values: values.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn get(path: &Path, key: &str) -> String {
        run_ok(path, ConfigAction::Get { key: key.into() })
            .trim_end()
            .to_string()
    }

    #[test]
    fn set_get_unset_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        assert_eq!(get(&path, "profile"), "generic");
        assert_eq!(get(&path, "svg"), "keep");
        run_ok(&path, set_action("profile", &["kindle"]));
        assert!(path.is_file());
        assert_eq!(get(&path, "profile"), "scribe");
        // Profile-derived defaults follow the configured profile.
        assert_eq!(get(&path, "svg"), "rasterize");
        run_ok(&path, set_action("notes_pages", &["3"]));
        run_ok(&path, set_action("line-spacing", &["1.5"]));
        run_ok(&path, set_action("grayscale", &["on"]));
        run_ok(&path, set_action("split-level", &["none"]));
        run_ok(
            &path,
            set_action("author", &["Ada Lovelace", "Charles Babbage"]),
        );
        run_ok(&path, set_action("lang", &["en_gb"]));
        run_ok(&path, set_action("output-location", &["input"]));
        run_ok(&path, set_action("follow-links", &["false"]));
        assert_eq!(get(&path, "notes-pages"), "3");
        assert_eq!(get(&path, "line-spacing"), "1.5");
        assert_eq!(get(&path, "grayscale"), "true");
        assert_eq!(get(&path, "split-level"), "none");
        assert_eq!(get(&path, "authors"), "Ada Lovelace, Charles Babbage");
        assert_eq!(get(&path, "lang"), "en-GB");
        assert_eq!(get(&path, "output-location"), "input");
        assert_eq!(get(&path, "follow-links"), "false");
        let list = run_ok(&path, ConfigAction::List);
        assert!(list.contains("user config"), "{list}");
        assert!(list.contains("built-in"), "{list}");
        for key in USER_KEYS {
            assert!(list.contains(key), "{key} missing from:\n{list}");
        }
        // Setting again replaces the value in place.
        run_ok(&path, set_action("profile", &["remarkable"]));
        let text = fs::read_to_string(&path).unwrap();
        let keys: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            keys.iter().filter(|l| l.starts_with("profile")).count(),
            1,
            "{text}"
        );
        assert!(text.contains("\nprofile = \"remarkable\"\n"), "{text}");
        // The template comments stay at the top.
        assert!(text.starts_with("# md2epub user defaults."), "{text}");

        let msg = run_ok(
            &path,
            ConfigAction::Unset {
                key: "profile".into(),
            },
        );
        assert!(msg.contains("removed profile"), "{msg}");
        assert_eq!(get(&path, "profile"), "generic");
        let msg = run_ok(
            &path,
            ConfigAction::Unset {
                key: "profile".into(),
            },
        );
        assert!(msg.contains("was not set"), "{msg}");
        assert_eq!(get(&path, "notes-pages"), "3", "other keys are kept");
    }

    #[test]
    fn set_keeps_comments_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# my settings\nnotes = \"end\" # trailing\n\n# margins\nmargin = \"wide\"\n",
        )
        .unwrap();
        run_ok(&path, set_action("margin", &["extra"]));
        run_ok(&path, set_action("math", &["mathml"]));
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my settings\nnotes = \"end\" # trailing\n"),
            "{text}"
        );
        assert!(text.contains("# margins\nmargin = \"extra\""), "{text}");
        assert!(text.contains("math = \"mathml\""), "{text}");
        // An alias spelling is updated where it is.
        fs::write(&path, "authors = [\"A\"]\n").unwrap();
        run_ok(&path, set_action("author", &["B"]));
        assert_eq!(fs::read_to_string(&path).unwrap(), "authors = \"B\"\n");
    }

    #[test]
    fn invalid_keys_and_values_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (key, values, needle) in [
            ("colour", &["red"][..], "unknown key"),
            ("title", &["T"], "single book"),
            ("output", &["x.epub"], "single book"),
            ("profile", &["kobo"], "unknown profile"),
            ("math", &["katex"], "math mode"),
            ("notes-pages", &["many"], "whole number"),
            ("notes-pages", &["1000"], "note pages"),
            ("line-spacing", &["wide"], "line spacing"),
            ("line-spacing", &["-1"], "line spacing"),
            ("grayscale", &["maybe"], "true or false"),
            ("split-level", &["9"], "split level"),
            ("lang", &["not a tag!"], "language"),
            ("css", &["/no/such/file.css"], "not found"),
            ("margin", &["wide", "extra"], "exactly one value"),
            ("output-location", &["desktop"], "output location"),
        ] {
            let err = run_err(&path, set_action(key, values));
            assert!(err.contains(needle), "{key} {values:?}: {err}");
        }
        assert!(!path.exists(), "failed sets must not create the file");
        let err = run_err(&path, ConfigAction::Get { key: "nope".into() });
        assert!(err.contains("unknown key"), "{err}");
        let err = run_err(
            &path,
            ConfigAction::Unset {
                key: "cover".into(),
            },
        );
        assert!(err.contains("single book"), "{err}");
    }

    #[test]
    fn broken_file_errors_name_file_and_key_but_reset_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "profile = \"scribe\"\nmath = \"katex\"\n").unwrap();
        let err = run_err(&path, ConfigAction::List);
        assert!(err.contains("config.toml"), "{err}");
        assert!(err.contains("`math`"), "{err}");
        let err = format!("{:#}", UserConfig::load(&path).unwrap_err());
        assert!(
            err.contains("config.toml") && err.contains("`math`"),
            "{err}"
        );
        // Fixing the bad key with set or unset works on a parseable file.
        run_ok(&path, ConfigAction::Unset { key: "math".into() });
        assert_eq!(get(&path, "profile"), "scribe");

        fs::write(&path, "title = \"x\"\n").unwrap();
        let err = run_err(&path, ConfigAction::List);
        assert!(err.contains("single book"), "{err}");

        fs::write(&path, "profile = [unclosed").unwrap();
        let err = run_err(&path, set_action("math", &["off"]));
        assert!(err.contains("not valid TOML"), "{err}");
        assert_eq!(
            run_ok(&path, ConfigAction::Path).trim_end(),
            path.display().to_string()
        );
        let msg = run_ok(&path, ConfigAction::Reset);
        assert!(msg.contains("removed"), "{msg}");
        assert!(!path.exists());
        let msg = run_ok(&path, ConfigAction::Reset);
        assert!(msg.contains("nothing to reset"), "{msg}");
    }

    #[test]
    fn reset_reports_the_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        run_ok(&path, set_action("profile", &["scribe"]));
        run_ok(&path, set_action("offline", &["yes"]));
        let msg = run_ok(&path, ConfigAction::Reset);
        assert!(msg.contains("profile, offline"), "{msg}");
        assert!(!path.exists());
    }

    #[test]
    fn list_is_the_default_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut out = Vec::new();
        run(None, &path, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("not created yet"), "{text}");
        assert!(text.contains("profile"), "{text}");
    }

    #[test]
    fn css_paths_are_absolute_and_home_relative() {
        let dir = tempfile::tempdir().unwrap();
        let css = dir.path().join("x.css");
        fs::write(&css, "p{}").unwrap();
        let path = dir.path().join("cfg/config.toml");
        run_ok(&path, set_action("css", &[css.to_str().unwrap()]));
        assert_eq!(get(&path, "css"), css.display().to_string());
        // In the file, relative paths are relative to the file's directory.
        let user = UserConfig::from_toml_str("css = \"a.css\"", Path::new("/cfg")).unwrap();
        let o = config::defaults(&user).unwrap();
        assert_eq!(o.extra_css, Some(PathBuf::from("/cfg/a.css")));
        if let Some(home) = dirs::home_dir() {
            let user = UserConfig::from_toml_str("css = \"~/a.css\"", Path::new("/cfg")).unwrap();
            let o = config::defaults(&user).unwrap();
            assert_eq!(o.extra_css, Some(home.join("a.css")));
        }
    }

    #[test]
    fn help_defaults_follow_the_user_config() {
        let builtin = help_defaults(&UserConfig::default());
        let find = |v: &[(&str, String)], id: &str| {
            v.iter()
                .find(|(k, _)| *k == id)
                .map(|(_, t)| t.clone())
                .unwrap()
        };
        assert_eq!(find(&builtin, "profile"), "generic");
        assert_eq!(find(&builtin, "svg"), "keep");
        assert_eq!(find(&builtin, "grayscale"), "off");
        assert!(find(&builtin, "output").contains("current directory"));
        let user = UserConfig::from_toml_str(
            "profile = \"scribe\"\ngrayscale = true\noutput-location = \"input\"\n",
            Path::new("."),
        )
        .unwrap();
        let mine = help_defaults(&user);
        assert_eq!(find(&mine, "profile"), "scribe");
        assert_eq!(find(&mine, "svg"), "rasterize");
        assert_eq!(find(&mine, "grayscale"), "on");
        assert!(find(&mine, "output").contains("beside the first input"));
    }

    /// `config edit` itself is tested in tests/integration.rs, where the
    /// editor can be set through the environment of the child process.
    #[test]
    fn template_is_an_empty_valid_config() {
        assert!(TEMPLATE.parse::<toml_edit::DocumentMut>().is_ok());
        let user = UserConfig::from_toml_str(TEMPLATE, Path::new(".")).unwrap();
        assert!(user.keys().is_empty());
        assert_eq!(user, UserConfig::default());
    }
}

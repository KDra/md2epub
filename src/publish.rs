//! `md2epub publish`: create or update a Confluence Data Center / Server
//! page through the REST API v1 (`/rest/api/content`).
//!
//! Flow:
//!
//! 1. The page is converted in memory (see [`crate::confluence`]), or read
//!    from a directory written by `--format confluence`.
//! 2. The page is found by `--page-id` (`GET /rest/api/content/{id}`) or by
//!    space and title (`GET /rest/api/content?spaceKey=..&title=..`).
//! 3. A new page is created (`POST /rest/api/content`). When it has
//!    attachments, it is first created with a one-line placeholder, the
//!    attachments are uploaded, and the real body is then written as
//!    version 2. So the page never shows broken images; it only says
//!    "publishing" for the few seconds of the upload.
//! 4. An existing page gets its new and changed attachments first
//!    (`POST .../child/attachment`, `POST .../child/attachment/{id}/data`),
//!    then the new body (`PUT /rest/api/content/{id}` with the version
//!    number plus one; a 409 conflict refetches the version once), and
//!    only then are unused attachments deleted (`--prune-attachments`).
//!    The old body never points to a missing file and the new body never
//!    points to a file that is not uploaded yet.
//!
//! Attachment names carry a content hash, so an attachment with the same
//! name is unchanged; its size and the `sha256:` in its comment are
//! compared too, and a mismatch re-uploads it.
//!
//! The parent page (`--parent-id`, or `confluence-parent-id` from a config)
//! is where a *new* page is created. An existing page stays where it is
//! (`ancestors` is left out of the update), unless `--parent-id` is given
//! on the command line: then the update moves it under that page.
//!
//! The personal access token comes from an environment variable and is
//! sent as `Authorization: Bearer`. It is never printed: every message that
//! leaves this module goes through [`redact`]. It is only sent over HTTPS,
//! or plain HTTP to this machine (`localhost`, `127.0.0.1`, `[::1]`), unless
//! `--allow-insecure-http` is given. Redirects are not followed (ureq would
//! drop the token on the way); a redirect is an error that names its target.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};

use crate::cli::{Cli, Command, PublishArgs};
use crate::config::{self, UserConfig};
use crate::confluence::{self, Attachment, Export};
use crate::pipeline;

/// Default environment variable holding the personal access token.
pub const TOKEN_ENV: &str = "MD2EPUB_CONFLUENCE_TOKEN";

/// Timeout for one whole request (uploads of large attachments included).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Timeout for connecting to the server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Page size of attachment listings.
const PAGE_LIMIT: usize = 100;
/// Prefix of the attachment comment that records the content hash.
const COMMENT_PREFIX: &str = "md2epub sha256:";
/// Body of a new page while its attachments upload.
const PLACEHOLDER: &str = "<p><em>Publishing with md2epub\u{2026}</em></p>";

/// Tokens shorter than this are not redacted: they are not real personal
/// access tokens (those have 44 characters), and replacing a short string
/// everywhere would garble unrelated words in the message.
const MIN_REDACT_LEN: usize = 8;

/// Replaces every occurrence of `token` in `text` with `***`.
pub fn redact(text: &str, token: &str) -> String {
    if token.chars().count() < MIN_REDACT_LEN {
        text.to_string()
    } else {
        text.replace(token, "***")
    }
}

/// Command-line flags that change the conversion, by name, for messages.
fn conversion_flags(f: &crate::cli::Flags) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut add = |set: bool, name: &'static str| {
        if set {
            v.push(name);
        }
    };
    add(f.profile.is_some(), "--profile");
    add(!f.authors.is_empty(), "--author");
    add(f.lang.is_some(), "--lang");
    add(f.cover.is_some(), "--cover");
    add(f.split_level.is_some(), "--split-level");
    add(f.notes.is_some(), "--notes");
    add(f.notes_style.is_some(), "--notes-style");
    add(f.notes_pages.is_some(), "--notes-pages");
    add(f.margin.is_some(), "--margin");
    add(f.line_spacing.is_some(), "--line-spacing");
    add(f.math.is_some(), "--math");
    add(f.svg.is_some(), "--svg");
    add(f.grayscale || f.no_grayscale, "--grayscale");
    add(f.css.is_some(), "--css");
    add(f.offline || f.no_offline, "--offline");
    add(f.follow_links || f.no_follow_links, "--follow-links");
    add(f.link_root.is_some(), "--link-root");
    add(f.format.is_some(), "--format");
    add(f.confluence_toc || f.no_confluence_toc, "--confluence-toc");
    v
}

/// Refuses options that write files: `publish` only uploads.
fn check_no_output_flags(f: &crate::cli::Flags) -> Result<()> {
    let given = [
        (f.output.is_some(), "-o/--output"),
        (f.output_location.is_some(), "--output-location"),
        (
            f.format == Some(crate::model::OutputFormat::Epub),
            "--format epub",
        ),
    ];
    if let Some((_, name)) = given.iter().find(|(set, _)| *set) {
        bail!(
            "{name} does not apply to `md2epub publish`, which writes no file; \
             to write a file, run `md2epub <inputs> -o FILE` (EPUB) or \
             `md2epub <inputs> --format confluence -o DIR` (export directory)"
        );
    }
    Ok(())
}

/// Whether `base` (`http(s)://host[:port]/...`) is plain HTTP to another
/// machine, where the token would travel unencrypted.
pub(crate) fn is_insecure_remote(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let loopback = host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    !loopback
}

/// Runs `md2epub publish`. Progress goes to `log` (stderr), the page URL
/// and the dry-run plan to `out` (stdout). Errors are already redacted.
pub fn run(
    cli: &Cli,
    args: &PublishArgs,
    user: &UserConfig,
    out: &mut dyn Write,
    log: &mut dyn Write,
) -> Result<()> {
    let token = std::env::var(&args.token_env).unwrap_or_default();
    let token = token.trim().to_string();
    let result = run_with_token(cli, args, user, &token, out, log);
    result.map_err(|e| anyhow!(redact(&format!("{e:#}"), &token)))
}

fn run_with_token(
    cli: &Cli,
    args: &PublishArgs,
    user: &UserConfig,
    token: &str,
    out: &mut dyn Write,
    log: &mut dyn Write,
) -> Result<()> {
    if token.is_empty() {
        bail!(
            "no Confluence token: set the {} environment variable to a personal access token \
             (in Confluence: your avatar > Profile > Personal Access Tokens > Create token)",
            args.token_env
        );
    }
    check_no_output_flags(&cli.flags)?;
    let all_inputs: Vec<PathBuf> = cli.inputs.iter().chain(&args.inputs).cloned().collect();
    let (export, opts, verbose) = match all_inputs.as_slice() {
        [dir] if dir.is_dir() => {
            if !confluence::is_export_dir(dir) {
                bail!(
                    "{} is not a directory written by --format confluence (it has no md2epub {}); \
                     give Markdown/HTML files, a book.toml, or such a directory",
                    dir.display(),
                    confluence::MANIFEST_FILE
                );
            }
            let ignored = conversion_flags(&cli.flags);
            if !ignored.is_empty() {
                writeln!(
                    log,
                    "warning: {} is already converted, so {} {} ignored; \
                     to change the conversion, publish the Markdown/HTML inputs \
                     or export them again with --format confluence",
                    dir.display(),
                    ignored.join(", "),
                    if ignored.len() == 1 { "is" } else { "are" }
                )?;
            }
            let mut export = confluence::read_dir(dir)?;
            if let Some(t) = cli.flags.title.as_ref().filter(|t| !t.trim().is_empty()) {
                export.title = t.trim().to_string();
            }
            let opts = config::defaults(user)?;
            let verbose = cli.flags.verbose || (opts.verbose && !cli.flags.no_verbose);
            (export, opts, verbose)
        }
        _ => {
            let convert_cli = Cli {
                command: Some(Command::Publish(args.clone())),
                inputs: cli.inputs.clone(),
                flags: cli.flags.clone(),
            };
            let user = user.clone();
            let (export, warnings, opts) =
                pipeline::on_big_stack(move || pipeline::confluence_export(&convert_cli, &user))?;
            if !opts.verbose {
                for w in &warnings {
                    writeln!(log, "warning: {w}")?;
                }
            }
            let verbose = opts.verbose;
            (export, opts, verbose)
        }
    };

    let base = match &args.url {
        Some(u) => config::check_confluence_url(u).context("--url")?,
        None => opts.confluence.url.clone().ok_or_else(|| {
            anyhow!(
                "no Confluence URL: pass --url https://confluence.example.com \
                 (or save it with `md2epub config set confluence-url <url>`)"
            )
        })?,
    };
    if is_insecure_remote(&base) {
        if !args.allow_insecure_http {
            bail!(
                "{base} uses plain http://, which would send the personal access token \
                 unencrypted; use the https:// URL of the server, or pass \
                 --allow-insecure-http if you really mean it"
            );
        }
        writeln!(
            log,
            "warning: --allow-insecure-http: the token is sent unencrypted to {base}"
        )?;
    }
    let space = match &args.space {
        Some(s) => Some(config::check_space_key(s).context("--space")?),
        None => opts.confluence.space.clone(),
    };
    // An explicit --parent-id also moves an existing page; the saved
    // default only places new pages.
    let move_to = match &args.parent_id {
        Some(p) => Some(config::check_content_id(p).context("--parent-id")?),
        None => None,
    };
    let parent = move_to
        .clone()
        .or_else(|| opts.confluence.parent_id.clone());
    let page_id = match &args.page_id {
        Some(p) => Some(config::check_content_id(p).context("--page-id")?),
        None => None,
    };
    let client = Client::new(&base, token, args.ca_cert.as_ref(), verbose)?;
    let plan = Plan {
        export: &export,
        space,
        parent,
        move_to,
        page_id,
        dry_run: args.dry_run,
        prune: args.prune_attachments,
    };
    let url = plan.execute(&client, out, log)?;
    if let Some(url) = url {
        writeln!(out, "{url}")?;
    }
    Ok(())
}

/// What to publish where.
struct Plan<'a> {
    export: &'a Export,
    space: Option<String>,
    /// Parent of a new page.
    parent: Option<String>,
    /// Parent an existing page moves to (`--parent-id` on the command line).
    move_to: Option<String>,
    page_id: Option<String>,
    dry_run: bool,
    prune: bool,
}

/// A page as the server describes it.
struct Page {
    id: String,
    version: u64,
    space: Option<String>,
    title: String,
    url: Option<String>,
}

impl Page {
    fn from_json(v: &Value) -> Result<Page> {
        let id = match &v["id"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => bail!("the server's answer has no page id"),
        };
        Ok(Page {
            id,
            version: v["version"]["number"].as_u64().unwrap_or(1),
            space: v["space"]["key"].as_str().map(str::to_string),
            title: v["title"].as_str().unwrap_or_default().to_string(),
            url: page_url(v),
        })
    }
}

/// `_links.base` + `_links.webui`.
fn page_url(v: &Value) -> Option<String> {
    let webui = v["_links"]["webui"].as_str()?;
    if webui.starts_with("http://") || webui.starts_with("https://") {
        return Some(webui.to_string());
    }
    let base = v["_links"]["base"].as_str()?;
    Some(format!("{}{webui}", base.trim_end_matches('/')))
}

/// An attachment that is already on the page.
#[derive(Debug, Clone)]
struct Existing {
    id: String,
    size: Option<u64>,
    sha256: Option<String>,
}

/// What happens to one attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Upload,
    Update(String),
    Skip,
}

impl Plan<'_> {
    fn execute(
        &self,
        client: &Client,
        out: &mut dyn Write,
        log: &mut dyn Write,
    ) -> Result<Option<String>> {
        let title = self.export.title.trim();
        if title.is_empty() {
            bail!("the page has no title; pass --title");
        }
        let page = match &self.page_id {
            Some(id) => {
                if let Some(p) = &self.move_to {
                    client.get_page(p).context("checking --parent-id")?;
                }
                Some(client.get_page(id)?)
            }
            None => {
                let space = self.space.as_deref().ok_or_else(|| {
                    anyhow!(
                        "no space: pass --space KEY (or save it with `md2epub config set confluence-space KEY`), \
                         or --page-id to update a page"
                    )
                })?;
                client.check_space(space)?;
                if let Some(p) = &self.parent {
                    client.get_page(p).context("checking --parent-id")?;
                }
                client.find_page(space, title)?
            }
        };
        if self.dry_run {
            writeln!(out, "dry run: nothing is changed on {}", client.base)?;
        }
        match page {
            None => self.create(client, title, out, log),
            Some(page) => self.update(client, page, title, out, log),
        }
    }

    fn create(
        &self,
        client: &Client,
        title: &str,
        out: &mut dyn Write,
        log: &mut dyn Write,
    ) -> Result<Option<String>> {
        let space = self.space.as_deref().unwrap_or_default();
        let under = self.parent.as_ref().map_or_else(
            || "at the top of the space".to_string(),
            |p| format!("under page {p}"),
        );
        if self.dry_run {
            writeln!(
                out,
                "would create page \"{title}\" in space {space} {under}"
            )?;
            for a in &self.export.attachments {
                writeln!(out, "would upload {} ({} bytes)", a.filename, a.data.len())?;
            }
            return Ok(None);
        }
        let has_attachments = !self.export.attachments.is_empty();
        let first_body = if has_attachments {
            PLACEHOLDER
        } else {
            self.export.page_xml.as_str()
        };
        let mut body = json!({
            "type": "page",
            "title": title,
            "space": { "key": space },
            "body": { "storage": { "value": first_body, "representation": "storage" } },
        });
        if let Some(p) = &self.parent {
            body["ancestors"] = json!([{ "id": p }]);
        }
        let created = client.send_json("POST", "/rest/api/content", &body)?;
        let page = Page::from_json(&created)?;
        writeln!(log, "created page \"{title}\" (id {})", page.id)?;
        if !has_attachments {
            return Ok(page.url.or_else(|| Some(client.fallback_url(&page.id))));
        }
        for a in &self.export.attachments {
            client.upload(&page.id, a, None)?;
        }
        writeln!(
            log,
            "uploaded {} attachments",
            self.export.attachments.len()
        )?;
        // The page was created under its parent; the update keeps it there.
        let updated = self.put_body(client, &page, title, None, log)?;
        Ok(updated
            .url
            .or(page.url)
            .or_else(|| Some(client.fallback_url(&page.id))))
    }

    fn update(
        &self,
        client: &Client,
        page: Page,
        title: &str,
        out: &mut dyn Write,
        log: &mut dyn Write,
    ) -> Result<Option<String>> {
        let existing = client.list_attachments(&page.id)?;
        let wanted: HashSet<&str> = self
            .export
            .attachments
            .iter()
            .map(|a| a.filename.as_str())
            .collect();
        let actions: Vec<(&Attachment, Action)> = self
            .export
            .attachments
            .iter()
            .map(|a| (a, action_for(a, existing.get(&a.filename))))
            .collect();
        let mut prune: Vec<(&String, &Existing)> = if self.prune {
            existing
                .iter()
                .filter(|(name, _)| !wanted.contains(name.as_str()))
                .collect()
        } else {
            Vec::new()
        };
        prune.sort_by(|a, b| a.0.cmp(b.0));
        if self.dry_run {
            let rename = if page.title != title {
                format!(", renamed from \"{}\"", page.title)
            } else {
                String::new()
            };
            let moved = self
                .move_to
                .as_ref()
                .map(|p| format!(", moved under page {p}"))
                .unwrap_or_default();
            writeln!(
                out,
                "would update page \"{title}\" (id {}, version {} -> {}{rename}{moved})",
                page.id,
                page.version,
                page.version + 1
            )?;
            for (a, action) in &actions {
                let what = match action {
                    Action::Upload => "would upload",
                    Action::Update(_) => "would update",
                    Action::Skip => "unchanged",
                };
                writeln!(out, "{what} {}", a.filename)?;
            }
            for (name, _) in &prune {
                writeln!(out, "would delete {name} (no longer used)")?;
            }
            return Ok(None);
        }
        let (mut up, mut changed, mut same) = (0, 0, 0);
        for (a, action) in &actions {
            match action {
                Action::Upload => {
                    client.upload(&page.id, a, None)?;
                    up += 1;
                }
                Action::Update(id) => {
                    client.upload(&page.id, a, Some(id))?;
                    changed += 1;
                }
                Action::Skip => same += 1,
            }
        }
        let updated = self.put_body(client, &page, title, self.move_to.as_deref(), log)?;
        if let Some(p) = &self.move_to {
            writeln!(log, "moved page \"{title}\" under page {p}")?;
        }
        for (name, e) in &prune {
            client.delete(&format!("/rest/api/content/{}", enc(&e.id)))?;
            writeln!(log, "deleted attachment {name}")?;
        }
        writeln!(
            log,
            "updated page \"{title}\" (id {}, version {}); attachments: {up} new, {changed} changed, {same} unchanged, {} deleted",
            page.id,
            updated.version,
            prune.len()
        )?;
        Ok(updated
            .url
            .or(page.url)
            .or_else(|| Some(client.fallback_url(&page.id))))
    }

    /// Writes the page body as the next version; on a 409 conflict the
    /// current version is fetched again and the update retried once.
    /// `ancestors` is sent only with `move_to`: Confluence moves a page to
    /// the parent given there, and keeps the current parent without it.
    fn put_body(
        &self,
        client: &Client,
        page: &Page,
        title: &str,
        move_to: Option<&str>,
        log: &mut dyn Write,
    ) -> Result<Page> {
        let space = page
            .space
            .clone()
            .or_else(|| self.space.clone())
            .unwrap_or_default();
        let mut version = page.version;
        for attempt in 0..2 {
            let mut body = json!({
                "id": page.id,
                "type": "page",
                "title": title,
                "space": { "key": space },
                "version": { "number": version + 1, "message": "Published with md2epub" },
                "body": { "storage": { "value": self.export.page_xml, "representation": "storage" } },
            });
            if let Some(p) = move_to {
                body["ancestors"] = json!([{ "id": p }]);
            }
            let path = format!("/rest/api/content/{}", enc(&page.id));
            match client.request_json("PUT", &path, Some(&body)) {
                Ok(v) => return Page::from_json(&v),
                Err(e) if attempt == 0 && e.status == Some(409) => {
                    writeln!(
                        log,
                        "version conflict; fetching the current version and retrying"
                    )?;
                    version = client.get_page(&page.id)?.version;
                }
                Err(e) => return Err(e.into_anyhow()),
            }
        }
        unreachable!("the loop returns")
    }
}

/// Whether an attachment must be uploaded, updated or can be skipped.
fn action_for(a: &Attachment, existing: Option<&Existing>) -> Action {
    let Some(e) = existing else {
        return Action::Upload;
    };
    let size_differs = e.size.is_some_and(|s| s != a.data.len() as u64);
    let hash_differs = e
        .sha256
        .as_ref()
        .is_some_and(|h| !h.eq_ignore_ascii_case(&a.sha256));
    if size_differs || hash_differs {
        Action::Update(e.id.clone())
    } else {
        Action::Skip
    }
}

/// Percent-encodes a path segment or query value.
fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// An HTTP error with the server's status, for retry decisions.
struct HttpError {
    status: Option<u16>,
    message: String,
}

impl HttpError {
    fn into_anyhow(self) -> anyhow::Error {
        anyhow!(self.message)
    }
}

/// A Confluence REST client.
struct Client {
    agent: ureq::Agent,
    base: String,
    auth: String,
    verbose: bool,
}

impl Client {
    fn new(base: &str, token: &str, ca_cert: Option<&PathBuf>, verbose: bool) -> Result<Client> {
        use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig};
        let roots = match ca_cert {
            Some(path) => {
                let pem = std::fs::read(path)
                    .with_context(|| format!("reading --ca-cert {}", path.display()))?;
                let certs: Vec<Certificate<'static>> = ureq::tls::parse_pem(&pem)
                    .filter_map(|item| match item {
                        Ok(PemItem::Certificate(c)) => Some(c),
                        _ => None,
                    })
                    .collect();
                if certs.is_empty() {
                    bail!("--ca-cert {}: no PEM certificate found", path.display());
                }
                RootCerts::new_with_certs(&certs)
            }
            // The operating system's trust store, so corporate CAs work.
            None => RootCerts::PlatformVerifier,
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            // ureq drops the Authorization header on a redirect, which
            // would turn a wrong --url into a misleading 401.
            .max_redirects(0)
            .user_agent(concat!("md2epub/", env!("CARGO_PKG_VERSION")))
            .tls_config(TlsConfig::builder().root_certs(roots).build())
            .build()
            .into();
        Ok(Client {
            agent,
            base: base.trim_end_matches('/').to_string(),
            auth: format!("Bearer {token}"),
            verbose,
        })
    }

    fn fallback_url(&self, id: &str) -> String {
        format!("{}/pages/viewpage.action?pageId={id}", self.base)
    }

    /// Sends a request and returns the parsed JSON answer (or `null` for an
    /// empty one). Statuses outside 2xx become an [`HttpError`].
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
        extra: &[(&str, &str)],
    ) -> std::result::Result<Value, HttpError> {
        let url = format!("{}{path}", self.base);
        if self.verbose {
            eprintln!("{method} {url}");
        }
        let fail = |message: String| HttpError {
            status: None,
            message,
        };
        macro_rules! with_headers {
            ($r:expr) => {{
                let mut r = $r
                    .header("Authorization", &self.auth)
                    .header("Accept", "application/json");
                for (k, v) in extra {
                    r = r.header(*k, *v);
                }
                r
            }};
        }
        let result = match (method, body) {
            ("GET", _) => with_headers!(self.agent.get(&url)).call(),
            ("DELETE", _) => with_headers!(self.agent.delete(&url)).call(),
            ("POST", Some((ct, data))) => with_headers!(self.agent.post(&url))
                .header("Content-Type", ct)
                .send(&data[..]),
            ("PUT", Some((ct, data))) => with_headers!(self.agent.put(&url))
                .header("Content-Type", ct)
                .send(&data[..]),
            _ => return Err(fail(format!("internal error: {method} without a body"))),
        };
        let mut resp =
            result.map_err(|e| fail(format!("{method} {url}: cannot reach Confluence: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .unwrap_or_default();
        if (300..400).contains(&status) {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .trim()
                .to_string();
            return Err(HttpError {
                status: Some(status),
                message: redirect_message(method, &url, &self.base, path, status, &location),
            });
        }
        if !(200..300).contains(&status) {
            return Err(HttpError {
                status: Some(status),
                message: status_message(method, &url, status, &text),
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|_| {
            fail(format!(
                "{method} {url}: the answer is not JSON (is --url the Confluence base URL, including its context path?)"
            ))
        })
    }

    fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<Value, HttpError> {
        let body = body.map(|b| ("application/json", b.to_string().into_bytes()));
        self.request(method, path, body, &[])
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.request_json("GET", path, None)
            .map_err(HttpError::into_anyhow)
    }

    fn send_json(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        self.request_json(method, path, Some(body))
            .map_err(HttpError::into_anyhow)
    }

    fn delete(&self, path: &str) -> Result<()> {
        self.request("DELETE", path, None, &[])
            .map(|_| ())
            .map_err(HttpError::into_anyhow)
    }

    fn check_space(&self, key: &str) -> Result<()> {
        match self.request_json("GET", &format!("/rest/api/space/{}", enc(key)), None) {
            Ok(_) => Ok(()),
            Err(e) if e.status == Some(404) => bail!(
                "space {key} not found, or you may not view it (check --space and the space permissions)"
            ),
            Err(e) => Err(e.into_anyhow()),
        }
    }

    fn get_page(&self, id: &str) -> Result<Page> {
        let path = format!("/rest/api/content/{}?expand=version,space", enc(id));
        match self.request_json("GET", &path, None) {
            Ok(v) => Page::from_json(&v),
            Err(e) if e.status == Some(404) => {
                bail!("page {id} not found, or you may not view it")
            }
            Err(e) => Err(e.into_anyhow()),
        }
    }

    fn find_page(&self, space: &str, title: &str) -> Result<Option<Page>> {
        let path = format!(
            "/rest/api/content?type=page&spaceKey={}&title={}&expand=version,space",
            enc(space),
            enc(title)
        );
        let v = self.get(&path)?;
        match v["results"].as_array().and_then(|r| r.first()) {
            Some(p) => Ok(Some(Page::from_json(p)?)),
            None => Ok(None),
        }
    }

    /// Every attachment of a page, by file name (all result pages).
    fn list_attachments(&self, page_id: &str) -> Result<HashMap<String, Existing>> {
        let mut found = HashMap::new();
        let mut start = 0usize;
        loop {
            let path = format!(
                "/rest/api/content/{}/child/attachment?start={start}&limit={PAGE_LIMIT}&expand=version,metadata",
                enc(page_id)
            );
            let v = self.get(&path)?;
            let results = v["results"].as_array().cloned().unwrap_or_default();
            for r in &results {
                let Some(name) = r["title"].as_str() else {
                    continue;
                };
                let id = match &r["id"] {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let comment = r["metadata"]["comment"]
                    .as_str()
                    .or_else(|| r["extensions"]["comment"].as_str())
                    .unwrap_or_default();
                let sha256 = comment
                    .find(COMMENT_PREFIX)
                    .map(|i| &comment[i + COMMENT_PREFIX.len()..])
                    .map(|rest| {
                        rest.chars()
                            .take_while(char::is_ascii_hexdigit)
                            .collect::<String>()
                    })
                    .filter(|h| h.len() == 64);
                found.insert(
                    name.to_string(),
                    Existing {
                        id,
                        size: r["extensions"]["fileSize"].as_u64(),
                        sha256,
                    },
                );
            }
            let has_next = v["_links"]["next"].as_str().is_some();
            if results.is_empty() || (!has_next && results.len() < PAGE_LIMIT) {
                break;
            }
            start += results.len();
            if start > 100_000 {
                bail!("the page has too many attachments to list");
            }
        }
        Ok(found)
    }

    /// Uploads a new attachment, or a new version of attachment `id`.
    fn upload(&self, page_id: &str, a: &Attachment, id: Option<&String>) -> Result<()> {
        let path = match id {
            Some(id) => format!(
                "/rest/api/content/{}/child/attachment/{}/data",
                enc(page_id),
                enc(id)
            ),
            None => format!("/rest/api/content/{}/child/attachment", enc(page_id)),
        };
        let (content_type, body) = multipart(a);
        self.request(
            "POST",
            &path,
            Some((&content_type, body)),
            &[("X-Atlassian-Token", "no-check")],
        )
        .map(|_| ())
        .map_err(|e| anyhow!("uploading {}: {}", a.filename, e.message))
    }
}

/// A `multipart/form-data` body with the file, its hash comment and
/// `minorEdit=true`. Returns the content type and the body.
fn multipart(a: &Attachment) -> (String, Vec<u8>) {
    let mut n = 0u32;
    let boundary = loop {
        let b = format!("md2epub-{}-{n}", &a.sha256[..a.sha256.len().min(24)]);
        if !a.data.windows(b.len()).any(|w| w == b.as_bytes()) {
            break b;
        }
        n += 1;
    };
    let name = a.filename.replace(['"', '\r', '\n'], "_");
    let mut body = Vec::with_capacity(a.data.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {}\r\n\r\n",
            a.media_type
        )
        .as_bytes(),
    );
    body.extend_from_slice(&a.data);
    body.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"comment\"\r\n\r\n{COMMENT_PREFIX}{}\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"minorEdit\"\r\n\r\ntrue\r\n--{boundary}--\r\n",
            a.sha256
        )
        .as_bytes(),
    );
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The message for a redirect: where it leads, and the `--url` to use
/// instead when the target is the same API path under another base.
fn redirect_message(
    method: &str,
    url: &str,
    base: &str,
    path: &str,
    status: u16,
    location: &str,
) -> String {
    if location.is_empty() {
        return format!(
            "{method} {url}: the server answered with a redirect (HTTP {status}); \
             check --url (the Confluence base URL, including its context path)"
        );
    }
    // A relative target is resolved against the scheme and host of `base`.
    let absolute = if location.starts_with('/') {
        let origin_end = base
            .find("://")
            .map(|i| i + 3)
            .and_then(|start| base[start..].find('/').map(|j| start + j))
            .unwrap_or(base.len());
        format!("{}{location}", &base[..origin_end])
    } else {
        location.to_string()
    };
    let target = absolute.split(['?', '#']).next().unwrap_or("");
    let api_path = path.split('?').next().unwrap_or("");
    let suggestion = target
        .strip_suffix(api_path)
        .filter(|b| b.starts_with("https://") || b.starts_with("http://"))
        .map(|b| format!("; pass --url {}", b.trim_end_matches('/')))
        .unwrap_or_else(|| {
            "; pass the base URL it leads to (including the context path) as --url".into()
        });
    format!(
        "{method} {url}: the server redirects (HTTP {status}) to {absolute}; \
         md2epub does not follow redirects, so the token is never sent elsewhere{suggestion}"
    )
}

/// A readable message for an error status, with the server's `message`.
fn status_message(method: &str, url: &str, status: u16, body: &str) -> String {
    let server = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["message"]
                .as_str()
                .or_else(|| v["data"]["errors"][0]["message"]["translation"].as_str())
                .map(str::to_string)
        })
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!(": {}", m.trim()))
        .unwrap_or_default();
    let hint = match status {
        401 => {
            " (authentication failed: check that the personal access token is valid and not expired)"
        }
        403 => {
            " (permission denied: the token's user needs View and Add/Edit page permissions in the space, and Add Attachment to upload files)"
        }
        404 => " (not found: check --url, the space key and the page id)",
        409 => {
            " (conflict: the page was changed at the same time, or a page with this title already exists)"
        }
        413 => " (an attachment is larger than the server's upload limit)",
        _ => "",
    };
    format!("{method} {url} failed with HTTP {status}{hint}{server}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(name: &str, data: &[u8]) -> Attachment {
        Attachment {
            filename: name.into(),
            media_type: "image/png".into(),
            sha256: confluence::sha256_hex(data),
            data: data.to_vec(),
        }
    }

    #[test]
    fn redaction() {
        assert_eq!(
            redact("Bearer abcdefgh and abcdefgh", "abcdefgh"),
            "Bearer *** and ***"
        );
        assert_eq!(redact("x", ""), "x");
        // A short placeholder token does not garble other words.
        assert_eq!(
            redact("expected letters and digits", "x"),
            "expected letters and digits"
        );
    }

    #[test]
    fn plain_http_is_insecure_except_to_this_machine() {
        assert!(is_insecure_remote("http://wiki.example.com/c"));
        assert!(is_insecure_remote("HTTP://10.0.0.1:8090"));
        assert!(!is_insecure_remote("https://wiki.example.com"));
        assert!(!is_insecure_remote("http://127.0.0.1:8090/c"));
        assert!(!is_insecure_remote("http://localhost:8090"));
        assert!(!is_insecure_remote("http://[::1]:8090/c"));
        assert!(is_insecure_remote("http://127.0.0.1.evil.example/c"));
    }

    #[test]
    fn redirect_messages_name_the_new_base() {
        let m = redirect_message(
            "GET",
            "https://w.example/c/rest/api/content/42?expand=version",
            "https://w.example/c",
            "/rest/api/content/42?expand=version",
            302,
            "https://w.example/c2/rest/api/content/42?expand=version",
        );
        assert!(m.contains("--url https://w.example/c2"), "{m}");
        assert!(!m.contains("401") && !m.contains("token is valid"), "{m}");
        let m = redirect_message(
            "GET",
            "http://h/c/rest/api/space/X",
            "http://h/c",
            "/rest/api/space/X",
            301,
            "/wiki/rest/api/space/X",
        );
        assert!(m.contains("to http://h/wiki/rest/api/space/X"), "{m}");
        assert!(m.contains("--url http://h/wiki"), "{m}");
        let m = redirect_message(
            "GET",
            "u",
            "https://h",
            "/rest/api/x",
            302,
            "https://sso/login",
        );
        assert!(
            m.contains("https://sso/login") && m.contains("as --url"),
            "{m}"
        );
    }

    #[test]
    fn attachment_actions() {
        let a = att("a.png", b"data");
        assert_eq!(action_for(&a, None), Action::Upload);
        let same = Existing {
            id: "1".into(),
            size: Some(4),
            sha256: Some(a.sha256.clone()),
        };
        assert_eq!(action_for(&a, Some(&same)), Action::Skip);
        let unknown = Existing {
            id: "1".into(),
            size: None,
            sha256: None,
        };
        assert_eq!(action_for(&a, Some(&unknown)), Action::Skip);
        let bigger = Existing {
            size: Some(9),
            ..same.clone()
        };
        assert_eq!(action_for(&a, Some(&bigger)), Action::Update("1".into()));
        let other = Existing {
            sha256: Some("0".repeat(64)),
            ..same
        };
        assert_eq!(action_for(&a, Some(&other)), Action::Update("1".into()));
    }

    #[test]
    fn multipart_body_and_messages() {
        let a = att("x\"y.png", b"\x89PNG");
        let (ct, body) = multipart(&a);
        let boundary = ct.split("boundary=").nth(1).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("filename=\"x_y.png\""));
        assert!(text.contains(&format!("{COMMENT_PREFIX}{}", a.sha256)));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        let m = status_message("GET", "u", 401, r#"{"message":"Bad token"}"#);
        assert!(
            m.contains("401") && m.contains("Bad token") && m.contains("token"),
            "{m}"
        );
        let m = status_message("GET", "u", 500, "<html>");
        assert!(m.ends_with("HTTP 500"), "{m}");
        let v = json!({"_links": {"base": "https://w.example/c/", "webui": "/display/K/T"}});
        assert_eq!(page_url(&v).unwrap(), "https://w.example/c/display/K/T");
    }
}

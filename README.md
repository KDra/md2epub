# md2epub

Convert rich Markdown or HTML into a **reflowable EPUB 3** that you can write on with a
**Kindle Scribe** (Active Canvas, via Send to Kindle) or a **reMarkable**, or into a
**Confluence Data Center / Server page** (see [Confluence](#confluence)).

- GitHub-flavoured Markdown: tables, footnotes, task lists, strikethrough, autolinks,
  YAML front matter, and raw HTML passed through.
- HTML input (full documents or fragments) is sanitized: scripts, iframes, forms and
  event handlers are removed.
- Images: local paths, `file://`, `data:` URIs and (optionally) `http(s)` URLs. WebP,
  GIF, BMP and TIFF are converted to PNG. Images are downscaled to the device screen,
  and transparency is flattened onto white where the device needs it.
- SVG (referenced files and inline `<svg>`) is rasterized to crisp PNGs for e-ink devices,
  or kept as SVG for desktop readers.
- LaTeX math (`$...$`, `$$...$$`) is rendered to images that sit on the text baseline,
  or converted to MathML.
- Mermaid diagrams (```` ```mermaid ````) are rendered in-process.
- Syntax-highlighted code that stays readable in grayscale.
- Local files that your documents link to (other Markdown or HTML files, images,
  Mermaid sources, code and text files) are added to the book, and the links point
  to them.
- Blank, lined, dotted or grid **note pages** after every chapter or at the end of the
  book, plus wide-margin presets that leave room for handwriting.
- One self-contained binary per OS. You do not need Java, Node, Python or a browser.

## Install

### Prebuilt binaries

Every tagged release has archives for macOS (Apple Silicon), Linux (x64 and arm64,
static musl) and Windows (x64 and arm64), plus installers:

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/KDra/md2epub/releases/latest/download/md2epub-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/KDra/md2epub/releases/latest/download/md2epub-installer.ps1 | iex"
```

You can also download an archive from the Releases page and put `md2epub` on your `PATH`.

### Docker (Confluence publishing)

A minimal multi-arch image (linux/amd64, linux/arm64) on Docker Hub,
[`kdrakop/md2epub`](https://hub.docker.com/r/kdrakop/md2epub), does one thing: publish
Markdown or HTML to Confluence Data Center (its entrypoint is `md2epub publish`). It is built
on `scratch` and holds only the static binary, CA certificates and an empty `/tmp`: no
shell, no package manager, no other Linux tools.

Mount your documents at `/work` and pass the token through the environment:

```sh
export MD2EPUB_CONFLUENCE_TOKEN=...   # Profile > Personal Access Tokens
docker run --rm -v "$PWD:/work" -e MD2EPUB_CONFLUENCE_TOKEN \
  kdrakop/md2epub docs/guide.md --url https://confluence.example.com --space DOCS --parent-id 12345
```

All `md2epub publish` options work (see [Confluence](#confluence)); with no arguments the
image prints `publish --help`. Only files under the mounted directory are visible, so links
and images outside it are skipped with a warning. Saved defaults are not used; pass
`--url`/`--space` each time or put them in a `book.toml`. If your Confluence uses a private
CA, mount its PEM file and pass `--ca-cert`. Mermaid's optional `mmdc` fallback is not
included (the built-in renderer is).

Build it yourself with `docker build -t md2epub .`.

### From source

You need a recent stable Rust (1.92 or newer).

```sh
cargo install --path .                        # full build (math + mermaid)
cargo install --path . --no-default-features  # slim build, see "Build features"
```

## Usage

```sh
md2epub notes.md -o notes.epub
md2epub intro.md chapter1.md appendix.html -o book.epub --profile scribe
md2epub paper.md --profile remarkable --notes after-chapter --notes-style lined --margin wide
md2epub build book.toml
```

Where the book goes (`name` is the first input's file name without its extension):

| You give | The EPUB is written to |
|---|---|
| no `-o` | `name.epub` in the current directory, or beside the first input with `--output-location input` (or `md2epub config set output-location input`) |
| `-o book.epub` | `book.epub` (any `.epub` spelling is kept as is) |
| `-o book` | `book.epub`: `.epub` is added |
| `-o notes.txt` | `notes.txt.epub`: `.epub` is appended rather than replacing your extension, and a `note:` line says so |
| `-o some/dir` (an existing directory) or `-o new/dir/` (ending in `/`) | `some/dir/name.epub`; a missing directory is created |

For `md2epub build book.toml`, an `output` key is relative to the TOML file and follows
the same rules; without one, the rules for "no `-o`" apply. md2epub never overwrites an
input file, the `--css` file or the cover image.

Warnings, such as a missing image or a remote image skipped offline, are
printed to stderr, and the book is still written. md2epub exits with a non-zero
status only for fatal problems, such as a missing input file, a missing `--cover` or
`--css` file, a bad option, or an `-o` path that would overwrite an input. In that
case it leaves no partial EPUB behind.

Inputs are read as UTF-8; UTF-16 files with a byte-order mark are converted, and other
encodings produce a warning. `.xhtml` inputs are read with XML rules (CDATA sections,
self-closed `<div/>`).

### Options

| Option | Values | Default | Meaning |
|---|---|---|---|
| `-o, --output <FILE>` | path or directory | `<stem>.epub` | Output EPUB (see above). |
| `--output-location <WHERE>` | `cwd`, `input` | `cwd` | Where the output (the EPUB, or the `.confluence` directory) goes without `-o`: the current directory, or beside the first input. |
| `--profile <NAME>` | `scribe`, `remarkable`, `remarkable-pro`, `generic` | `generic` | Target device (see below). |
| `--title <TITLE>` | text | front matter, then first `<h1>`, then file name | Book title. |
| `--author <NAME>` | text, repeatable | front matter | Author(s). |
| `--lang <LANG>` | BCP 47 tag | first HTML input's `<html lang>`, else `en` | Book language. `en_US` is normalized to `en-US`; an invalid tag falls back to `en` with a warning. A `lang` set with `md2epub config` replaces `en` here, but an HTML input's `<html lang>` still wins over it. |
| `--cover <IMG>` | PNG, JPEG, SVG, WebP, ... | generated title page | Cover image (must exist). |
| `--split-level <L>` | `1`, `2`, `none` | `1` | Start a new chapter (file) at each heading of this level. Each input file always starts a chapter. |
| `--notes <WHERE>` | `none`, `after-chapter`, `end` | `none` | Insert full-page note pages. |
| `--notes-style <S>` | `blank`, `lined`, `dotted`, `grid` | `lined` | Pattern on note pages. |
| `--notes-pages <N>` | integer | `1` | Note pages per insertion point. |
| `--margin <M>` | `normal`, `wide`, `extra` | `normal` | Side padding: none, about 8%, about 15% plus more paragraph spacing. |
| `--line-spacing <F>` | number, e.g. `1.6` | reader default | CSS `line-height`. |
| `--math <MODE>` | `image`, `mathml`, `off` | `image` | Render math to images, convert to MathML, or keep the LaTeX source as code. |
| `--svg <MODE>` | `rasterize`, `keep` | from profile | Rasterize SVG to PNG, or keep it as SVG. |
| `--grayscale` | flag | off | Convert all images to grayscale. `--no-grayscale` turns it off. |
| `--css <FILE>` | path | none | Extra CSS appended after the built-in stylesheet. Fonts and images it references with `url(...)` (relative to the CSS file) are embedded. |
| `--offline` | flag | off | Do not download remote images (in an EPUB their alt text is kept; with `--format confluence` they are linked by URL). `--no-offline` turns it off. |
| `-v, --verbose` | flag | off | Print progress and detailed diagnostics. `--no-verbose` turns it off. |
| `--follow-links` | flag | on | Add linked local files to the book (see below). `--no-follow-links` turns it off. |
| `--link-root <DIR>` | directory | git work tree of the inputs, else their common folder | Follow links only to files inside this folder; links that start with `/` resolve against it (see below). |
| `--format <FORMAT>` | `epub`, `confluence` | `epub` | Write an EPUB, or a Confluence export directory (see [Confluence](#confluence)). |
| `--confluence-toc` | flag | off | Confluence only: put a table-of-contents macro at the top of the page. `--no-confluence-toc` turns it off. |

The defaults above are the built-in ones. `md2epub --help` shows the defaults that
actually apply, in bold on a terminal, including your own (see
[Your defaults](#your-defaults-md2epub-config)).

**Precedence:** command-line flags > `book.toml` > YAML front matter of the first input >
your defaults (`md2epub config`) > profile defaults. The `--no-...` forms let a flag
switch off a boolean that `book.toml` or your defaults switch on.

Front-matter and `book.toml` dates should be `YYYY`, `YYYY-MM` or `YYYY-MM-DD`;
`2024/1/5` and unambiguous `DD/MM/YYYY` forms are converted, anything else is left out
with a warning.

### Profiles

| Profile | Screen | SVG | Math | Transparency |
|---|---|---|---|---|
| `scribe` | 1860×2480 | rasterized | image | flattened |
| `remarkable` | 1404×1872 | rasterized | image | flattened |
| `remarkable-pro` | 1620×2160 | rasterized | image | flattened |
| `generic` | 1600×2133 | kept | image | kept |

Under `scribe`, `remarkable` and `remarkable-pro`, the EPUB contains no `<svg>`, no
`<math>`, no SVG files and no images with an alpha channel. Every image is an opaque
PNG or a JPEG. MathML already in the input is rendered from its TeX annotation (or
shown as text, with a warning), and CSS that e-ink readers lay out badly (flex and grid
layout, fixed positioning, viewport units, opacity, filters) is removed from `style`
attributes and `<style>` blocks, with a warning. You can still pass `--svg keep` or
`--math mathml` explicitly; md2epub then warns that Send to Kindle or the reMarkable may
reject or drop that content.

### Front matter

The first Markdown file may start with YAML front matter:

```markdown
---
title: Linear Algebra Notes
author: Ada Lovelace        # or authors: [Ada Lovelace, Charles Babbage]
lang: en
date: 2026-01-15
cover: images/cover.jpg     # relative to the Markdown file
---
```

### `book.toml`

`md2epub build book.toml` reads every option from a TOML file. It takes the same keys as
the long flags (kebab-case, `snake_case` also works) plus `inputs`. Paths are relative
to the TOML file. Unknown keys are an error, so typos are caught.

```toml
title = "Linear Algebra Notes"
authors = ["Ada Lovelace", "Charles Babbage"]
lang = "en"
date = 2026-01-15
inputs = ["intro.md", "vectors.md", "matrices.md", "appendix.html"]
output = "linear-algebra.epub"
cover = "cover.jpg"
profile = "scribe"
split-level = 2
notes = "after-chapter"
notes-style = "lined"
notes-pages = 1
margin = "wide"
line-spacing = 1.5
math = "image"
svg = "rasterize"
grayscale = false
css = "extra.css"
offline = true
follow-links = true
link-root = ".."          # optional, see "Linked local files"
format = "epub"           # or "confluence"
# confluence-url = "https://wiki.example.com"   # for `md2epub publish`
# confluence-space = "DOCS"
# confluence-parent-id = "123456"
```

Flags given on the command line still win: `md2epub build book.toml --profile remarkable -o rm.epub`.
`output-location = "input"` is accepted too.

### Your defaults (`md2epub config`)

If you always build for the same device, save your choices once instead of repeating
the flags:

```sh
md2epub config set profile scribe
md2epub config set notes after-chapter
md2epub config set author "Ada Lovelace" "Charles Babbage"   # one or more names
md2epub config                     # same as `md2epub config list`
```

| Command | What it does |
|---|---|
| `md2epub config list` | Every setting with its effective default and where it comes from (`built-in`, `built-in (profile scribe)` or `user config`). This is the default action. |
| `md2epub config get <key>` | Prints one effective default. |
| `md2epub config set <key> <value>` | Checks the value with the same rules as the flag and saves it. `author` takes one or more names, each as its own argument (names are not split on commas). Comments and other keys in the file are kept. |
| `md2epub config unset <key>` | Removes the key, so the built-in default applies again. |
| `md2epub config reset` | Deletes the file without asking, and prints what it set. |
| `md2epub config path` | Prints the file's location. |
| `md2epub config edit` | Opens the file in `$VISUAL` or `$EDITOR` (otherwise `vi`, or `notepad` on Windows) and checks it when the editor exits. |

Keys: `profile`, `author`, `lang`, `split-level`, `notes`, `notes-style`, `notes-pages`,
`margin`, `line-spacing`, `math`, `svg`, `grayscale`, `offline`, `verbose`, `css`,
`output-location`, `follow-links`, `format` and `confluence-toc`. They take the same values
as the flags with the same names. `confluence-url`, `confluence-space` and
`confluence-parent-id` are the defaults of `md2epub publish --url`, `--space` and
`--parent-id`. The Confluence token can never be saved here. Booleans accept `true`/`false`, `on`/`off`, `yes`/`no`. Per-book settings
(`title`, `cover`, `output`, `date`, `identifier`, inputs) cannot be defaults.

The file is TOML, `config.toml` in your configuration directory: `~/.config/md2epub/` on
Linux, `~/Library/Application Support/md2epub/` on macOS, and `%APPDATA%\md2epub\` on Windows.
Set the `MD2EPUB_CONFIG` environment variable to use another file. A relative `css`
path in the file is relative to the file's directory (`config set css` stores an absolute
path), and `~/` means your home directory.

Your defaults sit just above the profile defaults, so front matter, `book.toml` and
flags still win. A `profile` you set also changes the profile-derived defaults (`svg`),
and an `svg` or `math` value you set yourself applies under every profile.
If the file has a mistake, conversions stop with an error that names the file and the
key. `md2epub config edit`, `config path` and `config reset` still work, and so does
`config unset` as long as the file is valid TOML, while `config set` refuses to save
until the file is valid again (setting the broken key itself is fine). `--help` then
shows the built-in defaults. A `css` file from your defaults that no longer exists stops
conversions with an error that names the config file; `md2epub config unset css`
removes it.

`config` and `build` are subcommand names. Inputs always need a `.md` or `.html`
extension, so a file named `config` or `build` cannot be an input; rename it (for example
to `config.md`).

### Linked local files

Links to local files (`[diagram](docs/flow.svg)`, `<a href="../NOTES.md#setup">`,
`file:///...`) are followed: each linked file that exists is added to the book once,
after the inputs, in the order it is first linked (breadth-first), and the link is
rewritten to point to it. Paths are relative to the linking file (relative `file:` URLs
such as `file:notes.md` too); `%20` escapes are decoded and `?query` parts are ignored.
A link that starts with `/` (`/docs/guide.md`, as on GitHub) is relative to the project
folder. A path written with backslashes (`docs\guide.md`) is also tried with `/`, and a
link to a missing `guide.html` finds `guide.md` beside it.

Only files inside the **project folder** are added: the `--link-root` folder if you give
one (`link-root` in `book.toml`), otherwise the git work tree that holds the inputs (the
nearest folder with `.git`), otherwise the folder that holds all the inputs. Hidden files
and files in hidden folders (names starting with `.`, such as `.env` or `.ssh/`) are
never added. A linked file from outside the inputs' own folder (for example
`../README.md` inside the git work tree) is added with a warning, so you always see when
a file from elsewhere ends up in the book.

| Linked file | Added as |
|---|---|
| `.md`, `.markdown`, `.html`, `.htm`, `.xhtml` | Chapters, converted like an input; their own links are followed too, and `#fragment` links land on the right heading. |
| Images (`.svg`, `.png`, `.jpg`, `.gif`, `.webp`, `.bmp`, `.tif`) | A page with the file name and the image (rasterized under device profiles). |
| Mermaid (`.mmd`, `.mermaid`) | A page with the rendered diagram (the source as a code block, with a warning, if it cannot be rendered). |
| Other UTF-8 text files up to 1 MB | A page with the file name and the contents as a code block, highlighted by file extension. |

Pages for files other than documents are grouped in the table of contents under a final
**Linked files** section, named by their path relative to the inputs' folder
(`a/mod.rs`, `b/mod.rs`). A linked document that does not start with a level-1 heading
(a changelog with only `##` sections) gets its own top-level entry, named after the file,
with its headings under it. Links to files that cannot be added keep their text but lose
the link, with one warning per file and reason that quotes the link as you wrote it (the
file does not exist, is a directory, is binary, is larger than 1 MB, is outside the
project folder or is hidden). Links to a file that is also an input go to that input's chapter.
At most 500 linked files are added. Web links, `mailto:` links and links inside the book
are not affected. Use `--no-follow-links` (or `follow-links = false` in `book.toml`, or
`md2epub config set follow-links false` to make it your default) to only convert the
inputs; links to other local files are then removed, with a warning.

## Confluence

md2epub can also turn the same inputs into a page for **Confluence Data Center or Server**
(self-hosted), either as files you can inspect or upload yourself, or published directly
through the REST API.

### Export

```sh
md2epub guide.md --format confluence               # writes guide.confluence/
md2epub intro.md setup.md -o docs/handbook.confluence --format confluence
md2epub config set format confluence               # make it your default
```

The output is a directory (named like the EPUB, with `.confluence` in place of `.epub`;
`-o book.epub` is an error under `--format confluence`). An `.epub` output from
`-o` or `book.toml` asks for an EPUB, so with `format = confluence` only as your
default, a `book.toml` with `output = "book.epub"` still builds its EPUB. `-o DIR` on
an existing directory that is not an md2epub export (even one with some other
`manifest.json`) writes `DIR/<name>.confluence` inside it and never changes its files:

| File | Contents |
|---|---|
| `page.xml` | The page body in [Confluence storage format](https://confluence.atlassian.com/doc/confluence-storage-format-790796544.html), UTF-8, without a root element. |
| `attachments/` | Every image, diagram and formula the page shows, named by content hash (`3f9a….png`). |
| `manifest.json` | The title and, per attachment, the file name, media type, size and SHA-256. |

All inputs and the local documents they link to become **one page**, in order. How the
content maps to Confluence:

| Markdown / HTML | Confluence |
|---|---|
| Headings, paragraphs, lists, tables (with header cells), quotes, rules, bold, italic, underline, sub/superscript, inline code | The same elements. Strikethrough becomes `text-decoration: line-through`. |
| Images, SVG, Mermaid diagrams, LaTeX math | Attachments shown with `<ac:image>`. SVG, diagrams and math are rasterized to PNG at twice their display size, with transparency kept (so they also work on dark themes); inline math sits in the text. |
| Remote images | Downloaded and attached; with `--offline` they are shown from their URL instead. |
| Fenced code | The **Code Block** macro with its language (`bash`, `py`, `js`, `rust`, …; an unknown language gives `none`). No highlighting markup is kept. |
| Links to headings and between your documents | Links to anchors on the page. md2epub adds an **Anchor** macro for every heading and element id, because Confluence's own heading anchors change when the page title or a heading changes. |
| External links | Plain links. |
| Footnotes | Superscript links to one **Footnotes** list at the end of the page, numbered through all documents, with links back. |
| Linked images (`[![alt](img.png)](#anchor)`) | The image inside the link (`<ac:link-body><ac:image>`), attached like any other image. |
| Definition lists, captions, `div`/`section`/`center` | Paragraphs (a `dd` indented); block content inside them stays as blocks. |
| Task lists | Confluence tasks (complete / incomplete). |
| `<details>` | The **Expand** macro. |

The cover, note pages, margins, `--css`, device profiles and the book navigation are EPUB
features and are left out. With `--confluence-toc` the page starts with a **Table of
Contents** macro.

### Publish

```sh
export MD2EPUB_CONFLUENCE_TOKEN=...                # your personal access token
md2epub publish guide.md --url https://wiki.example.com --space DOCS --parent-id 123456
md2epub publish guide.md --url https://example.com/confluence --space DOCS --dry-run
md2epub publish guide.confluence                   # upload an export as it is
md2epub publish book.toml --page-id 987654         # update a known page
```

| Option | Meaning |
|---|---|
| `--url <URL>` | Base URL of Confluence, including its context path if it has one (`https://example.com/confluence`). |
| `--space <KEY>` | Space key. |
| `--parent-id <ID>` | Create the page under this page (the number in `pageId=` of its URL). Without it, the page goes to the top of the space. Given on the command line, it also **moves** an existing page under that page (the dry run says so); a saved `confluence-parent-id` only places new pages. |
| `--title <TITLE>` | Page title (default: as for the book title: front matter, first heading, file name). |
| `--page-id <ID>` | Update this page instead of looking for a page with the same title in the space. |
| `--dry-run` | Print what would be created, updated, uploaded and deleted. Only read requests are sent. |
| `--prune-attachments` | Delete attachments of the page that the new version does not use (including ones added by hand). |
| `--ca-cert <FILE>` | Trust the CA certificates in this PEM file **instead of** the system trust store. |
| `--token-env <NAME>` | Read the token from this environment variable (default `MD2EPUB_CONFLUENCE_TOKEN`). |
| `--allow-insecure-http` | Allow an `http://` URL of another machine, which sends the token unencrypted. Without it, plain HTTP is only used for `localhost`/`127.0.0.1`/`[::1]`. |

`url`, `space` and `parent-id` can be saved with `md2epub config set confluence-url …`,
`confluence-space` and `confluence-parent-id`, or set in `book.toml`. A personal space key
is `~` plus the user name (`~john@corp.com`). Conversion options
(`--offline`, `--math`, `--follow-links`, `--confluence-toc`, …) work as for an export;
for an export directory, which is already converted, they are ignored with a warning
(only `--title` applies). `publish` writes no file, so `-o`, `--output-location` and
`--format epub` are errors there.

What publish does:

1. It looks for the page by `--page-id`, or by title in the space.
2. A **new page** with attachments is first created with a one-line "Publishing…" body,
   the attachments are uploaded, and then the real body is saved as version 2. The page
   never shows broken images.
3. An **existing page** first gets its new and changed attachments, then the new body as
   the next version (if someone saved the page at the same moment, md2epub fetches the
   new version number and tries once more), and only then are unused attachments
   deleted (with `--prune-attachments`). Unchanged attachments (same file name, size and
   hash) are not uploaded again.
4. It prints the page URL.

**Token.** Create a personal access token in Confluence: your avatar (top right) >
**Profile** > **Personal Access Tokens** > **Create token**. Personal access tokens exist in
Confluence Data Center and Server 7.9 and later. md2epub sends it as
`Authorization: Bearer <token>` and reads it only from the environment: it is never accepted
as an argument or a config key, and it is never printed (it is masked in error messages).
md2epub does not follow redirects, because the token would not go along: a redirect is an
error that names where it leads, so you can pass that base URL as `--url`.

**Permissions.** The token's user needs **View** and **Add** (pages) permission in the
space, **Add** for attachments, and **Delete** for attachments to use
`--prune-attachments`. To update a page with page restrictions, the user must be allowed
to edit it.

**Network.** HTTPS certificates are checked against the operating system's trust store,
so a corporate CA that your system trusts works; use `--ca-cert` otherwise. Proxies are
taken from `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` and `NO_PROXY`. Requests time out
after 20 s without a connection and 5 minutes in total.

**Limitations.**

- Only Confluence **Data Center / Server** (REST API v1 and personal access tokens) is
  supported, not Confluence Cloud.
- Everything goes on one page; `--split-level` does not apply.
- Math and diagrams are images: they cannot be edited in Confluence, and math does not
  scale with the text.
- The language names of the code macro follow Confluence's list; the newer ones (`rust`,
  `go`, `kotlin`, `typescript`, …) need a recent Data Center release.
- Raw HTML is reduced to what storage format supports: layout elements (`div`, `span`,
  ...) and inline styles are dropped, their text is kept.
- A page found by title is updated even if someone else created it; use `--dry-run` to
  check first.
- `--ca-cert` replaces the system trust store for that run instead of adding to it.

## Device guide

### Kindle Scribe

1. Build with `--profile scribe`. Adding `--notes after-chapter` or `--margin wide` gives
   you room to write.
2. Send the EPUB with **Send to Kindle** (the web uploader at amazon.com/sendtokindle,
   the desktop or mobile app, or email to your `@kindle.com` address). **Do not copy it
   over USB.** Handwriting on books (Active Canvas) only works for documents that come
   through Send to Kindle. Kindle never offers handwriting on fixed-layout books, which
   is why md2epub always produces reflowable EPUBs.
3. On the Scribe, open the book and write on the page with the pen. The note pages are
   full-page images, and they are the most reliable place for longer notes.

Kindle applies the reader's own font, margin and line-spacing settings, so `--margin` and
`--line-spacing` are hints there. The note pages always work.

### reMarkable (2, Paper Pro)

1. Build with `--profile remarkable` (reMarkable 2) or `--profile remarkable-pro`
   (Paper Pro).
2. Transfer the EPUB with the **reMarkable desktop or mobile app**, or with the
   **USB web interface** (connect by cable, enable *Settings → Storage → USB web
   interface*, then open http://10.11.99.1 in a browser and drag the file in).
3. The device lays the EPUB out into pages. You can then write anywhere. Use the margin
   presets and note pages for extra space.

### Desktop readers

`--profile generic` keeps SVG and is suitable for Apple Books, Calibre, Thorium, etc.

## Build features

| Feature | Default | What it adds | Without it |
|---|---|---|---|
| `math` | on | In-process LaTeX math rendering (mitex + typst, fonts embedded) | `--math image` keeps the LaTeX as code with a warning. `--math mathml` and `--math off` still work. |
| `mermaid` | on | In-process Mermaid rendering (mermaid-rs-renderer) | md2epub uses `mmdc` (Mermaid CLI) if it is on `PATH`. Otherwise the diagram stays a code block. |

Release binary sizes (macOS arm64, stripped, thin LTO):

| Build | Size |
|---|---|
| default (`math` + `mermaid`) | ~41 MB |
| `--no-default-features --features mermaid` | ~13 MB |
| `--no-default-features` (slim) | ~11 MB |

Most of the size comes from the Typst engine that renders math. If you never use LaTeX
math, the slim build is a good choice:

```sh
cargo build --release --no-default-features --features mermaid
```

## Known limits

- **Output is always reflowable.** Fixed-layout EPUBs do not get Kindle handwriting
  support, so precise page layout is not possible. Page breaks are placed only around
  chapters and note pages.
- **Mermaid is best-effort.** The built-in renderer covers flowcharts and sequence
  diagrams. Less common diagram types may fall back to `mmdc` (if installed) or stay
  a code block, with a warning.
- **Math** covers the common LaTeX subset (fractions, sums, integrals, matrices,
  `aligned`, Greek letters, accents). Unsupported macros are kept as code, with a
  warning. Math images do not scale with the reader's font size the way text does.
- **MathML and SVG** are not reliable through Send to Kindle, so the device profiles
  avoid both by default.
- **External stylesheets** (`<link rel="stylesheet">` and `@import`) in HTML inputs are
  ignored, with a warning. Use `--css` instead. `<style>` blocks and `style` attributes
  are kept.
- **Very large or deeply nested input** is refused: images over 100 megapixels are not
  embedded, and documents nested more than 2000 elements deep stop with an error. A
  formula or SVG that takes too long to render (20 s and 30 s) is skipped with a warning.
- **JavaScript** in HTML inputs is removed, and so are scripts and event handlers in SVG
  images that are kept as SVG. Pages that build their content with JavaScript
  convert to whatever static HTML they contain.
- **Remote images** are fetched with a 20 s timeout and a 50 MB cap. With `--offline` they
  are skipped and their alt text is kept.
- **CSS on e-ink readers:** flexbox, grid, fixed positioning and viewport units are
  avoided in the built-in stylesheet and removed from input styles under the device
  profiles. A `--css` file is used as written and may render differently on devices.
- On-device behaviour (handwriting on the Scribe and the reMarkable) cannot be tested
  automatically. Please report problems with a sample file.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test
cargo test --no-default-features
```

`tests/integration.rs` runs the built binary on `tests/fixtures/` under every profile. It
checks the zip layout, XML well-formedness, that every link and image resolves, and the
device constraints. `tests/confluence.rs` checks the Confluence export and runs
`md2epub publish` against an in-process mock of the Confluence REST API. The tests never
use the network.

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/)
(`dist-workspace.toml`, `.github/workflows/release.yml`). Pushing a tag such as `v0.1.0`
builds the five targets and the shell and PowerShell installers, and creates a GitHub
Release. `dist plan` shows what would be built.

## License

MIT. See [LICENSE](LICENSE). The bundled fonts are under the SIL Open Font License; see
`assets/fonts/` and `assets/math/`.

# md2epub

Convert rich Markdown or HTML into a **reflowable EPUB 3** that you can write on with a
**Kindle Scribe** (Active Canvas, via Send to Kindle) or a **reMarkable**.

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
- Blank, lined, dotted or grid **note pages** after every chapter or at the end of the
  book, plus wide-margin presets that leave room for handwriting.
- One self-contained binary per OS. You do not need Java, Node, Python or a browser.

## Install

### Prebuilt binaries

Every tagged release has archives for macOS (Apple Silicon and Intel), Linux (x64 and
arm64, static musl) and Windows (x64), plus installers:

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/KDra/md2epub/releases/latest/download/md2epub-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/KDra/md2epub/releases/latest/download/md2epub-installer.ps1 | iex"
```

You can also download an archive from the Releases page and put `md2epub` on your `PATH`.

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

If you leave out `-o`, the book is written to `<first-input-stem>.epub` in the current
directory. Warnings, such as a missing image or a remote image skipped offline, are
printed to stderr, and the book is still written. md2epub exits with a non-zero
status only for fatal problems, such as a missing input file or a bad option. In that
case it leaves no partial EPUB behind.

### Options

| Option | Values | Default | Meaning |
|---|---|---|---|
| `-o, --output <FILE>` | path | `<stem>.epub` | Output EPUB. |
| `--profile <NAME>` | `scribe`, `remarkable`, `remarkable-pro`, `generic` | `generic` | Target device (see below). |
| `--title <TITLE>` | text | front matter, then first `<h1>`, then file name | Book title. |
| `--author <NAME>` | text, repeatable | front matter | Author(s). |
| `--lang <LANG>` | BCP 47 tag | `en` | Book language. |
| `--cover <IMG>` | PNG, JPEG, SVG, WebP, ... | generated title page | Cover image. |
| `--split-level <L>` | `1`, `2`, `none` | `1` | Start a new chapter (file) at each heading of this level. Each input file always starts a chapter. |
| `--notes <WHERE>` | `none`, `after-chapter`, `end` | `none` | Insert full-page note pages. |
| `--notes-style <S>` | `blank`, `lined`, `dotted`, `grid` | `lined` | Pattern on note pages. |
| `--notes-pages <N>` | integer | `1` | Note pages per insertion point. |
| `--margin <M>` | `normal`, `wide`, `extra` | `normal` | Side padding: none, about 8%, about 15% plus more paragraph spacing. |
| `--line-spacing <F>` | number, e.g. `1.6` | reader default | CSS `line-height`. |
| `--math <MODE>` | `image`, `mathml`, `off` | `image` | Render math to images, convert to MathML, or keep the LaTeX source as code. |
| `--svg <MODE>` | `rasterize`, `keep` | from profile | Rasterize SVG to PNG, or keep it as SVG. |
| `--grayscale` | flag | off | Convert all images to grayscale. |
| `--css <FILE>` | path | none | Extra CSS appended after the built-in stylesheet. |
| `--offline` | flag | off | Do not download remote images (their alt text is kept). |
| `-v, --verbose` | flag | off | Print progress and detailed diagnostics. |

**Precedence:** command-line flags > `book.toml` > YAML front matter of the first input >
profile defaults.

### Profiles

| Profile | Screen | SVG | Math | Transparency |
|---|---|---|---|---|
| `scribe` | 1860×2480 | rasterized | image | flattened |
| `remarkable` | 1404×1872 | rasterized | image | flattened |
| `remarkable-pro` | 1620×2160 | rasterized | image | flattened |
| `generic` | 1600×2133 | kept | image | kept |

Under `scribe`, `remarkable` and `remarkable-pro`, the EPUB contains no `<svg>`, no
`<math>`, no SVG files and no images with an alpha channel. Every image is an opaque
PNG or a JPEG. You can still pass `--svg keep` or `--math mathml` explicitly. With
`--profile scribe --math mathml`, md2epub warns that Send to Kindle may reject the book.

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
```

Flags given on the command line still win: `md2epub build book.toml --profile remarkable -o rm.epub`.

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
- **External stylesheets** (`<link rel="stylesheet">`) in HTML inputs are ignored, with a
  warning. Use `--css` instead. `<style>` blocks and `style` attributes are kept.
- **JavaScript** in HTML inputs is removed. Pages that build their content with JavaScript
  convert to whatever static HTML they contain.
- **Remote images** are fetched with a 20 s timeout and a 50 MB cap. With `--offline` they
  are skipped and their alt text is kept.
- **CSS on e-ink readers:** flexbox, grid, fixed positioning and viewport units are
  avoided in the built-in stylesheet. Custom CSS that uses them may render differently
  on devices.
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
device constraints. The tests never use the network.

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/)
(`dist-workspace.toml`, `.github/workflows/release.yml`). Pushing a tag such as `v0.1.0`
builds the five targets and the shell and PowerShell installers, and creates a GitHub
Release. `dist plan` shows what would be built.

## License

MIT. See [LICENSE](LICENSE). The bundled fonts are under the SIL Open Font License; see
`assets/fonts/` and `assets/math/`.

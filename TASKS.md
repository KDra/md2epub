# md2epub: task board

A Rust CLI that converts rich Markdown (inline HTML, SVG/PNG/JPG references) or HTML into a reflowable EPUB 3 that can be handwritten on with a Kindle Scribe (via Send to Kindle) or a reMarkable. One static binary per OS.

This file is written so that independent workers can each take one task, implement it, and exit. **T0 must be merged first.** T1 to T9 can then run in parallel. T10 runs last.

```
T0 ──► T1 T2 T3 T4 T5 T6 T7 T8 T9 (parallel) ──► T10
```

## Rules for workers

1. Take exactly one task. Work on branch `task/T<n>` (or a git worktree). Exit when its "Done when" list passes.
2. Edit only the files your task **owns**. If you need a change in a shared file (`model.rs`, `ctx.rs`, `dom.rs`, `pipeline.rs`, `main.rs`), make the smallest possible change and list it in your final report.
3. In `Cargo.toml`, add dependencies only under your own `# T<n>` comment line, so merges stay trivial.
4. After T0, the code in `src/model.rs`, `src/ctx.rs` and `src/dom.rs` is the source of truth for the contracts below. If this file and the code disagree, follow the code.
5. Every stub from T0 returns valid placeholder output, so `cargo run` produces an EPUB at all times. Keep it that way: no panics on bad input, return `anyhow::Result` and push non-fatal problems to `ctx.warn(...)`.
6. Before exiting: `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all pass. Put unit tests in your own module (`#[cfg(test)]`).
7. No network access in tests. No new runtime requirements for end users (no Java, Node, Python, browser).

## Background that drives the design

- Kindle Scribe supports handwriting on **reflowable** EPUBs (Active Canvas) when they arrive through Send to Kindle. Fixed-layout EPUBs do not get handwriting. So the output is never fixed-layout.
- Send to Kindle tends to reject or drop SVG, MathML, transparent images and base64 images inside SVG. Under `--profile scribe`, no `<svg>` or `<math>` may remain and every image is an opaque PNG/JPEG.
- reMarkable renders the EPUB into pages on-device. Its SVG/CSS support is undocumented, so its profile is equally conservative.
- Kindle applies the reader's own margin and spacing settings, so CSS margins are only hints there. Note pages are the dependable writing room.

## Shared contracts (created by T0)

### CLI

```
md2epub <inputs...> -o book.epub
md2epub build book.toml
  --profile scribe|remarkable|remarkable-pro|generic   (default generic)
  --title --author --lang --cover <img>
  --split-level 1|2|none
  --notes none|after-chapter|end  --notes-style blank|lined|dotted|grid  --notes-pages N
  --margin normal|wide|extra  --line-spacing <float>
  --math image|mathml|off  --svg rasterize|keep  --grayscale
  --css <file>  --offline  -v
```

Precedence: flags > `book.toml` > YAML front matter of the first input > profile defaults.

### Pipeline (`src/pipeline.rs`)

```
for each input file:
    html = markdown::render(src)?.html      (.md)   or file contents (.html)
    doc  = dom::parse(html)
    html::sanitize(&doc, ctx)
    mermaid::transform(&doc, ctx)
    math::transform(&doc, ctx)
    assets::transform(&doc, ctx)            (img, inline <svg>, CSS url())
    chapters += html::split(&doc, ctx)      (serializes each chapter body to XHTML)
chapters = notes::insert(chapters, ctx)
epub::write(Book { meta, chapters, resources, css, cover }, out_path)
```

### Types (`src/model.rs`, `src/ctx.rs`)

```rust
pub struct Options { profile: Profile, meta: Metadata, cover: Option<PathBuf>,
    split_level: Option<u8>, notes: NotesPlacement, notes_style: NotesStyle, notes_pages: u32,
    margin: Margin, line_spacing: Option<f32>, math: MathMode, svg: SvgMode,
    grayscale: bool, offline: bool, extra_css: Option<PathBuf>, verbose: bool }
pub struct Profile { name, screen_w: u32, screen_h: u32, svg: SvgMode, math: MathMode, flatten_alpha: bool }
pub struct Metadata { title, authors: Vec<String>, lang, identifier: Option<String>, date: Option<String> }
pub struct Heading { level: u8, text: String, id: String }
pub struct Chapter { title: String, file_name: String /* ch001.xhtml */, body_xhtml: String,
    headings: Vec<Heading>, in_toc: bool }
pub struct Resource { href: String /* relative to OEBPS, e.g. images/ab12cd.png */, media_type: String, data: Vec<u8> }
pub struct Book { meta, chapters, resources, css: String, cover: Option<Resource> }

pub struct Ctx { pub opts: Options, pub base_dir: PathBuf /* dir of current input */, ... }
impl Ctx {
    fn add_image(&mut self, bytes: Vec<u8>, ext: &str) -> String;  // dedupes by hash, returns chapter-relative src "../images/<hash>.<ext>"
    fn add_vector(&mut self, svg: &str) -> Result<ImgRef>;         // rasterizes via svg::rasterize or keeps as .svg, per opts.svg
    fn warn(&mut self, msg: impl Into<String>);
}
pub struct ImgRef { src: String, width_px: u32, height_px: u32 }
```

### EPUB layout

```
mimetype                      (first entry, stored, no extra fields)
META-INF/container.xml
OEBPS/content.opf  nav.xhtml  toc.ncx
OEBPS/text/cover.xhtml  ch001.xhtml ...
OEBPS/images/<hash>.<ext>
OEBPS/css/base.css
```

### DOM conventions between stages

- DOM is `markup5ever_rcdom` (`Handle`), helpers in `src/dom.rs`: `parse`, `serialize_xhtml`, `select_by_tag`, `get_attr`, `set_attr`, `replace_with`, `text_content`.
- Math placeholder: `<span data-math-style="inline|display">LATEX</span>` (what comrak's `math_dollars` emits).
- Mermaid placeholder: `<pre class="mermaid">SOURCE</pre>`.
- An `<img>` that a stage has already finished carries `data-res="1"`. `assets::transform` skips those and removes the attribute.

---

## T0: Foundation (blocking, one worker)

**Owns:** `Cargo.toml`, `src/main.rs`, `src/model.rs`, `src/ctx.rs`, `src/dom.rs`, `src/pipeline.rs`, stub versions of every other module, `tests/fixtures/`, `.gitignore`, `.github/workflows/ci.yml`.

- Install Rust stable with `rustup` if missing. `git init`, `cargo init --name md2epub`.
- Implement the shared types, `Ctx` (resource store with content-hash dedupe, warnings), and `dom.rs`. `serialize_xhtml` must produce well-formed XML: void elements self-closed, attributes quoted, text escaped, named entities such as `&nbsp;` written as numeric references.
- Write `pipeline.rs` exactly as in the contract, and a stub for each module of T1 to T9 with its final public signature and a valid placeholder result (for example `svg::rasterize` returns a 1×1 white PNG, `markdown::render` wraps the text in `<pre>`, `epub::write` writes a minimal valid EPUB with the chapters).
- Fixtures: `kitchen-sink.md` (GFM table, footnotes, task list, inline HTML, inline `<svg>`, referenced `.svg`/`.png`/`.jpg`, a remote image URL, fenced code in two languages, inline and display math, a Mermaid block, YAML front matter), `page.html`, `book.toml`, and the small image files they reference.
- CI workflow: fmt, clippy, test on Linux, macOS, Windows.

**Done when:** `cargo run -- tests/fixtures/kitchen-sink.md -o out.epub` writes a file that unzips with `mimetype` first; fmt, clippy and tests pass; first commit made.

## T1: CLI, config and profiles

**Owns:** `src/cli.rs`, `src/config.rs`, `src/profile.rs`.

- `clap` derive for the CLI above, including the `build book.toml` subcommand.
- `config::resolve(cli, front_matter: Option<&str>) -> Result<(Options, Vec<PathBuf>)>` applying the precedence rule. `book.toml` holds the same keys as the flags plus `inputs = [...]`. Front matter is YAML (`title`, `author`/`authors`, `lang`, `date`, `cover`).
- Profiles: `scribe` 1860×2480, `remarkable` 1404×1872, `remarkable-pro` 1620×2160, all with rasterized SVG, image math, flattened alpha; `generic` 1600×2133 with SVG kept, image math, alpha kept.
- Default title: front matter, else first `<h1>` (pipeline passes it), else the input file stem. Clear errors for missing inputs and unknown extensions.

**Done when:** unit tests cover precedence, each profile, `book.toml` parsing and bad input errors.

## T2: Markdown rendering

**Owns:** `src/markdown.rs`.

- `render(src: &str) -> Result<Rendered { html: String, front_matter: Option<String> }>` using `comrak`: GFM tables, strikethrough, task lists, autolinks, footnotes, header ids, front matter delimiter `---`, `math_dollars`, raw HTML passed through.
- Code highlighting with `syntect` (pure-Rust regex feature, no Oniguruma) using inline styles and a theme that stays legible in grayscale on e-ink. Unknown languages fall back to plain `<pre><code>`.
- Fenced blocks tagged `mermaid` are not highlighted: emit `<pre class="mermaid">SOURCE</pre>` with the source HTML-escaped.

**Done when:** unit tests check tables, footnotes, raw HTML pass-through, front matter extraction, both math placeholders, the Mermaid placeholder and highlighted code.

## T3: HTML sanitizing and chapter splitting

**Owns:** `src/html.rs`.

- `sanitize(doc, ctx)`: for full HTML documents keep only `<body>` content (take `<title>` as a title hint); remove `script`, `iframe`, `form`, `object`, `embed`, `video`, `audio`, `link`, `meta`, event-handler attributes and `javascript:` links; drop attributes that are invalid in EPUB XHTML; keep `style` attributes and `<style>` contents.
- Ensure every heading has a unique `id` across the whole book.
- `split(doc, ctx) -> Result<Vec<Chapter>>`: one chapter per input file, further split before each heading at `opts.split_level`. Fill `title`, `headings`, `file_name` (`ch001.xhtml`, numbered across the book) and `body_xhtml` via `dom::serialize_xhtml`.
- Rewrite internal links: `#id` links that now point into another chapter become `chNNN.xhtml#id`; links between input files (`other.md#x`, `other.html`) map to the matching chapter. This needs a pass after all files are split; expose `fix_links(chapters: &mut [Chapter])` and call it from the pipeline.

**Done when:** unit tests cover sanitizing, splitting at levels 1, 2 and none, unique ids, and cross-chapter link rewriting; output of each chapter parses as XML.

## T4: Images and asset resolution

**Owns:** `src/assets.rs`.

- `transform(doc, ctx)`: for every `<img>` without `data-res`, load the source: path relative to `ctx.base_dir`, absolute path, `file://`, `data:` URI, or `http(s)` via `ureq` with rustls (skipped with a warning under `--offline`; 20 s timeout; 50 MB cap). On failure keep the alt text and warn.
- Raster images: decode with the `image` crate; convert anything that is not PNG/JPEG (WebP, GIF first frame, BMP, TIFF) to PNG; downscale to fit `profile.screen_w × screen_h`; flatten alpha onto white when `profile.flatten_alpha`; grayscale when `opts.grayscale`. Do not re-encode files that already comply. Store with `ctx.add_image`.
- SVG: referenced `.svg` files and inline `<svg>` elements go through `ctx.add_vector`; an inline `<svg>` is replaced by an `<img>`, using its `<title>` or `aria-label` as alt text. Resolve the SVG's own relative image references against the SVG file's directory.
- Keep `width`/`height` attributes sensible and add `alt=""` where missing. Remove `data-res`. Also resolve `url(...)` in `style` attributes and `<style>` blocks.
- Cover: `load_cover(path, ctx) -> Result<Resource>` with the same processing.

**Done when:** unit tests cover local, data-URI, missing, oversized, transparent and WebP inputs, inline SVG replacement and the `--offline` path, using fixtures only.

## T5: SVG rasterizing

**Owns:** `src/svg.rs`, `assets/fonts/`.

- `rasterize(svg: &[u8], resources_dir: Option<&Path>, max_w: u32, max_h: u32, scale: f32) -> Result<Raster { png: Vec<u8>, width: u32, height: u32 }>` with `resvg`/`usvg`/`tiny-skia`. White background, fit within the maximum box, render at 2× the SVG's nominal size by default so lines stay crisp at 300 ppi.
- `intrinsic_size(svg: &[u8]) -> Result<(f32, f32)>` for callers that keep the SVG.
- Fonts: load system fonts through `fontdb` and embed one open-licensed fallback sans font (and its license file) with `include_bytes!`, set as the default family so text renders identically on every OS.
- Handle SVGs with no `width`/`height` (use `viewBox`), with embedded base64 images, and malformed input (error, no panic).

**Done when:** unit tests rasterize fixtures with text, with only a `viewBox`, and with an embedded image, and assert size and non-blank pixels.

## T6: LaTeX math

**Owns:** `src/math.rs`. Cargo feature `math` (default on).

- **Start with a spike:** confirm that `mitex` (LaTeX → Typst math) plus `typst` used as a library with `typst-svg` builds on stable and renders `\frac{a}{b}`, `\sum_{i=0}^n`, `\begin{aligned}`, matrices and Greek letters. If that route is not workable, evaluate other pure-Rust LaTeX renderers. Write down the outcome in your final report.
- `render_svg(latex: &str, display: bool) -> Result<MathSvg { svg, width_em, height_em, depth_em }>` and `render_mathml(latex, display) -> Result<String>` (`latex2mathml` or equivalent).
- `transform(doc, ctx)`: replace each math placeholder according to `opts.math`. `image`: `ctx.add_vector` then an `<img data-res="1" class="math-inline|math-display" alt="LATEX">` sized in `em` with `vertical-align` set from the depth so inline math sits on the baseline. `mathml`: inline `<math>`. `off`: leave the LaTeX in `<code>`.
- On a render failure keep the source in `<code class="math-error">` and warn. Under `--profile scribe`, `mathml` mode warns that Send to Kindle may reject the file.
- Fonts needed for math are embedded in the binary.

**Done when:** unit tests cover inline and display rendering, baseline metrics present, failure fallback and each mode; the spike result is reported.

## T7: Mermaid diagrams (best effort)

**Owns:** `src/mermaid.rs`. Cargo feature `mermaid` (default on).

- **Start with a spike:** check whether the pure-Rust `mermaid-rs-renderer` crate builds and renders flowcharts and sequence diagrams to SVG acceptably. Report the outcome.
- `render(src: &str) -> Result<String /* SVG */>`: use the in-process renderer if the spike succeeds; otherwise, or when it fails on a diagram, run `mmdc` if it is on PATH (temp files in a temp dir, 30 s timeout).
- `transform(doc, ctx)`: replace each `<pre class="mermaid">` with `<figure class="mermaid"><img data-res="1" ...></figure>` through `ctx.add_vector`. If nothing can render it, leave it as a normal code block and warn once.

**Done when:** unit tests cover the transform with a successful render, the fallback to a code block, and never panic on garbage input.

## T8: Stylesheet, margins and note pages

**Owns:** `src/notes.rs`, `src/style.rs`, `assets/base.css`.

- `assets/base.css`: e-ink friendly defaults: black on white, no background colors, images `max-width: 100%` and never split across pages, tables with borders and small font, code blocks wrapping instead of scrolling, footnotes, figures, `.math-display` centered, `.notes-page` rules. Avoid CSS that Kindle or simple renderers ignore badly (flex, grid, fixed positioning, viewport units).
- `style::build_css(opts) -> Result<String>`: base CSS plus margin preset (`normal` none, `wide` about 8% side padding, `extra` about 15% plus larger paragraph spacing), `line_spacing`, then the user's `--css` file appended.
- `notes::insert(chapters, ctx) -> Result<Vec<Chapter>>`: for `after-chapter` or `end`, add `opts.notes_pages` note pages. Each page is its own chapter (`in_toc: false`, except one "Notes" TOC entry in `end` mode) containing a single full-page PNG generated at profile resolution: `blank` (white with a faint corner mark so the page is not collapsed), `lined`, `dotted` or `grid` in light gray. Generate each style once and reuse the resource. Force a page break before and after.

**Done when:** unit tests cover CSS for each margin preset and the user CSS append, note page counts and placement for each mode, and image dimensions matching the profile.

## T9: EPUB packaging

**Owns:** `src/epub.rs`.

- `write(book: &Book, out: &Path) -> Result<()>` producing the layout above with the `zip` crate. `mimetype` is the first entry, stored, with no extra field.
- `content.opf` (EPUB 3.0 package): `dc:identifier` as a UUID derived from the content hash unless supplied, `dc:title`, `dc:creator` per author, `dc:language`, `dcterms:modified`, manifest with correct media types and the `nav`, `cover-image` and `svg` properties where they apply (a chapter gets `svg` or `mathml` properties if its body contains those elements), spine in order, and the EPUB 2 `<meta name="cover">` for older readers.
- `nav.xhtml` with a nested TOC from chapter titles and headings (down to level 3, skipping `in_toc: false`) plus landmarks; `toc.ncx` with the same structure.
- Chapter wrapper: XML declaration, XHTML doctype-free EPUB 3 document with `xmlns:epub`, `lang`, title and the stylesheet link.
- Cover: `cover.xhtml` showing the cover image. If none is given, generate a simple title/author cover as SVG and pass it through `svg::rasterize`.
- Write to a temp file and rename, so a failed run never leaves a broken EPUB.

**Done when:** unit tests build a small `Book`, reopen the zip and assert entry order and compression of `mimetype`, that every manifest href exists and every spine idref resolves, and that OPF, nav, NCX and chapters parse as XML.

## T10: Integration, docs and release (after T1 to T9)

**Owns:** `tests/integration.rs`, `README.md`, `LICENSE`, `.github/workflows/release.yml`, dist config.

- Merge the task branches and fix seams between modules.
- Integration tests: build `kitchen-sink.md`, `page.html` and `book.toml` under every profile and assert: valid zip layout; all XHTML parses as XML; every `src`/`href` inside the book resolves; under `scribe` there is no `<svg>`, no `<math>`, no `.svg` resource and no image with alpha; note pages and margins appear when requested; a run with `--offline` succeeds with a warning for the remote image.
- Run `epubcheck` once locally if Java is available and fix what it reports (do not make it a test dependency).
- `README.md`: install, usage, options, `book.toml` example, device guide (Kindle Scribe: use Send to Kindle, not USB; reMarkable: transfer with the app or USB web interface), known limits.
- Release with `cargo-dist`: macOS arm64 and x64, Linux x64 and arm64 (musl), Windows x64; shell and PowerShell installers. Check binary size and report it; if it is excessive, document the slim build without the `math`/`mermaid` features. Do not publish or push anything without the user asking.

**Done when:** integration tests pass on the three CI platforms, and the three profile builds of the kitchen-sink fixture open correctly in a desktop EPUB reader. The on-device check (writing on the Scribe and the reMarkable) is left to the user.

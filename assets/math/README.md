# Math rendering assets

Embedded into the binary by `src/math.rs` when the `math` feature is on.

| File | Source | License |
| --- | --- | --- |
| `NewCMMath-Book.otf` | New Computer Modern Math (Book weight), from `typst-assets` 0.15.1 | GUST Font License, see `LICENSE-NewCMMath.txt` |
| `LibertinusSerif-Regular.otf` | Libertinus Serif, from `typst-assets` 0.15.1 (text fallback for scripts the math font lacks) | SIL OFL 1.1, see `LICENSE-LibertinusSerif.txt` |
| `mitex/prelude.typ`, `mitex/standard.typ` | MiTeX Typst package 0.2.4 (`packages/mitex/specs`) | Apache-2.0, see `mitex/LICENSE-Apache-2.0.txt` |

Changes to the MiTeX files: `standard.typ` no longer imports the GPL-licensed
`@preview/xarrow` package (replaced by Typst's built-in `stretch`), imports the
prelude from `/prelude.typ`, and spells `\KaTeX` without the `kai` symbol that
Typst 0.15 removed.

`NewCM10-Regular` is deliberately not bundled (GPL with font exception).

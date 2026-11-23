//! Image and asset resolution (owned by T4).
//!
//! Contract:
//! - [`transform`]: for every `<img>` without `data-res`, load the source
//!   (path relative to `ctx.base_dir`, absolute path, `file://`, `data:` URI,
//!   or `http(s)` via `ureq` + rustls, skipped with a warning under
//!   `--offline`; 20 s timeout; 50 MB cap). On failure keep the alt text and
//!   warn. Raster images: convert non PNG/JPEG to PNG, downscale to the
//!   profile screen, flatten alpha when `profile.flatten_alpha`, grayscale
//!   when `opts.grayscale`, never re-encode compliant files; store with
//!   `ctx.add_image`. SVG files and inline `<svg>` go through
//!   `ctx.add_vector_in` (inline `<svg>` becomes an `<img>` with its
//!   `<title>`/`aria-label` as alt). Keep `width`/`height` sensible, add
//!   `alt=""` where missing, remove `data-res`, resolve `url(...)` in `style`
//!   attributes and `<style>` blocks.
//! - [`load_cover`]: same processing for the cover image, returning the
//!   resource (href `images/cover.<ext>` or the hashed name) without adding
//!   it to `ctx`.
//!
//! The T0 stub drops unresolved images (keeping their alt text) and reads
//! the cover file as-is.

use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::ctx::Ctx;
use crate::dom::{self, Handle};
use crate::model::{Resource, media_type_for_ext};

/// Resolves and stores every image in the document. See the module docs.
pub fn transform(doc: &Handle, ctx: &mut Ctx) -> Result<()> {
    for img in dom::select_by_tag(doc, "img") {
        if dom::has_attr(&img, "data-res") {
            dom::remove_attr(&img, "data-res");
            if !dom::has_attr(&img, "alt") {
                dom::set_attr(&img, "alt", "");
            }
            continue;
        }
        let src = dom::get_attr(&img, "src").unwrap_or_default();
        let alt = dom::get_attr(&img, "alt").unwrap_or_default();
        ctx.warn(format!(
            "image not embedded (asset loading unavailable): {src}"
        ));
        let span = dom::create_element("span", &[("class", "img-missing")]);
        dom::append_child(&span, &dom::create_text(&alt));
        dom::replace_with(&img, &span);
    }
    Ok(())
}

/// Loads and processes the cover image. See the module docs.
pub fn load_cover(path: &Path, ctx: &mut Ctx) -> Result<Resource> {
    let _ = &ctx;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ext = if ext == "jpeg" {
        "jpg".to_string()
    } else {
        ext
    };
    if !matches!(ext.as_str(), "png" | "jpg" | "gif") {
        bail!("unsupported cover format: {}", path.display());
    }
    let data =
        std::fs::read(path).with_context(|| format!("reading cover image {}", path.display()))?;
    Ok(Resource {
        href: format!("images/cover.{ext}"),
        media_type: media_type_for_ext(&ext).to_string(),
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;

    #[test]
    fn stub_never_leaves_unresolved_images() {
        let mut ctx = Ctx::new(Options::default());
        let doc = dom::parse(
            r#"<p><img src="a.png" alt="A"><img src="../images/x.png" data-res="1"></p>"#,
        );
        transform(&doc, &mut ctx).unwrap();
        let out = dom::serialize_xhtml(&dom::body(&doc).unwrap());
        assert!(!out.contains("a.png"));
        assert!(out.contains(">A<"));
        assert!(!out.contains("data-res"));
        assert_eq!(ctx.warnings().len(), 1);
    }

    #[test]
    fn cover_from_fixture() {
        let mut ctx = Ctx::new(Options::default());
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let r = load_cover(&dir.join("photo.jpg"), &mut ctx).unwrap();
        assert_eq!(r.media_type, "image/jpeg");
        assert!(load_cover(&dir.join("missing.png"), &mut ctx).is_err());
    }
}

//! Device profiles (owned by T1).
//!
//! | name             | screen      | SVG       | math  | alpha     |
//! |------------------|-------------|-----------|-------|-----------|
//! | `scribe`         | 1860×2480   | rasterize | image | flattened |
//! | `remarkable`     | 1404×1872   | rasterize | image | flattened |
//! | `remarkable-pro` | 1620×2160   | rasterize | image | flattened |
//! | `generic`        | 1600×2133   | keep      | image | kept      |
//!
//! Send to Kindle tends to reject SVG, MathML and transparent images, and the
//! reMarkable's SVG/CSS support is undocumented, so the device profiles are
//! conservative. `generic` targets desktop and phone readers.

use anyhow::{Result, bail};

use crate::model::{MathMode, Profile, SvgMode};

/// Canonical names accepted by [`by_name`], in help order.
pub const NAMES: &[&str] = &["scribe", "remarkable", "remarkable-pro", "generic"];

/// Maps a user-supplied name (or a common alias) to its canonical name.
fn canonical(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    Some(match lower.as_str() {
        "scribe" | "kindle-scribe" | "kindle" => "scribe",
        "remarkable" | "remarkable2" | "remarkable-2" | "rm2" | "rm" => "remarkable",
        "remarkable-pro" | "remarkable-paper-pro" | "paper-pro" | "rmpp" | "rm-pro" => {
            "remarkable-pro"
        }
        "generic" | "default" => "generic",
        _ => return None,
    })
}

/// Looks up a profile by name (ASCII case-insensitive; `_` and `-` are
/// interchangeable, and a few aliases such as `kindle-scribe` or `paper-pro`
/// are accepted). The returned profile always carries its canonical name.
pub fn by_name(name: &str) -> Result<Profile> {
    let Some(canon) = canonical(name) else {
        bail!(
            "unknown profile '{}': expected one of {}",
            name.trim(),
            NAMES.join(", ")
        );
    };
    let (w, h) = match canon {
        "scribe" => (1860, 2480),
        "remarkable" => (1404, 1872),
        "remarkable-pro" => (1620, 2160),
        _ => return Ok(generic()),
    };
    Ok(device(canon, w, h))
}

/// All profiles, in the order of [`NAMES`].
pub fn all() -> Vec<Profile> {
    NAMES
        .iter()
        .map(|n| by_name(n).expect("every canonical name resolves"))
        .collect()
}

/// The default `generic` profile: 1600×2133, SVG kept, image math, alpha kept.
pub fn generic() -> Profile {
    Profile {
        name: "generic".into(),
        screen_w: 1600,
        screen_h: 2133,
        svg: SvgMode::Keep,
        math: MathMode::Image,
        flatten_alpha: false,
    }
}

/// A conservative e-ink device profile.
fn device(name: &str, screen_w: u32, screen_h: u32) -> Profile {
    Profile {
        name: name.into(),
        screen_w,
        screen_h,
        svg: SvgMode::Rasterize,
        math: MathMode::Image,
        flatten_alpha: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scribe() {
        let p = by_name("scribe").unwrap();
        assert_eq!(p.name, "scribe");
        assert_eq!((p.screen_w, p.screen_h), (1860, 2480));
        assert_eq!(p.svg, SvgMode::Rasterize);
        assert_eq!(p.math, MathMode::Image);
        assert!(p.flatten_alpha);
    }

    #[test]
    fn remarkable() {
        let p = by_name("remarkable").unwrap();
        assert_eq!(p.name, "remarkable");
        assert_eq!((p.screen_w, p.screen_h), (1404, 1872));
        assert_eq!(p.svg, SvgMode::Rasterize);
        assert_eq!(p.math, MathMode::Image);
        assert!(p.flatten_alpha);
    }

    #[test]
    fn remarkable_pro() {
        let p = by_name("remarkable-pro").unwrap();
        assert_eq!(p.name, "remarkable-pro");
        assert_eq!((p.screen_w, p.screen_h), (1620, 2160));
        assert_eq!(p.svg, SvgMode::Rasterize);
        assert_eq!(p.math, MathMode::Image);
        assert!(p.flatten_alpha);
    }

    #[test]
    fn generic_profile() {
        let p = by_name("generic").unwrap();
        assert_eq!(p.name, "generic");
        assert_eq!((p.screen_w, p.screen_h), (1600, 2133));
        assert_eq!(p.svg, SvgMode::Keep);
        assert_eq!(p.math, MathMode::Image);
        assert!(!p.flatten_alpha);
        assert_eq!(p, generic());
        // The model's default profile is the generic one.
        assert_eq!(p, Profile::default());
    }

    #[test]
    fn names_aliases_and_case() {
        for n in NAMES {
            assert_eq!(&by_name(n).unwrap().name, n);
        }
        assert_eq!(by_name(" SCRIBE ").unwrap().name, "scribe");
        assert_eq!(by_name("Kindle-Scribe").unwrap().name, "scribe");
        assert_eq!(by_name("remarkable_pro").unwrap().name, "remarkable-pro");
        assert_eq!(by_name("paper-pro").unwrap().name, "remarkable-pro");
        assert_eq!(by_name("rm2").unwrap().name, "remarkable");
        assert_eq!(by_name("default").unwrap().name, "generic");
        let names: Vec<_> = all().into_iter().map(|p| p.name).collect();
        assert_eq!(names, NAMES);
    }

    #[test]
    fn unknown_profile_errors() {
        let err = by_name("kindle-oasis").unwrap_err().to_string();
        assert!(err.contains("kindle-oasis"), "{err}");
        assert!(err.contains("scribe, remarkable, remarkable-pro, generic"));
        assert!(by_name("").is_err());
    }
}

//! Device profiles (owned by T1).
//!
//! - `scribe` 1860×2480, `remarkable` 1404×1872, `remarkable-pro` 1620×2160:
//!   rasterized SVG, image math, flattened alpha.
//! - `generic` 1600×2133: SVG kept, image math, alpha kept.

use anyhow::{Result, bail};

use crate::model::{MathMode, Profile, SvgMode};

/// Names accepted by [`by_name`].
pub const NAMES: &[&str] = &["scribe", "remarkable", "remarkable-pro", "generic"];

/// Looks up a profile by name (ASCII case-insensitive).
pub fn by_name(name: &str) -> Result<Profile> {
    let lower = name.trim().to_ascii_lowercase();
    let (w, h) = match lower.as_str() {
        "scribe" => (1860, 2480),
        "remarkable" => (1404, 1872),
        "remarkable-pro" => (1620, 2160),
        "generic" => return Ok(generic()),
        _ => bail!(
            "unknown profile '{name}': expected one of {}",
            NAMES.join(", ")
        ),
    };
    Ok(Profile {
        name: lower,
        screen_w: w,
        screen_h: h,
        svg: SvgMode::Rasterize,
        math: MathMode::Image,
        flatten_alpha: true,
    })
}

/// The default `generic` profile.
pub fn generic() -> Profile {
    Profile::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_profiles() {
        for n in NAMES {
            assert_eq!(&by_name(n).unwrap().name, n);
        }
        assert!(by_name("kindle-oasis").is_err());
        assert_eq!(by_name("scribe").unwrap().screen_w, 1860);
    }
}

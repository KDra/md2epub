//! Note pages for handwriting (owned by T8).
//!
//! Contract for [`insert`]: for `NotesPlacement::AfterChapter` or `End`, add
//! `opts.notes_pages` note pages (after each content chapter, or at the end).
//! Each page is its own chapter (`in_toc: false`, except one "Notes" TOC
//! entry in `End` mode, `source: None`, file name from
//! `ctx.next_chapter_file()`) whose body is a single full-page PNG generated
//! at the profile resolution in the chosen `NotesStyle` (`blank` = white with
//! a faint corner mark, `lined`, `dotted`, `grid` in light gray). Each style
//! image is generated once and reused (`ctx.add_image` dedupes). Page breaks
//! are forced before and after (`.notes-page` CSS in `assets/base.css`).
//!
//! The T0 stub returns the chapters unchanged.

use anyhow::Result;

use crate::ctx::Ctx;
use crate::model::Chapter;

/// Inserts note pages into the chapter list. See the module docs.
pub fn insert(chapters: Vec<Chapter>, ctx: &mut Ctx) -> Result<Vec<Chapter>> {
    let _ = &ctx;
    Ok(chapters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Options;

    #[test]
    fn stub_keeps_chapters() {
        let mut ctx = Ctx::new(Options::default());
        let out = insert(vec![Chapter::default()], &mut ctx).unwrap();
        assert_eq!(out.len(), 1);
    }
}

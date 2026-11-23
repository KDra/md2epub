//! md2epub: convert rich Markdown or HTML into a reflowable EPUB 3 for
//! handwriting on Kindle Scribe and reMarkable.
//!
//! See `TASKS.md` for the module contracts and [`pipeline`] for the flow.

pub mod assets;
pub mod cli;
pub mod config;
pub mod ctx;
pub mod dom;
pub mod epub;
pub mod html;
pub mod linked;
pub mod markdown;
pub mod math;
pub mod mermaid;
pub mod model;
pub mod notes;
pub mod pipeline;
pub mod profile;
pub mod style;
pub mod svg;

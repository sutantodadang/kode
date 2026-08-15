//! Semantic color palette for the TUI, per `DESIGN.md` ("Color" section).
//! Color = provenance, never decoration. Background is never set — the
//! terminal's own default is respected.
//!
//! ponytail: `Color::Rgb` needs a truecolor terminal. Windows Terminal (the
//! default target here) supports it; a 256-color fallback table can be
//! added later if a non-truecolor terminal becomes a real requirement.

use ratatui::style::Color;

/// zindeks / structural knowledge.
pub const Z: Color = Color::Rgb(0x63, 0xC5, 0xDA);
/// ingat / recalled memory.
pub const I: Color = Color::Rgb(0xD7, 0xA8, 0x5B);
/// git impact.
pub const G: Color = Color::Rgb(0x8F, 0xAE, 0x8B);
/// tools.
pub const T: Color = Color::Rgb(0x8C, 0x9B, 0xAB);
/// verified / pass.
pub const OK: Color = Color::Rgb(0x74, 0xB8, 0x8A);
/// failure.
pub const ERR: Color = Color::Rgb(0xD1, 0x6D, 0x72);
/// muted text.
pub const MUTED: Color = Color::Rgb(0x7C, 0x87, 0x93);
/// dim structure (rules, spacers).
pub const DIM: Color = Color::Rgb(0x52, 0x5C, 0x66);

//! Semantic color palette for the TUI, per `DESIGN.md` ("Color" section).
//! Color = provenance, never decoration. Background is never set — the
//! terminal's own default is respected. Git is rose, not green, so green
//! (`OK`) means verified and nothing else.
//!
//! ponytail: `Color::Rgb` needs a truecolor terminal. Windows Terminal (the
//! default target here) supports it; a 256-color fallback table can be
//! added later if a non-truecolor terminal becomes a real requirement.

use ratatui::style::Color;

/// zindeks / structural knowledge.
pub const Z: Color = Color::Rgb(0x4F, 0xD1, 0xC5);
/// ingat / recalled memory.
pub const I: Color = Color::Rgb(0xF2, 0xB8, 0x4B);
/// git impact.
pub const G: Color = Color::Rgb(0xE0, 0x70, 0x9B);
/// tools.
pub const T: Color = Color::Rgb(0x8C, 0x9B, 0xAB);
/// verified / pass.
pub const OK: Color = Color::Rgb(0x8B, 0xD4, 0x50);
/// failure.
pub const ERR: Color = Color::Rgb(0xFF, 0x5F, 0x56);
/// caution / skipped / permission attention.
pub const WARN: Color = Color::Rgb(0xF2, 0xC1, 0x4E);
/// muted text.
pub const MUTED: Color = Color::Rgb(0x87, 0x91, 0x9C);
/// dim structure (rules, spacers).
pub const DIM: Color = Color::Rgb(0x41, 0x49, 0x53);

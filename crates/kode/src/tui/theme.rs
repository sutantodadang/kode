//! Semantic color palette for the TUI, per `DESIGN.md` ("Color" section).
//! Color = provenance, never decoration. Background is never set — the
//! terminal's own default is respected. Git is rose, not green, so green
//! (`OK`) means verified and nothing else.
//!
//! The constants below are the dark truecolor palette. `adapt` remaps a
//! drawn buffer once per frame to a 256-color or light-background palette
//! (see `palette_mode`), so draw call sites keep using the constants.

use ratatui::buffer::Buffer;
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

/// Which palette the frame is remapped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    DarkTruecolor,
    Dark256,
    LightTruecolor,
    Light256,
}

/// Truecolor when `COLORTERM` says `truecolor`/`24bit` or under Windows
/// Terminal (`WT_SESSION`); otherwise the 256-color palette.
pub fn palette_mode(light: bool, colorterm: Option<&str>, wt_session: bool) -> PaletteMode {
    let truecolor = wt_session
        || colorterm.is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        });
    match (light, truecolor) {
        (false, true) => PaletteMode::DarkTruecolor,
        (false, false) => PaletteMode::Dark256,
        (true, true) => PaletteMode::LightTruecolor,
        (true, false) => PaletteMode::Light256,
    }
}

/// Replacement for one palette constant; identity for any other color.
pub fn remap(color: Color, mode: PaletteMode) -> Color {
    if mode == PaletteMode::DarkTruecolor {
        return color;
    }
    // (dark-256 index, light truecolor, light-256 index)
    let (d, l, l256): (u8, (u8, u8, u8), u8) = match color {
        c if c == Z => (80, (0x0E, 0x7C, 0x86), 30),
        c if c == I => (214, (0x9A, 0x67, 0x00), 136),
        c if c == G => (168, (0xB4, 0x23, 0x6A), 162),
        c if c == T => (103, (0x57, 0x60, 0x6A), 241),
        c if c == OK => (113, (0x2F, 0x7D, 0x32), 28),
        c if c == ERR => (203, (0xC6, 0x28, 0x28), 160),
        c if c == WARN => (221, (0x8A, 0x6D, 0x00), 100),
        c if c == MUTED => (245, (0x6B, 0x72, 0x80), 243),
        c if c == DIM => (238, (0xC9, 0xCE, 0xD6), 252),
        _ => return color,
    };
    match mode {
        PaletteMode::DarkTruecolor => color,
        PaletteMode::Dark256 => Color::Indexed(d),
        PaletteMode::LightTruecolor => Color::Rgb(l.0, l.1, l.2),
        PaletteMode::Light256 => Color::Indexed(l256),
    }
}

/// Remap every cell's fg/bg to `mode`. No-op (no iteration) for dark truecolor.
pub fn adapt(buf: &mut Buffer, mode: PaletteMode) {
    if mode == PaletteMode::DarkTruecolor {
        return;
    }
    for cell in buf.content.iter_mut() {
        cell.fg = remap(cell.fg, mode);
        cell.bg = remap(cell.bg, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    const ALL: [Color; 9] = [Z, I, G, T, OK, ERR, WARN, MUTED, DIM];

    #[test]
    fn palette_mode_selection() {
        assert_eq!(
            palette_mode(false, Some("truecolor"), false),
            PaletteMode::DarkTruecolor
        );
        assert_eq!(palette_mode(false, None, true), PaletteMode::DarkTruecolor);
        assert_eq!(palette_mode(false, None, false), PaletteMode::Dark256);
        assert_eq!(
            palette_mode(true, Some("24bit"), false),
            PaletteMode::LightTruecolor
        );
        assert_eq!(palette_mode(true, None, false), PaletteMode::Light256);
    }

    #[test]
    fn remap_known_and_unknown() {
        assert_eq!(remap(Z, PaletteMode::Dark256), Color::Indexed(80));
        assert_eq!(
            remap(G, PaletteMode::LightTruecolor),
            Color::Rgb(0xB4, 0x23, 0x6A)
        );
        assert_eq!(remap(Color::Reset, PaletteMode::Light256), Color::Reset);
    }

    #[test]
    fn remapped_colors_are_distinct_in_every_mode() {
        for mode in [
            PaletteMode::DarkTruecolor,
            PaletteMode::Dark256,
            PaletteMode::LightTruecolor,
            PaletteMode::Light256,
        ] {
            let out: Vec<Color> = ALL.iter().map(|c| remap(*c, mode)).collect();
            for (i, a) in out.iter().enumerate() {
                for b in &out[i + 1..] {
                    assert_ne!(a, b, "{mode:?}");
                }
            }
        }
    }

    #[test]
    fn adapt_changes_only_palette_cells() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf.content[0].fg = Z;
        buf.content[1].fg = Color::Reset;
        adapt(&mut buf, PaletteMode::Dark256);
        assert_eq!(buf.content[0].fg, Color::Indexed(80));
        assert_eq!(buf.content[1].fg, Color::Reset);
    }
}

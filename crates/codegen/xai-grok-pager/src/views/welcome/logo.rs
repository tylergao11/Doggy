//! Doggy brand logo for the welcome screen.
//!
//! Source art: CodeDoggy `tui/doggy_brand.py` neon couple portrait (palette
//! pixel map → half-block terminal glyphs). Replaces the former Grok braille
//! symbol so the product shows Doggy branding, not SpaceXAI/Grok.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::theme::Theme;

/// Full-resolution couple art (52×60 palette chars). From CodeDoggy.
const COUPLE_ART: &str = include_str!("../../../assets/logo/doggy_couple.txt");

/// Wordmark used when the terminal is too short for the portrait.
const WORDMARK: &str = "DOGGY";

/// Minimum window height (rows) to show the cropped portrait.
const SMALL_LOGO_MIN_HEIGHT: u16 = 22;
/// Minimum window height to show the full portrait.
const FULL_LOGO_MIN_HEIGHT: u16 = 40;

/// Source rows for the "small" tier (top of portrait, half-blocked).
const SMALL_SOURCE_ROWS: usize = 28; // → 14 terminal rows

fn logo_hidden() -> bool {
    // Half-block art is ASCII-block based (▀▄█), not braille — show even on
    // legacy consoles when possible. Still hide if glyphs are known broken.
    crate::glyphs::is_legacy_windows_console()
}

fn art_rows() -> Vec<&'static str> {
    COUPLE_ART.lines().filter(|l| !l.is_empty()).collect()
}

fn palette(ch: char) -> Color {
    // Mirrors CodeDoggy `_DOGGY_ART_PALETTE`.
    match ch {
        'C' => Color::Rgb(0x00, 0xba, 0xc5),
        'M' => Color::Rgb(0xee, 0x4b, 0x8d),
        'c' => Color::Rgb(0x0b, 0x66, 0x70),
        'm' => Color::Rgb(0x8f, 0x1b, 0x58),
        'G' => Color::Rgb(0xff, 0x7a, 0x32),
        'Y' => Color::Rgb(0xd9, 0xad, 0x32),
        'T' => Color::Rgb(0xf2, 0xca, 0x55),
        'P' => Color::Rgb(0xff, 0x68, 0xad),
        'R' => Color::Rgb(0x0a, 0x0a, 0x0a),
        'F' => Color::Rgb(0xe1, 0xd2, 0xae),
        'H' => Color::Rgb(0xc9, 0xa9, 0x78),
        'D' => Color::Rgb(0x1a, 0x1a, 0x1a),
        'S' => Color::Rgb(0x75, 0x64, 0x4a),
        'W' => Color::Rgb(0xf5, 0xf5, 0xf7),
        'B' => Color::Rgb(0x12, 0x12, 0x12),
        'N' => Color::Rgb(0x3a, 0x2a, 0x22),
        'L' => Color::Rgb(0xf0, 0xe6, 0xcc),
        'K' => Color::Rgb(0x05, 0x05, 0x07),
        'E' => Color::Rgb(0x3b, 0x2a, 0x20),
        _ => Color::Reset, // '.' void — transparent
    }
}

/// One terminal row of (glyph, fg, optional bg) cells.
type TermRow = Vec<(char, Color, Option<Color>)>;

fn half_block(top: char, bottom: char) -> (char, Color, Option<Color>) {
    if top == '.' && bottom == '.' {
        return (' ', Color::Reset, None);
    }
    let tc = palette(top);
    let bc = palette(bottom);
    if top == bottom {
        return ('█', tc, None);
    }
    if top == '.' {
        return ('▄', bc, None);
    }
    if bottom == '.' {
        return ('▀', tc, None);
    }
    ('▀', tc, Some(bc))
}

fn select_source_rows(window_height: u16) -> Option<Vec<&'static str>> {
    select_source_rows_for(window_height, logo_hidden())
}

fn select_source_rows_for(window_height: u16, hidden: bool) -> Option<Vec<&'static str>> {
    if hidden || window_height < SMALL_LOGO_MIN_HEIGHT {
        return None;
    }
    let rows = art_rows();
    if window_height < FULL_LOGO_MIN_HEIGHT {
        let n = SMALL_SOURCE_ROWS.min(rows.len());
        // Keep even count for half-block pairing.
        let n = n - (n % 2);
        Some(rows.into_iter().take(n).collect())
    } else {
        let mut r = rows;
        if r.len() % 2 == 1 {
            if let Some(last) = r.last().copied() {
                r.push(last);
            }
        }
        Some(r)
    }
}

fn to_term_rows(source: &[&str]) -> Vec<TermRow> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < source.len() {
        let top = source[i].chars().collect::<Vec<_>>();
        let bottom = source[i + 1].chars().collect::<Vec<_>>();
        let w = top.len().max(bottom.len());
        let mut row = Vec::with_capacity(w);
        for col in 0..w {
            let t = top.get(col).copied().unwrap_or('.');
            let b = bottom.get(col).copied().unwrap_or('.');
            row.push(half_block(t, b));
        }
        out.push(row);
        i += 2;
    }
    out
}

fn term_width(rows: &[TermRow]) -> u16 {
    rows.iter().map(|r| r.len()).max().unwrap_or(24) as u16
}

fn wordmark_mode(window_height: u16) -> bool {
    logo_hidden() || window_height < SMALL_LOGO_MIN_HEIGHT
}

/// Animation frame for welcome redraw throttling (chain blink).
pub fn shimmer_frame() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    if logo_hidden() {
        return 0;
    }
    (START.get_or_init(Instant::now).elapsed().as_secs_f32() * 5.0) as u64
}

fn apply_chain_blink(source: &mut [String], frame: u64) {
    // Lightweight jewellery blink — same idea as CodeDoggy chain details.
    const CHAIN: &[(usize, usize)] = &[
        (17, 20),
        (18, 21),
        (19, 22),
        (23, 20),
        (22, 21),
        (21, 22),
        (20, 23),
        (36, 23),
    ];
    let phase = (frame as usize) % 12;
    for (i, &(x, y)) in CHAIN.iter().enumerate() {
        if y >= source.len() {
            continue;
        }
        let row = &mut source[y];
        if x >= row.chars().count() {
            continue;
        }
        let ch = if i == phase % CHAIN.len() { 'T' } else { 'Y' };
        let mut chars: Vec<char> = row.chars().collect();
        if x < chars.len() {
            chars[x] = ch;
            *row = chars.into_iter().collect();
        }
    }
}

fn render_portrait(area: Rect, buf: &mut Buffer, source: &[&str]) {
    let frame = shimmer_frame();
    let mut owned: Vec<String> = source.iter().map(|s| (*s).to_string()).collect();
    apply_chain_blink(&mut owned, frame);
    let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
    let term_rows = to_term_rows(&refs);
    if term_rows.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }
    let art_w = term_width(&term_rows);
    let art_h = term_rows.len() as u16;
    let x0 = area.x + area.width.saturating_sub(art_w) / 2;
    let y0 = area.y + area.height.saturating_sub(art_h) / 2;
    for (ry, row) in term_rows.iter().enumerate() {
        let y = y0 + ry as u16;
        if y >= area.y + area.height {
            break;
        }
        for (cx, (glyph, fg, bg)) in row.iter().enumerate() {
            let x = x0 + cx as u16;
            if x >= area.x + area.width {
                break;
            }
            let mut style = Style::default().fg(*fg);
            if let Some(b) = bg {
                style = style.bg(*b);
            }
            buf[(x, y)].set_char(*glyph).set_style(style);
        }
    }
}

fn render_wordmark(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let w = WORDMARK.len() as u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(1) / 2;
    let style = Style::default().fg(theme.accent_plan);
    for (i, ch) in WORDMARK.chars().enumerate() {
        let px = x + i as u16;
        if px < area.x + area.width && y < area.y + area.height {
            buf[(px, y)].set_char(ch).set_style(style);
        }
    }
}

pub fn logo_line_count(window_height: u16) -> u16 {
    if wordmark_mode(window_height) {
        return 1;
    }
    select_source_rows(window_height)
        .map(|r| (r.len() / 2) as u16)
        .unwrap_or(1)
}

pub fn logo_visual_width(window_height: u16) -> u16 {
    if wordmark_mode(window_height) {
        return WORDMARK.len() as u16;
    }
    select_source_rows(window_height)
        .and_then(|r| r.first().map(|l| l.chars().count() as u16))
        .unwrap_or(24)
}

pub fn render_logo(area: Rect, buf: &mut Buffer, theme: &Theme, window_height: u16) {
    if wordmark_mode(window_height) {
        render_wordmark(area, buf, theme);
        return;
    }
    if let Some(rows) = select_source_rows(window_height) {
        render_portrait(area, buf, &rows);
    }
}

pub fn full_logo_line_count() -> u16 {
    full_logo_line_count_for(logo_hidden())
}

fn full_logo_line_count_for(hidden: bool) -> u16 {
    if hidden {
        1 // wordmark
    } else {
        let rows = art_rows();
        let n = if rows.len() % 2 == 1 {
            rows.len() + 1
        } else {
            rows.len()
        };
        (n / 2) as u16
    }
}

pub fn full_logo_visual_width() -> u16 {
    full_logo_visual_width_for(logo_hidden())
}

fn full_logo_visual_width_for(hidden: bool) -> u16 {
    if hidden {
        WORDMARK.len() as u16
    } else {
        art_rows()
            .first()
            .map(|l| l.chars().count() as u16)
            .unwrap_or(52)
    }
}

pub fn render_full_logo(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if logo_hidden() {
        render_wordmark(area, buf, theme);
        return;
    }
    let mut rows = art_rows();
    if rows.len() % 2 == 1 {
        if let Some(last) = rows.last().copied() {
            rows.push(last);
        }
    }
    render_portrait(area, buf, &rows);
}

pub fn compact_logo_line_count() -> u16 {
    if logo_hidden() {
        1
    } else {
        (SMALL_SOURCE_ROWS / 2) as u16
    }
}

pub fn render_compact_logo(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if logo_hidden() {
        render_wordmark(area, buf, theme);
        return;
    }
    let rows = art_rows();
    let n = SMALL_SOURCE_ROWS.min(rows.len());
    let n = n - (n % 2);
    let slice: Vec<&str> = rows.into_iter().take(n).collect();
    render_portrait(area, buf, &slice);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_loads_even_dimensions() {
        let rows = art_rows();
        assert!(rows.len() >= SMALL_SOURCE_ROWS);
        assert_eq!(rows[0].chars().count(), 52);
    }

    #[test]
    fn half_block_void_is_space() {
        let (g, _, _) = half_block('.', '.');
        assert_eq!(g, ' ');
    }

    #[test]
    fn half_block_solid_is_full() {
        let (g, _, _) = half_block('M', 'M');
        assert_eq!(g, '█');
    }

    #[test]
    fn logo_sizes_by_height() {
        assert!(select_source_rows_for(SMALL_LOGO_MIN_HEIGHT - 1, false).is_none());
        let small = select_source_rows_for(SMALL_LOGO_MIN_HEIGHT, false).unwrap();
        assert_eq!(small.len(), SMALL_SOURCE_ROWS);
        let full = select_source_rows_for(FULL_LOGO_MIN_HEIGHT, false).unwrap();
        assert!(full.len() >= small.len());
    }

    #[test]
    fn logo_hidden_uses_wordmark_metrics() {
        assert_eq!(full_logo_line_count_for(true), 1);
        assert_eq!(full_logo_visual_width_for(true), 5);
    }

    #[test]
    fn full_logo_wider_than_compact() {
        if !logo_hidden() {
            assert!(full_logo_line_count() >= compact_logo_line_count());
            assert!(full_logo_visual_width() >= 40);
        }
    }

    #[test]
    fn compact_logo_line_count_positive() {
        assert!(compact_logo_line_count() > 0);
    }
}

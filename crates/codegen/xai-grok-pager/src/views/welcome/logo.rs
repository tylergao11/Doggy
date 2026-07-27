//! Doggy brand logo for the welcome screen.
//!
//! Faithful port of CodeDoggy `tui/doggy_brand.py`:
//! base 52×60 palette map + female face/mask/bow overlays, then half-block
//! glyphs. Facial detail overlays are **required** — without them the female
//! face dissolves into tan fur after half-block downsampling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::theme::Theme;

/// Full-resolution couple art (52×60 palette chars). From CodeDoggy
/// `_DOGGY_COUPLE_ART` (before animate overlays).
const COUPLE_ART: &str = include_str!("../../../assets/logo/doggy_couple.txt");

/// Wordmark used when the terminal is too short for the portrait.
const WORDMARK: &str = "DOGGY";

/// Minimum window height (rows) to show the cropped portrait.
const SMALL_LOGO_MIN_HEIGHT: u16 = 22;
/// Minimum window height to show the full portrait.
const FULL_LOGO_MIN_HEIGHT: u16 = 40;

/// Source rows for the "small" tier (top of portrait, half-blocked).
const SMALL_SOURCE_ROWS: usize = 28; // → 14 terminal rows

/// CodeDoggy `_DOGGY_COUPLE_FRAMES`.
const COUPLE_FRAMES: u64 = 12;

// --- Face detail overlays (CodeDoggy `_animate_doggy_couple`) ----------------
// High-priority facial pixels survive half-block downsampling instead of
// dissolving into surrounding tan fur.

/// `(x, y, ch)` — CodeDoggy `_DOGGY_FEMALE_EYE_DETAILS`.
const FEMALE_EYE_DETAILS: &[(usize, usize, char)] = &[
    (32, 13, 'K'),
    (33, 13, 'K'),
    (34, 13, 'K'),
    (32, 14, 'K'),
    (33, 14, 'W'),
    (34, 14, 'K'),
    (38, 13, 'K'),
    (39, 13, 'K'),
    (40, 13, 'K'),
    (38, 14, 'K'),
    (39, 14, 'W'),
    (40, 14, 'K'),
];

/// `(y, x_start, x_end)` inclusive — `_DOGGY_FEMALE_MASK_SPANS`.
const FEMALE_MASK_SPANS: &[(usize, usize, usize)] = &[
    (16, 33, 40),
    (17, 31, 42),
    (18, 31, 42),
    (19, 31, 42),
    (20, 33, 40),
];

/// `(x, y, ch)` — `_DOGGY_FEMALE_MASK_HIGHLIGHTS`.
const FEMALE_MASK_HIGHLIGHTS: &[(usize, usize, char)] = &[
    (29, 17, 'm'),
    (30, 17, 'm'),
    (43, 17, 'm'),
    (44, 17, 'm'),
    (34, 18, 'P'),
    (35, 18, 'P'),
    (36, 18, 'P'),
    (37, 18, 'P'),
    (38, 18, 'P'),
    (39, 18, 'P'),
];

/// `(y, x_start, x_end, ch)` inclusive — `_DOGGY_FEMALE_CROWN_SPANS`.
const FEMALE_CROWN_SPANS: &[(usize, usize, usize, char)] = &[
    (7, 36, 42, 'H'),
    (8, 34, 44, 'H'),
    (9, 32, 45, 'H'),
];

/// `(x, y, ch)` — `_DOGGY_FEMALE_BOW_DETAILS`.
const FEMALE_BOW_DETAILS: &[(usize, usize, char)] = &[
    (42, 7, 'M'),
    (43, 7, 'M'),
    (46, 7, 'M'),
    (47, 7, 'M'),
    (42, 8, 'M'),
    (43, 8, 'M'),
    (44, 8, 'M'),
    (45, 8, 'P'),
    (46, 8, 'M'),
    (47, 8, 'M'),
    (48, 8, 'M'),
    (43, 9, 'M'),
    (44, 9, 'M'),
    (45, 9, 'P'),
    (46, 9, 'M'),
    (47, 9, 'M'),
];

/// `(x, y)` — `_DOGGY_CHAIN_DETAILS` (gold jewellery blink).
const CHAIN_DETAILS: &[(usize, usize)] = &[
    (17, 20),
    (18, 21),
    (19, 22),
    (23, 20),
    (22, 21),
    (21, 22),
    (20, 23),
    (36, 23),
];

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

/// Animation frame for welcome redraw throttling (chain / bow blink).
/// Matches CodeDoggy: `int(time.monotonic() * 5) % 12`.
pub fn shimmer_frame() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    if logo_hidden() {
        return 0;
    }
    (START.get_or_init(Instant::now).elapsed().as_secs_f32() * 5.0) as u64
}

fn put_pixel(canvas: &mut [Vec<char>], x: usize, y: usize, value: char) {
    if y < canvas.len() && x < canvas[y].len() {
        canvas[y][x] = value;
    }
}

/// CodeDoggy `_animate_doggy_couple`: crown, bow, **eyes**, pink mask, chain.
/// Base art alone is not enough — half-block drops female face without this.
fn animate_doggy_couple(source: &[&str], frame: u64) -> Vec<String> {
    let mut canvas: Vec<Vec<char>> = source
        .iter()
        .map(|row| row.chars().collect())
        .collect();
    if canvas.is_empty() {
        return Vec::new();
    }
    let height = canvas.len();
    let width = canvas[0].len();
    let phase = (frame % COUPLE_FRAMES) as usize;

    for &(y, start, end, value) in FEMALE_CROWN_SPANS {
        if y < height {
            for x in start..=end.min(width.saturating_sub(1)) {
                put_pixel(&mut canvas, x, y, value);
            }
        }
    }

    for &(x, y, value) in FEMALE_BOW_DETAILS {
        put_pixel(&mut canvas, x, y, value);
    }

    for &(x, y, value) in FEMALE_EYE_DETAILS {
        put_pixel(&mut canvas, x, y, value);
    }

    for &(y, start, end) in FEMALE_MASK_SPANS {
        if y < height {
            for x in start..=end.min(width.saturating_sub(1)) {
                let ch = if x == start || x == end { 'm' } else { 'M' };
                put_pixel(&mut canvas, x, y, ch);
            }
        }
    }

    for &(x, y, value) in FEMALE_MASK_HIGHLIGHTS {
        put_pixel(&mut canvas, x, y, value);
    }

    for (i, &(x, y)) in CHAIN_DETAILS.iter().enumerate() {
        let ch = if i == phase % CHAIN_DETAILS.len() {
            'T'
        } else {
            'Y'
        };
        put_pixel(&mut canvas, x, y, ch);
    }

    // Bow pulse: one M pixel cycles to hot pink, same as CodeDoggy.
    let mut bow_pixels: Vec<(usize, usize)> = Vec::new();
    for y in 0..height.min(14) {
        for x in 38..width {
            if canvas[y][x] == 'M' {
                bow_pixels.push((x, y));
            }
        }
    }
    if !bow_pixels.is_empty() {
        let (x, y) = bow_pixels[(phase / 2) % bow_pixels.len()];
        put_pixel(&mut canvas, x, y, 'P');
    }

    canvas
        .into_iter()
        .map(|row| row.into_iter().collect())
        .collect()
}

fn render_portrait(area: Rect, buf: &mut Buffer, source: &[&str]) {
    let frame = shimmer_frame();
    let owned = animate_doggy_couple(source, frame);
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
    fn animate_applies_female_eye_details() {
        // Without overlays the female face is just tan fur after half-block.
        // CodeDoggy paints K/W eye pixels at (33,14) and (39,14).
        let rows = art_rows();
        let refs: Vec<&str> = rows.iter().copied().collect();
        let animated = animate_doggy_couple(&refs, 0);
        assert!(animated.len() > 14);
        let r13: Vec<char> = animated[13].chars().collect();
        let r14: Vec<char> = animated[14].chars().collect();
        assert_eq!(r13[33], 'K', "left eye lid/brow");
        assert_eq!(r14[33], 'W', "left eye highlight");
        assert_eq!(r13[39], 'K', "right eye lid/brow");
        assert_eq!(r14[39], 'W', "right eye highlight");
        // Pink mask body on row 17 interior.
        let r17: Vec<char> = animated[17].chars().collect();
        assert_eq!(r17[35], 'M', "pink mask interior");
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

//! The boot splash: a centered logo with a small looping "." / ".." /
//! "..." activity indicator beneath it, in the spirit of what Fedora and
//! Windows show while their real init runs in the background -- while
//! the rest of boot logs to serial.

use super::fbconsole;
use crate::drivers::display::{self, Framebuffer};
use crate::drivers::timer;

/// Each braille dot becomes an ART_SCALE x ART_SCALE pixel block, times
/// the console's font scale so the logo grows with the text.
const ART_SCALE: u32 = 3;
const ART_COLOR: u32 = 0x00FF_FFFF; // BGRX8888: white.
const CAPTION: &str = "Anshi";

/// Real time, via the timer -- no guessed spin count.
const ANIM_FRAME_MS: u32 = 250;
const ANIM_CYCLES: u32 = 4;

/// Fixed-width so each frame fully overwrites the last -- otherwise ".."
/// would leave a stray "." behind when it shrinks back down to ".".
const DOT_FRAMES: [&str; 3] = [".  ", ".. ", "..."];

/// The artwork, encoded as Unicode Braille Patterns (U+2800-U+28FF) -- the
/// common way to pack a bitmap into monospace-safe text: each character is
/// a 2-wide x 4-tall grid of dots. Decoded and drawn as actual pixels, not
/// run through the font.
const ART: [&str; 22] = [
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡀⢠⣪⣿⢟⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣦⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⢟",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣐⣷⡿⣵⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢁⠊",
    "⠀⠀⠀⠀⠀⠀⠀⠀⢂⢊⣾⡿⣽⣿⣿⠏⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢂",
    "⠀⠀⠀⠀⠀⠀⠀⠀⢢⣿⣿⢻⣿⣿⠃⠂⣽⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣵⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠂",
    "⠀⠀⠀⠀⠀⠀⢄⢙⣽⣿⣿⣿⣿⡁⠀⠐⣿⣿⣟⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⡀⠀⠀⠀⠀⣠⣄⡀⠀⠀⠀⢁",
    "⠀⠀⠀⠀⠀⠌⢀⢢⣿⣿⡟⣿⢗⣀⡀⡄⢽⣿⣿⣿⣿⣿⡇⣿⣿⣿⣿⣿⣿⣿⣿⡿⣿⣿⣿⣿⣿⣷⡀⠀⠀⣼⣿⣿⣿⣦⠀⢀⠂",
    "⠀⠀⠀⠀⡐⠀⡢⣿⣿⣿⡷⢽⡟⢛⣽⣇⣾⡿⣿⣿⣿⣿⢯⠹⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⣀⣰⣿⣿⣿⣿⣿⣶⠠⣸",
    "⠀⠀⠀⠀⡀⡐⡱⣿⣿⢻⡇⢲⠁⣬⡿⣿⠗⢎⠙⢿⢾⣿⣬⣷⣽⣯⣻⢿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⣿",
    "⠀⠀⠀⠀⠀⠁⢸⣿⣿⡝⢎⡑⠂⠋⡣⠹⠁⠀⠀⠀⠁⠋⠫⢁⠼⣷⣷⣶⢽⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠀⠀⠀⠀⠀⠀⠘⢽⣟⡟⢦⠁⠀⠀⡀⠀⠀⠀⠀⠀⠀⠀⢀⡟⡿⠝⣿⣿⣿⣻⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠘⠻⣤⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠑⠈⣙⣈⠛⢸⣿⣿⣿⣿⣿⡿⡿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢄⠀⠀⠀⣈⣶⣿⣿⣿⣿⣿⡿⣻⢑⣻⣿⣿⣿⣿⣿⣿⣿⣿⣿⣽⣿⣾",
    "⠀⠉⠀⠀⠐⠂⠀⠀⠤⠀⠀⠠⢀⠀⠠⢀⠀⠀⠀⠀⠀⠀⠀⢁⣴⣿⣿⣿⣿⣿⣿⣿⣿⡽⢛⢓⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷",
    "⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢰⠀⠀⠀⠀⠀⠀⠀⢠⠴⢊⢫⣾⣿⣿⣿⣿⣿⣿⡟⢋⣰⣧⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠀⠀⠀⠀⠀⠄⡀⠀⠀⠀⠀⢀⣿⣇⠀⠀⠀⠀⠀⠀⠁⠑⢤⣾⣿⣿⣿⣿⣿⣿⣿⣗⣹⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠀⠀⠀⠀⠀⠀⠊⡀⠀⠀⢀⢾⣿⣿⣦⣄⡤⠠⡤⣤⣯⡤⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⠮⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡈⠉",
    "⠀⠀⠀⠀⠀⠀⡀⢀⠀⡀⠀⢸⣿⣿⣿⣿⣷⢁⢸⢦⡕⡿⣿⣿⣿⢿⣿⠿⢻⣿⣿⡡⠁⠰⣻⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡇⠀",
    "⠀⠀⠀⠀⠀⠀⠁⠈⠊⠀⠀⣾⣯⢿⣿⣿⣿⡖⡌⢱⣎⣿⡿⠋⣁⡼⠃⠈⠀⠀⠀⠈⠁⠐⠑⢿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡀",
    "⠀⠀⠀⠀⠀⠀⡄⠈⠀⠀⢀⡿⣧⢻⣿⣿⣿⣿⣔⣴⣿⡿⠉⠉⠀⠀⠁⠀⠀⠀⠀⠀⠀⠀⠀⢨⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷",
    "⠀⠀⠀⠀⠀⠀⠀⢀⠛⣤⠠⠀⢹⣿⣿⣿⢛⣷⣿⣿⣿⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⢄⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠐⠠⢀⠀⠀⣀⠔⠀⠀⣿⡀⠀⣼⡇⡿⢡⣟⣿⣿⣿⡿⠀⠀⠀⠀⠀⠀⠀⠀⡀⠀⠀⠈⠆⠀⢠⠹⣻⣟⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿",
    "⠀⠀⠈⠀⠁⠀⣀⣠⣴⡿⠠⣾⡿⣳⣧⣿⡷⣿⣿⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⡀⢸⠀⠙⠭⣪⣿⣿⣿⣿⣿⣿⣿⣿⣿",
];

/// A Braille Pattern character's 8-dot mask: its offset from U+2800.
/// Anything else (a stray plain space, say) is a blank cell.
fn dot_mask(c: char) -> u8 {
    match c as u32 {
        cp @ 0x2800..=0x28FF => (cp - 0x2800) as u8,
        _ => 0,
    }
}

/// Bit i of a dot mask -> its (column, row) position within the glyph's
/// 2x4 dot grid, per the Unicode Braille Patterns block layout.
const DOT_DX: [u32; 8] = [0, 0, 0, 1, 1, 1, 0, 1];
const DOT_DY: [u32; 8] = [0, 1, 2, 0, 1, 2, 3, 3];

fn draw_block(fb: &Framebuffer, x: u32, y: u32, scale: u32) {
    for py in y..(y + scale).min(fb.height) {
        for px in x..(x + scale).min(fb.width) {
            unsafe { fb.pixels.add((py * fb.pitch + px) as usize).write(ART_COLOR) };
        }
    }
}

/// Draws ART centered horizontally from pixel row `start_y`; the pixel row
/// just past its bottom edge.
fn draw_art(fb: &Framebuffer, start_y: u32, scale: u32) -> u32 {
    let art_w = ART[0].chars().count() as u32 * 2 * scale;
    let start_x = fb.width.saturating_sub(art_w) / 2;
    for (row, line) in ART.iter().enumerate() {
        for (col, c) in line.chars().enumerate() {
            let mask = dot_mask(c);
            for bit in (0..8).filter(|b| mask & 1 << b != 0) {
                let x = start_x + (col as u32 * 2 + DOT_DX[bit]) * scale;
                let y = start_y + (row as u32 * 4 + DOT_DY[bit]) * scale;
                draw_block(fb, x, y, scale);
            }
        }
    }
    start_y + ART.len() as u32 * 4 * scale
}

/// Draws the logo and caption, then animates the dots for a few cycles and
/// returns with the logo still on screen; the caller clears it when the
/// real console takes over. The console must be initialized.
pub fn show(fb: &Framebuffer) {
    let (cell_w, cell_h) = fbconsole::cell_size();
    let (cols, _) = fbconsole::size().unwrap_or((0, 0));
    let scale = ART_SCALE * (cell_w / 8);

    let art_h = ART.len() as u32 * 4 * scale;
    let caption_h = cell_h * 2; // Caption line + a blank line under the art.
    let start_y = fb.height.saturating_sub(art_h + caption_h) / 2;
    let art_bottom = draw_art(fb, start_y, scale);

    let caption_row = art_bottom / cell_h + 1;
    fbconsole::draw_text_at(cols.saturating_sub(CAPTION.len() as u32) / 2, caption_row, CAPTION);
    let dots_row = caption_row + 2;
    let dots_col = cols.saturating_sub(DOT_FRAMES[2].len() as u32) / 2;
    display::flush();

    for _ in 0..ANIM_CYCLES {
        for frame in DOT_FRAMES {
            fbconsole::draw_text_at(dots_col, dots_row, frame);
            display::flush();
            timer::sleep_ms(ANIM_FRAME_MS);
        }
    }
}

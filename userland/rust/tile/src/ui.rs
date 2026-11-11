//! Laying out and drawing tile's screen.
//!
//! The same look as scarf and play (anshi_tui::chrome): the header is a
//! "tile" tab and a grey rule with the pane counter at its end; every pane
//! has a tab for a title, followed by a rule; side-by-side panes are split
//! by a thin grey line. The focused pane's tab and its cursor are blue,
//! and the header tab turns yellow while tile waits for the key after
//! Ctrl-b. What runs *in* a pane keeps its own colors (vt.rs). Three
//! panes put the third across the full bottom row.

use alloc::format;
use alloc::vec::Vec;

use anshi_tui::chrome::{self, GREY, ON_ACCENT, fg};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::pane::Pane;
use crate::vt::Attr;

/// Where pane `i` goes: its title row plus the content area below it.
pub struct Slot {
    pub title: Rect,
    pub content: Rect,
}

/// The grid for `n` panes in `area` (the whole terminal): header row
/// on top, key-hint row at the bottom, panes between, with a one-column
/// divider between side-by-side panes.
pub fn layout(area: Rect, n: usize) -> (Vec<Slot>, Vec<Rect>) {
    let grid = Rect { y: area.y + 1, height: area.height.saturating_sub(2), ..area };
    let rows = if n <= 2 { 1 } else { 2 };
    let mut slots = Vec::new();
    let mut dividers = Vec::new();

    for r in 0..rows {
        let y = grid.y + grid.height / rows * r;
        let h = if r == rows - 1 { grid.bottom() - y } else { grid.height / rows };
        // Panes in this row: 1 or 2 (a 3-pane grid's bottom row has one).
        let first = r as usize * 2;
        let in_row = if n == 1 { 1 } else { (n - first).min(2) };
        let cell_w = (grid.width.saturating_sub(in_row as u16 - 1)) / in_row as u16;
        for c in 0..in_row as u16 {
            let x = grid.x + c * (cell_w + 1);
            let w = if c == in_row as u16 - 1 { grid.right() - x } else { cell_w };
            let cell = Rect { x, y, width: w, height: h };
            slots.push(Slot {
                title: Rect { height: 1, ..cell },
                content: Rect { y: y + 1, height: h.saturating_sub(1), ..cell },
            });
            if c + 1 < in_row as u16 {
                dividers.push(Rect { x: x + w, y, width: 1, height: h });
            }
        }
    }
    (slots, dividers)
}

/// The focused pane's tab and cursor.
const ACCENT: Color = Color::Blue;
/// The header tab while a Ctrl-b command is pending.
const PREFIX: Color = Color::Yellow;

/// `prefix`: Ctrl-b was pressed and tile is waiting for the command key.
pub fn render(frame: &mut Frame, panes: &[Pane], slots: &[Slot], dividers: &[Rect], focused: usize, prefix: bool) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    // Header: the tab, a rule, and the counter at the rule's end.
    let counter = format!(" pane {} of {}", focused + 1, panes.len());
    let counter_x = area.right().saturating_sub(counter.len() as u16 + 1);
    let x = chrome::tab(buf, area.x + 1, area.y, area.right(), "tile", if prefix { PREFIX } else { ACCENT }, ON_ACCENT);
    chrome::rule(buf, x + 1, area.y, counter_x);
    if counter_x > x + 1 {
        chrome::put(buf, counter_x, area.y, area.right(), &counter, fg(Color::Gray));
    }

    for d in dividers {
        for y in d.top()..d.bottom() {
            // Joined to the title rules on the first row.
            buf.set_string(d.x, y, if y == d.top() { "┬" } else { "│" }, fg(GREY));
        }
    }
    for (i, (pane, slot)) in panes.iter().zip(slots).enumerate() {
        pane_title(buf, slot.title, i, pane, i == focused);
        pane_content(buf, slot.content, pane, i == focused);
    }

    footer(buf, area, panes.len());
}

fn pane_title(buf: &mut Buffer, area: Rect, i: usize, pane: &Pane, focused: bool) {
    let status = if pane.alive() { "" } else { " · exited" };
    let label = format!("pane {}{status}", i + 1);
    let (color, text) = if focused { (ACCENT, ON_ACCENT) } else { (GREY, Color::Gray) };
    chrome::title(buf, area, &label, color, text);
}

/// Copies the pane's VT into its cell. The focused pane also shows its
/// cursor as a blue cell -- unless the program hid it (full-screen
/// programs draw their own).
fn pane_content(buf: &mut Buffer, area: Rect, pane: &Pane, focused: bool) {
    let vt = &pane.vt;
    for y in 0..vt.rows.min(area.height) {
        for x in 0..vt.cols.min(area.width) {
            let cell = vt.cell(x, y);
            let cursor = focused && pane.alive() && vt.cursor_visible && x == vt.cx && y == vt.cy;
            let style = if cursor { chrome::on(ON_ACCENT, ACCENT) } else { style_of(cell.attr) };
            if let Some(out) = buf.cell_mut((area.x + x, area.y + y)) {
                out.set_char(cell.ch);
                out.set_style(style);
            }
        }
    }
}

/// A VT cell's attributes as a ratatui style (anshi-tui's backend turns
/// it back into the same SGR codes).
fn style_of(a: Attr) -> Style {
    let color = |i: Option<u8>| i.map_or(Color::Reset, Color::Indexed);
    let mut style = Style::new().fg(color(a.fg)).bg(color(a.bg));
    if a.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if a.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if a.rev {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// Key hints along the bottom row.
fn footer(buf: &mut Buffer, area: Rect, n: usize) {
    let focus_keys = format!("^B 1-{n}");
    let keys = [(focus_keys.as_str(), "focus"), ("^B o", "next"), ("^B ^B", "send ^B"), ("^B q", "quit")];
    chrome::key_hints(buf, Rect { y: area.bottom() - 1, height: 1, ..area }, &keys);
}

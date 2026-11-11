//! Drawing scarf.
//!
//! The look scarf, play and tile share (anshi_tui::chrome), from sh's
//! prompt: every pane title is a
//! round-ended tab followed by a grey rule, panes are split by a thin grey
//! `│`, and the status bar is a dark band with a mode chip on the left
//! and the position on the right, joined to it by arrow ends. Everything
//! that says "this has focus" -- the focused pane's tab, the status
//! chips, the cursor and the sidebar selection's accent -- is drawn in the
//! mode's color: blue in normal mode, green in insert, yellow on the
//! command line, magenta in the sidebar. Text on those colors is black,
//! never bold: the console draws bold as the bright color, which would
//! turn black into grey.
//!
//! In the sidebar, directories are bold blue and dotfiles grey. The
//! selection is a grey band with an accent bar on its left edge while the
//! sidebar has focus, and a `❯` marker otherwise. Line numbers are grey;
//! the cursor line's is bright while the editor has focus.
//!
//! The tabs, arrows and box lines are extra glyphs in the console's font
//! (anshi_tui::console_has()); tile's per-pane terminal passes them
//! through too. Ratatui diffs every frame against the last, so only
//! changed cells reach the console.

use alloc::format;
use alloc::string::String;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use anshi_tui::chrome::{self, ARROW_LEFT, ARROW_RIGHT, GREY, ON_ACCENT, fg, fill, put};

use crate::editor::{Editor, Focus, Mode};

const SIDEBAR_W: u16 = 28;

/// Where everything goes for one terminal size and sidebar state.
pub struct Layout {
    sidebar: Option<Rect>,
    divider: Option<Rect>,
    editor: Rect,
    /// Text lines that fit in the editor / entries in the sidebar.
    pub text_rows: u16,
    pub list_rows: u16,
    pub text_cols: u16,
    gutter: u16,
    status: Rect,
    message: Rect,
}

impl Layout {
    pub fn new(area: Rect, ed: &Editor) -> Layout {
        let row = |y: u16| Rect { x: area.x, y, width: area.width, height: 1 };
        let status = row(area.bottom().saturating_sub(2));
        let message = row(area.bottom().saturating_sub(1));
        let panes = Rect { height: area.height.saturating_sub(2), ..area };

        // The sidebar never crowds out the text.
        let (sidebar, divider, editor) = if ed.sidebar_visible && panes.width >= 24 {
            let w = SIDEBAR_W.min(panes.width / 3).max(8);
            let sidebar = Rect { width: w, ..panes };
            let divider = Rect { x: panes.x + w, width: 1, ..panes };
            let editor = Rect { x: panes.x + w + 1, width: panes.width - w - 1, ..panes };
            (Some(sidebar), Some(divider), editor)
        } else {
            (None, None, panes)
        };

        // Line-number gutter: right-aligned numbers, then a space.
        let digits = digits(ed.buf.lines.len()).max(3);
        let gutter = (digits + 2).min(editor.width / 2);
        // Below the title row.
        let lines = panes.height.saturating_sub(1);
        Layout {
            sidebar,
            divider,
            editor,
            text_rows: lines,
            list_rows: lines,
            text_cols: editor.width.saturating_sub(gutter),
            gutter,
            status,
            message,
        }
    }
}

fn digits(mut n: usize) -> u16 {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

pub fn render(frame: &mut Frame, ed: &Editor, lay: &Layout) {
    let accent = accent(ed);
    let buf = frame.buffer_mut();
    if let (Some(sidebar), Some(divider)) = (lay.sidebar, lay.divider) {
        draw_sidebar(buf, sidebar, lay, ed, accent);
        draw_divider(buf, divider);
    }
    draw_editor(buf, lay, ed, accent);
    draw_status(buf, lay.status, ed, accent);
    draw_message(buf, lay.message, ed, accent);
}

/// The color of everything that marks focus, by mode.
fn accent(ed: &Editor) -> Color {
    match (ed.mode, ed.focus) {
        (Mode::Insert, _) => Color::Green,
        (Mode::Command, _) => Color::Yellow,
        (Mode::Normal, Focus::Sidebar) => Color::Magenta,
        (Mode::Normal, Focus::Editor) => Color::Blue,
    }
}

/// A pane's first row: a tab holding `text`, in the accent color when
/// the pane has focus and grey otherwise, then a grey rule.
fn title(buf: &mut Buffer, area: Rect, text: &str, focused: bool, accent: Color) {
    let (color, text_color) = if focused { (accent, ON_ACCENT) } else { (GREY, Color::Gray) };
    chrome::title(buf, Rect { height: 1, ..area }, text, color, text_color);
}

/// A thin grey line, joined to the title rules on its first row.
fn draw_divider(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        let glyph = if y == area.top() { "┬" } else { "│" };
        buf.set_string(area.x, y, glyph, fg(GREY));
    }
}

/// One line of file text as the console can show it: printable ASCII
/// as-is, a tab as one space, anything else as `?`.
fn shown(b: u8) -> char {
    match b {
        b' '..=b'~' => b as char,
        b'\t' => ' ',
        _ => '?',
    }
}

/// Screen row of the `i`th text line (or sidebar entry) in a pane, below
/// its title.
fn line_y(pane: Rect, i: usize) -> u16 {
    pane.y + 1 + i as u16
}

fn draw_sidebar(buf: &mut Buffer, area: Rect, lay: &Layout, ed: &Editor, accent: Color) {
    let sb = &ed.sidebar;
    let focused = ed.focus == Focus::Sidebar;
    let mut header = sb.cwd.clone();
    if !header.ends_with('/') {
        header.push('/');
    }
    title(buf, area, &header, focused, accent);

    let right = area.right();
    for (i, entry) in sb.entries.iter().enumerate().skip(sb.offset).take(lay.list_rows as usize) {
        let y = line_y(area, i - sb.offset);
        let selected = i == sb.sel;
        let band = selected && focused;

        let mut style = if entry.name == ".." || entry.name.starts_with('.') {
            fg(GREY)
        } else if entry.is_dir {
            fg(Color::LightBlue).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        if band {
            // A grey band with an accent bar on its left edge.
            fill(buf, Rect { y, height: 1, ..area }, Style::new().bg(GREY));
            buf.set_string(area.x, y, "▌", Style::new().fg(accent).bg(GREY));
            style = style.bg(GREY);
            if style.fg == Some(GREY) {
                style = style.fg(Color::Gray); // grey on grey would vanish
            }
        } else if selected {
            put(buf, area.x, y, right, "❯", fg(Color::Gray));
        }
        let slash = if entry.is_dir { "/" } else { "" };
        put(buf, area.x + 2, y, right, &format!("{}{slash}", entry.name), style);
    }
}

fn draw_editor(buf: &mut Buffer, lay: &Layout, ed: &Editor, accent: Color) {
    let area = lay.editor;
    let focused = ed.focus == Focus::Editor;
    let name = if ed.buf.filename.is_empty() { "[no name]" } else { ed.buf.filename.as_str() };
    let dirty = if ed.buf.dirty { " ●" } else { "" };
    title(buf, area, &format!("{name}{dirty}"), focused, accent);

    let show_cursor = focused && ed.mode != Mode::Command;
    let text_x = area.x + lay.gutter;
    for screen_row in 0..lay.text_rows as usize {
        let y = line_y(area, screen_row);
        let n = ed.rowoff + screen_row;
        let Some(line) = ed.buf.lines.get(n) else {
            break; // past the end of the file: the gutter's numbers stop
        };

        // Line numbers are grey; the cursor line's is bright while the
        // editor has focus.
        let number = format!("{:>w$} ", n + 1, w = (lay.gutter - 1) as usize);
        let number_style = if focused && n == ed.cy { fg(Color::White).add_modifier(Modifier::BOLD) } else { fg(GREY) };
        buf.set_stringn(area.x, y, number, lay.gutter as usize, number_style);

        let visible: String =
            line.iter().skip(ed.coloff).take(lay.text_cols as usize).map(|&b| shown(b)).collect();
        buf.set_string(text_x, y, visible, Style::new());

        // The cursor is a cell in the accent color -- including one past
        // the end of the line (insert mode, or an empty line), so it's
        // always visible.
        if show_cursor && n == ed.cy && ed.cx >= ed.coloff {
            let col = (ed.cx - ed.coloff) as u16;
            if col < lay.text_cols {
                let under = line.get(ed.cx).map_or(' ', |&b| shown(b));
                buf.set_string(text_x + col, y, under.encode_utf8(&mut [0; 4]), Style::new().fg(ON_ACCENT).bg(accent));
            }
        }
    }
}

/// A dark band: the mode chip on the left, arrow-joined to the file name;
/// the position chip on the right.
fn draw_status(buf: &mut Buffer, area: Rect, ed: &Editor, accent: Color) {
    let band = Style::new().fg(Color::Gray).bg(GREY);
    fill(buf, area, band);
    let chip = Style::new().fg(ON_ACCENT).bg(accent);
    let (y, right) = (area.y, area.right());

    let pos = format!(" ln {}/{}  col {} ", ed.cy + 1, ed.buf.lines.len(), ed.cx + 1);
    let pos_w = pos.len() as u16 + 1; // plus its arrow end
    let pos_x = right.saturating_sub(pos_w);

    let mode = match (ed.mode, ed.focus) {
        (Mode::Insert, _) => " INSERT ",
        (Mode::Command, _) => " COMMAND ",
        (Mode::Normal, Focus::Sidebar) => " FILES ",
        (Mode::Normal, Focus::Editor) => " NORMAL ",
    };
    let mut x = put(buf, area.x, y, right, mode, chip);
    x = put(buf, x, y, right, ARROW_RIGHT, Style::new().fg(accent).bg(GREY));

    let name = if ed.buf.filename.is_empty() { "[no name]" } else { ed.buf.filename.as_str() };
    let dirty = if ed.buf.dirty { " ●" } else { "" };
    put(buf, x + 1, y, pos_x.saturating_sub(1), &format!("{name}{dirty}"), band);

    if pos_x > x + 1 {
        let px = put(buf, pos_x, y, right, ARROW_LEFT, Style::new().fg(accent).bg(GREY));
        put(buf, px, y, right, &pos, chip);
    }
}

fn draw_message(buf: &mut Buffer, area: Rect, ed: &Editor, accent: Color) {
    let right = area.right();
    if ed.mode == Mode::Command {
        let x = put(buf, area.x + 1, area.y, right, ":", fg(accent).add_modifier(Modifier::BOLD));
        let x = put(buf, x, area.y, right, &ed.cmd, Style::new());
        put(buf, x, area.y, right, " ", Style::new().bg(accent));
    } else {
        put(buf, area.x + 1, area.y, right, &ed.message, fg(Color::Gray));
    }
}

//! How the prompt looks: the choices `configure` walks through, kept in
//! `/.sh_prompt` as `key=value` lines. A missing file (or a missing or
//! unknown value) means the default for that setting.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use anshi::{CString, File};

const FILE: &str = "/.sh_prompt";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Colored text, no backgrounds.
    Lean,
    /// Segments on one grey band, colored text, thin dividers.
    Classic,
    /// Each segment its own background color.
    Rainbow,
}

/// The shape of a segment block's ends (Classic and Rainbow).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ends {
    Sharp,
    Round,
    Flat,
}

/// What fills the gap between the left and right of a two-line prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Blank,
    Dotted,
    Solid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub style: Style,
    pub two_lines: bool,
    /// `╭─`/`╰─` joining the two lines.
    pub frame: bool,
    pub fill: Fill,
    pub ends: Ends,
    /// `❯ ✔ ✘` rather than `> ok x`.
    pub glyphs: bool,
    pub show_system: bool,
    /// A blank line before each prompt.
    pub sparse: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            style: Style::Rainbow,
            two_lines: true,
            frame: true,
            fill: Fill::Dotted,
            ends: Ends::Sharp,
            glyphs: true,
            show_system: true,
            sparse: true,
        }
    }
}

impl Config {
    /// The saved configuration, and whether there was one.
    pub fn load() -> (Config, bool) {
        let mut c = Config::default();
        let Some(mut f) = CString::new(FILE).ok().and_then(|p| File::open(&p)) else {
            return (c, false);
        };
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = f.read(&mut chunk);
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let yes = v.trim() == "yes";
            match (k.trim(), v.trim()) {
                ("style", "lean") => c.style = Style::Lean,
                ("style", "classic") => c.style = Style::Classic,
                ("style", "rainbow") => c.style = Style::Rainbow,
                ("lines", "1") => c.two_lines = false,
                ("lines", "2") => c.two_lines = true,
                ("frame", _) => c.frame = yes,
                ("fill", "blank") => c.fill = Fill::Blank,
                ("fill", "dotted") => c.fill = Fill::Dotted,
                ("fill", "solid") => c.fill = Fill::Solid,
                ("ends", "sharp") => c.ends = Ends::Sharp,
                ("ends", "round") => c.ends = Ends::Round,
                ("ends", "flat") => c.ends = Ends::Flat,
                ("glyphs", _) => c.glyphs = yes,
                ("system", _) => c.show_system = yes,
                ("sparse", _) => c.sparse = yes,
                _ => {}
            }
        }
        (c, true)
    }

    /// Writes the file and syncs it to disk. False if it couldn't.
    pub fn save(&self) -> bool {
        let yn = |b: bool| if b { "yes" } else { "no" };
        let style = match self.style {
            Style::Lean => "lean",
            Style::Classic => "classic",
            Style::Rainbow => "rainbow",
        };
        let fill = match self.fill {
            Fill::Blank => "blank",
            Fill::Dotted => "dotted",
            Fill::Solid => "solid",
        };
        let ends = match self.ends {
            Ends::Sharp => "sharp",
            Ends::Round => "round",
            Ends::Flat => "flat",
        };
        let text = format!(
            "style={style}\nlines={}\nframe={}\nfill={fill}\nends={ends}\nglyphs={}\nsystem={}\nsparse={}\n",
            if self.two_lines { 2 } else { 1 },
            yn(self.frame),
            yn(self.glyphs),
            yn(self.show_system),
            yn(self.sparse),
        );
        let ok = CString::new(FILE).ok().and_then(|p| File::create(&p)).is_some_and(|mut f| f.write_all(text.as_bytes()));
        anshi::sync();
        ok
    }
}

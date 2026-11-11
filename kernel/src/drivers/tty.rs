//! Terminal settings, per process: Linux's real struct termios layout and
//! flag/index values -- reused for free future compatibility (the same
//! reasoning as the syscall numbers, see exec/syscall.rs), even though
//! only a small slice of what termios describes is honored: ICANON/ECHO in
//! c_lflag, and VMIN/VTIME in c_cc[] (see sys_read()). Everything else
//! (baud rates, IXON/ISIG flow control, ...) is stored and read back
//! faithfully by TCGETS/TCSETS but otherwise a no-op.

pub const NCCS: usize = 19;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; NCCS],
}

pub const ICANON: u32 = 0x0002;
pub const ECHO: u32 = 0x0008;
pub const VMIN: usize = 6;
pub const VTIME: usize = 5;

impl Termios {
    pub const ZERO: Termios = Termios { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; NCCS] };

    /// What a fresh process starts with: line-buffered with echo, one
    /// byte at a time once raw (VMIN=1, VTIME=0).
    pub fn default_console() -> Termios {
        let mut t = Termios { c_lflag: ICANON | ECHO, ..Termios::ZERO };
        t.c_cc[VMIN] = 1;
        t.c_cc[VTIME] = 0;
        t
    }
}

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;

/// Linux's real TIOCGWINSZ request and struct winsize, same free-compat
/// reasoning. A full-screen program (scarf) can't lay anything out
/// without knowing how big the terminal is; it's answered from the
/// framebuffer console's glyph grid. ws_xpixel/ws_ypixel are reported as
/// 0, which is what real terminals that don't know their pixel size do.
pub const TIOCGWINSZ: u64 = 0x5413;
/// TIOCSWINSZ sets the calling process's own window size (kept in its
/// task, inherited by fork() and kept across exec()), which TIOCGWINSZ
/// then reports instead of the physical console's. On Linux this lives on
/// the pty; Anshi has no ptys, so it lives on the process. It's how tile
/// tells each pane's programs how big their pane is.
pub const TIOCSWINSZ: u64 = 0x5414;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

impl Winsize {
    pub const ZERO: Winsize = Winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
}

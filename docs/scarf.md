# scarf

A small modal text editor with vim keybindings and a file-explorer
sidebar, written in Rust with [ratatui](https://ratatui.rs)
(`userland/rust/scarf/`). It was the first Anshi program to draw a full
screen rather than scroll log lines past; it started as C
(`userland/scarf.c`) and was ported to Rust with identical behavior.

```
> scarf                  # sidebar on the current directory
> scarf notes.txt        # open a file
> scarf .                # sidebar on the shell's directory
```

A directory argument opens the sidebar there; a file argument opens
straight into the editor. With no argument it starts in whatever
directory the shell was in, since a launched process inherits the
shell's cwd.

## Screen

A file sidebar on the left and the file on the right, split by a thin
grey line; a status bar and the message/command line along the bottom.
It uses the same palette and shapes as [sh](sh.md)'s prompt, shared
with play and tile (`anshi_tui::chrome`):

- each pane's title is a round-ended tab, followed by a grey rule;
- the status bar is a dark band with the mode chip (`NORMAL`, `INSERT`,
  `COMMAND`, `FILES`) on the left, arrow-joined to the file name, and
  the cursor position in a chip on the right;
- line numbers are grey, and the cursor line's is bright;
- in the sidebar, directories are bold blue and dotfiles grey;
- an unsaved file shows a `●` in its tab and in the status bar.

**Everything that marks focus is in the mode's color**: blue in normal
mode, green in insert mode, yellow on the command line, magenta in the
sidebar. That covers the focused pane's tab, both status chips, the
cursor, and the accent bar on the sidebar's selection. While the sidebar
has focus, its selection is a grey band with that accent bar down its
left edge; otherwise the selection is marked with `❯`. Text on those
colors is black and never bold, because the console draws bold as the
bright color, which would turn black into grey.

The tabs, arrows and lines are extra glyphs in the console's font (see
[architecture.md](architecture.md#console)). tile's per-pane terminal
passes them and the colors through, so scarf looks the same in a pane.

Bytes outside printable ASCII show as `?`, and a tab shows as one space;
the console can't display anything else.

## Keys

### Sidebar

| Key | Action |
|---|---|
| `Ctrl-b` | show/hide the sidebar (the key VS Code uses) |
| `Ctrl-e` | move focus between sidebar and editor |
| `j` / `k` | move the selection |
| `Enter` / `l` | open a file, or descend into a directory |
| `h` / `-` | go up to the parent directory |
| `g` / `G` | jump to first/last entry |
| `r` | re-read the listing |
| `Esc` | back to the editor |

Directories are shown with a `>` marker and a trailing `/`. When you are
not at the root, `..` is always the first entry.

### Normal mode

| Key | Action |
|---|---|
| `h` `j` `k` `l` | move by character/line |
| `w` / `b` | forward/back a word |
| `0` / `$` | start/end of line |
| `gg` / `G` | first/last line |
| `i` | insert before the cursor |
| `a` / `A` | insert after the cursor / at end of line |
| `o` / `O` | open a line below/above |
| `x` | delete the character under the cursor |
| `dd` / `dw` | delete line / delete word |
| `:` | command line |

### Insert mode

`Esc` returns to normal mode, leaving the cursor on the last inserted
character the way vim does. `Enter` splits the line, `Backspace` at
column 0 joins with the previous line, and `Tab` inserts four spaces.

### Command line

| Command | Action |
|---|---|
| `:w` / `:w <path>` | save / save as |
| `:q` / `:q!` | quit / quit discarding changes |
| `:wq` / `:x` | save and quit |
| `:e <path>` / `:e! <path>` | open a file / open discarding changes |

## Design notes

**Structure.** scarf is a member of the `userland/rust/` Cargo workspace
(see [rust.md](rust.md) for how Rust programs are built and linked). It's a `no_std` staticlib split into:

- `buffer.rs`: the text (lines of bytes), loading and saving;
- `sidebar.rs`: one directory's listing and the selection;
- `editor.rs`: modes, motions, edits and the `:` command line;
- `ui.rs`: drawing.

It shares the `anshi` runtime (syscall wrappers: files, `chdir`/`getcwd`,
directory listing, raw mode) and `anshi-tui` (the ratatui console
backend) with `play` and `tile`.

**The cursor is drawn as a colored cell** rather than the terminal's own
cursor, because that renders identically on the framebuffer console and
over serial. `anshi_tui::init()` hides the real cursor, and `restore()`
brings back normal video, a cleared screen and a visible cursor on exit.
Otherwise the shell prompt would inherit scarf's colors and an
invisible cursor.

**Ratatui does the redraw bookkeeping.** The C version positioned every
line explicitly, cleared with `ESC[K`, and repainted the sidebar only when
a `sidebar_dirty` flag said so, all to keep the bytes per keystroke down.
Ratatui diffs each frame against the previous one and sends only the
cells that changed, so none of that is needed. The backend
(`userland/rust/tui/src/backend.rs`) still positions every run of cells
explicitly, and still reports one row fewer than the console has, because
writing the bottom-right cell would scroll the screen.

**Directory detection uses `chdir()`, not `opendir()`.** `opendir()` is
just `open(path, O_RDONLY)`, which succeeds on a regular file too, so it
cannot tell the two apart. `chdir()` rejects anything that is not a
directory and canonicalises as a side effect, which is why `scarf .`
shows `/docs` in the header rather than the literal `/docs/.`.

**Saving re-reads the sidebar,** so a file created with `:w` appears in
the listing straight away.

**Window splits were tried and removed** in the C version. Tiled panes
couldn't use `ESC[K` to clear a line (it would erase the pane beside it),
so every cell had to be padded, roughly 20 KB of output per keystroke.
With ratatui's diffing that particular cost is gone, so splits would be
cheaper to revisit now.

## Performance

**Bytes per keystroke:** handled by ratatui's diffing (see above). Typing
a character sends that cell, the cursor cell and the status bar's
position, not the screen.

**Framebuffer flush (not addressed).** virtio-gpu's `flush()` transfers
the *entire* framebuffer -- 4 MB at 1280x800 -- with two synchronous
virtqueue round trips, on every redraw, no matter how little changed.
Under TCG this dominates. The fix is a dirty-rectangle flush: track the
changed region in `fbconsole.rs` (it already does, for the GOP path)
and pass those bounds in the TRANSFER_TO_HOST_2D and RESOURCE_FLUSH
rectangles instead of the whole screen.

## What it needed from the kernel

scarf is the reason several kernel capabilities exist. All of them
landed alongside it:

| Capability | Why |
|---|---|
| `TIOCGWINSZ` | nothing could ask how big the terminal was |
| `O_TRUNC` | writes only ever *grew* a file, so saving a shortened buffer left the old tail behind |
| ANSI/CSI parser in `fbconsole.rs` | cursor addressing had nowhere to land -- escapes were drawn as literal glyphs |
| Escape in the virtio keymap | no way to leave insert mode in a graphical window |
| Shifted symbol row in the keymap | `:` was unreachable, so `:w` and `:q` could not be typed |
| Ctrl tracking in the keymap | `Ctrl-b`/`Ctrl-e` are control bytes a serial terminal sends itself, but the keymap had no concept of Ctrl |
| `argv` | no way to pass a path to a program |
| cwd inheritance | `.` always resolved to `/`, whatever directory you launched from |
| `getcwd` | the sidebar header needs a canonical path to show |

## Not supported

Visual mode, registers/yank/put, undo, and search. Deliberately, to keep
the thing reviewable.

Arrow keys, Home/End and Delete arrive as escape sequences (`ESC [ A`,
...). In normal mode and the sidebar they act as the matching vim key
(`k`/`j`/`h`/`l`, `0`/`$`, `x`); in insert mode they move the cursor
without leaving insert mode. A lone Esc is still Esc: scarf checks
whether more bytes follow right behind it.

# sh

The Anshi shell (`userland/rust/sh/`). The kernel boots straight into
it, and [tile](tile.md) runs one in every pane. If it exits, the
kernel-resident shell (`kernel/src/shell.rs`) takes over; typing
`sh` there starts it again.

```
╭─ Anshi  /code/project ··························  2.3s  ✔
╰─❯ ls /b_
```

## The prompt

The prompt is built from *segments*: the system name, the working
directory, how long the last command took (only when it took half a
second or more), and its status (`✔`, or `✘` and the exit code). How
they're drawn is up to you: run `configure`.

### `configure`

A step-by-step wizard. Each step asks one thing and shows every answer
as a live preview: the real prompt drawn in a short sample session, with
that answer applied on top of the ones already chosen. A digit picks an
answer, `r` starts over, and `q` leaves without changing anything.
Questions that don't apply are skipped: the frame and connection only
exist on two lines, and segment ends only with backgrounds. At the end
it shows the result once more and saves it on `y`.

| Step | Choices |
|---|---|
| Style | **Lean**: colored text. **Classic**: one grey band, thin dividers. **Rainbow**: each segment its own color |
| Lines | **One**: segments, then `❯` and the command. **Two**: segments on the first line (status and duration at its far end), the command after `❯` on the second |
| Frame (two lines) | none, or `╭─`/`╰─` joining the lines on the left |
| Connection (two lines) | what fills the first line's gap: nothing, dots `·`, or a solid `─` |
| Segment ends (Classic, Rainbow) | sharp arrows, round caps, or flat |
| Symbols | glyphs (`❯ ✔ ✘`) or plain text (`> ok x`) |
| System name | shown or not |
| Spacing | compact, or sparse (a blank line before each prompt) |

The answers are kept in `/.sh_prompt` as `key=value` lines, synced to
disk, and read by every new shell, including the ones in tile panes. The
default (with no file) is Rainbow, two lines, framed, dotted, sharp,
glyphs, system name shown, sparse.

On a narrow terminal (a tile pane) the prompt fits itself: the path
shortens from the left first (`…/project`), then the system name goes,
then the right-hand segments. On one line it keeps at least 24 columns
for the command.

The arrows, rounds, frame lines and icons aren't ASCII. They're extra
glyphs in the console's font (see
[architecture.md](architecture.md#console)), sent as UTF-8.

## Typing

**Highlighting.** The line is colored as it's typed:

- the command word **blue** if it exists (a builtin, or a program here
  or in `/bin`) and **red** if not, so a typo shows before Enter;
- arguments naming something that exists **cyan**;
- `-options` **yellow**.

**Suggestions.** The rest of the most recent matching command from
history appears after the cursor in grey. Only commands that would still
run are suggested: a past typo isn't. With no history match, the only
possible completion of the current word is suggested instead. Right,
End or Ctrl-F at the end of the line accepts it.

**Tab completion.** The first word completes to commands: builtins, and
programs in the current directory and `/bin` (files starting with the ELF
magic). Any other word completes to paths. One candidate is filled in,
with a `/` for a directory and a space otherwise. Several are filled in as
far as they agree; if that's no further, they're listed under the line in
columns with a short description:

```
> c_
cat      print files               cd       change directory (no dir: /)
clear    clear the screen          copy     same as cp
cp       copy a file or directory  crash    program
create   make a directory or empty file
```

Tab again cycles a highlighted selection through the list, filling each
one in; Up/Down move it a row; Enter keeps the choice; any other key
carries on editing with it. Lists longer than 8 rows scroll. Names
starting with `.` are only offered once the word starts with `.`.

**History.** Up/Down step through past commands that start with whatever
was typed before the first Up (all of them, if nothing was). History is
kept in `/.sh_history`, most recent last, without duplicates (a repeated
command moves to the end), at most 500 entries.

| Key | Action |
|---|---|
| Tab | complete; again to cycle the list |
| Right, End, Ctrl-F | at the end of the line: accept the suggestion |
| Up / Down, Ctrl-P / Ctrl-N | history, or move in the completion list |
| Left / Right, Ctrl-B / Ctrl-F | move by a character |
| Home / End, Ctrl-A / Ctrl-E | start / end of line |
| Backspace, Delete, Ctrl-D | delete before / at the cursor |
| Ctrl-W, Ctrl-U, Ctrl-K | delete a word back / to the start / to the end |
| Ctrl-C | abandon the line |
| Ctrl-L | clear the screen |
| Esc | close the completion list |

## Commands

| Command | Does |
|---|---|
| `cd [dir]` | change directory (no argument: `/`) |
| `pwd` | print the working directory |
| `ls [dir]` | list in columns: directories blue with `/`, programs in `/bin` green, dotfiles hidden |
| `cat <file>...` | print files |
| `echo [text]` | print text |
| `write <file> <text>` | replace a file's contents with a line of text |
| `mkdir <dir>...`, `touch <file>...` | create directories / empty files |
| `rm <path>...` | remove files, or directories with everything in them |
| `cp <src> <dest>`, `mv <src> <dest>` | copy / move; a `dest` that's a directory means into it |
| `create`, `delete`, `copy`, `move` | the kernel shell's spellings (`create dir x`, `delete file x`, ...) |
| `sync` | write the filesystem to disk now |
| `configure` | choose how the prompt looks (above) |
| `history`, `clear`, `help`, `exit` | |

Anything else runs a program: a word containing `/` as a path, otherwise
from the current directory, then `/bin`. Words are separated by spaces;
`"..."` or `'...'` quote a word containing spaces.

**Persistence.** The filesystem lives in memory and is written to disk
only by a sync. Every builtin that changes it (`mkdir`, `touch`, `rm`,
`cp`, `mv`, `write`, ...) syncs afterwards, as the kernel shell does.
Files a *program* writes (scarf's `:w`) need a `sync` to survive a
reboot. So does the history file.

`rm` refuses to remove a directory that any shell is currently inside,
not just this one: another tile pane's shell would otherwise be left
inside a directory that no longer exists.

## How it's built

A `no_std` Rust staticlib in the `userland/rust/` workspace (see
[rust.md](rust.md) for how those are built and linked),
using the `anshi` runtime. It draws with plain escape codes rather than
ratatui, because a shell scrolls like any other terminal program rather
than owning the screen.

- `term.rs`: key decoding, including the escape sequences for arrows,
  Home/End and Delete, and the colors.
- `config.rs`: the prompt settings and `/.sh_prompt`.
- `prompt.rs`: drawing the prompt from them, fitted to the width.
- `configure.rs`: the `configure` wizard.
- `editor.rs`: the line editor. On every key it redraws the input line
  in full (`\r`, the line, `ESC[J` to clear any old completion list
  below, the list if open, then the cursor moved back). The console
  draws no cursor, so the editor draws one: the character under it in
  reverse video. A line longer than the terminal scrolls sideways rather
  than wrapping, and nothing is written into the last column, because
  the console wraps the moment a glyph lands there.
- `highlight.rs`, `complete.rs`, `history.rs`: as above.
- `commands.rs`: builtins, quoting, running programs.

The line editor puts the terminal in raw mode only while reading a line.
Programs run with the normal (cooked) terminal, and the colors reset
before and after them.

### What it needed from the kernel

| Capability | Why |
|---|---|
| `unlink` (87), `rename` (82), `copy` (905), `sync` (162) | the file commands; the same filesystem operations as the kernel shell's |
| `clock_gettime` (228, monotonic only) | the prompt's command duration |
| 16 colors, bold and dim in `fbconsole.rs` | the prompt, highlighting, grey suggestions |
| UTF-8 and extra glyphs in `fbconsole.rs` | the prompt's arrows, rounds, frame and icons |
| escape sequences for arrows, Home/End, Delete from the virtio keyboard | cursor movement and history |
| boot into `/bin/sh` | it's the shell you land in |

See [syscalls.md](syscalls.md) for the syscalls.

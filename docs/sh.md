# sh

The Anshi shell (`userland/rust/sh/`). The kernel boots straight into
it, and [tile](tile.md) runs one in every pane. If it exits, the
kernel-resident shell (`kernel/src/shell.rs`) takes over; typing
`sh` there starts it again.

It's also a scripting language, a POSIX-shaped subset: pipes,
redirections, variables, `$(...)`, `if`/`for`/`while`/`case`, functions
and script files -- see [The language](#the-language).

```
sh                      interactive, on a terminal
sh script.sh [args]     run a script ($0 is its name, $1... the args)
sh -c 'cmds' [name ...] run text ($0 is name, then $1...)
... | sh                run what comes in on stdin
```

`-e` and `-x` can come first, as with `set`. A file starting `#!` runs
as a script when named as a command (`./build.sh`), whatever
interpreter the line names -- `sh` is the only one.

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

- a word in command position (first, or after `|`, `;`, `&&`, `then`,
  `do`, ...) **blue** if it exists (a builtin, keyword, function, or a
  program here or in `/bin`) and **red** if not, so a typo shows before
  Enter;
- arguments naming something that exists **cyan**;
- `-options` **yellow**.

**Suggestions.** The rest of the most recent matching command from
history appears after the cursor in grey. Only commands that would still
run are suggested: a past typo isn't. With no history match, the only
possible completion of the current word is suggested instead. Right,
End or Ctrl-F at the end of the line accepts it.

**Tab completion.** A word in command position completes to commands:
builtins, and programs in the current directory and `/bin` (ELF files and
`#!` scripts). Any other word completes to paths. One candidate is filled in,
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

**Continuation.** A line that leaves something open -- a quote, an
`if` without `fi`, a trailing `|` or `&&`, a heredoc -- gets another
line after a grey `> `, until it's complete. History keeps the whole
thing as one line (`for i in 1 2; do echo $i; done`).

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
| Ctrl-C | abandon the line (and stop a loop that's been running a second or more) |
| Ctrl-L | clear the screen |
| Esc | close the completion list |

## The language

What it reads, roughly in POSIX's terms. What's missing is at the end.

**Commands and lists.** `a; b` runs both, `a && b` runs b if a
succeeded, `a || b` if it failed, `! a` inverts the status. A newline is
a `;`. `# comment` to the end of the line. `{ a; b; }` groups, `( a; b )`
runs in a forked copy of the shell (a `cd` or variable there stays
there).

**Pipes.** `a | b | c` runs every stage at once, each one's stdout
into the next one's stdin; the status is the last one's. Builtins work
in pipelines like programs (`ls | grep txt`), each stage in its own
process -- so `... | while read x; do ...; done` sets `x` in that
process only, as in bash.

**Redirections**, on any command (builtin, program, function, loop):

| | |
|---|---|
| `> file`, `>> file` | stdout to a file, replacing or appending |
| `< file` | stdin from a file |
| `2> file`, `2>> file` | stderr to a file (any fd number works) |
| `2>&1`, `>&2` | an fd to where another one goes; `n>&-` closes n |
| `&> file` | stdout and stderr both |
| `<<EOF` ... `EOF` | a heredoc: the lines up to `EOF` as stdin, with `$x` and `$(...)` expanded; `<<'EOF'` takes them literally, `<<-` strips leading tabs |

**Variables.** `name=value` sets one (no spaces around `=`); `$name` or
`${name}` uses it. `NAME=value cmd` passes it to that one program only.
`export name` passes it to every program from then on -- programs get a
real environment (`anshi::env()`). Special ones: `$?` (last status),
`$$`, `$#`, `$0`, `$1`..`$9` and `${10}`, `"$@"` (each argument as its
own word), `$*`. `HOME` and `PATH` are set if missing.

| | |
|---|---|
| `${x:-word}` | x, or word if x is unset or empty (`-` without `:`: only if unset) |
| `${x:=word}` | the same, also setting x |
| `${x:+word}` | word if x is set and not empty |
| `${x:?msg}` | x, or stop with msg if it's unset or empty |
| `${#x}` | the length of x |
| `${x#pat}`, `${x##pat}` | x without the shortest / longest prefix matching pat |
| `${x%pat}`, `${x%%pat}` | x without the shortest / longest suffix: `${f%.txt}` |

**Substitution.** `$(cmd)` (or `` `cmd` ``) is what cmd prints, without
trailing newlines. `$((expr))` is integer arithmetic: `+ - * / %`,
comparisons, `&& || !`, bit operators, parentheses, and variables by
name (`$((i + 1))`). `~` at the start of a word is `$HOME`.

**Quoting and words.** `'...'` is literal; `"..."` keeps spaces but
expands `$` and `` ` ``; `\x` is a literal x. The results of unquoted
`$x` and `$(...)` are split at spaces, tabs and newlines into separate
words, and words with an unquoted `*`, `?` or `[...]` are replaced by
the matching paths, sorted (`*.txt`, `/bin/p*`; left as they are if
nothing matches; names starting with `.` only for patterns starting
with one).

**Control flow.**

```sh
if test -d /bin; then echo yes; elif [ "$x" = y ]; then echo y; else echo no; fi
for f in *.txt; do echo "$f"; done          # without `in ...`: over "$@"
while [ $i -lt 10 ]; do i=$((i + 1)); done  # until ...; do ...; done too
case "$file" in
    *.md | *.txt) echo text ;;
    *) echo other ;;
esac
greet() { local who=${1:-world}; echo "hello $who"; return 0; }
```

`break`/`continue` take a count for nested loops. Functions get their
own `$1`..., `$#` and `"$@"`; `local` variables go back to what they
were when the function returns.

**Options.** `set -e` stops the script when a command fails (not in an
`if`/`while` condition or before `&&`/`||`); `set -x` prints each
command to stderr as it runs (`+ cmd args`). `set -- a b` sets `$1`...

**Not supported:** background jobs (`&`, `wait`, job control) -- there
is no way to wait for one later; signals and `trap` (Ctrl-C only stops a
loop or `sleep`); `$(( ))` assignment operators (`i=$((i+1))` works,
`((i++))` doesn't); arrays, `[[ ]]`, `${x/a/b}`, `${x:1:2}`, brace
expansion `{a,b}`, process substitution, `select`, aliases. `PATH` is
not searched: commands come from the current directory, then `/bin`.

## Commands

Run `help` for the full list. File commands:

| Command | Does |
|---|---|
| `cd [dir]` | change directory (no argument: `$HOME`, `/`) |
| `pwd` | print the working directory |
| `ls [dir]` | list in columns: directories blue with `/`, programs in `/bin` green, dotfiles hidden; one name per line into a pipe or file |
| `cat [file]...` | print files; with none (or `-`), stdin -- Ctrl-D ends it on the console |
| `echo [-n] [-e] [text]` | print text; `-n` no newline, `-e` understands `\n`, `\t`, `\e` ... |
| `write <file> <text>` | replace a file's contents with a line of text |
| `mkdir <dir>...`, `touch <file>...` | create directories / empty files |
| `rm <path>...` | remove files, or directories with everything in them |
| `cp <src> <dest>`, `mv <src> <dest>` | copy / move; a `dest` that's a directory means into it |
| `create`, `delete`, `copy`, `move` | the kernel shell's spellings (`create dir x`, `delete file x`, ...) |
| `sync` | write the filesystem to disk now |
| `configure` | choose how the prompt looks (above) |
| `history`, `clear`, `help`, `exit [n]` | |

Text tools, so pipelines have something to feed. Each reads its files,
or stdin:

| Command | Does |
|---|---|
| `grep [-ivcnqh] <text> [file]...` | lines containing text -- a fixed string, not a regex; matches red on a terminal |
| `head [-n N] [file]`, `tail [-n N] [file]` | the first / last N lines (10); `-5` works too |
| `wc [-lwc] [file]...` | lines, words, bytes |
| `sort [-rnu] [file]...` | sort lines: reversed, numerically, without repeats |
| `seq [first [step]] last` | numbers, one per line |
| `printf <format> [arg]...` | `%s %d %i %u %x %X %o %c %b %%` with `-`/`0`, a width and a precision; repeats while arguments remain |
| `basename <path> [suffix]`, `dirname <path>` | |

Scripting: `test`/`[` (`-e -f -d -s -x -n -z`, `= != < >`, `-eq -ne -lt
-le -gt -ge`, `! -a -o`), `true`, `false`, `:`, `read [-r] [-p prompt]
var...` (a line from stdin, split into the variables), `export`,
`unset [-f]`, `local`, `set`, `shift [n]`, `source`/`.` (a script in this
shell), `eval`, `exec`, `return`, `break`, `continue`, `type`, `command
[-v]`, `env`, `sleep <seconds>` (fractions work).

Anything else runs a program: a word containing `/` as a path, otherwise
from the current directory, then `/bin`.

**Persistence.** The filesystem lives in memory and is written to disk
only by a sync. Every builtin that changes it (`mkdir`, `touch`, `rm`,
`cp`, `mv`, `write`, ...) syncs afterwards, as the kernel shell does.
Files a *program* writes, or a redirection does (`curl -o f`, `echo x >
f`, scarf's `:w`), need a `sync` to survive a reboot. So does the
history file.

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
- `syntax.rs`: the parser, text to a tree of commands. It reports
  "incomplete" separately from a syntax error, which is what the
  continuation lines run on.
- `expand.rs`: parameters, command substitution, arithmetic, field
  splitting, globbing, and the pattern matcher `case` and `${x#pat}` share.
- `shell.rs`: running the tree: variables, functions, control flow,
  redirections, pipelines.
- `commands.rs`: the builtin table and the file commands; `builtins.rs`
  the scripting ones, `tools.rs` the text ones.

Every pipeline stage and program runs in a `fork()`ed child, which
points fd 0/1/2 at the pipe or file with `dup2()` and then `execve()`s
(a stage that's a program doesn't fork twice). A builtin with a
redirection runs in the shell itself, its fds saved with `dup()` and put
back afterwards. `$(...)` is a child whose stdout is a pipe the shell
reads to the end. Pipes never block in this kernel, so whatever waits on
one -- `cat` reading, a stage writing into a full pipe -- yields until
it can go on.

The line editor puts the terminal in raw mode only while reading a line.
Programs run with the normal (cooked) terminal, and the colors reset
before and after them.

### What it needed from the kernel

| Capability | Why |
|---|---|
| `unlink` (87), `rename` (82), `copy` (905), `sync` (162) | the file commands; the same filesystem operations as the kernel shell's |
| `pipe` (22), `dup` (32), `dup2` (33), fd 0-2 as ordinary fds (M33) | pipelines and redirections |
| `execve` with `envp`, larger argument limits (M33) | `export`; long command lines |
| pipe reads: -11 while empty, 0 at EOF; console Ctrl-D = EOF (M33) | telling "nothing yet" from "the end" for `cat`, `read`, `$(...)` |
| `TCGETS` failing on a non-console fd (M33) | `isatty()`: interactive or not, `ls` columns or lines |
| `clock_gettime` (228, monotonic only) | the prompt's command duration |
| 16 colors, bold and dim in `fbconsole.rs` | the prompt, highlighting, grey suggestions |
| UTF-8 and extra glyphs in `fbconsole.rs` | the prompt's arrows, rounds, frame and icons |
| escape sequences for arrows, Home/End, Delete from the virtio keyboard | cursor movement and history |
| boot into `/bin/sh` | it's the shell you land in |

See [syscalls.md](syscalls.md) for the syscalls.

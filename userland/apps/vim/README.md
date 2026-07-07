# `rustos-vim`

The RustOS `vim`: the modal text editor, drawn with the OS curses library
(`lib/curses`). It implements the vim *core* — the modal command set
virtually every vim user exercises daily — with everything beyond that core
deliberately staged, feature by feature, in `plans/VIM.md`.

Stability tier: **experimental**.

## What it implements

- **Modes**: normal, insert, replace (`R`), visual (`v`) and visual-line
  (`V`), and the command line (`:`, `/`, `?`), with `Esc` (and `Ctrl-C`)
  returning to normal mode.
- **Motions**: `h j k l` + arrows, `w W b B e E`, `0 ^ $`, `f F t T` with
  `;`/`,` repeats, `gg G`, `{ }`, `%`, `H M L`, Enter, PageUp/PageDown —
  all count-aware, and the same code path bounds operators, so motion and
  operator can never disagree.
- **Operators**: `d c y` over motions and text objects (`iw aw`,
  `i(`/`a(` and the other bracket pairs, `i"`/`a"`/`i'`/`` i` ``),
  doubled forms `dd cc yy`, and the shorthands `x X s S D C Y r ~ J`.
- **Registers**: unnamed plus `"a`–`"z` (capitals append), linewise and
  charwise puts with `p`/`P`.
- **Undo/dot**: grouped, unlimited undo/redo (`u`, `Ctrl-R`) whose memory
  is proportional to the lines a change touched, and `.` replaying the
  last change including its insert session.
- **Search**: `/` `?` `n` `N` `*` over a bounded vim-subset pattern engine
  (literals, `.`, `*`, `^`, `$`, `[...]`, `\<` `\>`), with wrap-around,
  match highlighting, and `:noh`.
- **Ex core**: `:w[!] [file]`, `:q[!]`, `:wq`/`:x`, `:e[!]`, `:enew`,
  `:r`, `:n`/`:prev`, `:set nu`/`:set nonu`, line addresses and ranges
  (`:12`, `:$`, `:.+2`, `:%`, `:1,5`), `:[range]d`, and
  `:[range]s/pat/rep/[g]` with `&` in the replacement.
- **Startup**: `vim [-R] [+num | + | +/pattern] [--] [file ...]`, with the
  reserved `-h`/`-?` short help served from the bundle's own `Help/` tree
  through the shared `lib/help` engine.

## Architecture

The editor is a fully host-testable, I/O-free state machine (`Editor`)
between three seams:

- `FileIo` — the named-file channel `:w`/`:e`/`:r` use. The `Run` binary
  implements it over the kernel-authorised `fs_*` syscalls (every
  per-inode and mount check stays kernel-side); the tests implement it
  over an in-memory map.
- `Tty` (from `lib/curses`) — the terminal byte channel. The program
  binds only its inherited standard streams (fd 0/1), never a console
  device, so the same binary drives a serial line, a framebuffer console,
  or a windowed terminal unchanged.
- `render` — one function drawing the whole state into a curses `Window`
  (text, tab expansion, number gutter, visual/search highlights, status
  line, message/command line), which also owns view scrolling.

The buffer records span-based inverse edits into grouped undo steps; the
pattern engine is a compiled node list under a fixed backtracking budget
that fails closed on pathological patterns; the session loop blocks in the
kernel for every keystroke — there is no polling.

The modal editor needs two input events the OS input layer did not carry
before this crate: the bare Escape key and control-chorded letters. Both
were added in place to `lib/vt`/`lib/curses` (`Event::Esc`, delivered when
a read ends on a dangling `ESC`, and `Event::Ctrl`), shared by every
curses consumer.

## Layering & capabilities

`no_std` (with `alloc`), `#![forbid(unsafe_code)]`. It links only `lib/*`
crates — `rustos-abi`, `rustos-curses`, `rustos-termcap`, `rustos-vt`,
`rustos-help`, plus `rustos-rt` for the freestanding `Run` binary — never
a kernel or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE` (the full-screen display on fd 1), `CAP_CONSOLE_READ`
(raw-mode keystrokes on fd 0), and `CAP_FS_ACCESS` (its file operands and
its own `Help/` tree through the secured VFS, which authorises every path
per-inode under the caller's attested identity). No ambient authority; a
refused open or write is a status-line message, never a crash.

## Deviations from vim (deliberate, documented)

The staged feature list lives in `plans/VIM.md`. Notable core-level
deviations of this stage: no line wrap (long lines side-scroll), width-1
column arithmetic (double-width CJK cell math is staged), undo
conservatively re-marks the buffer modified, and visual `S` acts like
visual `s` (charwise). Each is recorded with its stage in the plan.

## Testing

`cargo test -p rustos-vim` drives the whole editor host-side: the buffer's
grouped undo/redo, every motion family and text object, operators with
counts and registers, dot-repeat (including insert replay), the search
engine (with the pathological-pattern budget), the ex command set over the
in-memory file seam, command-line editing, readonly enforcement, and the
renderer's cells, highlights, scrolling, and cursor placement against a
real curses window.

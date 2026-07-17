# VIM.md — the `vim` editor: shipped core and staged features

Binding under `AGENTS.md`. This plan records what the `vim` command app
(`userland/apps/vim`, `tairix-vim`) guarantees today and stages, feature
by feature, everything the upstream vim package provides beyond that
core, so later sessions can grow the editor toward a full implementation
without re-deriving the gap analysis.

## Status

`in progress` — the vim core (stage V0 below) is done; every later stage
is `planned`.

## V0 — the shipped core (done)

What a user gets from `vim [-R] [+num | + | +/pattern] [--] [file ...]`:

- Modes: Normal, Insert, Replace (`R`), Visual (`v`) + Visual-Line (`V`),
  Command-line (`:`, `/`, `?`); `Esc`/`Ctrl-C` return to Normal.
- Motions (count-aware, shared verbatim between movement and operators):
  `h j k l`, arrows, Enter, Space, Backspace, `w W b B e E`, `0 ^ $`,
  `f F t T` with `;`/`,`, `gg G`, `{ }`, `%`, `H M L`,
  PageUp/PageDown, `Ctrl-D/U/F/B`, `Ctrl-G`.
- Operators `d c y` over motions and objects
  (`iw aw`, `i(/a(`, `i[/a[`, `i{/a{`, `i</a>`, `i"/a"`, `i'/a'`,
  `` i`/a` ``), doubled `dd cc yy`; shorthands `x X s S D C Y r ~ J`;
  `cw`/`cW` behave as `ce`/`cE` (vim's special case).
- Registers: unnamed + `"a`–`"z`, capitals append; `p`/`P` linewise and
  charwise.
- Undo/redo (`u`/`Ctrl-R`, grouped per change/insert session, memory
  proportional to lines touched) and dot-repeat (`.`, including insert
  text).
- Search `/ ? n N *` with wrap + hlsearch + `:noh`; pattern subset:
  literals, `.`, `*`, `^`, `$`, `[...]` (ranges, negation), `\<` `\>`,
  escaped specials; fixed fail-closed backtracking budget.
- Ex core: `:w[!] [file]`, `:q[!]`, `:wq`, `:x`, `:e[!] [file]`,
  `:enew[!]`, `:r file`, `:n`/`:next`, `:prev`/`:previous`/`:N`,
  `:noh[lsearch]`, `:set nu|number|nonu|nonumber`, addresses
  (`N`, `.`, `$`, `±N` offsets, `%`, `a,b`), bare-address goto,
  `:[range]d`, `:[range]s/pat/rep/[g]` (any non-alphanumeric delimiter,
  `&`/`\&`/`\\` in the replacement, empty pattern reuses the last
  search).
- `ZZ`/`ZQ`; `-R` readonly (memory edits allowed, writes need `!`);
  status line, message line with vim's `E…` diagnostics, `'number'`
  gutter, visual/search highlighting, tab expansion (tabstop 8),
  vertical + horizontal scrolling.
- Infrastructure this stage added for every curses consumer:
  `Event::Esc` (dangling-`ESC`-at-read-end resolution in `lib/vt`) and
  `Event::Ctrl` in `lib/curses`.

Known deliberate deviations of V0 (each staged below where applicable):
no line wrap (long lines side-scroll); width-1 column arithmetic
(double-width CJK cells mis-place the cursor column); undo re-marks the
buffer modified even when it returns to the written state; visual `S`
acts charwise like visual `s`; normal-mode `Paste` events are ignored
(insert-mode and command-line pastes work); a lone `Esc` typed faster
than the read loop (same read as following bytes) is consumed silently.

## Staged features (the road to full vim)

Each stage is independent unless noted. "vim parity" means the behaviour
of upstream vim 9 with `nocompatible`.

- **V1 — display parity.**
  - Line wrap (`'wrap'`, default on in vim) with `gj`/`gk` display
    motions; keep `'nowrap'` + side-scroll as the option.
  - Double-width (CJK) cell arithmetic via `tairix_vt::char_width` in
    `render.rs` and the cursor-column math.
  - `'tabstop'`/`'shiftwidth'`/`'expandtab'` as settable options.
  - `'relativenumber'`; `'ruler'`; `'list'`.
  - Terminal-resize handling (needs a resize event from the stream
    layer; today the grid is fixed at session start).
- **V2 — editing parity.**
  - Indent operators `>` `<` `=` and `Ctrl-T`/`Ctrl-D` in insert mode.
  - Marks (`m`, `` ` ``, `'`) and jumps (`Ctrl-O`/`Ctrl-I`, jumplist).
  - Macros (`q`, `@`, `@@`) — the dot-recorder generalises.
  - Numbered registers `"0`–`"9`, the small-delete register `"-`, the
    read-only registers `":` `".` `"%`; `Ctrl-A`/`Ctrl-X`.
  - Text objects `ip ap is as it at`; `gU gu g~` case operators;
    `gv`, visual block mode (`Ctrl-V`) with block insert `I`/`A`.
  - Replace-mode backspace restoring overwritten text; `gi`; counts for
    `i`/`a`/`o` (`3ix<Esc>`); `.` honouring a new count (`3.`).
  - Undo: restore the unmodified flag when undo returns to the written
    state; undo tree (`g-`/`g+`, `:earlier`/`:later`).
- **V3 — pattern-engine parity.**
  - `\+ \= \? \{n,m}` multis, `\( \)` groups and `\1`–`\9`
    back-references (in patterns and `:s` replacements), `\|`
    alternation, character-class names (`\d \s \w \a` and `[[:alpha:]]`),
    `\c \C` case controls, `'ignorecase'`/`'smartcase'`, offsets
    (`/pat/e`), `\%V`, multi-line patterns (`\n`).
  - `#` (backward `*`), `g*`/`g#`.
  - Incremental search (`'incsearch'`) and search history (arrows on the
    `/` prompt).
- **V4 — ex parity.**
  - `:g`/`:v` global commands; `:m`/`:t` move/copy; `:>`/`:<`; `:j`;
    `:normal`; `:y`/`:pu`; pattern addresses (`:/pat/,/pat/`); marks in
    ranges (`:'a,'b`); `:s` flags `c i I n &`; `:&`/`:~`.
    Command-line history and completion (Tab), `:!` filters (needs the
    shell/spawn seam), `:w >>`, `:sav`, `:file`.
  - The full `:set` option machinery (booleans, values, `:set all`,
    per-option help) over a shared options table.
- **V5 — buffers, windows, tabs.**
  - Multiple in-memory buffers (`:b :bn :bp :bd :ls`) decoupled from the
    argument list; `Ctrl-^`.
  - Split windows (`:sp :vsp Ctrl-W` family) — requires the renderer to
    manage window trees; tabs (`:tabnew`, `gt`).
  - The quickfix list (`:make`, `:cn`) once a build seam exists.
- **V6 — durability and environment.**
  - Swap files / crash recovery and file-change detection (needs mtime
    and advisory locking from the VFS seam); `'backup'`/`'writebackup'`;
    `:w` atomic rename semantics; encoding and `'fileformat'` handling
    (CRLF round-trip); large-file strategy (today the whole file loads
    into memory; stage a rope/paged buffer for the multi-GB case).
  - viminfo (persistent registers/marks/history) under the app's
    per-user state directory; `vimrc` (an ex-command startup file read
    through the same `FileIo` seam) — note the §16.5 bundle rules: state
    lives in the user's Settings, never beside the binary.
  - `-d` diff mode, `-b` binary mode, `-es` silent-ex batch mode, `-u`,
    `+cmd`/`-c cmd` general startup commands, reading from standard
    input (`vim -`).
- **V7 — beyond the editor core (needs OS seams, likely never 1:1).**
  - Syntax highlighting and filetype detection (a declarative grammar
    format shipped as bundle resources — vim's regex-per-line syntax
    files are the reference, not the format).
  - Vimscript/Vim9script, autocommands, plugins, `:help` (serve vim's
    documentation set through `lib/help`), spell checking, folds,
    completion (`Ctrl-N`), digraphs, the mouse (the `Event::Mouse`
    plumbing already exists), clipboard registers (`"+`/`"*`) once a
    desktop clipboard service exists.

## Invariants every stage keeps

- The editor core stays I/O-free behind the `FileIo`/`Tty` seams and
  host-testable; every feature lands with tests in the same change.
- Untrusted input (files, patterns, keys) fails closed; the pattern
  engine keeps a bounded budget regardless of added syntax.
- No polling: every wait is a blocking read or a kernel-armed timeout.
- Coreutils-style option compatibility does not apply (vim is not a
  coreutil); *vim* compatibility is the bar, and any deliberate
  deviation is documented in the bundle's `Help/` and here.

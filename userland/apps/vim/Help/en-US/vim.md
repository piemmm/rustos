## NAME

vim — the modal text editor

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Edits text files with the modal command set of the well-known vim
editor. The session starts in normal mode: keys are commands, and
`i` (or `a`, `o`, and their variants) enters insert mode where typing
becomes text. `Esc` returns to normal mode. `:q` quits; `:wq` (or `ZZ`)
writes and quits.

Several files may be named; the session opens the first and `:n` /
`:prev` step through the argument list. A file that does not exist yet
is a `[New File]`, created on the first write.

Normal-mode commands (the implemented vim core):

- Motions: `h j k l`, the arrow keys, `w W b B e E`, `0 ^ $`,
  `f F t T` with `;`/`,` repeats, `gg G`, `{ }`, `%`, `H M L`, and
  `Enter`. A count prefix repeats a motion: `3w`.
- Operators: `d` (delete), `c` (change), `y` (yank), applied over any
  motion or text object (`iw aw i( a( i[ i{ i" i' i<` and their pairs);
  doubled (`dd cc yy`) they act on whole lines. Shorthands: `x X s S D
  C Y r ~ J`.
- Registers: `"a`–`"z` before an operator or put select a named
  register; capitals append. `p`/`P` put after/before the cursor.
- Undo history: `u` undoes whole changes, `Ctrl-R` redoes, and `.`
  repeats the last change (including its inserted text).
- Search: `/pattern` forward, `?pattern` backward, `n`/`N` repeat,
  `*` finds the word under the cursor. Patterns support literals, `.`,
  `*`, `^`, `$`, `[...]` classes, and `\<` `\>` word boundaries.
  Matches highlight until `:noh`.
- Visual selection: `v` (characters) and `V` (lines), extended by any
  motion or text object, then operated on with `d x c s y J`.
- Scrolling: `Ctrl-D Ctrl-U` (half window), `Ctrl-F Ctrl-B` and
  PageUp/PageDown (full window); `Ctrl-G` shows the file summary.

The ex (`:`) command core: `:w [file]`, `:q`, `:wq`, `:x`, `:e file`,
`:enew`, `:r file`, `:n`, `:prev`, `:noh`, `:set number` /
`:set nonumber`, line addresses (`:12`, `:$`, `:.+2`), `:[range]d`, and
`:[range]s/pattern/replacement/[g]` (with `&` for the whole match in
the replacement, `%` for every line in the range). A `!` after `w`,
`q`, or `e` forces past the readonly posture or unwritten changes.

Everything vim ships beyond this core is staged for later stages; the
staging list lives in the source tree's `plans/VIM.md`.

## OPTIONS

- `-R` — readonly: the buffer edits in memory but `:w` is refused
  unless forced with `:w!`.
- `+num` — start on line `num` of the first file.
- `+` — start on the last line of the first file.
- `+/pattern` — start on the first match of `pattern` in the first
  file.
- `--` — end of options; every following argument is a file name.
- `-h, -?` — show this command's own short help and exit.

## EXIT STATUS

- `0` — the session ended with a quit command, or the short help was
  shown.
- `1` — the terminal failed; the reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).
- `TERM` — the terminal profile the session drives; unknown values
  degrade to the dumb baseline.

## SEE ALSO

- `man`
- `cat`

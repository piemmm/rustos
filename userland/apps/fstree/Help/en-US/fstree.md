## NAME

fstree — the full-screen tree file manager

## SYNOPSIS

`fstree [directory]`

## DESCRIPTION

Browses the filesystem in a full-screen, keyboard-driven session: a
directory-tree pane on the left and a file pane on the right listing
the selected directory's entries with their sizes and modification
stamps. The session starts at `directory` (the root view `/` when
omitted).

The tree is read lazily: a directory's contents are fetched only when
it is first shown or expanded, so browsing a huge volume costs only
the directories actually opened. A directory the caller may not list
is refused in place — the error appears on the message line and the
previous view is kept; nothing is fabricated.

Keys:

- `Up`/`Down` or `k`/`j` — move the focused pane's cursor. Moving the
  tree cursor lists the newly selected directory in the file pane.
- `Left`/`Right` or `h`/`l` — collapse/expand the tree row under the
  cursor.
- `Enter` — in the tree, toggle expansion; in the file pane, descend
  into the selected directory (both panes follow), or open the
  selected file in a full-screen viewer: the disassembly viewer for a
  recognised executable, the text pager when the file's opening bytes
  read as text, the hex dump otherwise (the viewers are described
  below).
- `o` — open the selected file in a viewer of your choosing:
  `t` text, `x` hex, or `d` disassembly.
- `Tab` — switch the focused pane.
- `s` — open the sort menu: `n` name, `e` extension, `s` size,
  `m` modification stamp, `r` reverse the direction, `Esc` cancels.
  Directories always group before files.
- `c` — copy the selected entry: a prompt asks for the destination.
  A relative destination lands in the listed directory; a destination
  that is an existing directory receives the copy inside it under the
  source's name. `Tab` completes the typed path — a unique match is
  filled in (a directory staying open with its `/`), several matches
  extend to their shared stem or are listed. A directory is copied
  with everything under it. Copying an entry onto itself or a
  directory into its own subtree is refused before anything is
  written.
- `m` — move the selected entry, asked for the destination the same
  way. Within one volume the move is an atomic rename; across volumes
  the entry is copied and the source then removed.
- `r` — rename the selected entry in place: the prompt is pre-filled
  with the current name.
- `d` — delete the selected entry after a confirmation; only `y`
  proceeds. Deleting a directory removes everything under it, and the
  confirmation says so. The confirmation can be turned off in the
  settings menu (`S`).
- `M` — create a directory in the listed directory, asked for its name.
- `a` — edit the selected entry's permission bits: an octal prompt
  pre-filled with the current mode. Enter applies (only the entry's owner
  may change it — the kernel refuses anyone else), Esc cancels.
- `t` — tag or untag the selected file-pane entry and step down, so
  repeated presses mark a run. Tagged entries carry a `*` marker.
- `T` — tag by pattern: a glob (`*`, `?`, `[...]`) matched against
  the visible names, or a range — `size:MIN..MAX` (bytes, `K`/`M`/
  `G`/`T` suffixes, either bound may be left open: `size:1M..`,
  `size:..64K`) or `date:YYYY-MM-DD..YYYY-MM-DD` (modification date,
  end date included; an entry whose backing stores no stamp never
  date-matches). Every match is added to the tagged set; a malformed
  pattern or range tags nothing and says why.
- `i` — invert the tags across the visible entries.
- `C` — clear every tag.
- `f` — filter the file pane by a filename glob, applied live as it is
  typed. `Enter` keeps the filter (shown in the status line), `Esc`
  restores the filter as it stood. A pattern that does not compile
  hides nothing — the status line marks it `(bad pattern)` instead.
  Emptying the pattern clears the filter.
- `/` — search the branch under the focused directory by filename
  glob, matched against each file's branch-relative path. Results
  arrive in the flattened view as they are found.
- `F` — search file contents for a literal text, matched
  case-insensitively and streamed in bounded windows (a match spanning
  a read boundary is still found). With entries tagged, the search
  covers the tagged set (tagged directories recursively); otherwise it
  covers the focused branch. Each result row carries its match count;
  a file whose contents look binary is reported as a binary match —
  its bytes are never shown. A file that refuses to read is listed in
  the walk's report, never silently dropped.
- `u` — count disk usage under the focused directory: files, bytes,
  and directories, walked incrementally in the background. `Esc`
  cancels, keeping the figures counted so far.
- `v` — flatten the branch under the focused directory: one list of
  every file beneath it, filling page by page (`Space` loads the next
  page). Inside the view, `t`/`T`/`i`/`C` tag its rows, `c`/`m`/`d`
  run batch operations over the tagged set, `Enter` jumps to the
  selected row's directory in the panes (landing the cursor on it),
  and `Esc` returns to the panes. While a walk or search is still
  running, `Esc` first stops it, keeping the rows found so far. Rows
  are named relative to the flattened branch. Search results (`/`,
  `F`) fill this same view, so their rows are taggable and operable
  exactly like a flattened listing.
- `H` — toggle hidden (dot-named) entries in both panes.
- `.` — repeat the last file operation on the current selection: a
  copy or move goes into the same destination directory again, a
  delete asks again per the confirmation setting. With no operation
  yet, the key says so.
- `V` — list the mounted volumes: each row shows the mount point, the
  filesystem type, and the free/total bytes when the volume reports
  them. `Enter` re-roots the whole session at the chosen volume;
  `Esc` closes the list. When no volumes are reported the key says
  so.
- `S` — the settings menu: `1` toggles the single-delete
  confirmation, `2` the batch-delete confirmation. Changes persist in
  your own `Settings/fstree/` and load at the next start; without a
  home directory they last the session and the menu says so.
- `?` — show this help over the panes; any key dismisses it.
- `q` — quit, restoring the terminal.

While entries are tagged, `c`, `m`, and `d` operate on the whole
tagged set instead of the selection: `c`/`m` ask for an existing
destination directory the entries land in (`Tab` completes it), and
`d` confirms the batch delete (unless that confirmation is turned
off in the settings menu). Entries are processed in tag order; a failed entry never
stops the rest, and the completion report counts what succeeded while
a report screen lists every failure by name — a batch is never
silently partial. Entries that succeeded are untagged; failures stay
tagged for a retry.

When a copy or move would overwrite an existing file, the session
asks per file: `o` overwrites it, `s` skips it (a skipped source is
left in place), and `c` cancels the remaining steps — in a batch,
cancel drops all remaining entries — work already
applied stays applied, and the completion report says what happened.
A failure mid-copy removes the half-written target and surfaces the
kernel's error; nothing ever masquerades as a complete copy. Every
operation is authorised by the kernel — a refusal appears verbatim on
the message line with nothing changed.

The status line shows the listed path, its visible entry count, the
sort order, the backing volume's free/total bytes (when the System
Information service can report them), whether hidden entries are
shown, the active filename filter, and — while anything is tagged —
the tagged count and byte total. A file whose backing format stores
no modification stamp shows `-` in the stamp column.

When the file pane omits hidden entries, the session also notes the
omission on the Standard Information Stream (fd 3) — one advisory
record per change, capturable with `fstree 3>info.jsonl`; the
session itself is unaffected.

The viewers: `Enter` on a regular file opens it read-only in a
full-screen viewer.
A recognised executable — an `rxe` image, a 64-bit ELF, a wasm
module, or a standalone signed manifest — opens in the
**disassembly viewer**; a file whose opening bytes are NUL-free,
valid UTF-8 opens in the **text pager**; anything else opens in the
**hex dump**. `o` overrides the pick. `x` switches the open viewer
to the hex dump at the same place, `t` to the text pager (snapped to
the start of the line containing the shown offset), and `d` to the
disassembly viewer (a file no container format claims asks for an
instruction set and decodes as a raw fragment from the current
place); `q` or `Esc` returns to the panes.

The viewers page through the file in bounded windows — the file is
never held in memory whole, so a file of any size (well past 4 GiB)
pages correctly — and share the keys:

- `Up`/`Down` or `k`/`j` — one row; `PageUp`/`PageDown`, `b`/`Space`
  — one page; `Home`/`End` — the start / the last page.
- `g` — go to a place: a 1-based line number in the text pager, a
  byte offset (decimal or `0x`-hex) in the hex dump, an address in
  the disassembly viewer.
- `/` — search forward from the current place; `n` repeats the last
  search past its previous hit. The scan runs in the background in
  bounded steps — the session stays responsive over a huge file, the
  status line says `searching…`, and `Esc` stops it in place.
- `?` — this help; any key dismisses it.

The **text pager** decodes UTF-8 with a visible replacement for
invalid bytes, expands tabs, and shows every other control byte as a
visible `·` — file contents are never sent to the terminal as raw
escape sequences. Long lines wrap by default; `w` toggles wrapping
off (the tail is then clipped at the right edge). `/` searches for
literal text, matched case-insensitively, across read boundaries.
The status line shows the place as a line number; after a jump from
the hex dump the line number is unknown (counting was not paid for)
until a `g` goto re-anchors it.

The **hex dump** shows the classic layout — offset column, sixteen
bytes as hex pairs, and an ASCII column with unprintable bytes as
`.`. The offset column is as wide as the file needs (at least eight
hex digits). `/` searches for literal text (case-insensitive) or,
spelled `0x` followed by hex byte pairs (`0xdeadbeef`), for an exact
byte sequence.

The **disassembly viewer** opens on a summary page: the container's
format, instruction set, and entry point, its regions (address, file
extent, memory size, permissions), and its symbol count; a
standalone signed manifest instead lists its ABI version and the
capabilities it requests. An `rxe` image carries its manifest beside
it, never embedded, so the summary says so. `Up`/`Down` move over
the region rows and `Enter` opens the selected code region's
disassembly (a data region shows as hex at its file bytes). A
container that names no instruction set — an `rxe` image runs on
whatever machine loads it — asks once: `x` x86-64, `a` aarch64,
`r` riscv64, `w` wasm.

The code pane shows one instruction per row — address, encoding
bytes, mnemonic, operands — with `<symbol>:` label lines and
symbolised branch targets (`<main+0x8>`) where the container names
symbols. Instructions are decoded per screenful, never the whole
binary up front. `g` jumps to an address, `/` searches the
instruction text, `End` walks to the region's end in the background
(`Esc` stops the walk), and `I` re-decodes at another instruction
set. `Esc` steps back to the summary page.

Every container and instruction decode runs in a locked-down helper
process (the parser sandbox), never in the file manager itself: a
malicious executable can crash the helper, not the session. A file
the decoder refuses — malformed, or too large to hand to the sandbox
— falls back to the hex dump with a one-line notice.

A read the kernel refuses mid-view closes the viewer and surfaces
the error on the message line — stale content is never shown as
live.

## OPTIONS

- `directory` — the directory the session starts in; the default is
  the root view `/`.
- `-h`, `-?` — print this document's short form and exit.

## EXIT STATUS

- `0` — the session ended by the user's `q`.
- `1` — the starting directory could not be listed, or the terminal
  path failed.
- `2` — the arguments could not be understood.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df, find

# The `fstree` file manager

`fstree` (`userland/apps/fstree`, crate `tairix-fstree`) is the full-screen,
keyboard-driven **tree file manager** for the terminal: a persistent
directory-tree pane beside a file pane over the storage forest, drawn with
the OS curses library (`lib/curses`). It is a command app in the system app
store — a sealed `.app` bundle with `AppInfo`, `Run`, and a `Help/` locale
tree, discovered from disk like every other bundle. The staged plan for the
whole tool lives in `.junie/fstree-next-plan.md`; this page describes what
is built.

## What is built (S1 model core, S2 file operations, S3 tagging/batches/walks, S4 search/filter, S5 viewers, S9 disassembly viewer)

- **The tree pane.** A lazily populated directory tree: a directory is read
  through one `fs_readdir` call when it is first shown or expanded — never
  a whole-volume scan, so browsing costs the working set and a huge volume
  costs only the directories actually opened. Rows carry expansion markers
  (`+`/`-`), indentation by depth, and a cursor with scrolling.
- **The file pane.** The selected directory's entries with name, size
  (`<dir>` for directories), and modification-stamp columns. The stamp is
  the `Time64` value the kernel's listing stream carries per entry; a
  backing format that stores no per-node stamp reports the epoch, which
  renders as `-` — an absent figure, never a fabricated 1970 date.
- **Sorting.** Name, extension, size, or modification stamp, ascending or
  descending, directories always grouped first. The `s` menu selects the
  key (`n`/`e`/`s`/`m`) or reverses (`r`); `Esc` cancels.
- **Hidden entries.** Dot-named entries are hidden by default in both
  panes; `.` toggles them, with cursors clamped to the shrunken lists.
- **Status and message lines.** The listed path, visible entry count, sort
  order, the backing volume's free/total bytes (the System Information
  API's `MOUNT_LIST` query through the shared `tairix_procinfo` mount walk,
  best-effort — an unreachable service simply omits the figure), and either
  an error, the sort prompt, or the key hints.
- **The attributes editor.** `a` opens the focused selection's
  attributes (the file pane's entry, or the tree pane's directory): an
  overlay showing the entry's permission bits and its extended
  attributes (`lib/fsmeta` `namespace.key` pairs, loaded through the
  `fs_attr_list`/`fs_attr_get` syscalls; the kernel omits namespaces
  the caller may not read). Inside it, arrows move over the entries,
  `m` opens the mode prompt — a modal octal line pre-filled with the
  current bits (octal digits and Backspace edit; a non-octal key and a
  fifth digit are refused, matching the kernel's `FS_MODE_MASK`
  ceiling; Enter applies through `fs_set_mode`, whose owner-only rule
  decides) — `n` adds an attribute as a `key=value` line applied
  through `fs_attr_set`, Enter edits the selected one (pre-filled; a
  binary value pre-fills the key alone, never a lossy round-trip), `d`
  removes it through `fs_attr_remove`, and Esc leaves. Every change is
  the kernel's decision (per-inode read/write permission, the shared
  key grammar, the reserved `system.`/`trusted.` namespaces); a refusal
  is surfaced verbatim and nothing changes. A mount whose format stores
  no attributes answers `NotSupported`, which the view states honestly
  in place of an empty list; values render escaped (`\xNN`) when not
  printable text, never raw onto the terminal.
- **File operations.** `c` copies and `m` moves the focused selection to
  a typed destination, `r` renames it in place (prompt pre-filled with the
  current name), `d` deletes it after an explicit confirmation (a
  directory with everything under it — the question says so), and `M`
  creates a directory in the listed one. The destination spelling is
  parsed by the one shared path grammar (`lib/path`): a relative spelling
  is joined onto the listed directory, and an existing directory receives
  the transfer inside it under the source's name. A transfer onto itself
  or into the source's own subtree is refused before any I/O. A move is
  an atomic `fs_rename` on one volume and falls back to copy-then-remove
  across volumes (the kernel's honest `CrossVolume` report drives the
  fallback — the target is probed *before* the rename, so an existing
  file is asked about, never silently replaced).
- **The overwrite question.** When a transfer would overwrite an existing
  file the operation pauses per file: `o` overwrites, `s` skips (the
  skipped source stays in place, and the emptied-source-directory removal
  above it is withheld so a skip never fails a move), `c` cancels the
  remaining steps — applied work stays, and the completion report counts
  entries and skips. Copy streams through a fixed bounded buffer (never a
  whole-file slurp); a mid-stream failure removes the partial target and
  surfaces the kernel's errno, so a half-written file never masquerades
  as a copy.
- **Tagging.** `t` toggles a tag on the file pane's selected entry (and
  steps down, so a held key marks a run); `T` tags by a filename glob
  compiled by the one shared matcher (`lib/glob`) against the visible
  names; `i` inverts the tags over the visible entries; `C` clears them.
  Tagged entries carry a `*` marker and the status line shows the tagged
  count and listed-byte total. Tag order is preserved — it is the batch
  order.
- **Batch operations.** While anything is tagged, `c`/`m`/`d` operate on
  the whole tagged set: `c`/`m` ask for an existing destination directory
  (validated before anything runs) and `d` confirms the batch delete.
  The batch driver (`src/tag.rs`) plans each entry's landing spot as it
  comes up, **continues past a failed entry**, and collects a per-file
  failure report shown on a dismissable report overlay — a batch is never
  silently partial. An overwrite conflict pauses the whole batch through
  the same o/s/c question a single operation uses; cancel drops the
  remaining entries (applied work stays, and the report says so).
  Succeeded sources are untagged; failures stay tagged for a retry, and a
  delete or move prunes tags only for paths verified gone.
- **Disk usage (`u`) and the flattened branch view (`v`).** Both are fed
  by one bounded, cancellable walker (`src/walk.rs`): a depth-first,
  deterministic (name-ordered) descent that reads at most a fixed number
  of directories per tick, counts files/bytes/directories, and records an
  unreadable directory instead of stopping — the summary says exactly
  what could not be read. `u` reports its running figures on the message
  line and `Esc` cancels keeping the count so far. `v` lists every file
  under the focused directory (paths relative to the branch root) and
  pauses at page boundaries so a huge branch fills memory only as far as
  asked — `Space` loads the next page; inside the view `t`/`T`/`i`/`C`
  tag rows and `c`/`m`/`d` run batches; `Esc` returns to the panes.
- **The live filename filter (`f`).** A glob (`lib/glob`) narrowing the
  file pane as it is typed: every keystroke in the prompt re-applies the
  pattern, Enter keeps it (shown in the status line), and Esc restores
  the filter held before the prompt opened. A pattern that does not
  (yet) compile — an unclosed bracket mid-edit — hides nothing: the pane
  stays unfiltered and the status line marks the filter `(bad pattern)`,
  so entries are never silently hidden behind an uncompilable pattern.
  Emptying the pattern clears the filter.
- **Branch filename search (`/`).** The S3 walker with a name sieve: the
  focused branch is descended in the same bounded, cancellable ticks,
  and every file whose branch-relative path matches the glob lands in
  the flattened view as it is found. Results are ordinary flat rows —
  taggable, batchable, and jumpable.
- **Content search (`F`).** A literal, ASCII-case-insensitive text
  search through file contents — over the tagged set when anything is
  tagged (tagged files directly, tagged directories walked), otherwise
  over the focused branch. The scanner (`src/search.rs`) streams each
  file in bounded windows through the seam's `read`, carrying the last
  `needle-1` bytes across reads so a match spanning a window boundary
  is still found; a file is never held in memory whole. Each result row
  carries its match count; a file whose first window contains a NUL is
  reported as a `binary` match — its bytes are counted, never rendered.
  A file that refuses to read is recorded in the walk's report, never
  silently dropped.
- **Flat-view navigation.** `Enter` on any flattened row (a listing or a
  search hit) jumps to the row's directory in the panes: the tree is
  expanded down to it, the file cursor lands on the entry (lifting the
  hidden toggle or clearing the filter when either hides it — with the
  change visible in the status line), and a directory that refuses to
  list keeps the flattened view with the error surfaced. While a walk
  or search is still running, `Esc` first stops it in place (the rows
  found so far stand and stay browsable); a second `Esc` leaves the
  view.
- **The file viewers (Enter on a file).** A regular file opens read-only
  in a full-screen viewer: the **disassembly viewer** when
  `tairix_binfmt::detect` (or the `RXM1` manifest magic) recognises the
  head sample, the streaming **text pager** when the head sample (the
  first 4 KiB) is NUL-free, valid UTF-8, the **hex dump** otherwise; `o`
  asks instead of auto-picking. `x`/`t`/`d` switch the open viewer at
  the same place, `q`/`Esc` return to the panes. All page through the
  seam's `read` in bounded
  windows — never the whole file in memory, offsets 64-bit throughout, so
  a file past 4 GiB pages correctly. Shared keys: arrows/`k`/`j` per row,
  `PageUp`/`PageDown`/`b`/`Space` per page, `Home`/`End`, `g` goto (a
  1-based line, or a decimal/`0x`-hex byte offset), `/` search with `n`
  repeat. The text pager (`src/view_text.rs`) treats a row as the bytes
  up to a newline capped at 4 KiB (a newline-free file still pages in
  bounded memory), decodes UTF-8 lossily, expands tabs, and renders every
  other control byte as a visible `·` — untrusted file content reaches
  the curses grid as printable characters only, never as raw terminal
  escapes; `w` toggles line wrap. The hex dump (`src/view_hex.rs`) shows
  the classic offset/16-byte-hex/ASCII rows with a size-derived offset
  width (at least eight hex digits) and searches literal text
  (case-insensitive) or an exact `0x…` byte sequence. Goto-line and both
  searches run as byte-budgeted background scans on the same timed tick
  the walks use — the session stays responsive over a huge file, the
  status line shows `searching…`, and `Esc` stops the scan in place. A
  read the kernel refuses mid-view closes the viewer with the error
  surfaced — stale content is never shown as live.
- **The disassembly viewer (`src/view_disasm.rs`).** A recognised
  container opens on a summary page — format, ISA, entry, the region
  table (address, file extent, memory size, permissions), the symbol
  count — and a standalone signed manifest lists its ABI version and
  requested capabilities by their `CAP_*` names (an rxe image's summary
  states the manifest travels beside the image, never inside it). Enter
  on a code region opens the paged code pane: one instruction per row
  (address, encoding bytes, mnemonic, operands), `<symbol>:` label
  lines, and branch targets symbolised against the address-sorted symbol
  table; a data region hex-switches to its file bytes. Every container
  summary, manifest summary, and instruction window is decoded by the
  sandboxed `lib/sandbox` decode service through the object-safe
  `Decode` seam — the app's only in-process inspection is the bounded
  magic-prefix routing. Windows are decoded per screenful from a bounded
  region read; scrolling records `(address, depth)` resynchronisation
  anchors and walks forward from the nearest one (fixed-length ISAs may
  start from a short backscan; wasm threads its nesting depth through
  the service's window hand-off), so memory never follows the binary.
  Goto-address, instruction-text search, and `End` run as chunked
  background walks on the shared tick. A container that names no ISA
  (rxe) asks once (`x`/`a`/`r`/`w`); `I` re-decodes at another ISA; a
  refused or oversize decode falls back to the hex dump with a one-line
  notice, and a worker crash surfaces as a typed error — never a session
  crash.
- **Help.** `-h`/`-?` print the bundle's own Help document through the
  shared `lib/help` engine; the in-session `?` overlay shows the same
  document decoded to plain text through the one `lib/vt` parser. Nothing
  is embedded in the binary.

## Design

The charter's seam pattern (as `vim` and `top`): an I/O-free state machine
(`src/model.rs`) the pure renderer (`src/render.rs`) draws and the key
grammar (`src/app.rs`) mutates, over two injected seams —

- `Fs` (`src/fs.rs`) — `list_dir`, `volume_space`, the attributes
  editor's `stat_mode`/`set_mode` and
  `attr_list`/`attr_get`/`attr_set`/`attr_remove`, and the S2 mutating
  operations (`stat_kind`,
  streamed `read`/`write`, `create`, `mkdir`, `remove_file`,
  `remove_dir`, `rename` with its `RenameOutcome::CrossDevice` report);
  the `Run` binary implements it over the kernel-authorised `fs_*`
  syscalls and the sysinfo mount walk (with one-slot source/destination
  handle caches hoisting the per-chunk open off the copy path), the
  tests over an in-memory tree. The trait carries exactly the operations
  the landed stages perform; each later stage extends it in place with
  the operations it introduces, together with their callers.
- The operations themselves live in `src/ops.rs`: `resolve_destination`
  and `plan_target` validate before any I/O, and the resumable `FileOp`
  executor drives a work stack that reads directories incrementally as
  the walk reaches them (memory follows the frontier, never the subtree)
  and pauses on each overwrite conflict, handing the decision to the key
  loop — a paused operation holds no filesystem handle, only the paths
  still to process.
- The tag set and batch driver live in `src/tag.rs` (an ordered `TagSet`
  and a `Batch` that drives one `FileOp` per entry, pausing through the
  same conflict machinery), and the bounded walker in `src/walk.rs`
  (memory follows the frontier, never the subtree). The searches are the
  same walk with a `Sieve` deciding which found files reach the list:
  `All` (the plain flattened view), `Name` (the `/` glob against the
  branch-relative path), or `Content` (the `F` scanner, `src/search.rs`,
  which queues found files and streams them within its own per-tick byte
  budget). One walk definition feeds the flat view, the usage figures,
  and both searches — there is no second descent.
- The viewers live in `src/view_text.rs` and `src/view_hex.rs`: each is
  a state machine over the seam holding only its top position and the
  one decoded page the renderer draws (refreshed from the seam each
  frame), plus the byte-budgeted background scan (goto-line / search)
  the key loop ticks. Backward paging finds the previous row start
  through one bounded window read — no line index is built, so memory
  never follows the file.
- `Tty` — the terminal byte channel over the inherited fd 0/1: the `Run`
  binary links the one shared `tairix_curses::StreamTty` (`lib/curses`,
  feature `program`); the program names only its standard descriptors,
  never a console device.

Every wait parks in the kernel: `Screen::getch` blocks normally, and while
a walk or a viewer scan is live the wait carries a short timeout so an
elapsed read advances it one bounded tick — there is
no polling loop. A refused listing fails closed: the error is surfaced on
the message line, and the cursor, listing, and expansion state are left
untouched — a denied directory is never shown as empty. The session runs
inside the terminal's alternate screen and restores it (and cooked input)
on every exit path, stating any abnormal reason on standard error after
the restore.

## Capabilities

`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, `CAP_FS_ACCESS`, and
`CAP_PROC_SPAWN` — the ambient file authority of the launching user plus
the right to re-spawn its own binary as the parser-sandbox decode worker,
nothing more. Every path the session touches is authorised per-inode by
the kernel under the caller's attested identity; the tool holds no
private channel and adds no authority of its own.

## Testing

Host tests drive the whole session without a kernel or terminal: model
unit tests (lazy expansion counted through the seam, every sort key and
direction, hidden-toggle cursor clamping, fail-closed denial), golden-grid
renders (pane layout, the absent-stamp `-`, real stamp formatting, the
help overlay, the sort prompt, the mode prompt), the mode-editor flows
(pre-filled prompt, digit/Backspace editing with non-octal and
fifth-digit refusal, apply, Esc cancel, refused stat, kernel-refused
change, emptied prompt), and a scripted end-to-end session
(browse → expand → pane switch → sort → hidden toggle → help → quit).
The S2 operations are covered end to end through the same in-memory
seam: mkdir (creation, invalid-name refusal, Esc cancel), rename (in
place, onto an existing file via the overwrite question, session-root
refusal), delete (file, recursive directory with the file pane climbing
to the surviving ancestor, declined confirmation), copy (absolute and
relative destinations, tree reproduction, self/subtree/bad-spelling
refusals), the overwrite matrix (overwrite, skip, cancel, per-file
questions in a merging directory copy), move (same-volume rename,
cross-volume copy-then-remove, skip preserving the skipped source), and
failure injection (a read or write failure mid-copy removes the partial
target and surfaces the errno with the panes consistent). The S3 surface
is covered the same way: tag toggle/figures, tree-pane refusal, glob
tagging (matching and malformed patterns), invert/clear, batch delete
continuing past a denied entry with the report in tag order, batch copy
with skip and with cancel (remaining entries dropped, still tagged),
destination validation, the walker's per-tick read budget and error
recording, the usage walk's bounded ticks/cancellation/final figures,
flattened-view pagination with resume, flat-view tagging and exit, and
the status line's tag figures. The S4 surface: the live filter
(narrowing as typed, Esc restore, Enter keep with the status-line
indicator, the honest bad-pattern behaviour), the name search (matching
rows with the denied directory still reported, bad-pattern refusal, the
Enter jump landing both panes on the hit), and the content search
(case-insensitive matching with overlap counting, a match spanning a
read-window boundary found through the carried tail, the binary-match
note, the tagged-set scope, a read refusal recorded rather than
dropped, Esc stopping a live search while keeping its results, and the
empty-needle refusal). The S5 viewers: the head-sample auto-pick (text
vs hex, with a refused head opening nothing), golden text and hex
grids, control-byte/invalid-UTF-8/tab sanitising, wrap segmentation
and the `w` toggle, the goto-line scan (target, past-end landing on
the last row, non-number refusal), text search across the read-window
boundary with `n` repeats and the not-found report, the hex dump's
byte-sequence and case-insensitive text searches across the window
boundary (with the malformed-`0x` refusal), goto-offset in decimal and
hex with end clamping, paging a sparse 5 GiB backing at full 64-bit
offsets, in-place view switching, Esc stopping a live scan before
leaving, and a mid-view read refusal closing the viewer. The S9
disassembly viewer runs over the same in-process loopback worker the
`lib/sandbox` tests use — the genuine sandbox request/reply path with no
kernel — covering: the rxe summary page (region rows, the
manifest-beside-the-image note, the ISA chooser on Enter, a golden
aarch64 `nop` window), the wasm module page (ISA known, golden op
rows), the standalone-manifest capability listing, malformed and
oversize containers falling back to hex with their notices, per-ISA raw
fragments through the `o`/`d` choosers with `I` re-decode, paging and
resynchronising scroll-back at region bounds with `End`'s background
walk, instruction-text search with `n` and goto-address (including the
no-region refusal), symbol labels and `<name+0xoff>` branch targets,
and the framed render of both pages.

## The disassembly viewer's sandbox posture

Every container and instruction decode runs inside the parser sandbox
(the kernel sandbox spawn mode plus the `lib/sandbox` seam —
[the parser sandbox](../security/sandbox.md),
[`tairix-sandbox`](../lib/sandbox.md)): the production `Run` binary
re-spawns **itself** in the worker role over the kernel's reserved
self-token and serves the `lib/binfmt`/`lib/disasm` decode service over
its wired standard streams, so in-process decode of an untrusted
executable never happens and a hostile file's blast radius is one
disposable worker. Reply validation is the seam's caller-side
fail-closed decoding; the viewer trusts nothing the worker says beyond
it.

## The completeness pass (S10)

The stage rounds the tool out with the storage-forest surface and the
remaining conveniences; every piece runs through the same seams and is
host-tested end to end.

- **The volume list (`V`).** `Fs::list_volumes` reports the published
  storage roots (`VolumeInfo`: mount target, filesystem type, optional
  free/total bytes); the production seam walks the same System
  Information API `MOUNT_LIST` pages the status line's free-space figure
  uses (`tairix_procinfo::for_each_mount`). The overlay lists one row
  per volume (an unreported size renders `-`, never a zero-byte disk),
  Enter re-roots the whole session at the chosen target, and a refused
  root listing keeps the session and the list with the errno on the
  message line. An empty report is a message, never an empty screen.
- **Destination completion (Tab).** The copy/move/batch-destination
  prompts complete the typed path through the shared `lib/complete`
  engine ([`tairix-complete`](../lib/complete.md)) — the same
  directory-part/leaf, dotfile, and common-prefix policy the shell's Tab
  uses — over a read-only adapter on the `Fs` seam that resolves
  relative spellings against the same base the submit resolves onto. A
  unique candidate replaces the word (a directory staying open with its
  `/`); several extend to the longest common prefix or are listed,
  bounded, on the message line. Prompts that ask for a *new* name or a
  pattern do not complete.
- **Range tagging.** The `T` prompt accepts `size:MIN..MAX` (binary
  `K`/`M`/`G`/`T` suffixes, either bound open) and
  `date:YYYY-MM-DD..YYYY-MM-DD` (end date inclusive; dates validated by
  round-trip through the shared `lib/fsmeta` calendar) beside the glob
  form (`tag::TagRange`). An entry whose backing stores no stamp (the
  epoch marker) never date-matches — an unknown date is not a date — and
  a malformed range is a typed refusal that tags nothing.
- **Repeat (`.`).** The last completed copy/move destination directory
  or delete is remembered (`model::RepeatOp`) and re-applied to the
  focused selection through the ordinary planning path — same refusals,
  same overwrite questions. The hidden-entries toggle moved to `H`.
- **Persisted confirmations (`S`).** `settings::Settings` carries the
  single- and batch-delete confirmation toggles, stored as a two-line
  `key=value` file under the user's own `Settings/fstree/` through the
  `Fs` seam (`HOME` names the tree; without one, changes are
  session-only and the menu says so). Parsing fails safe: garbage,
  unknown keys, and bad values leave the confirmations **on**. A
  disabled confirmation makes `d` act immediately; the per-file
  overwrite questions are never configurable away.
- **The Standard Information Stream.** When the file pane omits hidden
  entries the session emits one `fs.hidden_entries_omitted` advisory
  record per omission-state change on fd 3 through the injected `Info`
  seam (`info::note_hidden_entries`); the production seam writes
  best-effort via `lib/rt`'s fd-3 wrapper. Directory names are
  JSON-escaped so a hostile name cannot break the framing, and a record
  that cannot frame is dropped whole.

Tests cover the volume list (open/navigate/re-root, the refused root
keeping the session, the empty report, the rendered rows), settings
(fail-safe parse, encode round-trip, toggle persistence and reload
through the seam, the no-home session-only path, both confirm-off
delete paths), range tagging (parse matrix with malformed refusals,
size and date tagging over the pane, the epoch exclusion), repeat
(copy re-applied to a new selection, the empty-history message, a
repeated delete still asking), completion (unique with the open
directory, common-prefix extension then candidate listing, the
no-match and non-path-prompt cases), the once-per-change omission
records, and a scripted session across the new surfaces.

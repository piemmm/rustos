# `tairix-files` — filesystem browser

Stage 7 deliverable (`AGENTS.md` §10, `PLAN.md` Stage 7,
`plans/APPWIN.md` AW3). The default graphical file manager: the `Run`
entry-point binary of the on-disk `files.app` bundle the taskbar's
permanent Files button spawns. Installed as a `.app` bundle in the system
app store (`AGENTS.md` §16.2/§16.5).

Stability tier: **experimental**.

## What this crate is

Almost only the program: behaviour worth testing lives in shared,
host-tested crates the binary composes over the live syscalls, and the two
parts that are this app's alone — the places rail's input routing and the
command-line decision — are host-visible modules (`sidebar`, `command`)
rather than code buried in the freestanding program where no test could
reach it.

- the directory-browser **engine** — the transactional navigation model
  (`Browser`), the themed listing renderer (`render`), the validated
  path spelling, and the `VfsDirectorySource` composition — is the
  shared `lib/browse` crate (`tairix-browse`), the same engine the
  desktop session's trusted file picker drives (`plans/APPWIN.md` AW5),
  so the file manager and the picker can never diverge;
- the **places rail's** model and geometry (`Places`, `SidebarView`) are
  that same `lib/browse` crate; the mounted volumes it is built from come
  from the `MOUNT_LIST` System Information query through `lib/procinfo`;
- the window channel's client half (`WindowClient` / `WindowEvents`) is
  `lib/window`;
- the grid's icon **artwork** — the reclaim-governed decode cache, the
  asset-path spelling, and the read/rasterise seams — is the shared
  `lib/icon::artwork` layer, and the decode itself is the shared
  `lib/sandbox` `imagerender` service;
- the runtime (`_start`, allocator, syscall wrappers, the shared
  `read_dir_all` listing call) is `lib/rt`.

## Invocation

`files [directory] [-h | -?]`

The optional `directory` operand is the location to open — the desktop
opens a folder by launching the file manager with that folder's path.
With no operand the window opens at the launching user's home directory
(`HOME`), and at the root view when `HOME` is unset or cannot be listed.
The grammar is the one every command app uses: `-h`, `-?`, and `--help`
win wherever they appear and print the bundle's own short help through
the shared help engine; `--` ends the options; an unknown option or a
second operand is a usage refusal (exit `2`), never silently ignored.

The operand is untrusted input, decided by the pure `command` module
before any syscall:

- a raw argument longer than the kernel's `FS_PATH_MAX` is refused
  *before* it is parsed, and its text is never echoed back;
- the spelling must be an absolute path whose every component is a real
  directory name — a relative or alias-rooted path, `.`, `..`, an empty
  or over-long component, or a control character is refused, so a
  spelling can never mean somewhere other than it reads as;
- the single acceptance oracle is the shared
  `tairix_browse::vfs::components_from_absolute_path`; the per-component
  walk exists only to *phrase* the diagnosis and can never admit a path
  the shared parser rejected;
- a refusal states the offending spelling as a quoted, escaped literal,
  so a hostile escape sequence is shown rather than replayed at the
  error stream.

A refused or unlistable location **degrades, it does not exit**: the
reason is stated on `stderr` and the window opens at the home directory
instead (then the root view), so a bad argument never leaves the user
with no window. Only a command line the program cannot act on at all —
an unknown option, a second operand, non-UTF-8 argv — is a refusal, and
it exits `2` after stating the reason and the usage banner.

The invocation surface is documented for users in the bundle's own
`Help/` tree, one document per required locale, and is the only place
that text lives.

## What the program wires

One `shm_create`d frame region granted to the reserved window endpoint
(the zero-copy window surface), one `port_bind`-bound event mailbox the
app **parks** on through its wait-set (every accepted event
authenticated against the kernel-attested session identity the create
reply named), and the `WindowClient` calls over `ipc_call`.

The desktop is asked for first, before anything is sized or painted
(`WindowClient::desktop`), so the window opens at a size the screen can
hold, the listing is set at the desktop's own UI density, and a session in
light mode gets a light window rather than a dark one corrected after the
user has seen it. A `DesktopChanged` afterwards is adopted and repainted —
during a long copy too, where the modal progress panel is re-presented each
pass — so a light/dark switch reaches this window at once. A desktop query
the session refuses, or a density this client cannot draw at, exits
fail-loud with the stated reason rather than falling back to a guess.

Keyboard navigation drives the browser (`Down`/`Up` select, `Enter` activates the
selection — descends into a directory or launches a selected `<Name>.app`
bundle by spawning the bundle's own `Run` through the ordinary signed
app-load gate (asynchronously, with the launched child reaped on the
wait-set's any-child member so it is never left a zombie; a refusal stated
fail-loud on `stderr`), `Backspace` goes up); `F2` renames the selected item,
`Ctrl+Shift+N` makes a new folder, `Ctrl+X`/`Ctrl+C`/`Ctrl+V` cut, copy,
and paste the selection (a same-volume move is one `fs_rename`, a
cross-volume move copies-then-deletes, a copy streams in bounded chunks),
`Delete` removes it after a modal confirmation, and `Alt+Enter` shows its
properties — every write the launching user's own permission-checked VFS
call, no new capability, stopping fail-loud on `stderr` at the first
refusal (`AGENTS.md` §2.24, §5.4); a `CloseRequested` from the desktop
ends the program cleanly; every bring-up refusal exits fail-loud with a
reserved code and a stated reason on `stderr`.

The window's **title is the directory it is showing**, retitled over the
window channel whenever — and only when — the browser moves, so the desktop
and its taskbar always name the location rather than the program. A location
too long for the bounded title field drops whole leading components behind
the shared ellipsis, keeping the folder the user is in.

A **secondary press on the window's Close control means "leave this
folder"**: it climbs to the parent and closes the window only at the top,
where there is nothing left to leave. A parent that cannot be listed keeps
the window open and states which place was refused — an unreadable ancestor
must never destroy the window. A primary press always closes.

## The places / devices sidebar

A shortcut rail runs down the leading edge of the window, below the toolbar
band: the user's own places above, every mounted volume below, with the
listing and its scrollbar gutter inset beside it. A place name too long for
the rail ends in the shared ellipsis, so hidden text is never silent. The
row order is fixed, so the rail never reshuffles under the user: Home,
Desktop, Documents, `Apps`, `System`, a drawn separation, then the volumes
sorted by label.

The volumes are **real mount data, not a guess**. The app reads the
`MOUNT_LIST` System Information query through the shared `lib/procinfo`
client and offers only mounts reporting themselves *available*, so a
surprise-removed device is never drawn as a row that would fail on the
first click; a refused or failed query yields no volumes at all rather than
a fabricated list. Each offered volume carries the storage medium its
backing device actually reports, and `tairix_icon::disk_icon` turns that
medium into the shipped drive artwork — rotational, solid-state, and
removable each draw their own icon, and a paravirtual or unknown class
draws the generic drive glyph. Nothing here classifies a device by its
name.

The rail model itself is the shared, host-tested `lib/browse` `places`
layer, and it validates every offered volume rather than trusting it: an
empty, over-long, or control-character label, a target that is not absolute
or does not parse, or a target an earlier row already covers **drops** the
row. A malformed volume is never repaired or guessed at, and a stale volume
row is never fabricated. The fixed user places are always listed — the
model does no I/O, so it cannot know whether a directory is there, and a
place that turns out not to be listable says so (below) rather than
silently vanishing.

Pointer and keyboard both reach every state the row control offers. Motion
tracks the hover highlight (and still reaches the view below, so a bundle
drag-out is unaffected); a primary press on a row focuses the rail, puts
its cursor there, and navigates. `Tab` moves the keyboard focus between the
rail and the file view **from either side**; while the rail holds it, the
arrows walk the cursor (clamped at both ends, never wrapping), `Enter`
navigates to it, `Escape` hands the focus back, and any other key is
swallowed rather than navigating the listing behind it. The row matching
the browser's current location draws selected, through the control's own
selection state.

A place that cannot be listed **reports and stays put**: the reason is
stated on `stderr` through the single fail-loud reporting path the app
already uses for every refusal, the row is marked unavailable so it reads
disabled from then on, and the browser stays exactly where it was. It never
wedges or blanks the window.

The kernel publishes **no mount-change notification**, so a newly attached
volume appears when the user asks the window to re-read what is there —
`F5` or the toolbar's Refresh, the same gesture that re-lists the
directory. No polling loop and no timer were added to stand in for the
missing event.

The desktop session's trusted file picker composes the same renderer and
deliberately passes no rail: it is bounded to the tree the requesting
application was authorised to be shown, so one-click jumps to arbitrary
mounted volumes would widen the pick beyond what was asked for.

## Grid-view icon artwork

In grid view each tile draws the OS's shipped raster icon master for the
entry's content type — one `<asset-id>.png` per kind under
`/System/Graphics/Icons`, the same store the desktop session resolves —
and falls back to the built-in vector glyph when there is none. The
fallback chain is **total**: a missing, over-long, undecodable, or
disbelieved asset draws the glyph, never a blank tile.

The bytes are read through the app's own capability-checked VFS read
under the launching user's identity, bounded to one byte past the shared
`MAX_ARTWORK_BYTES` ceiling so an over-long asset is *detected* rather
than silently truncated into something decodable-looking (the same
bounded open/read/close the bundle-manifest scan uses, with a different
ceiling — there is no second copy of that loop).

The **decode never runs in this process**. Icon artwork is a file on a
volume, i.e. untrusted input, so the bytes go to the shared `lib/sandbox`
`imagerender` service in a minimum-capability worker (`AGENTS.md` §19.5):
the `Run` binary re-enters itself in the reserved worker role over a
fresh pipe pair — the same production launcher the desktop session uses,
not a second mechanism — and the kernel brands that child
capability-empty. The reply is not trusted either: the echoed side and
the exact pixel length are validated before any surface is built from it.

Only the tiles actually drawn are resolved, at the exact side each tile
reserves, so a hundred-entry directory costs one read and one decode per
*visible kind*; nothing pre-warms an icon for an entry scrolled out of
view. The cache is built through the one shared
`tairix_icon::artwork_cache` constructor with the app's real seat, frame
size, live pressure gauge, and audit sink, so its budget is the shared
desktop policy rather than numbers picked here. The app parks on a
`MemoryPressure` wait-set member (it reads the band once before the first
present, because the member reports only *changes* and the process gauge
starts at the fail-closed unknown band) and trims the cache at each
pressure wake — no timer, no poll. Dropping the pipeline tears the cache
down, overwriting the artwork first, so the pixels go back on a window
close and a fail-loud exit alike.

## Capabilities

`CAP_CONSOLE_WRITE` (fail-loud diagnostics), `CAP_FS_ACCESS` (its
directory listings and the shipped icon assets it reads — every read
still permission-checked per inode under the launching user's identity),
`CAP_SHM` (the granted window frame region), and `CAP_PROC_SPAWN`. See
`AppInfo.toml`.

`CAP_PROC_SPAWN` covers two uses and **no new capability was added for
the artwork**: launching an activated `<Name>.app` bundle through the
signed load gate (the child runs as the launching user, no ambient
authority), and hosting the icon-rasterisation sandbox worker, which is
an ordinary restricted spawn of this same binary in its worker role. A
later reader should not add one: the charter requires the untrusted
decode to happen in a minimum-capability sandbox, and the authority to
start that sandbox is exactly the spawn authority the bundle already
requests. Decoding in-process to avoid the spawn is not an option.

## Test surface

The engine's behaviour is exhaustively host-tested in `lib/browse`
(`cargo test -p tairix-browse`): the rail's model (row order, per-medium
icons, the fail-closed rejection of malformed and duplicate volumes,
selection tracking the browser's location), its geometry (the hit-test
inverts the drawn rectangles exactly at the row boundaries and rejects a
point outside the rail), the content-area inset with and without a rail,
and the grid's artwork lookup with fake read and rasterise seams.

The rail's *input routing* is this crate's own, so it lives in the
host-visible `sidebar` module (`cargo test -p tairix-files`) rather than
in the freestanding `Run` program where no host test could reach it:
navigation on activation, keyboard traversal in both directions, focus
toggling, the keys the rail must not steal, the refresh gesture, hover
tracking, the focus-preserving rebuild, and the refusal path (the exact
text to state, the row marked unavailable, the browser unmoved).

The command-line decision is host-visible for the same reason, in the
`command` module: the accepted absolute path (collapsed separators and
the bare root included), no operand at all, the refused traversal (`..`
and `.`), the over-long argument and the over-long single component, the
relative, empty, `-`, and alias-rooted spellings, the control character
escaped rather than replayed, the refused second operand, the unknown
option and each usage-error wording, `-h`/`-?`/`--help` winning wherever
they appear, `--` ending the options, and the unlistable-location
wording.

Every rail and command-line *decision* is therefore covered. The one line
that is not is the app's single `stderr` write that states a reason — the
pre-existing shared reporting path the delete, paste, and launch refusals
already use — which both modules hand their text to rather than writing it
themselves.

The rest of this package is the freestanding `Run` program; a host build
compiles only the inert stub in its place.

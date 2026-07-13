# rustos-desktop-session

The RustOS desktop **session glue** (`AGENTS.md` §10, `PLAN.md` Stage 7): the
component that owns the shared theme registry and the taskbar model and ties
the desktop's parts together. The crate also ships the `Run` binary of the
`desktop` **command app** (`desktop.app` in the system app store): a shell
user starts the desktop by typing `desktop`, and a login configured with
`os.loginType graphical` spawns the same bundle directly after
authentication — one bundle, one spelling. The command's grammar is closed
(`src/cli.rs`): bare `desktop` starts the session, the reserved `-h`/`-?`
switches serve its own `Help/` documents, anything else is a usage error.

The taskbar deliberately owns no theme registry and no spawn capability:
activating a start-menu entry only *reports* an abstract `MenuAction` (a
session control, an application launcher, or the light/dark
`ToggleAppearance`). Resolving that action is the session glue's job. This
crate is that glue's first increment — the runtime **light/dark switch**.

## What this crate owns

- **The shared `rustos-theme` `ThemeRegistry`** — the one runtime registry the
  whole desktop reads its theme from (`AGENTS.md` §6, §10).
- **The `rustos-taskbar` `Taskbar` model** — so a theme switch is a single
  in-place operation: the registry's active theme changes and the taskbar is
  re-themed to match.

## What it does

`DesktopSession::resolve` turns a `TaskbarResponse` into a `SessionEvent`:

- A selection of the start menu's appearance-toggle entry is the one response
  the session acts on itself. It switches the built-in light/dark theme on the
  registry (driven by the *active* theme's appearance, so a custom dark theme
  toggles to the built-in light theme and vice versa), re-themes the taskbar,
  and returns `SessionEvent::AppearanceChanged(ThemeId)`. The embedder relays
  the now-active theme — `DesktopSession::active_theme` — to the window manager
  and apps.
- Everything else is `SessionEvent::Forward`ed unchanged: a launcher or
  session-control selection, a task activation, a notification or clock press.
  Those need capabilities the session does not hold (a window-manager handle,
  the power/spawn capabilities), so the embedder performs them (`AGENTS.md`
  §10, §16.5). (`DesktopShell` applies a task activation's window effect to the
  compositor itself — see *Running-task list ↔ window stack* — before
  forwarding it.)

`toggle_appearance`, `set_theme`, and `register_theme` expose the same theme
control directly. `toggle_appearance` and `set_theme` re-theme the taskbar
through one private apply path, so the relay logic is never duplicated
(`AGENTS.md` §2.2). `set_theme` fails closed on an unknown id and
`register_theme` on a duplicate id, leaving the active theme and the taskbar
untouched (`AGENTS.md` §5.4 / §2.9).

## Loading the on-disk graphics assets

The desktop's cursors and notification icons are authored as SVG under
`/System/Graphics` (the SVG-first asset rule, `AGENTS.md` §10 / §16.2).
`lib/cursor` and `lib/icon` own the decode-and-fall-back logic but stay
`no_std` with no path of their own; reading the bytes needs a filesystem
capability, so it is the session's job (`AGENTS.md` §17.4 / §19.5). The
`assets` module is that job:

- A caller supplies a `GraphicsAssetReader` (VFS-backed on a running system,
  an in-memory table in tests).
- `DesktopSession::load_cursors` reads one asset per cursor kind named by the
  active theme's `CursorSet`, from
  `/System/Graphics/Cursors/<asset-id>.svg`, and returns a `CursorTheme` the
  window manager registers through its `CursorRegistry`.
- `DesktopSession::load_icons` reads one asset per icon kind, from
  `/System/Graphics/Icons/<asset-id>.svg`, and returns an `IconSet` the
  taskbar installs through `TaskbarRenderer::set_icons`.

Both are **total and fail-closed per kind** (`AGENTS.md` §2.9): a kind whose
asset is missing, unreadable, malformed, or out of subset keeps its built-in
artwork, so a corrupt or absent `/System/Graphics` can never blank the
pointer or a status icon — it simply yields the built-in set.

## Presenting the taskbar through the window manager

`TaskbarPresenter` joins the taskbar to the compositor. The taskbar paints a
*rectangular* `rustos_raster::Surface` and the window manager composites and
rounds windows; neither depends on the other (`AGENTS.md` §17.4), so the join
is session glue. Given a `&mut rustos_wm::Compositor` and the taskbar's own
`TaskbarRenderer` (which holds the across-frame glyph cache), `present`:

- paints the bar, places it at `BarLayout::bar`'s origin, and rounds it with
  `Corners::from_radius(BarLayout::corner_radius)` — the compositor's single
  anti-aliased rounded-corner path, the same one it uses for application
  windows, never a second one (`AGENTS.md` §2.2);
- while the start menu is open, paints its popup, places it above the bar at
  `MenuLayout::panel`'s origin, and rounds it the same way; closing the menu
  removes the popup window.

The presenter owns only the two compositor `WindowId` tokens it minted, so the
session composes the GUI crates without holding the window-manager handle. It
is total and fails closed (`AGENTS.md` §2.9): a render that cannot allocate
leaves the on-screen window untouched, a window the compositor no longer knows
is re-created on the next present, and `teardown` removes both windows.

## Routing one input stream to both routers

The desktop has two input routers — the window manager's `InputRouter` and the
taskbar's `TaskbarInput` — and both consume the **same** shared `rustos_input`
event vocabulary (`AGENTS.md` §17.4, §2.2). A real input source produces one
stream, so `SessionInputRouter` fans it to the right router through
`handle(event, &mut Compositor, &mut Taskbar)`:

- a **primary press** goes to the taskbar when its menu is open (modal) or the
  pointer is over the bar, and to the window manager otherwise — never both, so
  a click on the bar never also activates a window beneath it;
- **pointer motion** is fanned to both so their pointers stay in step; only the
  window manager acts on it, dragging a grabbed window;
- a **primary release** ends a window move-grab in the window manager;
- a **key event** goes to the window manager, which delivers it to the focused
  window; the taskbar takes no keyboard input;
- anything else is `SessionInputResponse::Ignored`.

Decorations arm a title-bar drag through `begin_move`; the embedder reads the
keyboard owner through `focused`. The router holds no pixels and grants itself
no authority; every routed sub-call is total and fails closed (`AGENTS.md`
§2.9).

## Driving the desktop from a live input stream

`DesktopShell` composes all of the above — the `DesktopSession`, the
`SessionInputRouter`, the `TaskbarPresenter`, and the `TaskbarRenderer` — into
one event-driven frontend, the long-open "feed the router and presenter from
live device events" thread:

- It `pump`s the pending events from an injected `InputSource` seam (a real
  pointer/keyboard channel on a running system, an in-memory queue in tests,
  `AGENTS.md` §7), routing each through the `SessionInputRouter` and returning
  a `ShellOutcome` per event.
- A taskbar action is `resolve`d (the light/dark toggle is applied here, every
  other response forwarded) and the bar is re-presented, so an opened/closed
  menu or a re-themed bar reaches the screen; a window-manager action needs no
  re-present, so motion and drags stay cheap.
- A faulting `InputSource` ends the `pump` with its `Errno`; the events drained
  before the fault stay applied and the embedder replaces or re-polls the
  source (`AGENTS.md` §2.9 / §19.5).

The shell holds no framebuffer: the `Compositor` is the embedder's and is
passed in on each call. A loaded notification-icon set is installed with
`set_icons`, a title-bar drag armed with `begin_move`, and the desktop torn
down with `teardown`.

## Live device input source

`DeviceInputSource` (the `device` module) is the live backing for the shell's
`InputSource` seam. It wraps an injected `PointerInputChannel` — a
capability-checked kernel input channel on a running system, an in-memory queue
in tests (`AGENTS.md` §7) — that hands the desktop one framed
`rustos_abi::input::PointerInput` record at a time. Each `poll` decodes one
record through `PointerInput::from_bytes` into the `lib/input` `InputEvent` the
window manager and taskbar route: an absolute `PointerMoved`, or a
`PointerPressed` / `PointerReleased` carrying the resolved `PointerButton`. The
crate holds no input capability of its own — the channel delivers the bytes and
the decode runs above the device (`AGENTS.md` §17.4 / §19.5) — and a malformed
record fails closed with its `Errno` rather than being misinterpreted. The ABI
record is the desktop-level pointer event, a distinct layer from the
device-level driver input ABI, not a duplicate of it (`AGENTS.md` §2.2).

## Live keyboard input source

`KeyboardInputSource` (the `keyboard` module) is the keyboard counterpart of
`DeviceInputSource`. It wraps an injected `KeyInputChannel` — a
capability-checked kernel keyboard channel on a running system, an in-memory
queue in tests (`AGENTS.md` §7) — and each `poll` decodes one framed
`rustos_abi::input::KeyInput` record through `KeyInput::from_bytes` into the
same `lib/input` `InputEvent` stream the shell pumps: a `KeyPressed` /
`KeyReleased` carrying the resolved `Key` (a produced `Char`, or a `NamedKey` —
the twelve wire function-key codes fold into one `NamedKey::Function`) and the
held `Modifiers`. The `SessionInputRouter` routes it to the window manager,
which delivers it to the focused window. Like the pointer source it holds no
input capability and fails closed on a malformed record (`AGENTS.md` §5.4 /
§2.9).

## Seat-backed input channels

`SeatInputChannel` (the `seat` module) is the kernel backing for both the
`PointerInputChannel` and `KeyInputChannel` seams above: it drains each
fixed-width input record from the per-seat, owner-gated channel the kernel
seat registry routed the desktop's input to (`plans/DISPLAY.md`;
`docs/src/desktop/seat.md`). The records arrive through an injected
`SeatEventReader` seam — the seat-addressed `pointer_read` / `keyboard_read`
syscalls (`rustos_rt::pointer_read` / `rustos_rt::keyboard_read`) on a
running system, an in-memory queue in tests (`AGENTS.md` §7) — so the crate
holds no seat lease of its own and stays host-testable (`AGENTS.md` §17.4).

The security property is kernel-side: every drain is gated on
`CAP_INPUT_READ` **and** owner-gated against the seat's live lease, so only
the session that acquired the seat receives the stream. Desktop input is
deliberately not a named IPC port — a port's receive gate is capability-only
and cannot express "only the live seat-lease holder may drain". The
channel's own validation is narrow and fails closed (`AGENTS.md` §5.4 /
§2.9): an empty drain is `None`, and a drain of anything other than exactly
one whole record surfaces `LengthOutOfRange` rather than handing truncated
bytes to the decoder. A pointer record and a key record are each a
fixed-width drain, so the channel implements **both** seam traits through
one shared validation path rather than two (`AGENTS.md` §2.2); which records
flow is decided by the reader it wraps. Wrap a pointer reader in
`DeviceInputSource`, or a keyboard reader in `KeyboardInputSource`.

## Running-task list ↔ window stack

`TaskBridge` keeps the taskbar's running-task list in step with the window
manager's window stack. The taskbar models a list — one entry per top-level
window, with the click-to-activate / minimise rule — but owns no window
manager, and the window manager owns no task list (`AGENTS.md` §17.4). A task
is named by a `TaskId` and a window by an opaque `WindowId`, so the bridge owns
the correspondence: it mints a stable task id per tracked window and translates
between the two. Every operation is total and fails closed (`AGENTS.md` §2.9):

- `open` adds a window to the compositor, lists it as a running task, and
  shows, raises, and focuses it; it opens nothing only if the task-id space is
  exhausted.
- `close` removes the window and its task and drops focus if it held it; an
  untracked window is a no-op.
- `activate` applies the bar's `ActivateOutcome` — an activated task is shown,
  raised, and focused; a minimised one is hidden and unfocused — and is a no-op
  for an unknown task.
- `sync_focus` mirrors a window-manager focus change back into the bar's
  highlight, leaving it untouched (and forcing no repaint) for a focused window
  that owns no task.

`DesktopShell` drives it: `open_window` / `close_window` manage the lifecycle,
and `handle` applies a `TaskActivated` outcome to the compositor and mirrors a
window-manager focus change into the bar, moving keyboard focus through the
window manager's `InputRouter::focus` / `unfocus`. The bridge holds no pixels
and grants itself no authority — the compositor, router, and taskbar are the
embedder's, passed in per call.

## The `Run` binary — the live desktop session (`plans/DISPLAY.md` D7c)

The crate also ships the desktop session's `Run` entry-point binary
(`src/run.rs`, built freestanding on the native Tier-1 targets and an inert
host stub elsewhere), the first live embedder of everything above. It wires
the real seams end to end:

- `display_acquire(SEAT_PRIMARY)` binds the session as the boot seat's
  owner; the kernel owner-gates every later drain and present against that
  live, revocable lease — the session asserts nothing itself.
- `DisplayClient` over `ipc_call` to the reserved `DISPLAY_ENDPOINT`
  performs the bring-up handshake: query the mode (checked frame
  arithmetic, fail closed on overflow or a zero-sized mode), `shm_create`
  the double-buffered frame region, `shm_grant` it **to the serving task of
  the display endpoint** (never a raw, recyclable PID), configure, then
  present by frame index through `RemoteDisplay` — no frame bytes ever
  cross the IPC.
- The `DesktopShell` is driven from the two live seat readers (the
  seat-addressed `pointer_read` / `keyboard_read` behind the
  `SeatEventReader` seam), with the queried mode as the pointer's screen
  rectangle and the compositor's background taken from the active theme's
  desktop colour.
- The session **parks on a `SeatInput` wait-set member** between events —
  never a poll loop — woken by input delivery *and* by lease loss. Losing
  the seat (the typed `SeatRevoked` / `SeatNotOwner` on any drain or
  present) tears the session down fail-loud: the reason lands on `stderr`
  and a reserved exit code (90–97, documented in `src/run.rs`) tells the
  spawning supervisor what happened; the owner-checked `display_release`
  runs on every exit path after the acquire.

The manifest (`AppInfo.toml`) requests exactly `CAP_DISPLAY`,
`CAP_INPUT_READ`, and `CAP_SHM`. The bundle's image planting and the
end-to-end QEMU vertical ride the D7d autoload world (`plans/DISPLAY.md`).

## Dependencies and layering

The crate composes the other GUI crates and `lib/*` only — `rustos-taskbar`,
`rustos-wm`, and the shared `rustos-theme` definition, plus `rustos-cursor` /
`rustos-icon` (the SVG set builders) and `rustos-abi` (the `Errno` the read
seam returns and the `PointerInput` / `KeyInput` records the device and
keyboard sources decode)
(`AGENTS.md` §17.4). Composing GUI crates is the permitted
`userland/gui/*` edge; nothing outside `userland/gui/*` depends on it (§17.3),
so a headless image omits it cleanly.

The `Run` binary additionally links `rustos-display` (the client half of the
present protocol) and `rustos-rt` (the pure-Rust userland runtime), for the
bare-metal targets only.

The library is `no_std` with `#![forbid(unsafe_code)]`; no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). The `Run`
binary holds the one justified `unsafe` — the slice view of its own
kernel-mapped frame region, with its invariants stated in a `// SAFETY:`
block (`AGENTS.md` §2.10).

## Still to come (Stage 7)

The D7d end-to-end QEMU vertical (the autoload world grows the display node,
the framebuffer service bundle, and this session, started by typing
`desktop` at the shell the text login drops to), relaying the active
theme to the window manager and apps over live IPC, resolving launcher /
session-control actions once the process and window-manager capabilities are
wired (deferred Stage 6 work), and the VFS-backed `GraphicsAssetReader` that
reads `/System/Graphics` on a running system (the in-memory-tested loader and
its fallbacks now exist; the `Run` binary installs the built-in artwork until
then).

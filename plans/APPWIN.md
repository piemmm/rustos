# APPWIN.md — Default apps go live: real channels + WM-presented windows

This is the staged build plan for the Stage 7 remainder that wires the two
default apps — the filesystem browser (`userland/apps/files`) and the
terminal emulator (`userland/apps/terminal`) — to **live** data channels and
to windows presented by the compositing desktop session. It is **binding
under `AGENTS.md`** — read `AGENTS.md`, `PLAN.md` Stage 7,
`plans/DISPLAY.md` (the seat/display model this builds on), and
`plans/CAPABILITY_USE.md` (CU6, whose picker-issued one-shot descriptors
ride this work) first; every rule in all of them applies here without
exception.

## 0. Scope and decisions (binding for this plan)

- **Apps are their own processes, never session plug-ins.** Each default
  app ships as its own self-contained `.app` bundle with its own `AppInfo`
  manifest and `Run` binary (`AGENTS.md` §16.5), spawned through the
  ordinary load gate and holding **only** its own manifest ∩ ceiling set:
  the files app requests `CAP_FS_ACCESS` (its directory reads are ordinary
  §5.3-checked VFS calls under its own identity), the terminal requests
  `CAP_PROC_SPAWN` (it spawns the user's shell over pipes). The desktop
  session's manifest stays exactly the D7 graphical class
  (`CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM`) — hosting the apps in the
  session process would demand widening it with filesystem and spawn
  authority, the opposite of the CU6 sizing rule, and would make the CU6
  picker model (a one-shot descriptor handed *across* a process boundary)
  meaningless. This decision is final for this plan.
- **The window channel reuses the D7 surfaces; kernel additions are
  in-place evolutions only.** An app's window content travels zero-copy:
  the app `shm_create`s its window surface and `shm_grant`s it to the
  session (endpoint-directed, exactly the D7c client pattern), and a
  fixed-width, versioned, fuzzed `lib/abi` window protocol (the
  `display_ipc` discipline: bounded decode, fail closed, reply status
  frames) carries create/present-with-damage/close requests to a
  session-served endpoint. Input travels the other way over the same
  channel — the session routes the focused window's keyboard/pointer
  events to its owning app, and an app with no pending event **parks**
  (never polls, §2.23). The exact request/event vocabulary is fixed in
  AW2 when the engine lands, not speculated here (§2.3/§2.4).
- **The stream/pipe wait source (AW4).** `WaitSourceKind::Stream` (wire
  value 5, added in place — abi-v1 is not frozen, §2.13, in the stage that
  consumed it): `id` names a pipe read end of the caller's **own** open
  table, owner- and descriptor-checked at member add (every refusal the
  same oracle-free `NotFound`), ready — as a non-consuming, per-scan
  re-resolved peek — on buffered bytes or end-of-stream, woken by the
  pipe layer's existing write/close wakes (`PIPE_WAITQ`, joined only by
  sets holding a `Stream` member). Conformance-tested like the D7a
  members, plus `PipeEnd::readable` / registry-peek unit tests (the peek
  borrows in place — a clone/drop of a pipe end would spuriously wake
  every pipe waiter).
- **The mailbox-room wait source (AW4).** `WaitSourceKind::PortRoom` (wire
  value 10, added in place in the stage that consumed it) is the **send**
  side of the same discipline: `id` names a port the caller may post to,
  admitted by the *send*-authority check `ipc_send` applies (the caller is
  a sender, not the binder, so the `Port` kind's owner check does not
  fit), refusing the same oracle-free `NotFound`. It is ready when a send
  would not be refused for want of room — below capacity, port gone, or
  send authority lost — as a non-consuming, **level-triggered** peek: the
  member is armed *after* a send was refused, so an edge on the occupancy
  falling would already have passed if the receiver drained in between and
  the sender would park forever. The wake is targeted (a port records the
  tasks parked for its room; a committed `ipc_recv` names exactly them),
  with one broadcast on teardown, and only sets holding a `PortRoom`
  member join the queue. It is what lets a sender hold an event the
  receiver must not lose — a window resize, a picker conclusion — instead
  of dropping it or polling for capacity (the desktop's app-ward
  hold-back, `plans/OPEN-DEFECTS.md` D35).
- **Fail closed, no ambient authority** (§5.4, §4): the session accepts a
  window request only from the task the shm grant and the protocol
  identify; a refused or malformed request is a typed reply, never a
  partial window; a closed/killed app's windows are torn down by the
  session, never leaked. Every security-relevant refusal is logged with a
  stable event id (§19.4).
- **Headless stays first-class** (§17.3): everything here lives in
  `userland/gui/*`, `userland/apps/*`, and `lib/*`; no non-GUI crate
  gains a GUI edge, and a headless image simply ships no desktop session.
- **Not in this plan:** the input-device→seat topology policy
  (`plans/PI.md` P11 / the seat manager), and the GUI widget vocabulary
  (`plans/GUI-CONTROLS-DESIGN.md`).

## 1. Stages

### AW1 — the live directory channel plumbing `[x]`

The shape-neutral data plumbing both the files app's future `Run` binary
and today's `ls` share, landed with today's consumers so nothing is
speculative:

- `tairix_abi::fs::DirEntries` — the **one** whole-stream walker over the
  `fs_readdir` byte stream: a fused iterator yielding each decoded
  `DirEntry` and surfacing the first decode error as a terminal
  `Err` (the caller refuses the whole listing, never a partial one).
  Unit-tested beside the codec; the `fuzz_decode` harness walks arbitrary
  streams (termination, fail-closed refusal, no panic).
- `lib/rt` grows the shared directory-read call: `read_all_growing`
  (the pure retry policy — grow a buffer on `BufferTooSmall`, doubling to
  a hard ceiling, host-tested with closures) and `read_dir_all(path)`
  (open-as-directory + grow loop against the kernel's `FS_IO_MAX`
  transfer cap). `ls`'s private copy of that loop is deleted and its
  `read_dir` re-built on the shared pair — one definition (§2.2).
- `userland/apps/files::vfs` — the production `DirectorySource` engine:
  `absolute_path` (root-first components → a bounded, validated absolute
  path; refuses an empty/`/`-bearing/NUL-bearing component or a path over
  `FS_PATH_MAX` before any syscall), `entries_from_dir_stream` (the
  `DirEntries` walk mapped onto the browser's `Entry` vocabulary, whole
  listing refused on any bad record), and `VfsDirectorySource<F>` — the
  composition over an injected `fetch(path) -> stream` primitive, so the
  engine is host-proven end to end (a `Browser` navigating an in-memory
  tree of *encoded* `DirEntry` streams). The freestanding fetcher is one
  line (`tairix_rt::read_dir_all`) and lands wired inside the files `Run`
  binary in AW3, exactly as staged consumers landed for the encrypted-swap
  layer and the SP11 stack spans.

### AW2 — the window protocol + engine `[x]`

- `tairix_abi::window_ipc` — the fixed-width, versioned, fail-closed
  vocabulary: `WindowRequest` (`Create` with the granted shm surface, the
  app's event endpoint — reserved endpoints refused — geometry, and a
  bounded control-character-free `WindowTitle`; `Present` by frame index
  + non-empty damage; `Close`), the 12-byte create reply carrying the
  session-minted non-zero window id, and the app-ward `WindowEvent`s
  (`Focus`, `Key` embedding the one `KeyInput` codec, window-local
  `Pointer`, `CloseRequested`, `RedrawRequested`). `WINDOW_ENDPOINT`
  (`0x5749_1001`) joined `is_reserved_endpoint`; the decoders are
  enrolled in `fuzz_decode`.
- `lib/window` — both halves over injected seams (the `lib/display`
  precedent): `WindowServer` (decode → `CallerIdentity` attestation
  (`call_peer_origin`) → owner/bounds validation → the `WindowHost`
  compositor bridge; windows keyed to the owner's `ProcId`, `NotFound`
  for any window the caller does not own — `SetTitle` included, so an app
  can retitle only its own window and the attested `ProcId` reaches
  `window_opened` as the sole source of a window's identity icon —
  map-once regions via the
  shared `tairix_display::ShmMapper`, `WINDOWS_PER_CLIENT_MAX` cap,
  `client_exited` teardown, `deliver_event` app-ward routing validated
  against the live window) and `WindowClient`/`WindowEvents` (typed
  calls over `WindowTransport`, parked — never polling — event wait
  over `EventSource`). Host-proven against an in-process loopback; no
  kernel change.
- **The redraw handshake.** The session may release a window's retained
  content to reclaim memory (`docs/src/desktop/wm.md`, "Releasable window
  content"), which costs the app nothing but leaves the window blank
  until a *full-window* present arrives — a present carries only a damage
  rectangle, so a re-established surface starts transparent.
  `RedrawRequested` is that ask. `lib/window` answers it for the app:
  `WindowClient` records each window's last presented frame index and
  current extent and `WindowEvents::wait` re-presents that frame with
  full-window damage before returning the event, so an app inherits the
  handshake by using the client library and an app that wants genuinely
  fresh pixels still sees the event. A window that has never presented
  has nothing to re-send. An app that decodes its mailbox directly rather
  than through `WindowEvents` gets no auto-answer and repaints itself
  (the terminal and the Switchboard panel do); an app that ignores the
  event simply shows a blank window until it next presents.

### AW3 — session window server + the files app goes live `[x]`

Done. What now holds:

- The desktop session binds `WINDOW_ENDPOINT` authorised by its live,
  kernel-attested seat lease (no privileged-bind capability, no ceiling
  widening) and serves it from its wait-set loop via the `ShellWindowHost`
  bridge into `DesktopShell` (`userland/gui/session/src/windows.rs` +
  `run.rs`): served windows are composited, taskbar-tasked, and focused
  like native windows; app-ward `Focus`/`Pointer`/`Key` events flow over
  the AW2 `EventSink` (`ipc_send`), and a kernel-reclaimed event port
  (dead app) tears the owner's windows down. The loop dispatches on the
  woken member's token — `call_recv` blocks when nothing is pending, so a
  seat wake never touches the window endpoint (the wedge the first
  end-to-end run exposed); readiness is a non-consuming peek, so pending
  members re-report on the next wait.
- A served window is declared **app-presented** to the compositor when it
  opens, which is what makes its content pixels releasable under memory
  pressure; the windows the session paints itself (bar, lock screen,
  picker, confirmation prompt) never are, because no client would answer
  their redraw. The memory-pressure member of the same wait set drives
  the release: the woken branch trims the desktop's caches, runs the
  content ladder, then drains `Compositor::pending_redraws` and delivers
  `RedrawRequested` to each released window's owner through the existing
  served-window id mapping — the same route every other app-ward event
  takes, with no second table. The drain also runs after a window becomes
  visible again, so a restored window that lost its pixels is asked for
  them at once.
- `userland/apps/files` ships its `Run` binary + signed `AppInfo`
  (`CAP_FS_ACCESS` only) inside the bundle: `VfsDirectorySource` over
  `tairix_rt::read_dir_all`, rendering through the shared theme, window
  create/present over the AW2 client, parked event wait, redraw on
  focus/theme-relevant events, clean exit on `CloseRequested`. The
  taskbar's permanent Files button spawns it
  (`tairix_desktop_session::config` — the one production configuration
  the QEMU harness also imports for its click coordinates).
- The autoload QEMU vertical drives the full click-through with injected
  pointer buttons (`tools/qemu` ordered `PointerStep` script + ordered
  marker-gated screendumps): the Files button → served-window clicks →
  the Library popup's terminal launch (`plans/NEW-TASKBAR.md` T5), with
  two host-verified screendumps (dark desktop;
  the served window at the cascade origin). All
  gates are kernel-attested serial records (the window endpoint's first
  `CallReplied`, `MessageDelivered` counts per the interaction contract in
  the test crate's lib target); the guest PASS rides the AW4 tail below,
  so the run cannot pass without the whole chain. Uncovered and fixed
  along the way: the session's blocking-recv
  loop wedge (above) and the virtio-input event queue's 8-descriptor
  ceiling silently dropping bursts under a saturated CPU (now the
  device's full 64, `lib/virtio_input`).
- Every coordinate the vertical uses is the desktop's own: a served
  window's footprint is the shared cascade origin — its **outer**
  top-left — with the app's client surface grown by the furniture band
  the window manager reserves (`WindowFrame::insets`, the same frame the
  compositor decorates with), and each app declares its own resizability
  once (`WIN_RESIZABLE`, beside its window size) because that flag sizes
  the band. `served_window_layout` returns both rectangles, so a
  screendump assertion measures the whole window while a click is
  measured inside the *client* and therefore reaches the application
  rather than the title bar above it.

### AW4 — the terminal goes live `[x]`

Done. What now holds:

- `WaitSourceKind::Stream` (see §0): the kernel wait-set wakes a parked
  owner on its own pipe read end's buffered bytes or end-of-stream.
- `tairix_terminal::spawned` — the production `ShellSource`, host-tested
  over injected closures: `shell_wires` (the one attach-block layout —
  child stdin from the keystroke pipe, stdout *and* stderr onto the one
  output pipe, fd 3 closed; canonical under `SpawnAttach::parse`) and
  `PipeShellSource` (one bounded chunk per wake, end-of-stream surfaced
  as the typed "shell exited" refusal, short-write resume, wedged-channel
  fail-closed). The `Run` binary supplies `pipe_create` ×2,
  `spawn_attached` of `tairix_users::policy::DEFAULT_SHELL` (with `TERM`
  exported and the child-side ends closed after the spawn), and
  `fs_read`/`fs_write` under the seam.
- `userland/apps/terminal` ships its `Run` bundle (signed `AppInfo`:
  `CAP_CONSOLE_WRITE` + `CAP_PROC_SPAWN` + `CAP_SHM`, 13-locale `Help/`):
  the 80×24 grid presented over the AW2 client, parked on one wait-set
  (window-event `Port`, shell-output `Stream`, shell `Child`), token
  dispatch, `lib/keymap`-encoded key presses, pump-and-present on shell
  output, clean teardown on end-of-stream / child exit / close. The
  program-library popup's terminal entry spawns it
  (`plans/NEW-TASKBAR.md` T5); the app event-mailbox
  naming rule is now `tairix_window::event_endpoint_for` (one definition,
  shared with the files app), and the cascade placement is
  `tairix_desktop_session::windows::cascade_origin_for` (shared with the
  vertical's click script).
- The autoload QEMU vertical drives the AW4 tail after the AW3
  click-through: the Library button → the popup's terminal entry
  (`plans/NEW-TASKBAR.md` T5), the terminal-window click gated on the
  third window-frame map (the terminal's create — map counts track
  window creation, never repaints), `true` + Enter typed at the seat
  keyboard gated on the
  click's deliveries, and guest PASS latching a kernel `ProcessSpawned`
  record observed at/after the Enter press's delivery count — the only
  spawn possible then is the shell executing the typed command, so PASS
  proves the whole keyboard → session → terminal → pipe → shell → spawn
  round trip (the interaction contract lives in the test crate's lib
  target).

### AW5 — CU6 picker-issued one-shot descriptors `[x]`

Done (code + host coverage). What now holds:

- **Kernel one-shot read delegation** (`fd_grant` 90 / `fd_redeem` 91,
  in-place `abi-v1` additions): `fd_grant(fd, pid)` delegates the
  caller's **own** plain read-only, non-directory filesystem descriptor
  to a live task (pid from a kernel-attested source; task ids never
  reused), capturing the grantor's uid + effective capability set beside
  the path; the recipient-owner-bound handle travels in-band and
  `fd_redeem` consumes it **once** (atomically — only after the
  descriptor allocation succeeds) into an
  `OpenBacking::Delegated` entry. Delegated reads (`fs_read` and the
  wired stream arm) are re-authorised through the secured VFS under the
  **grantor's** captured identity on every call; the delegation is
  read-only by construction (write/readdir/stat/truncate/sync/file_map
  and re-delegation all refuse), the grant is dispatcher-audited with
  `CAP_FS_ACCESS`, redemption is unprivileged and audited, and an
  exited recipient's pending delegations are reclaimed. `lib/rt`
  wrappers, `lib/abi-sys` `tairix_sys_*` stubs, and the regenerated C
  header carry the surface.
- **Protocol**: `WindowRequest::PickFile { window_id }` (op 4, status
  reply = acceptance only) and the conclusions
  `WindowEvent::FilePicked { window_id, handle }` (kind 5, non-zero
  handle) / `WindowEvent::PickCancelled` (kind 6). The `lib/window`
  engine keys the pick to the attested owner, enforces one pending pick
  per window (`AlreadyExists`), forwards acceptance through the new
  `WindowHost::pick_requested` bridge (a refusal records nothing), and
  `deliver_event` requires-and-clears the pending pick on a conclusion
  so exactly one conclusion follows each acceptance; the client half is
  `WindowClient::pick_file`.
- **The shared browser engine moved to `lib/browse`** (`tairix-browse`):
  the AW1 model/renderer/path-spelling hoisted out of the files app (its
  package is now the `Run` binary only) because the picker is its second
  consumer, plus the renderer-mirroring row hit-test
  (`render::entry_index_at`/`row_height`) the picker's clicks resolve
  through.
- **The session's trusted picker** (`tairix_desktop_session::picker`):
  `SessionPicker` — one picker slot at a time, a fresh root listing under
  the session's own authority per pick (a refused listing refuses the
  pick), a session-owned window at the deterministic `PICKER_ORIGIN`,
  key (`Down`/`Up`/`Enter`/`Backspace`/`Escape`) and click (shared
  hit-test) navigation, conclusions delivered by the `Run` binary's
  privileged tail (`fs_open` → `fd_grant` to the owner's attested pid →
  `fs_close` → `FilePicked`, any refusal honestly `PickCancelled`), and
  a requesting window's death aborts its pick via the
  `ShellWindowHost` bridge. The session's manifest gained
  `CAP_FS_ACCESS` (AppInfo + kernel pin) — the CU6 trusted-UI widening.
- **The consumer**: `userland/apps/viewer` (`viewer.app`, a
  program-library entry), manifest `CAP_CONSOLE_WRITE` + `CAP_SHM` and
  deliberately **no filesystem capability** — it window-creates, asks
  `pick_file` at startup, redeems the delegated handle, reads at most
  `CONTENT_MAX` bytes through the delegated descriptor, and renders the
  sanitised text (host-tested `content_lines` + themed renderers);
  `Enter` re-picks, cancellation shows a notice.
- Coverage: kernel grant/redeem/delegated-read/withdraw unit tests, the
  window-protocol round-trip/fail-closed tests (decoders remain in
  `fuzz_decode`), the `lib/window` loopback pick suite, the `lib/browse`
  hit-test tests, the session picker suite, and the viewer engine tests.
- **Remaining:** extending the autoload QEMU vertical with a
  picker-driven stage (menu → `Viewer` → picker clicks → delegated read,
  gated on the `fd_grant`/`fd_redeem` audit records before the typing
  gate) — every delivery count, reply index, and cascade slot in the
  AW3/AW4 interaction contract shifts, so it is staged as its own
  increment rather than landed blind.

### AW6 — app-owned popup surfaces `[x]`

Done. What now holds:

- **Protocol**: `WindowRequest::CreatePopup { parent_window_id,
  shm_handle, event_endpoint, frame_count, width_px, height_px,
  stride_bytes, format, offset_x, offset_y }` (op 11) — an undecorated,
  parent-anchored surface any app opens for an overlay its own window
  would otherwise clip. Its own variant, not extra `Create` fields,
  because a popup carries no title (never a taskbar entry) and no
  `resizable` flag. The frame-layout block is shared verbatim with
  `Create`/`Resize`, so one definition validates the geometry of all
  three. The reply is the `Create` reply shape (minted id + the serving
  session's `ProcId`).
- **Placement is parent-relative; the session resolves and clamps it.**
  An app is never told its own window's screen position, so the offsets
  are physical pixels from the **parent's client origin** and any signed
  value is legitimate. `ShellWindowHost::popup_opened` adds them to the
  parent's live client origin and clamps the whole popup on screen with
  the shared `tairix_geometry::Rect::clamped_onto` — hoisted out of the
  taskbar's `BarMenu::layout`, which now calls it, so there is one
  placement clamp.
- **Engine rules** (`lib/window`): no kernel caller; the parent must be a
  live window the attested caller owns (foreign *and* unknown parents both
  answer `NotFound`); the geometry is validated exactly as `Create`; the
  popup counts against the same `WINDOWS_PER_CLIENT_MAX` budget; the host
  is told before anything is committed, so a refusal leaves no record, no
  id consumed, and the mapping dropped. One shared `PopupSpec` describes
  the request on both halves (`WindowClient::create_popup`).
- **Undecorated by construction**: the session opens it through
  `DesktopShell::open_popup_window` (compositor `add_window` + `raise` +
  focus), which never decorates and never opens a taskbar entry — the path
  the trusted picker already used. No protocol "undecorated" bit exists.
- **Stacking**: the session holds the parent→popup link on its own
  `WindowRecord` and re-asserts `raise(parent)` then `raise(popup)` once
  per wake immediately before `present`
  (`SessionWindows::keep_popups_stacked`, beside `LockOverlay::
  keep_topmost`), so nothing raised earlier in the frame lands between
  them. No new compositor primitive.
- **Lifetime**: closing the parent (channel close, frame control, or
  `client_exited`) tears down every popup keyed to it; closing a popup's
  own id tears down only the popup. The session clears the link on both
  paths.
- **Consumer**: the terminal's context menu and settings sheet are each a
  popup, sized from the overlay's own preferred extent rather than the
  window's, with popup-local event coordinates demultiplexed on
  `WindowEvent::window_id` (`plans/GUI-TERMINAL.md` §9).
- Coverage: `lib/abi` round-trip + fail-closed decode tests (reserved
  tail, zero ids, reserved endpoint, signed/negative offsets),
  `lib/window` loopback popup suite (round trip, present/close on the
  popup id, foreign/unknown parent, the shared cap, kernel refusal, a
  refused host committing nothing, parent-close cascade, dead-client
  teardown), the session suite (undecorated + off-taskbar placement at the
  parent's client origin, screen clamp, unknown parent, popup close
  leaving the parent's task, re-glued stacking after an intruder raise),
  and `lib/geometry`'s `clamped_onto` tests.

## 2. Documentation

`docs/src/desktop/apps.md` carries the app-side design as each stage
lands; the window protocol joins `docs/src/desktop/wm.md` (AW2/AW3); the
ABI additions keep `docs/src/architecture/syscalls.md` and the generated C
headers current in the same change (§9, §13).

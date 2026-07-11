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
- **Known kernel gap — no stream/pipe wait source.** `WaitSourceKind` is
  today `{Endpoint, Irq, Child, SeatInput}`: a windowed terminal must wake
  on "the shell wrote output" as well as on window input, so the terminal
  stage extends `WaitSourceKind` **in place** (abi-v1 is not frozen,
  §2.13) with a stream-readable source, conformance-tested like the D7a
  members. It is added in the stage that consumes it, never ahead
  (§5.2's discipline applied to ABI surface).
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

- `rustos_abi::fs::DirEntries` — the **one** whole-stream walker over the
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
  line (`rustos_rt::read_dir_all`) and lands wired inside the files `Run`
  binary in AW3, exactly as staged consumers landed for the encrypted-swap
  layer and the SP11 stack spans.

### AW2 — the window protocol + engine `[ ]`

- `lib/abi::window_ipc`: the fixed-width, versioned request/reply/event
  vocabulary (create window with a granted shm surface, present with
  damage, close; focus/key/pointer events app-ward), `display_ipc`-shaped:
  bounded fail-closed decode, reply status frames, fuzz harness enrolled.
- A `lib/*` engine crate with both halves (the `lib/display` precedent):
  the server side the desktop session composes (window table keyed by the
  kernel-attested requesting task, per-window surface bookkeeping,
  fail-closed teardown on client exit) and the client side an app links
  (connect, create, present, parked event wait). Host-tested against an
  in-process loopback; no kernel change.

### AW3 — session window server + the files app goes live `[ ]`

- The desktop session binds the window endpoint (squat-protected, the
  `DISPLAY_ENDPOINT` reservation precedent), serves it from its existing
  waitset loop, and composes served windows into `DesktopShell`
  (`open_window`/`close_window`, focus mirroring, taskbar task entries —
  all already landed).
- `userland/apps/files` gains its `Run` binary + signed `AppInfo` bundle
  (manifest: `CAP_FS_ACCESS` + the window-channel class), wiring
  `VfsDirectorySource` over `rustos_rt::read_dir_all` and presenting
  through the AW2 client. Start-menu launch entry spawns it.
- The QEMU autoload vertical grows pointer-**button** injection and a
  second verified screendump: click the start menu, launch the browser,
  assert the window's presence (and carry the staged theme-toggle
  screendump from `plans/DISPLAY.md` D7's follow-on on the same runner
  work).

### AW4 — the terminal goes live `[ ]`

- `WaitSourceKind` gains the stream-readable source (in-place, owner- and
  descriptor-checked at member add, conformance-tested like `SeatInput`).
- The terminal's production `ShellSource`: `pipe_create` twice, spawn the
  user's shell with `FdWire`-attached standard streams (the elsh
  `wireplan` machinery is the precedent), drain/write over
  `stream_read`/`stream_write`, `Child`-source teardown when the shell
  exits. Host-tested over an injected pipe/spawn seam.
- `userland/apps/terminal` gains its `Run` bundle (manifest:
  `CAP_PROC_SPAWN` + the window-channel class) and its vertical: type into
  the windowed terminal at the seat keyboard, assert the shell round trip.

### AW5 — CU6 picker-issued one-shot descriptors `[ ]`

- The user-mediated file picker: the session (the trusted UI) opens the
  chosen file under *its* authority delegated one-shot to the requesting
  app across the window channel — the CU6 remainder, designed against the
  then-live AW2/AW3 machinery (`plans/CAPABILITY_USE.md` CU6).

## 2. Documentation

`docs/src/desktop/apps.md` carries the app-side design as each stage
lands; the window protocol joins `docs/src/desktop/wm.md` (AW2/AW3); the
ABI additions keep `docs/src/architecture/syscalls.md` and the generated C
headers current in the same change (§9, §13).

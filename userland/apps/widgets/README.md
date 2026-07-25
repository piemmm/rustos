# `tairix-widgets` — Reactive Alloy widget gallery

A first-class demo desktop app (`AGENTS.md` §10,
`plans/GUI-CONTROLS-DESIGN.md`). It showcases every shared TAIRiX GUI
control on its own tab, each with several role/state/value variations, so
the full behaviour of the `lib/controls` widget set is visible and
interactive in one place. Installed as a `.app` bundle in the system app
store (`AGENTS.md` §16.2/§16.5).

## What this crate is

Two targets:

- a host-tested **gallery model** `[lib]` (`tairix_widgets`) — the shared
  home for everything with behaviour worth testing: which tab is shown,
  how a panel of demo widgets lays out within the window client rectangle,
  and how a routed pointer/key event reacts a widget's own typed state back
  into it. It composes the shared `lib/controls` controls, `lib/theme`
  tokens, `lib/geometry` coordinates, `lib/font` text, `lib/input`
  vocabulary, `lib/raster` surface, and `lib/icon` glyphs. It is `no_std` +
  `alloc`, so it links unchanged into the freestanding binary and runs
  under the host test harness (`AGENTS.md` §2.2, §7);
- the `Run` entry-point **binary** (`tairix-widgets-run`) — a thin
  freestanding pure-Rust program that composes the gallery over the live
  window channel, exactly as `userland/apps/files` composes `lib/browse`.

Every widget the gallery shows is drawn and driven by the shared
`lib/controls` crate; the gallery adds no second control implementation
(`AGENTS.md` §2.2).

## What the program wires

One `shm_create`d frame region granted to the reserved window endpoint
(the zero-copy window surface the desktop session maps once at create),
one `port_bind`-bound event mailbox the app **parks** on through its
wait-set (never a busy-poll; every accepted event is authenticated against
the kernel-attested session identity the create reply named), and the
`WindowClient` create/present/close calls. On the host the binary is an
inert stub so `cargo build --workspace`, clippy, and fmt still cover it.

The controls perform no privileged work: the gallery is their owner and
simply reflects each control's typed action back into it, so the demo
needs no ambient authority — only `CAP_CONSOLE_WRITE` (fail-loud
diagnostics) and `CAP_SHM` (the window frame region).

## Stability

`experimental`.

# `rustos-top`

A live process-overview TUI for RustOS, in the spirit of the Linux `top`, and
the first in-tree consumer of the OS curses library (`plans/CURSES.md` Stage
C5). It draws a scrolling, selectable process list that refreshes on demand.

`top` reads the same `sysinfo-v1` process list as `ps` — RustOS has no `/proc`
to scrape (`AGENTS.md` §16.6) — and draws it through `lib/curses`'s screen
model rather than emitting escape sequences by hand (`AGENTS.md` §2.2).

## What it shows

- A status line: the process count and the active scope (`own` / `all`).
- The shared `lib/procinfo` column header and one row per process (PID, PPID,
  UID, GID, state, CPU, name) — the same columns `ps` prints, rendered once in
  `lib/procinfo`, not re-implemented here.
- A selection highlight, a scrolling viewport, and a `?` help overlay.

## Keys

| Key            | Action                                   |
| -------------- | ---------------------------------------- |
| `up` / `down`  | move the selection                       |
| `PgUp` / `PgDn`| move a page                              |
| `Home` / `End` | jump to the first / last process         |
| `a`            | toggle all processes ↔ your own          |
| `r`            | refresh now                              |
| `?` / `h`      | toggle the help overlay                  |
| `q`            | quit                                     |

The system-wide view (`a`) issues `GLOBAL_PROCESS_LIST`, which `sysinfod`
gates on `CAP_SYSINFO_GLOBAL`; without it the toggle surfaces
`TopError::PermissionDenied` rather than a partial listing (fail closed,
`AGENTS.md` §5.4). The capability check lives in the service, never here.

## How it is built

- **`model`** — the I/O-free view state (`Model`): the process snapshot, the
  selection, the scroll offset, the scope, and the help flag, plus how an
  input `Event` moves them. Pure, so it is exhaustively unit-tested.
- **`app`** — `render` draws the model into curses windows (the help overlay
  is a second window composited on top through the same renderer — no panel
  machinery, `AGENTS.md` §2.3), and `run` is the event-driven loop tying the
  `Screen` driver, the model, and the `sysinfo` transport together.

Both the `sysinfo` transport and the curses tty are object-safe seams, so the
whole viewer runs against in-memory fixtures with no kernel (`AGENTS.md` §7).

## Linking

`top` is an OS-bundled app, so it **dynamically links** the OS-provided
curses/terminal library (`lib/curses`, the curated `/System/Libraries/`
Terminal/TUI class, `AGENTS.md` §16.4) rather than compiling it in. In the
workspace this is an ordinary cargo path dependency; the dynamic-vs-static
distinction is the OS-image runtime linking model the charter governs.

## Layering & safety

`no_std` (with `alloc`). It links only `lib/*` crates — `lib/abi`,
`lib/procinfo`, and the OS `lib/curses`/`lib/termcap`/`lib/vt` — never a
kernel or driver crate (`AGENTS.md` §17.4). No `unsafe`, no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9), and nothing
writes to fd 3 (`stdinfo`, §20).

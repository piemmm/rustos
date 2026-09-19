# `tairix-settings` — Settings (`settings.app`)

The windowed application that configures the desktop and the machine, opened
from the *Settings…* row of the Switchboard capsule's system quick-actions
menu. Staged in `plans/NEW-DESKTOP-SETTINGS.md`; the surface and its authority
map are documented at `docs/src/desktop/settings.md`.

**Stability tier:** `experimental`.

## What it is

Two targets in one crate, the shape every windowed first-party app here takes:

- the `[lib]` (`tairix_settings`) is the host-tested shell — the closed pane
  registry, the frame resolver, the sidebar/search/trail navigation, the
  composed Appearance and Accessibility forms, the Storage pane's volume
  cards, and the one renderer for a pane that states how the machine
  actually stands;
- the `[[bin]]` (`src/run.rs`) is the on-disk bundle's `Run` entry point,
  which composes that shell over the window channel. It is a freestanding
  pure-Rust program on the Tier-1 bare-metal targets and an inert stub on the
  host, so `cargo build --workspace`, clippy and fmt still cover the file.

## Required capabilities

`CAP_CONSOLE_WRITE` and `CAP_SHM`, and nothing else — the same class as the
widget gallery. Settings holds no domain authority: it renders state and hands
a typed intent to the process that already owns the domain, so there is no
capability in it to escalate with. `AppInfo.toml` records why each request is
there.

## What the surface guarantees

- **Every category is reachable, and every absence is honest.** A category
  this system cannot serve says so and names what would have to exist; one it
  can serve but whose controls this stage does not compose says where the
  setting is reached instead. No control that would change nothing is ever
  drawn.
- **Appearance and Accessibility are two views of one registry.** Light/dark
  is Appearance's alone; contrast, density, motion and the interface scale
  appear in both, from one row definition, because a reader looks for them in
  either place. Each row commits on the choice and posts **only the keys it
  edits**, which the session merges over what it already holds — so a
  wallpaper change and an appearance change cannot undo each other.
- **A change is asked for, never written, and never on the loop.** The apply
  goes to a worker; the rows show the reader's choice at once and adopt the
  durable value when the session answers, so a refusal states its reason and
  puts the row back rather than leaving a value the next login would not
  restore.
- **Storage reports what the machine holds and changes nothing.** One card
  per mounted volume — its name, where it is mounted, its filesystem, device
  and medium, a capacity track, and its banded health — derived by the one
  shared volume view model (`lib/procinfo`), so a disk cannot read half full
  here and nearly full in the Switchboard. A volume whose format tracks no
  capacity says so rather than drawing a bar of nothing. The mount walk is an
  IPC round trip and runs on a worker: the pane asks when it comes on show
  and draws what has arrived.
- **One registry table is the whole surface.** `registry::CATEGORIES` is the
  single definition of the sidebar strip, the search index, the location
  trail, the keyboard cursor and the pane dispatch, so a category cannot exist
  without a row or a row without a pane.
- **No second control, theme or rasteriser.** Every pixel is a shared
  `lib/controls` control drawn from the shared theme.

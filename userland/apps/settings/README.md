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
  composed Appearance, Accessibility and General forms, the Storage pane's
  volume cards, the read-only fact columns, the staged-apply model with its
  action band, and the one renderer for a pane that states how the machine
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
- **A machine setting is staged, and applied by the tool that owns it.**
  Login & startup and Caching read `system.conf` through the ungated
  `SYSTEM_CONFIG` query and parse it with `lib/sysconfig` — the engine
  `configure` writes through — so a row and the tool cannot disagree about
  what the store admits. A choice edits a **working copy**: the pane's action
  band says how many rows differ, Revert puts them back, and Apply asks for
  an account once and runs `configure` once with every changed key, so the
  document is rendered a single time and a group can never be left half
  written. A refusal leaves the working copy standing and states why; only a
  run that succeeded makes the window re-read the store. A row with no
  reading yet says so rather than showing defaults the reader could not have
  set, and the master caching switch's ceiling is stated on the rows it takes
  away rather than silently rewriting their values.
- **About and Date & Time measure, and say when they could not.** Each figure
  is its own ungated query, so one refusal costs one row. Date & Time's band
  starts `datetime.app` as an authenticated account and leaves it running —
  the clock is that application's to set, not this one's.
- **One credential question, shared with the desktop.** The account and
  password are asked for through `lib/controls::CredentialSheet`, the same
  surface the session puts up for a command it may not perform: one focus
  order, one wording, one place the secret lives. It is modal while it is up,
  so a press behind it cannot change a pane the reader is about to
  authenticate for, and the offered password is zeroed as soon as the
  exchange resolves.
- **One registry table is the whole surface.** `registry::CATEGORIES` is the
  single definition of the sidebar strip, the search index, the location
  trail, the keyboard cursor and the pane dispatch, so a category cannot exist
  without a row or a row without a pane.
- **No second control, theme or rasteriser.** Every pixel is a shared
  `lib/controls` control drawn from the shared theme.

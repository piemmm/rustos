# `tairix-taskpins` — the desktop pinned-shortcut store engine

`lib/taskpins` is the shared per-user pinned-shortcut store engine for the
desktop taskbar. The pin list is **data on the volume**, never a compiled-in
table: a per-user store at `tairix_taskpins::user_pins_path(user)`
(`/Users/<u>/Settings/Taskbar/pins.conf`). This crate owns the ordered pin
list model, the line grammar and its closed key registry, the bounded
fail-closed parser, the canonical render, and the pin/unpin/reorder
operations every producer and consumer shares — so the taskbar that draws the
pins and the session settings that edit them can never disagree about what the
shortcut list says.

## The store

One text document per user: `entry <entry-id>` or `bundle <bundle-path>`
lines, one per pin in display order; `#` begins a comment to end of line;
blank and comment-only lines carry no setting. Key and value are split at the
first whitespace run.

An **absent** store is not an error: it means "no pinned shortcuts"
(`PinList::default`). Pins are per-user state only; there is no machine-wide
pin store.

## The registry

Every line is drawn from the closed `PinKey` set:

| Key      | Value                                   | Meaning |
|----------|-----------------------------------------|---------|
| `entry`  | a program-library catalog entry id      | a pin referencing a catalog entry |
| `bundle` | an absolute `.app` path                 | a direct pin of an application bundle |

Uniqueness is enforced: a target is either pinned or it is not. Adding a
key means adding a `PinKey` variant plus its parse/render arms in the same
change; there is no free-form key namespace and no second store.

## Security

A pin store is **untrusted input** to every consumer. The parser is bounded
(`MAX_PINS_LEN`, `MAX_PINS`, `MAX_LINE_LEN`), validates every target through
the model's own validators, and refuses the **whole** document (`PinsError`,
carrying the offending 1-based line where one is meaningful) on anything it
does not fully understand: an unknown key, a duplicate pin, an over-long
line, or a malformed target. A half-read list would silently drop or
reorder pins a user expects to find, so a reader that cannot fully parse a
store runs on the empty list rather than guessing at a partial intent, and a
writer refuses the edit outright.

The engine performs no I/O and holds no authority: reading and writing the
document goes through the secured VFS under the caller's own kernel-attested
identity — a per-user store is an ordinary write into the user's home.

## Operations

`PinList` maintains the display order and uniqueness invariants through its
public methods:

- `pin(target)` / `pin_at(index, target)` — append or insert a new pin.
- `unpin(index)` — remove by index.
- `move_pin(from, to)` — reorder a pin using the "remove then insert at clamped
  destination" model.
- `position(target)` / `get(index)` / `iter()` / `len()` / `is_empty()` —
  inspection.

Operations fail closed with `PinError::Full` at `MAX_PINS` rather than growing
without bound, and `PinError::AlreadyPinned` on duplicate targets.

## API shape

- `parse(&str) -> Result<PinList, PinsError>` — the bounded, fail-closed,
  line-numbered parse.
- `render(&PinList) -> String` — the canonical document (one line per pin in
  order), so render→parse round-trips exactly.
- `PinList::{new, pin, pin_at, unpin, move_pin, position, get, iter, len,
  is_empty}` — the store operations.
- `PinTarget` — the target union (`Entry` / `Bundle`); `PinError` — refusal
  reasons (`AlreadyPinned` / `Full`).
- `PinKey::{ALL, as_str, from_id}` — the closed key registry.
- `PINS_SETTINGS_SUBDIR` / `PINS_FILE` / `user_pins_path` — the path
  spellings, defined once here.

The crate is `no_std` + `alloc`, performs no I/O, holds no authority, is
host-unit-tested beside the code, and is fuzzed by `tests/fuzz_taskpins.rs`.
Stability tier: experimental (`lib/taskpins/README.md`). The staged design is
`plans/NEW-TASKBAR.md`.

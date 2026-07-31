# tairix-taskpins

Stability tier: **experimental**.

The desktop pinned-shortcut store engine: the ordered pin list, its on-disk
grammar, fail-closed bounded parse, canonical render, and the pin/unpin/reorder
operations. It defines the validated pin model (`PinList`, `PinTarget`), the
line grammar and its closed key registry (`entry`, `bundle`), the bounded
fail-closed parser, and the canonical render.

The pin store is data on the volume, never a compiled-in table: a per-user
store at `/Users/<u>/Settings/Taskbar/pins.conf` ([`user_pins_path`]). The
taskbar that draws the pins and the session settings that edit them both go
through this engine, so a writer and a reader can never disagree about what the
shortcut list says. An absent store means "no pinned shortcuts", not an error.
Pins are per-user state only; there is no machine-wide pin store.

A pin store is untrusted input: the parser is bounded ([`MAX_PINS_LEN`] /
[`MAX_PINS`] / [`MAX_LINE_LEN`]) and refuses the **whole** document
([`PinsError`], with the offending line) on anything it does not fully
understand — an unknown key, a duplicate pin, an over-long line, or a malformed
target. A half-read list would silently drop or reorder pins a user expects to
find, so a reader runs on the empty list rather than guessing at a partial
intent, and a writer refuses the edit outright. Uniqueness is enforced: a
target is either pinned or it is not.

The crate performs no I/O and holds no authority: file access goes through the
secured VFS under the caller's own kernel-attested identity — a per-user store
is an ordinary write under that user's own identity. Launching a pin remains
subject to the loader's signature and capability gate; the model validation
here is the earlier, cheaper refusal.

`no_std` + `alloc`; host-unit-tested beside the code and fuzzed by
`tests/fuzz_taskpins.rs`. The staged design is `plans/NEW-TASKBAR.md`; the
subsystem page is `docs/src/lib/taskpins.md`.

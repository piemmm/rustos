# tairix-datetime — the Date & Time app (`datetime.app`)

Stability: **experimental**

The windowed application that shows the machine's wall clock in six
editable civil fields and steps the clock to what they say
(`plans/NEW-TASKBAR.md` T17).

## What is where

- **`src/lib.rs` — the engine.** The six fields, their per-field
  validation, the `Time64` instant they compose, and the one `Status` line
  every surface states a result through. `no_std` + `alloc`, so it links
  unchanged into the freestanding binary and is unit-tested on the host.
- **`src/view.rs` — the window.** Its geometry (a three-column grid: the
  date on the first row, the time on the second) and its paint, composed
  from the shared `lib/controls` dialog and text field. Every length is
  authored in logical pixels and converted through the one shared
  `tairix_geometry::Scale`.
- **`src/run.rs` — the `Run` binary.** The on-disk bundle's entry point:
  one granted frame region, one event mailbox parked on a wait-set, and
  the `WindowClient` calls. An inert stub on the host.

## The authority, and why it is asked for elsewhere

Stepping the clock needs `CAP_TIME_SET`. This bundle's signed manifest
requests it, and the kernel grants `manifest ∩ the launching account's
ceiling` — so what the app can actually do depends on *who started it*.

A desktop session holds no such capability and must never hold one. It
re-authenticates an account that does through its console's elevation
broker, and the broker starts this program as that account
(`plans/CAPABILITY_USE.md` CU5). The app itself performs no
authentication and asks for no elevation.

A refused set is therefore an ordinary outcome, not a defect: the app
states it in its window **and** on `stderr`, leaves the clock untouched,
and keeps running. It never reports a clock it did not change as changed.

## No calendar of its own

Seeding decomposes an instant with `CivilTime::from_time64`; committing
composes one with `days_from_civil` — the exact inverse, from the same
`lib/fsmeta` calendar the desktop clock and `ls`'s date column read.
There is no second day-counting rule here and no leap-year table of this
app's own.

## An unset clock shows nothing

A machine whose wall time has never been established reports
`WallTimeState::Unset`, whose instant is the epoch placeholder and means
nothing. The fields are left empty and the window says so, rather than
presenting `1970-01-01` as a reading the user is invited to correct.

## Refusals are stated, never smoothed over

Validation refuses the whole edit on the first fault and names it — a
month outside 1–12, an hour outside 0–23, a minute or second outside
0–59, a day that does not exist in the entered month and year. Nothing is
clamped, wrapped, or saturated into range, because that would set a time
the user did not ask for. Dates before 1970 and beyond 2038 are ordinary
input: the instant is a 64-bit `Time64`.

## Tests

`cargo test -p tairix-datetime` covers the engine: seeding from a set and
an unset reading, the seed → compose round trip (including 1900, 2100,
the 2^32-second instant in 2106, and year 9999), a negative year, every
missing and out-of-range field, the leap-day and leap-century rules, the
character filter, the typing bound, the tab cycle, and every fault and
status stating a reason.

The freestanding `Run` body is not compiled by the host tooling, so it is
checked with
`cargo clippy -p tairix-datetime --target aarch64-unknown-none -Zbuild-std=core,alloc`.

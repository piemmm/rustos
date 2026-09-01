//! Strictly justified shared utilities.
//!
//! Per every item in this crate must be used by at least
//! two independent callers outside `lib/util` itself. The crate exists in
//! Stage 1 as the *destination* for such items as they are extracted out
//! of subsequent stages — adding speculative helpers ahead of demand is
//! explicitly forbidden.
//!
//! Members:
//!
//! * [`conf`] — the `#`-comment line grammar every line-oriented
//!   configuration store shares, consumed by `lib/sysconfig`,
//!   `lib/netconfig`, and `userland/system/init`'s service registry and
//!   startup list, so a comment is recognised the same way in all of them.
//! * [`cfloat`] — C-locale `printf(3)` floating-point rendering, consumed
//!   by the `seq` and `printf` command apps (`plans/APPS.md`), so C's
//!   rounding, flag, and padding rules exist in exactly one place.
//! * [`cnum`] — C-locale `strtod(3)` scanning (decimal and hexadecimal
//!   floats, `inf`/`nan`, longest-prefix `endptr` semantics), consumed by
//!   the `seq` and `printf` command apps, so C's number grammar and its
//!   exact one-step rounding exist in exactly one place.
//! * [`mathf`] — bounded `f64` maths (`floor`/`round`, `sqrt`, `sin`/`cos`,
//!   `atan2`) for `no_std` geometry, consumed by the glyph rasteriser
//!   (`lib/fontface`) and the SVG decoder (`lib/svg`), so an external libm
//!   stays out of the trusted computing base and both round and rotate
//!   identically.
//! * [`retry`] — the bounded, doubling one-shot schedule a reader climbs when
//!   the thing it needs appears after it starts and no readiness event says
//!   so, consumed by the clock service (its configuration store and RTC) and
//!   by the service manager (the administrator enrolment overrides on the
//!   encrypted root), so one boot-order problem has one schedule.
//! * [`fmt`] — no-allocation numeric formatters for audit-log fields.
//!   Promoted from `kernel/sec` in Stage 2.5 (`kernel/ipc`) once a second
//!   caller materialised; consumed by `kernel/sec` and `kernel/ipc`.
//! * [`size`] — GNU coreutils-style block-size parsing and human-readable
//!   size formatting, consumed by the `du` and `df` command apps
//!   (`plans/APPS.md`), so the size grammar and ceiling-rounding rules
//!   exist in exactly one place.
//! * [`count`] — GNU coreutils count-with-multiplier-suffix parsing
//!   (`-c`/`-n` values), consumed by the `head` and `tail` command apps
//!   (`plans/APPS.md` §12.1 Stage C), so the GNU multiplier alphabet
//!   exists in exactly one place.
//! * [`secret`] — erasing a password/key buffer so the optimiser cannot
//!   delete the erasure, consumed by the `lib/rt` elevation client, the
//!   shell's `elevate` builtin, the login supervisor's elevation broker,
//!   and `lib/controls`' masked text field, so "the secret is gone" means
//!   the same thing everywhere a credential is marshalled.
//! * [`tailwindow`] — bounded rolling "keep the last N bytes/lines"
//!   windows, consumed by the `head` and `tail` command apps
//!   (`plans/APPS.md` §12.1 Stage C), so the constant-memory window
//!   mechanics exist in exactly one place.
//!
//! The flat device-tree (FDT) parser that once lived here has been folded
//! into the single shared `lib/fdt` reader, which now owns the generic
//! node-iteration API every device-tree consumer uses.
//!
//! Promoting code into this crate requires a `PLAN.md` note documenting
//! the two-or-more concrete callers.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod cfloat;
pub mod cnum;
pub mod conf;
pub mod count;
pub mod fmt;
pub mod mathf;
pub mod retry;
pub mod secret;
pub mod size;
pub mod tailwindow;

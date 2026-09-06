//! The `stalltrace` fixture's shared vocabulary: the command word it installs
//! under and the marker line its `Run` binary prints
//! (`plans/FIX-STALLTRACE.md`).
//!
//! Both are defined here rather than in the program, because the vertical's
//! `tools/xtask` serial script keys on them: the script and the program are
//! two consumers of one definition, so they cannot drift into each other's
//! silence. The program's own failure lines are not here — nothing else
//! reads them.

#![no_std]
#![deny(missing_docs)]

/// The command word the fixture bundle installs under, and the bare word the
/// vertical's script types at the shell.
pub const COMMAND: &str = "stalltrace";

/// The line the fixture prints once it has provoked its overrun.
///
/// The vertical's script waits for this before typing the shell `exit` that
/// completes the PASS chain, so the kernel's report provably reached the
/// transcript before the run ended.
pub const PROVOKED_MARKER: &str = "STALLTRACE PROVOKED";

/// The line the fixture prints instead on an image that compiled the
/// latency diagnostics out, so an inert run is distinguishable from a broken
/// one in the transcript.
pub const INERT_MARKER: &str = "STALLTRACE INERT";

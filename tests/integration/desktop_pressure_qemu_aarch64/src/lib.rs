//! The desktop-under-pressure vertical's shared contract
//! (`plans/SMARTRAM.md`, `plans/ICONS.md`).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witnesses the guest
//! latches can never drift apart.
//!
//! # What the vertical is for
//!
//! Opening windows is how an ordinary user spends memory. Each one costs a
//! frame region on both sides of the window channel plus the shell behind it,
//! so a screenful of terminals on a small machine is enough to take free
//! memory below the mild-pressure watermark — which is exactly where a
//! desktop must keep working. This vertical drives that state on purpose and
//! asserts what the user sees: the icon bar still draws its real artwork.
//!
//! The guest's PASS therefore latches *three* facts, not two, because any one
//! of them alone would let the run pass without testing anything: the
//! application launched, it opened every window the script asked for, and the
//! machine's published pressure band actually left normal. A world that never
//! left normal is not the world under test, and the run fails loudly rather
//! than reporting a pass it did not earn.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the application the script opens windows of — the bundle is
/// `<system application store>/<name>.app`, composed from the shared
/// `lib/abi` spellings on both sides rather than written out here.
///
/// The terminal, because its icon-bar declaration makes a primary click on
/// its slot open another window in the *same* process, so one gesture,
/// repeated, is the whole script.
///
/// It **is** the icon-bar vertical's own subject rather than a second spelling
/// of it: the host launches through the one shared bar-launch reconstruction,
/// so the bundle this PASS gate waits for and the bundle that reconstruction
/// launches are equal by definition and are defined once.
pub use tairix_test_appbar_qemu_aarch64::BAR_APP_NAME;

/// Windows the whole script opens: the one the launch itself opens plus one
/// per slot click after it.
///
/// Thirty-two is the count the defect this vertical pins was reported at, and
/// on the board's default RAM it is comfortably past the mild watermark. The
/// guest does not *require* the band to move at any particular count — it
/// requires it to have moved by the end — so a future change in what a window
/// costs cannot silently turn this into a test of nothing.
pub const WINDOWS_OPENED: u32 = 32;

/// Windows on screen when the display is photographed: one fewer than
/// [`WINDOWS_OPENED`].
///
/// The guest's own witness is a *create reply* on the window endpoint, which
/// the session answers before it has composited the window and announced it —
/// so a dump gated on the last window being on screen would be asked for after
/// the guest had already reported PASS and torn the machine down. Photographing
/// the frame before the last one keeps the guest alive across the readback: the
/// runner holds the click that opens the last window until the dump has been
/// read back and parsed, and that click is what completes the PASS.
pub const WINDOWS_SHOWN_AT_DUMP: u32 = WINDOWS_OPENED - 1;

/// Serial marker the guest emits, once, when the published pressure band first
/// deepens past moderate.
///
/// The artwork assertion is scoped by it. `plans/ICONS.md` keeps the decoded
/// artwork through mild and moderate pressure and **gives it up at severe and
/// critical**, where the built-in glyph is the honest answer — so "the slot is
/// byte-identical" is a true statement about the shallower bands only, and the
/// host needs to know which bands a run actually reached. The band is not
/// steerable (there is no test hook in the pressure path), so the guest reports
/// what it observed rather than the run asserting against the policy.
pub const PRESSURE_DEEPENED_MARKER: &str = "desktop pressure band deepened past moderate";

/// Stable log id of the [`PRESSURE_DEEPENED_MARKER`] record.
pub const PRESSURE_DEEPENED_EVENT: u32 = 4520;

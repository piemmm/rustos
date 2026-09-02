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
//! so terminals on a small machine are enough to take free memory below the
//! mild-pressure watermark — which is exactly where a desktop must keep
//! working. This vertical drives that state on purpose and asserts what the
//! user sees: the icon bar still draws its real artwork.
//!
//! # Windows until the band moves, not a fixed count
//!
//! The target is *relative* — below a fraction of the machine's free memory —
//! so the spend that reaches it cannot be a fixed number of windows. A count
//! that is too small never leaves normal and the run tests nothing; one that
//! is too large runs the machine out of memory, and a window whose surface is
//! refused never opens, so a PASS gated on the count never latches and the run
//! burns to its ceiling. Only a ~34 MB span of post-boot free memory on this
//! board satisfies both at thirty-two windows, and which side of it a boot
//! lands on turns on how much reclaimable cache it happened to accumulate.
//!
//! So the script *bounds* the clicks ([`WINDOW_CLICK_BOUND`]) and the **band**
//! ends the run: the guest reports the moment the published band first leaves
//! normal ([`PRESSURE_LEFT_NORMAL_MARKER`]), the host photographs the screen
//! there, and one further window completes the PASS. The run therefore stops
//! about nine windows short of the refusal that ends the machine, and it stops
//! in the band the artwork assertion is *about*, so that assertion always
//! applies instead of being scoped out of the deep-pressure runs it used to
//! reach.
//!
//! The guest's PASS latches *three* facts, not two, because any one of them
//! alone would let the run pass without testing anything: the application
//! launched, the machine's published pressure band actually left normal, and
//! the desktop served a further window *after* it did. A world that never left
//! normal is not the world under test, and the run fails loudly rather than
//! reporting a pass it did not earn.

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

/// Most windows the script will ask for: the one the launch itself opens plus
/// one per slot click after it.
///
/// A **bound**, not a target. The run ends when the pressure band moves, so
/// slack here costs nothing and only a bound that is too *small* can fail a
/// run — by never reaching the mild watermark. Sixty-four is well past the
/// ~55 windows a board that booted into an entirely free 177 MiB would need
/// to cross it at some 2.6 MiB of retained picture per window, so it holds
/// even if a future change makes a window cheaper.
pub const WINDOW_CLICK_BOUND: u32 = 64;

/// Windows the desktop must still serve *after* the published band leaves
/// normal for the guest's third witness to be in.
///
/// Two, because one scripted click can already be in flight when the band
/// moves — it is gated on the previous window reaching the screen, and the
/// band may move just after that record is written. The click after it waits
/// on a record written long after the marker, which the host cannot see
/// without having seen the marker too, so it is held for the frame. One
/// window can slip past the marker and a second cannot, which is what keeps
/// the guest alive across the host's readback.
pub const WINDOWS_AFTER_PRESSURE: u32 = 2;

/// Serial marker the guest emits, once, when the published pressure band first
/// leaves normal — the moment the machine enters the state this vertical
/// exists to photograph.
///
/// The host gates its under-pressure screendump on it. That is what keeps the
/// frame inside the bands `plans/ICONS.md` promises the decoded artwork
/// through: a window costs some 2.6 MiB against watermarks a tenth of the
/// board apart, so the first reading above normal is mild, and the assertion
/// is a true statement about the frame it judges rather than one scoped out
/// whenever the run went deeper.
///
/// A pending screendump holds every later pointer step, so the guest cannot
/// finish before the frame has been read back: the click that opens the window
/// completing the PASS is the first one released afterwards.
pub const PRESSURE_LEFT_NORMAL_MARKER: &str = "desktop pressure band left normal";

/// Stable log id of the [`PRESSURE_LEFT_NORMAL_MARKER`] record.
pub const PRESSURE_LEFT_NORMAL_EVENT: u32 = 4521;

/// Serial marker the guest emits, once, if the published pressure band ever
/// deepens past moderate.
///
/// Diagnostics, not a gate. The run stops one window after the band leaves
/// normal, so it should never reach severe — and if the artwork assertion ever
/// does fail, this record in the transcript is what distinguishes "the desktop
/// dropped its artwork when it should have kept it" from "the machine fell far
/// enough that dropping it was the honest answer" (`plans/ICONS.md` gives the
/// cache up at severe and critical). Written straight to the serial sink
/// because it is emitted inside an audit callback, where a log event would
/// re-enter the sink that called it.
pub const PRESSURE_DEEPENED_MARKER: &str = "desktop pressure band deepened past moderate";

/// Stable log id of the [`PRESSURE_DEEPENED_MARKER`] record.
pub const PRESSURE_DEEPENED_EVENT: u32 = 4520;

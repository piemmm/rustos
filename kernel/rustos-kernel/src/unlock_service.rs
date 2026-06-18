//! The in-kernel root-unlock service (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (2)).
//!
//! Once the boot path has bound the bootstrap root block device
//! ([`crate::root_storage`]) and the console keyboard is live, the
//! encrypted root must be unlocked — the operator types the passphrase
//! ([`crate::root_mount::unlock_root_disk_interactively`]) — before
//! `login` can authenticate (the loaded database is published into
//! [`crate::root_mount::LATE_USERS_DB`]). A blocking console read before
//! the dispatch loop runs would deadlock, so the unlock runs as a
//! **scheduler kthread** admitted at the init seam, exactly like the
//! USB-keyboard service ([`crate::keyboard_service`]).
//!
//! This module is split into two halves (`AGENTS.md` §2.2):
//!
//! * a host-compiled, host-tested, device-independent core — the
//!   post-MMU boot stash ([`record_boot`] / [`take_boot`]) carrying the
//!   resolved [`RootBlockBinding`] and the firmware DTB pointer to the
//!   init seam, plus the console-0 ownership gate ([`Console0Gate`] /
//!   [`CONSOLE0_GATE`] / [`GatedConsoleRead`]) that keeps `login` from
//!   stealing the passphrase bytes while the unlock is in progress; and
//! * a `#[cfg(all(freestanding, kernel_isa = "aarch64"))] mod metal`
//!   that performs the live virtio-blk-MMIO bring-up and runs the unlock
//!   policy inside the kthread (the QEMU `virt` path; EMMC2 on the
//!   Raspberry Pi 4 is the staged metal increment).

use rustos_abi::Errno;
use rustos_kernel_core::ConsoleRead;
use rustos_sync::SpinLock;

use crate::root_storage::RootBlockBinding;

/// The audit message the unlock kthread logs once it has brought the root
/// block device up, mounted the encrypted root, and installed the users
/// database into [`crate::root_mount::LATE_USERS_DB`] (the `UNLOCK_SERVICE`
/// event, logged from `metal::run_unlock`'s caller).
///
/// Exposed as a stable `pub const` so the `-M virt` admission vertical can
/// key its PASS on the production message — the witness that the in-kernel
/// kthread (not a directly-driven policy) reached a mounted, installed root
/// — without re-declaring the literal (`AGENTS.md` §2.2).
pub const USERS_DB_INSTALLED_MESSAGE: &str =
    "root-unlock: users database installed; login can authenticate";

/// The boot facts the init seam hands the unlock kthread: which discovered
/// node bound the root block driver, and the firmware device-tree pointer
/// the live bring-up walks.
///
/// Carried by value (a [`RootBlockBinding`] is a fixed-size record and the
/// DTB pointer is a `u64`) so the init seam reads it once without holding a
/// lock across the kthread admission.
#[derive(Copy, Clone)]
pub struct UnlockBoot {
    /// The resolved root block binding, or [`None`] when no single block
    /// device was bound (headless / no disk / ambiguous — the unlock is a
    /// no-op and `login` finds no accounts, `AGENTS.md` §18.4).
    pub binding: Option<RootBlockBinding>,
    /// The firmware/loader device-tree pointer (`0` when none was handed
    /// over), used by the live bring-up to construct the virtio-MMIO bus
    /// and resolve the device's GIC SPI.
    pub dtb: u64,
}

impl UnlockBoot {
    /// The empty stash: nothing bound, no device tree.
    const EMPTY: Self = Self {
        binding: None,
        dtb: 0,
    };
}

/// Post-MMU boot stash the boot path fills and the init seam drains.
///
/// Set once after the MMU is enabled (the `SpinLock`'s atomic
/// read-modify-write is UNPREDICTABLE on the MMU-off Device memory the
/// boot CPU runs on, `plans/PI.md` P6c-2 — the same constraint as
/// [`crate::keyboard_service`]'s discovery stash), read once at the init
/// seam. Single producer, single consumer, so the lock never contends.
static UNLOCK_BOOT: SpinLock<UnlockBoot> = SpinLock::new(UnlockBoot::EMPTY);

/// Record the resolved root binding and the firmware DTB pointer for the
/// init seam.
///
/// MUST be called **after** the MMU is enabled (see `UNLOCK_BOOT`).
pub fn record_boot(binding: Option<RootBlockBinding>, dtb: u64) {
    *UNLOCK_BOOT.lock() = UnlockBoot { binding, dtb };
}

/// Read the boot stash once at the init seam.
#[must_use]
pub fn take_boot() -> UnlockBoot {
    *UNLOCK_BOOT.lock()
}

/// The console-0 input ownership gate (`plans/PI.md` P11 Chunk B-2 item 5).
///
/// Both the in-kernel unlock kthread and the per-console `login` would
/// otherwise drain console index 0's input concurrently, racing for the
/// passphrase bytes. The gate resolves that without an ABI change: the
/// console-0 `login` reads through a [`GatedConsoleRead`] that yields no
/// input (so kernel-core's `BlockingConsoleRead` parks the login) until
/// the gate is **opened**, while the unlock kthread reads the raw device
/// directly. The kthread opens the gate the instant the unlock resolves
/// (installed or gave up) — and immediately when there is no disk to
/// unlock — so `login` then takes over console 0 with no byte contention.
///
/// It is a one-way latch (closed → open, never back), so once `login`
/// owns the console no later code can re-gate it (`AGENTS.md` §5.4 — fail
/// closed: a gate that never opened would only ever *withhold* input, it
/// can never grant unauthorized access).
pub struct Console0Gate {
    open: core::sync::atomic::AtomicBool,
}

impl Console0Gate {
    /// A fresh, **closed** gate: console-0 input is withheld from `login`
    /// until the unlock kthread opens it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Open the gate, releasing console-0 input to `login`. Idempotent.
    pub fn open(&self) {
        self.open.store(true, core::sync::atomic::Ordering::Release);
    }

    /// Whether console-0 input has been released to `login`.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.load(core::sync::atomic::Ordering::Acquire)
    }
}

impl Default for Console0Gate {
    fn default() -> Self {
        Self::new()
    }
}

/// The single `'static` console-0 ownership gate (see [`Console0Gate`]).
///
/// The boot path's console-0 read half is a [`GatedConsoleRead`] over this
/// gate; the unlock kthread opens it once the unlock resolves.
pub static CONSOLE0_GATE: Console0Gate = Console0Gate::new();

/// A [`ConsoleRead`] adapter that withholds input until a [`Console0Gate`]
/// is opened, then delegates to the wrapped device.
///
/// While the gate is closed every read reports a zero-length read, which
/// kernel-core's `BlockingConsoleRead` turns into a scheduler park
/// (`AGENTS.md` §20) — so the console-0 `login` waits rather than draining
/// the passphrase bytes the unlock kthread is reading off the same device.
/// Once the gate opens, reads delegate verbatim to `inner`.
///
/// `Sync` (it holds only `&'static` references and an atomic gate), so it
/// is storable in the shared `'static` console list.
pub struct GatedConsoleRead {
    inner: &'static (dyn ConsoleRead + Sync + 'static),
    gate: &'static Console0Gate,
}

impl GatedConsoleRead {
    /// Wrap `inner` so its reads are withheld until `gate` opens.
    #[must_use]
    pub const fn new(
        inner: &'static (dyn ConsoleRead + Sync + 'static),
        gate: &'static Console0Gate,
    ) -> Self {
        Self { inner, gate }
    }
}

impl ConsoleRead for GatedConsoleRead {
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if !self.gate.is_open() {
            // Withhold input: a zero-length read parks the caller in
            // `BlockingConsoleRead` until the gate opens (`AGENTS.md` §20).
            return Ok(0);
        }
        self.inner.read(buf)
    }
}

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
mod metal;

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub use metal::spawn_if_present;

#[cfg(test)]
mod tests {
    use super::*;

    /// A console read source that hands out a fixed byte once per poll and
    /// records how many times it was polled, so a test can prove the gate
    /// withholds polls while closed.
    struct CountingRead {
        polls: core::sync::atomic::AtomicUsize,
    }

    impl CountingRead {
        const fn new() -> Self {
            Self {
                polls: core::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl ConsoleRead for CountingRead {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            self.polls
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if buf.is_empty() {
                return Ok(0);
            }
            buf[0] = b'x';
            Ok(1)
        }
    }

    #[test]
    fn a_closed_gate_withholds_input_without_polling_the_device() {
        static INNER: CountingRead = CountingRead::new();
        static GATE: Console0Gate = Console0Gate::new();
        let gated = GatedConsoleRead::new(&INNER, &GATE);
        let mut buf = [0u8; 4];
        // Closed: reports a zero-length read and never touches the device.
        assert_eq!(gated.read(&mut buf), Ok(0));
        assert_eq!(INNER.polls.load(core::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn an_open_gate_delegates_to_the_wrapped_device() {
        static INNER: CountingRead = CountingRead::new();
        static GATE: Console0Gate = Console0Gate::new();
        let gated = GatedConsoleRead::new(&INNER, &GATE);
        GATE.open();
        let mut buf = [0u8; 4];
        assert_eq!(gated.read(&mut buf), Ok(1));
        assert_eq!(buf[0], b'x');
        assert_eq!(INNER.polls.load(core::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn the_gate_is_a_one_way_latch() {
        let gate = Console0Gate::new();
        assert!(!gate.is_open());
        gate.open();
        assert!(gate.is_open());
        // A second open is idempotent and never re-closes.
        gate.open();
        assert!(gate.is_open());
    }

    #[test]
    fn the_boot_stash_round_trips_the_dtb_and_an_absent_binding() {
        record_boot(None, 0xDEAD_0000);
        let boot = take_boot();
        assert!(boot.binding.is_none());
        assert_eq!(boot.dtb, 0xDEAD_0000);
    }
}

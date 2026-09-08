//! The `Run` entry-point binary of the Raspberry Pi CPU frequency driver,
//! installed as a signed `/System/Drivers/` bundle and **autoloaded into user
//! space** by `devmgr` when a `raspberrypi,firmware-clocks` node is
//! discovered (`plans/CPUFREQ.md`).
//!
//! The ARM core clock belongs to the `VideoCore` firmware rather than to any
//! register the ARM cores can reach, so this process owns no MMIO window and
//! takes no interrupt: its only path to the clock is a property exchange with
//! the `vcmailbox` service driver, gated kernel-side by that endpoint's
//! `CAP_MAILBOX` send requirement.
//!
//! It serves nobody. It takes the machine's frequency **mechanism** role with
//! `cpufreq_bind`, then parks in `cpufreq_wait` for the kernel governor's next
//! target and applies it. Policy stays kernel-side because it must answer
//! within the idle transition it is reacting to; deciding here would put an
//! IPC round trip in front of every wake, which is the latency the whole
//! subsystem exists to remove.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, the stack canary, the panic handler, and the
//! `cpufreq_bind` / `cpufreq_wait` wrappers), never the C ABI. `main` wires
//! the real seams:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the host's
//!   `MailboxChannel` marshals each property exchange to the `vcmailbox`
//!   service over the kernel's call surface. The matched node requests no
//!   resources, so the grant set is legitimately empty and nothing is mapped.
//! * the firmware's own minimum and maximum become the declared range — never
//!   a board constant, so a board whose `config.txt` moves either end is
//!   driven over the range it actually has.
//! * then the apply loop, which blocks in the kernel between targets.
//!
//! A bring-up failure exits with a reserved fail-closed code, leaving the
//! machine's clock wherever the firmware had it rather than wedged; the
//! spawning supervisor decides whether to relaunch, and the kernel releases
//! the mechanism role with the process so a replacement can take it. On the
//! host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::cpufreq::CpuFreqTarget;
    use tairix_abi::CapabilityId;
    use tairix_caps::CapabilitySet;
    use tairix_drv_cpufreq_rpi::RpiCpuFreq;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the firmware would not report the ARM clock's operating
    /// range — an absent mailbox service, a doorbell timeout, or a firmware
    /// that does not know the clock. Without a range there is nothing to
    /// declare, so the driver stands down rather than guessing at one.
    const EXIT_NO_RANGE: i32 = 81;

    /// Exit code when the mechanism role could not be taken: another driver
    /// holds it, or this one lacks `CAP_CPUFREQ`. A reserved, fail-closed
    /// value.
    const EXIT_BIND_FAILED: i32 = 82;

    /// Exit code when the wait for the next target failed — a released
    /// binding or a torn-down task, both terminal. Exiting fail-loud beats
    /// retrying a dead binding forever, which is a busy spin.
    const EXIT_WAIT_FAILED: i32 = 83;

    /// Exit code when the firmware refused [`APPLY_FAILURE_BUDGET`]
    /// consecutive rate changes. A mechanism that cannot apply a rate is not
    /// one, and staying bound would keep a working replacement out of the
    /// role.
    const EXIT_MECHANISM_DEAD: i32 = 84;

    /// Consecutive refused rate changes tolerated before the driver stands
    /// down.
    ///
    /// A single refusal is worth riding out — the governor's next target
    /// arrives within a response window and the firmware may well take it —
    /// but a mechanism that has refused several in a row is not applying
    /// anything, and the machine is better served by the supervisor
    /// relaunching it. A fail-closed budget, not a retry-until-it-works loop:
    /// it is bounded, and each attempt is a target the governor genuinely
    /// asked for rather than a re-poll of the same one.
    const APPLY_FAILURE_BUDGET: u32 = 4;

    /// The capability set the host re-checks before marshalling a property
    /// exchange, plus the mechanism role this driver takes. The kernel is the
    /// authority and re-checks every trap.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MAILBOX);
        caps.insert(CapabilityId::CPUFREQ);
        caps
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the apply loop runs for the life of the
    /// driver process.
    fn main() -> i32 {
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        let driver = RpiCpuFreq::new(host);

        let Ok(limits) = driver.limits() else {
            return EXIT_NO_RANGE;
        };
        let handle = tairix_rt::cpufreq_bind(&limits);
        if handle < 0 {
            return EXIT_BIND_FAILED;
        }

        apply_loop(&driver, handle)
    }

    /// The apply loop: block in `cpufreq_wait` for the governor's next target
    /// (a genuine park in the kernel, never a poll), then ask the firmware for
    /// it.
    ///
    /// The applied rate is *not* reported back to the kernel. The firmware
    /// clamps and rounds, so it is not what was asked for, and the kernel has
    /// a better witness than this driver's word for it: the per-CPU estimator
    /// measures the live core clock from the silicon's own counters, so a
    /// mechanism whose rate never took effect shows up as a measured
    /// frequency that does not match the target.
    fn apply_loop(driver: &RpiCpuFreq<RtDriverHost<RtGrantSyscalls>>, handle: i64) -> i32 {
        let mut target = CpuFreqTarget::default();
        let mut refused = 0u32;
        loop {
            if tairix_rt::cpufreq_wait(handle, target.seq, &mut target) != 0 {
                return EXIT_WAIT_FAILED;
            }
            if driver.apply(target.target_hz).is_ok() {
                refused = 0;
            } else {
                refused += 1;
                if refused >= APPLY_FAILURE_BUDGET {
                    return EXIT_MECHANISM_DEAD;
                }
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}

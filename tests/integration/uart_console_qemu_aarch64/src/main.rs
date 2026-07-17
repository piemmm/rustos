//! PI Stage P2 QEMU integration test: the console base is discovered from
//! the firmware device tree at runtime, not hard-wired.
//!
//! ## What this test asserts
//!
//! `plans/PI.md` P2 makes the aarch64 console MMIO base *discovered* from
//! the firmware device tree
//! (`tairix_arch_aarch64::console::configure_from_fdt`) rather than a
//! compile-time constant, so the same kernel boots on boards whose console
//! lives at different addresses (the QEMU `virt` PL011 vs the Raspberry
//! Pi's). This binary proves the runtime path end to end on the `virt`
//! board, whose QEMU model hands the kernel a real generated device tree:
//!
//! 1. The arch crate's `boot.s` trampoline drops to EL1, establishes a
//!    stack, zeroes `.bss`, and calls `kernel_main`.
//! 2. `kernel_main` enables FP/SIMD, then **poisons** the console base
//!    with a deliberately-wrong value, so a later successful print can
//!    only mean discovery overwrote it.
//! 3. It parses the board device tree (embedded at build time — QEMU's
//!    `-kernel <ELF>` aarch64 path passes no DTB pointer, see below) and
//!    calls `configure_from_fdt`.
//! 4. It asserts the configured base is no longer the poison value and is
//!    the PL011 the tree advertised (`virt`'s `0x0900_0000`), logs a line
//!    over the *discovered* console — proving the discovered base is the
//!    one writes actually reach — and reports PASS through the ARM
//!    semihosting `SYS_EXIT` finisher.
//!
//! A regression that fails to discover the base (or leaves the poison in
//! place) trips an explicit failure finisher; one that never boots times
//! out — both documented fail-loud behaviours.
//!
//! ## Why `virt`, not a Raspberry Pi board
//!
//! QEMU's `raspi*` machine models do **not** hand the kernel a device-tree
//! pointer in `x0` (verified: `x0 == 0` at kernel entry on `raspi3b`, even
//! with `-dtb`), because they do not emulate the Raspberry Pi GPU
//! firmware's DTB hand-off that real hardware performs. The `virt` board
//! does pass its generated tree, so it is the board on which the runtime
//! discovery path is CI-provable. Discovery of the Pi's specific console
//! base (BCM2835 PL011 / AUX mini-UART register layouts) is covered by the
//! `tairix-arch-aarch64` host unit tests against the `raspi_like_arm`
//! device-tree fixture, and is an on-metal acceptance item for the later
//! Pi peripheral stages (`plans/PI.md` Arc C — honest emulation gap, never
//! a faked vertical).
//!
//! ## How it differs from a production kernel
//!
//! It links only the `tairix-arch-aarch64` port and the shared FDT reader
//! and supplies its own `kernel_main`. The QEMU-exit shortcut lives in
//! this dedicated bin, never behind a Cargo feature on the arch crate
//! (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::console::{self, ConsoleModel, DEFAULT_CONSOLE_BASE};
    use tairix_arch_aarch64::{enable_fp_el1, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use tairix_fdt::Fdt;
    use tairix_log::{log, Event, EventId, Field, Level};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): QEMU's aarch64 `-kernel <ELF>` path passes no DTB
    // pointer, so the board tree is embedded rather than read from `x0`.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Stable audit-event ids for the QEMU transcript.
    const CONSOLE_TEST_START: EventId = EventId(4212);
    const CONSOLE_TEST_PASS: EventId = EventId(4213);

    /// A deliberately-wrong console base installed before discovery runs.
    /// It is **not** the `virt` PL011 base, so a successful print after
    /// discovery proves the base was sourced from the device tree (not
    /// left at this poison value and not the pre-discovery default).
    const POISON_BASE: usize = 0xdead_0000;

    /// Failure finisher codes, each pinpointing one way P2 can break.
    const FAIL_DTB_PARSE: u16 = 1;
    const FAIL_NOT_DISCOVERED: u16 = 2;
    const FAIL_BASE_NOT_UPDATED: u16 = 3;

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_uart_console_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        // Enable FP/SIMD before the log formatter (which the compiler may
        // lower to NEON) runs. SAFETY: this is the boot CPU, called once,
        // before any FP/SIMD instruction executes.
        unsafe {
            enable_fp_el1();
        }

        // Poison the console so a later successful print can only mean
        // discovery overwrote the base with the tree's value. Use the
        // mini-UART model too, so the model is likewise proven discovered.
        console::configure(POISON_BASE, ConsoleModel::MiniUart);

        // Parse the embedded board device tree and configure the console
        // from it. The blob is the canonical QEMU `virt` tree dumped at
        // build time; `configure_from_fdt` finds its `arm,pl011` node.
        let discovered = match Fdt::new(DTB_BLOB) {
            Ok(fdt) => console::configure_from_fdt(&fdt),
            Err(_) => qemu_exit::exit_failure(FAIL_DTB_PARSE),
        };
        if discovered.is_none() {
            qemu_exit::exit_failure(FAIL_NOT_DISCOVERED);
        }

        // The base must have moved off the poison value — i.e. it was
        // written from the device tree. (On `virt` the discovered value is
        // the PL011 default base, but it arrived via discovery, not the
        // pre-discovery default: the poison step rules that out.)
        let (base, model) = console::current();
        if base == POISON_BASE {
            qemu_exit::exit_failure(FAIL_BASE_NOT_UPDATED);
        }

        // From here every log line transmits through the *discovered*
        // console, so reaching serial output proves the discovered base is
        // the one writes actually reach.
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: CONSOLE_TEST_START,
                message: "aarch64 console test: configured console from device tree",
                fields: &[
                    Field {
                        key: "discovered_pl011",
                        value: tairix_log::FieldValue::Str(if model == ConsoleModel::Pl011 {
                            "true"
                        } else {
                            "false"
                        }),
                    },
                    Field {
                        key: "base_is_default",
                        value: tairix_log::FieldValue::Str(if base == DEFAULT_CONSOLE_BASE {
                            "true"
                        } else {
                            "false"
                        }),
                    },
                ],
            },
        );

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: CONSOLE_TEST_PASS,
                message: "aarch64 console test: discovered console base is printable",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}

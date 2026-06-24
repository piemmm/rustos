//! PL011 line and BCM2711 pin-mux bring-up for the discovered console.
//!
//! QEMU's PL011 model powers up enabled, so the `virt` boot log flows
//! with no initialisation at all. Real Pi 4 silicon does not: with
//! `dtoverlay=disable-bt` the firmware routes `UART0` towards the
//! GPIO 14/15 header, but the kernel still owns (1) muxing those pins to
//! `ALT0` and releasing their pulls, and (2) programming the PL011 line
//! registers — baud divisors, frame format, FIFO, and the
//! `UARTEN`/`TXE`/`RXE` enables. Skipping either leaves `UART0`
//! permanently silent on metal (the regression this module fixes), so
//! the boot path applies both right after the console is discovered
//! (`init_from_fdt`, the freestanding entry point), while the same
//! writes are harmless no-ops in behaviour under QEMU.
//!
//! The split mirrors `crate::console`: pure, host-tested register
//! arithmetic here, with one thin freestanding apply layer doing the
//! volatile MMIO on the target (one definition shared
//! by the metal path and the unit tests).

use rustos_fdt::Fdt;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use crate::console;

// --- BCM2711 GPIO block (BCM2711 ARM Peripherals) ---------------------

/// Device-tree `compatible` string of the BCM2711 GPIO controller.
const GPIO_COMPATIBLE: &[u8] = b"brcm,bcm2711-gpio";

/// Byte length of the BCM2711 GPIO register block the mux needs:
/// `GPIO_PUP_PDN_CNTRL_REG3` at `0xF0` is the last register (datasheet
/// register table), so the window spans `0xF4` bytes. The device
/// tree's `reg` length (`0xB4`) predates the BCM2711 pull registers and
/// is deliberately not used to size the window.
pub const GPIO_REGS_LEN: usize = 0xF4;

/// `GPFSEL1` — function select for GPIO 10–19 (3 bits per pin).
pub const GPFSEL1: usize = 0x04;
/// `GPIO_PUP_PDN_CNTRL_REG0` — pull state for GPIO 0–15 (2 bits per pin).
pub const GPIO_PUP_PDN_CNTRL_REG0: usize = 0xE4;

// Both mux registers must lie inside the datasheet-sized window — the
// reason the window is not sized from the device tree's historical
// `0xB4` `reg` length, which excludes the pull register.
const _: () = assert!(GPFSEL1 + 4 <= GPIO_REGS_LEN);
const _: () = assert!(GPIO_PUP_PDN_CNTRL_REG0 + 4 <= GPIO_REGS_LEN);

/// `UART0` transmit pin (header pin 8).
const TXD0_PIN: u32 = 14;
/// `UART0` receive pin (header pin 10).
const RXD0_PIN: u32 = 15;
/// Function-select encoding for `ALT0` (the PL011 `UART0` function on
/// GPIO 14/15 — datasheet alternative-function table).
const FSEL_ALT0: u32 = 0b100;

/// Route GPIO 14/15 to the PL011 (`ALT0`) in a read `GPFSEL1` value,
/// leaving every other pin's function untouched.
#[must_use]
pub fn fsel1_route_uart0(current: u32) -> u32 {
    let mut v = current;
    for pin in [TXD0_PIN, RXD0_PIN] {
        let shift = (pin - 10) * 3;
        v = (v & !(0b111 << shift)) | (FSEL_ALT0 << shift);
    }
    v
}

/// Release the pulls on GPIO 14/15 (`00` = no pull) in a read
/// `GPIO_PUP_PDN_CNTRL_REG0` value, leaving every other pin's pull
/// untouched. A UART line must float: a residual pull fights the
/// transceiver and corrupts the frame.
#[must_use]
pub fn pull_none_uart0(current: u32) -> u32 {
    let mut v = current;
    for pin in [TXD0_PIN, RXD0_PIN] {
        let shift = pin * 2;
        v &= !(0b11 << shift);
    }
    v
}

// --- PL011 line registers (ARM DDI 0183) -------------------------------

/// `UARTIBRD` — integer baud-rate divisor.
const PL011_IBRD: usize = 0x24;
/// `UARTFBRD` — fractional baud-rate divisor (1/64ths).
const PL011_FBRD: usize = 0x28;
/// `UARTLCR_H` — line control: frame format and FIFO enable.
const PL011_LCRH: usize = 0x2C;
/// `UARTCR` — control: UART/transmit/receive enables.
const PL011_CR: usize = 0x30;
/// `UARTIMSC` — interrupt mask (all masked: the console polls).
const PL011_IMSC: usize = 0x38;
/// `UARTICR` — interrupt clear.
const PL011_ICR: usize = 0x44;

/// `UARTLCR_H` for 8 data bits, no parity, 1 stop bit, FIFOs enabled
/// (`WLEN = 0b11`, `FEN = 1`) — the fixed framing the boot console
/// speaks (`docs/src/platform/aarch64.md`, "Boot protocol").
pub const LCRH_8N1_FIFO: u32 = (0b11 << 5) | (1 << 4);
/// `UARTCR` enabling the UART with both directions (`UARTEN` bit 0,
/// `TXE` bit 8, `RXE` bit 9).
pub const CR_ENABLE: u32 = (1 << 9) | (1 << 8) | 1;
/// `UARTICR` write-one-to-clear mask covering every interrupt source.
const ICR_CLEAR_ALL: u32 = 0x7FF;

/// `UARTFR.BUSY` (bit 3): the transmitter is still shifting a frame out.
/// The PL011 TRM requires the UART disabled and idle before the line
/// registers are rewritten.
pub const FR_BUSY: u32 = 1 << 3;

/// The PL011 reference clock the divisors are computed from. On the Pi
/// this is the firmware's `init_uart_clock`, which the generated
/// `config.txt` pins to 48 MHz (`tools/mkimage`'s `config_txt`) so the
/// divisor arithmetic and the silicon agree; QEMU ignores baud entirely.
pub const UART_CLOCK_HZ: u32 = 48_000_000;

/// The boot console line rate: 9600 baud (8N1), the documented serial
/// configuration of the Pi image (`docs/src/install/raspberry_pi.md`).
pub const CONSOLE_BAUD: u32 = 9600;

/// Compute the PL011 `(IBRD, FBRD)` divisor pair for `baud` from
/// `clock_hz` (divider = clock / (16 × baud); `FBRD` is the fraction in
/// rounded 1/64ths — ARM DDI 0183 §3.3.6).
///
/// Returns `None` — fail closed, leaving the firmware's line state in
/// force — when the pair is unprogrammable: a zero baud, a divider of
/// zero (baud too fast for the clock), or an integer part beyond the
/// 16-bit `IBRD` field (baud too slow).
#[must_use]
pub fn pl011_divisors(clock_hz: u32, baud: u32) -> Option<(u32, u32)> {
    if baud == 0 {
        return None;
    }
    let denom = u64::from(baud).checked_mul(16)?;
    let clock = u64::from(clock_hz);
    let ibrd = clock / denom;
    if ibrd == 0 || ibrd > 0xFFFF {
        return None;
    }
    let fbrd = ((clock % denom) * 64 + denom / 2) / denom;
    // Rounding can carry into the integer part (fbrd == 64) only when
    // the fraction is ≥ 63.5/64; fold the carry rather than emit an
    // out-of-range FBRD.
    if fbrd == 64 {
        let ibrd = ibrd + 1;
        if ibrd > 0xFFFF {
            return None;
        }
        return Some((u32::try_from(ibrd).ok()?, 0));
    }
    Some((u32::try_from(ibrd).ok()?, u32::try_from(fbrd).ok()?))
}

/// The ordered `(offset, value)` writes that program a disabled PL011 to
/// the `ibrd`/`fbrd` divisors at 8N1 with FIFOs and re-enable it:
/// clear pending interrupts, divisors, line format (which latches the
/// divisors — `LCR_H` must be written last of the three), mask all
/// interrupts, then enable. The caller disables the UART and waits out
/// `FR.BUSY` first (`apply_pl011_init`, the freestanding apply layer).
#[must_use]
pub fn pl011_init_writes(ibrd: u32, fbrd: u32) -> [(usize, u32); 6] {
    [
        (PL011_ICR, ICR_CLEAR_ALL),
        (PL011_IBRD, ibrd),
        (PL011_FBRD, fbrd),
        (PL011_LCRH, LCRH_8N1_FIFO),
        (PL011_IMSC, 0),
        (PL011_CR, CR_ENABLE),
    ]
}

// --- Device-tree discovery ---------------------------------------------------

/// The BCM2711 GPIO controller located in a flattened device tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DiscoveredGpio {
    /// CPU-physical MMIO base of the GPIO register block (the node's
    /// `reg`, bus-translated through the ancestor `ranges` exactly like
    /// the console — [`crate::fdt::translated_reg`]).
    pub base: u64,
}

/// Locate the BCM2711 GPIO controller in `fdt`.
///
/// Early-returns at the matched node, so the walk is safe MMU-off like
/// the console discovery. Returns `None` on a tree without the
/// controller (QEMU `virt`) — the caller then skips the pin mux, which
/// only BCM2711 boards need.
#[must_use]
pub fn find_gpio(fdt: &Fdt<'_>) -> Option<DiscoveredGpio> {
    crate::fdt::scan_translated(fdt, |node, levels, depth| {
        node.property("compatible")?
            .iter_strings()
            .find(|s| *s == GPIO_COMPATIBLE)?;
        let (base, _) = crate::fdt::translated_reg(node, depth, levels, 0)?;
        Some(DiscoveredGpio { base })
    })
}

/// Bring the discovered console's line up from `fdt` facts: mux
/// GPIO 14/15 to the PL011 and release their pulls when the tree carries
/// a BCM2711 GPIO controller, then program the PL011 line registers
/// (9600 8N1, FIFOs, polled) and enable it.
///
/// Must run after [`console::configure_from_fdt`] has pointed the
/// console at the discovered UART, and before the first log byte.
/// No-ops fail closed: a non-PL011 console (the
/// mini-UART fallback keeps the firmware's line state), a tree without
/// the GPIO controller (QEMU `virt` — no pins to mux), or an
/// unprogrammable divisor pair each leave the corresponding state
/// untouched rather than guessing.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn init_from_fdt(fdt: &Fdt<'_>) {
    let (base, model) = console::current();
    if model != console::ConsoleModel::Pl011 {
        return;
    }
    if let Some(gpio) = find_gpio(fdt) {
        if let Ok(gpio_base) = usize::try_from(gpio.base) {
            // SAFETY: `gpio_base` is the firmware tree's bus-translated
            // BCM2711 GPIO block, identity-addressed MMU-off at boot;
            // `GPFSEL1` and `GPIO_PUP_PDN_CNTRL_REG0` lie inside the
            // datasheet-sized block ([`GPIO_REGS_LEN`]). Read-modify-
            // write through the pure helpers touches only GPIO 14/15's
            // fields, and this runs once, single-threaded, on the boot
            // CPU before any other GPIO user exists.
            unsafe {
                let fsel = (gpio_base + GPFSEL1) as *mut u32;
                core::ptr::write_volatile(fsel, fsel1_route_uart0(core::ptr::read_volatile(fsel)));
                let pull = (gpio_base + GPIO_PUP_PDN_CNTRL_REG0) as *mut u32;
                core::ptr::write_volatile(pull, pull_none_uart0(core::ptr::read_volatile(pull)));
            }
        }
    }
    apply_pl011_init(base);
}

/// Program the PL011 at `base`: disable, wait out a transmitting frame
/// (bounded — a wedged transmitter must not hang the boot), write the [`pl011_init_writes`] sequence, leaving the UART
/// enabled at [`CONSOLE_BAUD`] 8N1.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn apply_pl011_init(base: usize) {
    let Some((ibrd, fbrd)) = pl011_divisors(UART_CLOCK_HZ, CONSOLE_BAUD) else {
        return;
    };
    // SAFETY: `base` is the discovered (or default `virt`) PL011 block,
    // identity-addressed MMU-off at boot; every offset written is a
    // PL011 register inside the device's `0x200` window (ARM DDI 0183). The TRM-ordered sequence — disable, drain, reprogram,
    // re-enable — runs once, single-threaded, on the boot CPU before
    // the first console byte, so no concurrent transmit is cut short.
    unsafe {
        let reg = |offset: usize| (base + offset) as *mut u32;
        core::ptr::write_volatile(reg(PL011_CR), 0);
        let fr = reg(console::ConsoleModel::Pl011.status_offset());
        let mut budget = console::TX_POLL_BUDGET;
        while budget != 0 && core::ptr::read_volatile(fr) & FR_BUSY != 0 {
            budget -= 1;
            core::hint::spin_loop();
        }
        for (offset, value) in pl011_init_writes(ibrd, fbrd) {
            core::ptr::write_volatile(reg(offset), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_fdt::fixture::{raspi_like_arm, virt_like_arm};

    #[test]
    fn fsel1_routes_only_pins_14_and_15_to_alt0() {
        // From all-zero (all inputs): pins 14/15 become ALT0 (0b100 at
        // bit 12 and bit 15), nothing else.
        assert_eq!(fsel1_route_uart0(0), (0b100 << 12) | (0b100 << 15));
        // Other pins' functions (e.g. GPIO 10 output, GPIO 19 ALT5) are
        // preserved; a stale function on 14/15 is replaced.
        let other = 0b001 | (0b010 << 27);
        let stale = (0b111 << 12) | (0b111 << 15);
        assert_eq!(
            fsel1_route_uart0(other | stale),
            other | (0b100 << 12) | (0b100 << 15)
        );
        // Idempotent.
        let once = fsel1_route_uart0(other | stale);
        assert_eq!(fsel1_route_uart0(once), once);
    }

    #[test]
    fn pull_none_clears_only_pins_14_and_15() {
        // Pull fields are 2 bits per pin: 14 → bits 28/29, 15 → 30/31.
        assert_eq!(pull_none_uart0(u32::MAX), !(0b1111 << 28));
        // Other pins' pulls survive untouched.
        let others = 0b01 | (0b10 << 26);
        assert_eq!(pull_none_uart0(others | (0b01 << 28)), others);
        assert_eq!(pull_none_uart0(0), 0);
    }

    #[test]
    fn divisors_match_the_pl011_worked_examples() {
        // 48 MHz / (16 × 9600) = 312.5 → IBRD 312, FBRD round(.5 × 64) = 32.
        assert_eq!(pl011_divisors(48_000_000, 9600), Some((312, 32)));
        // The TRM's canonical 48 MHz / 115200 example: 26 + 3/64.
        assert_eq!(pl011_divisors(48_000_000, 115_200), Some((26, 3)));
        // QEMU-typical 24 MHz clock still programs cleanly.
        assert_eq!(pl011_divisors(24_000_000, 9600), Some((156, 16)));
    }

    #[test]
    fn divisors_fail_closed_outside_the_programmable_range() {
        // Zero baud is undefined.
        assert_eq!(pl011_divisors(48_000_000, 0), None);
        // Baud faster than clock/16 yields IBRD 0 — unprogrammable.
        assert_eq!(pl011_divisors(48_000_000, 4_000_000), None);
        // Baud so slow IBRD exceeds 16 bits — unprogrammable.
        assert_eq!(pl011_divisors(u32::MAX, 1), None);
    }

    #[test]
    fn divisor_rounding_carry_folds_into_the_integer_part() {
        // clock/denominator with fraction ≥ 63.5/64 rounds up to the next
        // integer divisor with a zero fraction, never FBRD = 64.
        // 16 × 1000 = 16000; 16000 × 312 + 15995 → fraction 0.9997.
        let (ibrd, fbrd) = pl011_divisors(16_000 * 312 + 15_995, 1000).expect("programmable");
        assert_eq!((ibrd, fbrd), (313, 0));
    }

    #[test]
    fn init_writes_program_line_then_enable_last() {
        let writes = pl011_init_writes(312, 32);
        // Divisors and frame format are written while disabled; the
        // LCR_H write follows the divisors (it latches them), and the
        // enable is the final write.
        assert_eq!(writes[1], (PL011_IBRD, 312));
        assert_eq!(writes[2], (PL011_FBRD, 32));
        assert_eq!(writes[3], (PL011_LCRH, LCRH_8N1_FIFO));
        assert_eq!(writes[5], (PL011_CR, CR_ENABLE));
        // 8N1 + FIFO: WLEN = 0b11 (bits 5/6), FEN (bit 4), no parity.
        assert_eq!(LCRH_8N1_FIFO, 0x70);
        // UARTEN + TXE + RXE.
        assert_eq!(CR_ENABLE, 0x301);
    }

    #[test]
    fn finds_the_bcm2711_gpio_block_in_a_raspi_tree() {
        // The fixture's `/soc/gpio@7e200000` bus `reg` translates through
        // the `0x7E00_0000 → 0xFE00_0000` range like every peripheral.
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let gpio = find_gpio(&fdt).expect("gpio controller present");
        assert_eq!(gpio.base, 0xfe20_0000);
    }

    #[test]
    fn no_gpio_controller_in_a_virt_tree_is_none() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(find_gpio(&fdt), None);
    }
}

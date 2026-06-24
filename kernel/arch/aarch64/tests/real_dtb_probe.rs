//! Real-firmware-tree regression probe: run the production boot-path
//! discovery walks — console selection, the BCM2711 GPIO controller, the
//! `VideoCore` mailbox, and the `/memory` window — over the *pinned* Pi 4
//! firmware DTB, exactly as `boot_aarch64::configure_mmio_from_dtb` does
//! on metal. The synthetic `raspi_like_arm` fixture covers the shapes;
//! this test pins the walks against the real 55 KiB tree (its node
//! order, depth, and property layout), where a parser defect would
//! otherwise only surface as a silent on-metal boot death.
//!
//! The blob is the checksummed firmware input `tools/xtask` fetches into
//! `target/pi-firmware/` for the Pi image build. When that cache has not
//! been populated the test reports a skip and passes: the fixture-based
//! unit tests still cover the logic, and an absent download must not
//! fail an offline `cargo test` (fail closed is for
//! authority, not for missing optional inputs).
//!
//! Note: the on-disk file's `/memory@0` carries a zero `reg` — the
//! firmware patches the real RAM ranges in at boot — so the memory walk
//! is asserted for *shape* (`Some`), not for a size.

use rustos_arch_aarch64::{console, uart_init, video};
use rustos_fdt::Fdt;

#[test]
fn real_pi4_dtb_discovery() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../target/pi-firmware/bcm2711-rpi-4-b.dtb"
    );
    let blob = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP: no firmware DTB at {path}: {e}");
            return;
        }
    };
    let fdt = Fdt::new(&blob).expect("real DTB parses");

    let con = console::find_console(&fdt).expect("console discovered");
    assert_eq!(
        con.base, 0xfe20_1000,
        "PL011 UART0 at its bus-translated base"
    );
    assert_eq!(con.model, console::ConsoleModel::Pl011);

    let gpio = uart_init::find_gpio(&fdt).expect("BCM2711 GPIO controller discovered");
    assert_eq!(gpio.base, 0xfe20_0000);

    let mailbox = video::find_mailbox(&fdt).expect("VideoCore mailbox discovered");
    assert_eq!(mailbox.base, 0xfe00_b880);

    assert!(
        fdt.first_memory_region().is_some(),
        "a /memory node walks (the firmware patches its reg at boot)"
    );
}

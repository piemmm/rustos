//! Canonical bind table and log-event vocabulary of the framebuffer
//! display service.
//!
//! The `Run` binary (`src/main.rs`) is the service itself; this
//! host-buildable lib target carries the crate's canonical
//! [`BIND_KEYS`] — the single source of truth the signed-manifest bind
//! table is authored from (`tools/xtask` `image_drivers`, the autoload
//! fixtures) and `devmgr` (or the in-kernel bootstrap-floor autoload)
//! resolves a discovered display node against — and the service's stable
//! [`tairix_log::EventId`] constants ([`FIRST_PRESENT`]), so the emitting
//! binary and every log consumer (the D7d QEMU vertical keys its
//! host-side scan-out readback on the rendered [`FIRST_PRESENT_MESSAGE`])
//! share one definition.
//!
//! The match key is the canonical `simple-framebuffer` model name
//! ([`SIMPLE_FRAMEBUFFER_COMPATIBLE`]) a platform's boot path publishes
//! for its programmed linear scan-out surface (`plans/DISPLAY.md` D7d):
//! the FDT `simple-framebuffer` shape, QEMU `ramfb`, the `VideoCore`
//! mailbox surface, or a UEFI GOP hand-off all normalise to it, so one
//! driver binds them all — the surface's base, geometry, and pixel format
//! travel in the node's `Framebuffer` resource, never in the key.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::{DriverBindKey, HwMatchKey, SIMPLE_FRAMEBUFFER_COMPATIBLE};
use tairix_log::EventId;

/// Range start (inclusive) reserved for the display service's event
/// identifiers. Per `lib/log` convention every subsystem owns a
/// 1 000-wide reserved range; the display service occupies
/// `15000..16000` (adjacent to `seatmgr`'s `14000..15000`). Once shipped
/// the numeric values must never be re-used or re-numbered.
pub const DISPLAY_SERVICE_RANGE_START: u32 = 15_000;
/// Range end (exclusive) reserved for display-service event identifiers.
pub const DISPLAY_SERVICE_RANGE_END: u32 = 16_000;

/// One-shot: the first client frame was successfully presented to the
/// scan-out surface since this service started — the operational witness
/// that the session → display-service → surface path is live end to end
/// (`plans/DISPLAY.md` D7d). Emitted at most once per service lifetime,
/// off the present hot path (a latched fact checked after the reply).
pub const FIRST_PRESENT: EventId = EventId(15_001);

/// The exact message [`FIRST_PRESENT`] is emitted with. A log consumer
/// (the D7d vertical's host runner) keys on this rendered text, so it is
/// defined once beside the id and imported by both sides.
pub const FIRST_PRESENT_MESSAGE: &str = "first client frame presented to scan-out";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks at the concrete-identity tier alongside the other
/// exact-match drivers (higher matched priority binds; an unbroken tie is
/// a packaging defect).
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: a platform-published linear
/// scan-out surface, matched by the canonical model name.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(SIMPLE_FRAMEBUFFER_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the shared constant is well within
        // `HW_COMPATIBLE_MAX`. A too-long value would be a compile-time
        // const-eval error here, never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bind_table_matches_a_boot_display_node_key() {
        // The one bind key matches the exact key the kernel's boot-display
        // publication emits, so the emitted node and the signed manifest
        // can never drift (both sides import the shared constant).
        let node_key = HwMatchKey::compatible(SIMPLE_FRAMEBUFFER_COMPATIBLE).expect("fits");
        assert_eq!(BIND_KEYS.len(), 1);
        assert!(BIND_KEYS[0].key.matches(&node_key));
        assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    }

    #[test]
    fn an_unrelated_compatible_does_not_match() {
        let other = HwMatchKey::compatible(b"virtio,mmio").expect("fits");
        assert!(!BIND_KEYS[0].key.matches(&other));
    }

    #[test]
    fn the_event_ids_are_inside_the_reserved_range() {
        assert!((DISPLAY_SERVICE_RANGE_START..DISPLAY_SERVICE_RANGE_END).contains(&FIRST_PRESENT.0));
    }
}

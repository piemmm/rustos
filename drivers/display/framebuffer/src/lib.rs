//! Canonical bind table of the framebuffer display service.
//!
//! The `Run` binary (`src/main.rs`) is the service itself; this
//! host-buildable lib target carries only the crate's canonical
//! [`BIND_KEYS`] — the single source of truth the signed-manifest bind
//! table is authored from (`tools/xtask` `image_drivers`, the autoload
//! fixtures) and `devmgr` (or the in-kernel bootstrap-floor autoload)
//! resolves a discovered display node against.
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

use rustos_abi::{DriverBindKey, HwMatchKey, SIMPLE_FRAMEBUFFER_COMPATIBLE};

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
}

//! Lock-free vector↔GSI routing table for x86_64 external IRQs.
//!
//! The table holds one [`AtomicU32`] per reserved external vector
//! (`0x30..=0xFE`). The kernel binary's `Phase::Irq` step calls
//! [`Routing::install`] once per IO-APIC pin during boot; after
//! `Phase::Irq` completes the table is read-only by contract
//! (`AGENTS.md` §2.1 — one-shot publish, no mutable static).
//!
//! `u32::MAX` is the unmapped sentinel; reads through
//! [`Routing::gsi_for_vector`] return `None` for unmapped slots.

use core::sync::atomic::{AtomicU32, Ordering};

use super::{EXTERNAL_VECTOR_COUNT, EXTERNAL_VECTOR_FIRST, EXTERNAL_VECTOR_LAST};

const GSI_UNMAPPED: u32 = u32::MAX;

/// Vector↔GSI routing table.
///
/// `#[repr(transparent)]` over `[AtomicU32; EXTERNAL_VECTOR_COUNT]`
/// so the in-arch-crate `GLOBAL_ROUTING` static can expose a
/// `&'static Routing` without copying.
#[repr(transparent)]
pub struct Routing {
    slots: [AtomicU32; EXTERNAL_VECTOR_COUNT],
}

/// Failure modes of [`Routing::install`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RoutingError {
    /// `vector` is outside the reserved external-IRQ range.
    VectorOutOfRange,
    /// `gsi == u32::MAX`. The unmapped sentinel is rejected at
    /// install time so subsequent
    /// [`Routing::gsi_for_vector`] reads remain unambiguous.
    GsiUsesSentinel,
    /// A different GSI is already bound to `vector`. Routing is
    /// set-once per vector per boot.
    VectorAlreadyBound,
}

impl Routing {
    /// Empty table — every slot unmapped.
    #[must_use]
    pub const fn new() -> Self {
        // The const-context array initialiser idiom; the lint is
        // benign for `AtomicU32` whose interior mutability is the
        // entire point.
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU32 = AtomicU32::new(GSI_UNMAPPED);
        Self {
            slots: [Z; EXTERNAL_VECTOR_COUNT],
        }
    }

    /// Record `vector → gsi`.
    ///
    /// Returns [`RoutingError::VectorAlreadyBound`] on a second install
    /// at the same vector unless the existing binding maps to the same
    /// GSI (idempotent re-publish).
    pub fn install(&self, gsi: u32, vector: u8) -> Result<(), RoutingError> {
        if gsi == GSI_UNMAPPED {
            return Err(RoutingError::GsiUsesSentinel);
        }
        if !(EXTERNAL_VECTOR_FIRST..=EXTERNAL_VECTOR_LAST).contains(&vector) {
            return Err(RoutingError::VectorOutOfRange);
        }
        let idx = (vector - EXTERNAL_VECTOR_FIRST) as usize;
        match self.slots[idx].compare_exchange(
            GSI_UNMAPPED,
            gsi,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(existing) if existing == gsi => Ok(()),
            Err(_) => Err(RoutingError::VectorAlreadyBound),
        }
    }

    /// Look up the GSI bound to `vector`.
    #[must_use]
    pub fn gsi_for_vector(&self, vector: u8) -> Option<u32> {
        if !(EXTERNAL_VECTOR_FIRST..=EXTERNAL_VECTOR_LAST).contains(&vector) {
            return None;
        }
        let idx = (vector - EXTERNAL_VECTOR_FIRST) as usize;
        let g = self.slots[idx].load(Ordering::Acquire);
        if g == GSI_UNMAPPED {
            None
        } else {
            Some(g)
        }
    }

    /// Reverse lookup: which vector is bound to `gsi`?
    ///
    /// Linear scan — the table has 207 slots, so the slow-path cost
    /// is bounded. Production callers cache the vector at install
    /// time; this lookup exists for diagnostics and host tests.
    #[must_use]
    pub fn vector_for_gsi(&self, gsi: u32) -> Option<u8> {
        if gsi == GSI_UNMAPPED {
            return None;
        }
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.load(Ordering::Acquire) == gsi {
                // `i + EXTERNAL_VECTOR_FIRST` is by construction in
                // `EXTERNAL_VECTOR_FIRST..=EXTERNAL_VECTOR_LAST` and
                // therefore fits in `u8`.
                #[allow(clippy::cast_possible_truncation)]
                return Some((i as u8) + EXTERNAL_VECTOR_FIRST);
            }
        }
        None
    }
}

impl Default for Routing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_returns_none_everywhere() {
        let r = Routing::new();
        assert!(r.gsi_for_vector(EXTERNAL_VECTOR_FIRST).is_none());
        assert!(r.gsi_for_vector(EXTERNAL_VECTOR_LAST).is_none());
        assert!(r.vector_for_gsi(0).is_none());
        assert!(r.vector_for_gsi(42).is_none());
    }

    #[test]
    fn install_then_lookup_round_trips() {
        let r = Routing::new();
        r.install(2, 0x30).expect("install");
        r.install(42, 0xFE).expect("install");
        assert_eq!(r.gsi_for_vector(0x30), Some(2));
        assert_eq!(r.gsi_for_vector(0xFE), Some(42));
        assert_eq!(r.vector_for_gsi(2), Some(0x30));
        assert_eq!(r.vector_for_gsi(42), Some(0xFE));
    }

    #[test]
    fn install_rejects_vectors_outside_range() {
        let r = Routing::new();
        assert_eq!(r.install(1, 0x20), Err(RoutingError::VectorOutOfRange));
        assert_eq!(r.install(1, 0xFF), Err(RoutingError::VectorOutOfRange));
    }

    #[test]
    fn install_rejects_sentinel_gsi() {
        let r = Routing::new();
        assert_eq!(
            r.install(u32::MAX, 0x30),
            Err(RoutingError::GsiUsesSentinel),
        );
    }

    #[test]
    fn second_install_with_different_gsi_fails_closed() {
        let r = Routing::new();
        r.install(2, 0x30).expect("first");
        assert_eq!(r.install(3, 0x30), Err(RoutingError::VectorAlreadyBound));
        // The first binding is preserved.
        assert_eq!(r.gsi_for_vector(0x30), Some(2));
    }

    #[test]
    fn second_install_with_same_gsi_is_idempotent() {
        let r = Routing::new();
        r.install(2, 0x30).expect("first");
        r.install(2, 0x30).expect("idempotent re-publish");
        assert_eq!(r.gsi_for_vector(0x30), Some(2));
    }

    #[test]
    fn vector_for_gsi_returns_first_match() {
        let r = Routing::new();
        r.install(5, 0x40).expect("install");
        assert_eq!(r.vector_for_gsi(5), Some(0x40));
        assert_eq!(r.vector_for_gsi(6), None);
    }
}

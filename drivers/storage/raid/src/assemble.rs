//! The shared **reassembly → member** bridge: turn a reassembled slot table
//! ([`ArrayIdentity::fill_slots`](crate::ArrayIdentity::fill_slots)) into a
//! redundant RAID engine's member buffer, in one place every consumer reuses
//! (`AGENTS.md` §2.2, `plans/FIX-IO.md` IO6).
//!
//! The metadata layer resolves a discovered array to a [`SlotDisposition`]
//! per slot (present-and-current, present-but-stale, or missing); the
//! composition engines each consume a caller-owned member buffer
//! ([`MirrorMember`] / [`ParityMember`] / [`DualParityMember`] /
//! [`TripleParityMember`], and [`MirrorMember`] again for RAID10). Nothing
//! else bridges the two, so every consumer that assembles a *discovered*
//! array — the autoloaded serve process and the ARXFS-native multi-device
//! composition alike (`plans/FIX-IO.md` IO6 remaining) — would otherwise
//! hand-roll the same placement loop, and a subtle slip (admitting a slot the
//! generation counter proved stale as a trusted read source, or losing a
//! device when the buffer width and the slot table disagree) is a
//! data-integrity fault, not a cosmetic one (`AGENTS.md` §5.4, §26.5 "a disk
//! that missed writes is a disk that can lie"). Centralising the placement
//! here makes that mapping impossible to get subtly wrong twice.
//!
//! The bridge is defined only over the **redundant** levels, whose members
//! carry the current/stale/absent vocabulary. A RAID0 stripe is deliberately
//! excluded: it has no redundancy, so [`StripeArray::assemble`](crate::StripeArray::assemble)
//! fails closed on a missing member rather than assembling around a gap, and a
//! [`StripeMember`](crate::StripeMember) has neither a stale nor an absent
//! state to build here.
//!
//! [`SlotDisposition`]: crate::SlotDisposition

use tairix_abi::driver::block::Block;

use crate::dualparity::DualParityMember;
use crate::mirror::{MemberRole, MirrorMember};
use crate::parity::ParityMember;
use crate::superblock::SlotDisposition;
use crate::triple::TripleParityMember;

/// A redundant RAID engine member that [`fill_members`] can build directly
/// from a reassembly verdict.
///
/// Implemented by the member types of every level whose slots carry the
/// current/stale/absent vocabulary ([`MirrorMember`] — shared by the RAID1
/// mirror and the RAID10 stripe of mirrors, [`ParityMember`],
/// [`DualParityMember`], and [`TripleParityMember`]). It is a thin, uniform
/// façade over each type's existing `with_role` / `absent` constructors so
/// the placement loop in [`fill_members`] is written once, not once per level
/// (`AGENTS.md` §2.2).
pub trait AssembleMember<B: Block>: Sized {
    /// Build a member backed by `device`, joining the array with `role`
    /// ([`MemberRole::Current`] for an in-sync copy, [`MemberRole::Stale`] for
    /// a rebuild target that must not serve reads until resynced).
    #[must_use]
    fn make_present(device: B, role: MemberRole) -> Self;

    /// Build an absent-slot member: a copy the array is defined to have but
    /// which currently holds no device (a Linux md "removed" slot).
    #[must_use]
    fn make_absent() -> Self;
}

impl<B: Block> AssembleMember<B> for MirrorMember<B> {
    fn make_present(device: B, role: MemberRole) -> Self {
        Self::with_role(device, role)
    }

    fn make_absent() -> Self {
        Self::absent()
    }
}

impl<B: Block> AssembleMember<B> for ParityMember<B> {
    fn make_present(device: B, role: MemberRole) -> Self {
        Self::with_role(device, role)
    }

    fn make_absent() -> Self {
        Self::absent()
    }
}

impl<B: Block> AssembleMember<B> for DualParityMember<B> {
    fn make_present(device: B, role: MemberRole) -> Self {
        Self::with_role(device, role)
    }

    fn make_absent() -> Self {
        Self::absent()
    }
}

impl<B: Block> AssembleMember<B> for TripleParityMember<B> {
    fn make_present(device: B, role: MemberRole) -> Self {
        Self::with_role(device, role)
    }

    fn make_absent() -> Self {
        Self::absent()
    }
}

/// A reason [`fill_members`] could not populate the member buffer.
///
/// These are reassembly-input failures, not device I/O outcomes, so they are
/// their own type distinct from [`DriverError`](tairix_abi::DriverError).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AssembleError {
    /// The member buffer is not the same length as the slot table, so the
    /// caller sized it wrong: placing the slots would either overrun the
    /// buffer or leave defined slots unbuilt. Fails closed rather than
    /// composing a partial array.
    WidthMismatch,
    /// A slot the reassembly marked present offered a `tag` the caller's
    /// device supplier could not resolve to a device. Fails closed rather
    /// than silently demoting the present slot to absent (which would drop a
    /// copy the array believes it has).
    MissingDevice {
        /// The reassembly tag (the caller's device handle) that could not be
        /// resolved.
        tag: usize,
    },
}

/// Populate `members` from the reassembled `slots`, taking each present slot's
/// device from `take_device` keyed by the tag the reassembly recorded.
///
/// `slots` is the table [`ArrayIdentity::fill_slots`](crate::ArrayIdentity::fill_slots)
/// produced — one entry per array slot, in slot order — and `members` is the
/// caller-owned buffer the composition engine will borrow, which must be the
/// same length. `take_device(tag)` yields the device for a present slot's
/// reassembly `tag` (typically an index into the caller's discovered-device
/// list), moving it out of the caller's keeping; it is called **at most once
/// per present slot**, and never for a missing slot.
///
/// Each slot is placed through the single role authority
/// [`MemberRole::for_slot`], so a slot the generation counter proved stale
/// (`in_sync == false`) becomes a [`MemberRole::Stale`] member — a rebuild
/// target the engine admits [`Resyncing`](crate::MemberState::Resyncing), never
/// an immediate read source — and the composition layer can never disagree
/// with the metadata layer on what "in sync" means (`AGENTS.md` §2.2, §5.4,
/// §26.5). A missing slot becomes an [`AssembleMember::make_absent`] member so
/// the array knows its true width and reports the reduced redundancy rather
/// than masquerading as a smaller, optimal array.
///
/// The engine's own `assemble` still re-derives each present member's real
/// state from a live geometry probe, so a device that is absent or unwell at
/// assembly is faulted rather than trusted; this bridge only fixes the
/// *role* each slot joins with.
///
/// # Errors
///
/// * [`AssembleError::WidthMismatch`] if `members.len() != slots.len()`; the
///   buffer is left untouched.
/// * [`AssembleError::MissingDevice`] if a present slot's `tag` could not be
///   resolved by `take_device`; members already placed before the failure may
///   have been built (and their devices moved), so the caller discards the
///   partially-filled buffer.
pub fn fill_members<B, M>(
    slots: &[SlotDisposition],
    members: &mut [M],
    mut take_device: impl FnMut(usize) -> Option<B>,
) -> Result<(), AssembleError>
where
    B: Block,
    M: AssembleMember<B>,
{
    if members.len() != slots.len() {
        return Err(AssembleError::WidthMismatch);
    }
    for (member, &slot) in members.iter_mut().zip(slots) {
        *member = match (MemberRole::for_slot(slot), slot) {
            (Some(role), SlotDisposition::Present { tag, .. }) => {
                let device = take_device(tag).ok_or(AssembleError::MissingDevice { tag })?;
                M::make_present(device, role)
            }
            // `MemberRole::for_slot` yields `Some` only for a present slot and
            // `None` only for a missing one, so the remaining arm is exactly
            // the missing-slot case: an absent member with no device.
            _ => M::make_absent(),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests;

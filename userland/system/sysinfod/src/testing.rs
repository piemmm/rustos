//! Attested fixture principals shared by this crate's host tests.
//!
//! Every test caller is minted here rather than assembled test by test, for
//! one security-relevant reason: the reporter registry keys and refuses
//! reporters by the kernel-attested process-instance id, and the all-zero
//! [`ProcId::KERNEL`] is reserved for a kernel principal that may never
//! self-report. A fixture that spells that id by accident — filling all
//! sixteen bytes with a discriminating value that happens to be zero — is
//! refused exactly as a hostile kernel-domain caller would be, and the test
//! then fails for a reason unrelated to what it was checking. Minting every
//! identity through [`instance`], where the reserved value is unreachable,
//! puts that mistake out of reach instead of asking each test to remember
//! it.

use tairix_abi::{
    CapabilityId, CapabilitySummary, Origin, ProcId, TrustDomain, ORIGIN_CONSOLE_NONE, PROC_ID_LEN,
};

use crate::source::Caller;

/// Leading byte of every fixture process-instance id.
///
/// Non-zero, which is what makes the reserved [`ProcId::KERNEL`]
/// unreachable from [`instance`].
const INSTANCE_MARKER: u8 = 0xA5;

/// The uid every fixture user principal is attested as owning.
const FIXTURE_UID: u32 = 1000;

/// The primary gid every fixture user principal is attested with.
const FIXTURE_GID: u32 = 100;

/// A distinct process-instance identity for the fixture `tag`.
///
/// `tag` tells one fixture process from another, and one process lifetime
/// from the next when a test recycles a numeric pid. Any `tag` is safe,
/// zero included: the leading marker byte is never zero, so no `tag` can
/// produce the reserved [`ProcId::KERNEL`] sentinel.
pub fn instance(tag: u8) -> ProcId {
    let mut raw = [tag; PROC_ID_LEN];
    raw[0] = INSTANCE_MARKER;
    ProcId::from_raw(raw)
}

/// An attested user-process caller holding `caps`, identified by
/// [`instance(tag)`](instance) and the numeric `pid`.
pub fn user_caller(caps: &[CapabilityId], tag: u8, pid: u64) -> Caller {
    let mut summary = CapabilitySummary::EMPTY;
    for cap in caps {
        summary.insert(*cap);
    }
    Caller::new(Origin::new(
        TrustDomain::User,
        FIXTURE_UID,
        FIXTURE_GID,
        pid,
        instance(tag),
        summary,
        ORIGIN_CONSOLE_NONE,
    ))
}

/// A kernel-domain principal: the reserved [`ProcId::KERNEL`] instance a
/// real user process can never carry.
pub fn kernel_caller() -> Caller {
    Caller::new(Origin::new(
        TrustDomain::Kernel,
        0,
        0,
        0,
        ProcId::KERNEL,
        CapabilitySummary::EMPTY,
        ORIGIN_CONSOLE_NONE,
    ))
}

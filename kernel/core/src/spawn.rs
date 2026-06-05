//! Capability-checked, audited process-spawn caller.
//!
//! [`rustos_kernel_mem::build_process_image`] is the architecture-neutral
//! *memory mechanism*: given a validated [`rustos_abi::rxe::LoadImage`] it
//! materialises a runnable user address space (segments mapped and filled,
//! a zeroed user stack, and the `rustos_abi::process` startup-vector block)
//! and reports the [`rustos_kernel_mem::ProcessImage`] register state. It is
//! deliberately capability-agnostic and never logs (`AGENTS.md` §17.4 —
//! `kernel/mem` does not depend on the security policy or `lib/log`).
//!
//! This module is the *policy* half: the one path that authorises a spawn,
//! audits the decision, builds the image, and drops the calling CPU into the
//! new program through the Arch HAL [`EnterUser`] primitive
//! (`AGENTS.md` §17.2). Keeping the capability gate and the audit record
//! here — in the caller, not in `kernel/mem` — is what preserves the §17.4
//! layering while still satisfying §5.4 (capability check before any state
//! touch) and §5.4.4 (security-relevant decisions are audited).
//!
//! # Security
//!
//! Spawning a program is privileged: it materialises a new principal's
//! address space and hands it the CPU. [`spawn_and_enter`] therefore
//! requires the caller to hold [`CapabilityId::PROC_SPAWN`] and fails closed
//! (`AGENTS.md` §4 — no ambient authority; §2.9 — fail closed) — the check
//! happens *before* `build_process_image` touches any page table. The hosted
//! program still receives only the capabilities its own signed manifest
//! requests intersected with its user's grants (`AGENTS.md` §16.5); this gate
//! authorises the *act* of spawning, it does not widen the new program's
//! authority.

use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery};
use rustos_arch_api::{EnterUser, UserEntry};
use rustos_kernel_mem::{
    build_process_image, AddressSpace, Frame, PageTable, PhysMap, SpawnError, UserStack,
};
use rustos_log::{Event, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::audit::AuditEvent;

/// Why a [`spawn_and_enter`] call did not transfer control to a new program.
///
/// On success the call diverges into user mode and never returns, so the
/// `Ok` variant carries [`core::convert::Infallible`]: a returning call is
/// always one of these failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnCallerError {
    /// The caller does not hold [`CapabilityId::PROC_SPAWN`]; no address
    /// space was built (`AGENTS.md` §5.4 — fail closed).
    Denied,
    /// Building the process image failed (see [`SpawnError`]); the partially
    /// built address space is discarded by the caller.
    Build(SpawnError),
}

/// What to spawn: the validated image, its backing bytes, and the user-space
/// layout [`build_process_image`] needs.
///
/// Bundled into one struct so [`spawn_and_enter`] keeps a small, readable
/// argument list rather than the ten positional parameters of the underlying
/// builder.
pub struct SpawnRequest<'a> {
    /// The validated `rxe` load image (holding one is proof the §19.2
    /// load-time invariants hold).
    pub image: &'a LoadImage,
    /// The whole `rxe` file the segments' `file_offset`s index into.
    pub image_bytes: &'a [u8],
    /// Relocation bias applied to the image's link addresses.
    pub bias: u64,
    /// Where, and how large, the initial user stack is.
    pub stack: UserStack,
    /// Page-aligned user virtual address the startup-vector block is written
    /// at (the value handed to the program in the first-argument register).
    pub start_block_base: u64,
    /// The argument vector, each entry a NUL-free byte string.
    pub args: &'a [&'a [u8]],
    /// The environment vector, each entry a NUL-free byte string.
    pub env: &'a [&'a [u8]],
    /// Per-process random seed for the §19.2 stack canary.
    pub canary: u64,
}

/// A stable `&'static str` naming a [`SpawnError`] for the audit `cause`
/// field. The audit record never formats untrusted data; it names which
/// closed-fail branch the builder took.
const fn spawn_error_cause(error: SpawnError) -> &'static str {
    match error {
        SpawnError::Load(_) => "mapping_failed",
        SpawnError::Layout(_) => "layout_overflow",
        SpawnError::SegmentContentOutOfRange => "segment_content_out_of_range",
        SpawnError::PhysUnmapped => "phys_unmapped",
        SpawnError::EmptyStack => "empty_stack",
        SpawnError::Misaligned => "misaligned",
        SpawnError::StartBlock(_) => "startup_block",
        // `SpawnError` is `#[non_exhaustive]`; a future variant audits as a
        // generic build failure until it earns its own stable cause string.
        _ => "build_failed",
    }
}

/// Authorise, audit, build, and enter a freshly spawned process.
///
/// The call:
///
/// 1. checks `caps` holds [`CapabilityId::PROC_SPAWN`], failing closed with
///    [`SpawnCallerError::Denied`] and an [`AuditEvent::ProcessSpawnDenied`]
///    record if not — *before* any page table is touched (`AGENTS.md` §5.4);
/// 2. calls [`build_process_image`] to materialise the user address space in
///    `space`, emitting [`AuditEvent::ProcessSpawnFailed`] and returning
///    [`SpawnCallerError::Build`] on failure;
/// 3. emits [`AuditEvent::ProcessSpawned`] (carrying the relocated entry
///    point), then transfers control to the new program through
///    [`EnterUser::enter_user`], which never returns.
///
/// # Safety
///
/// On the authorised, successful path this calls [`EnterUser::enter_user`],
/// whose contract the caller must uphold: `space` must already be the
/// **active** address space on the calling CPU and the kernel's user→kernel
/// trap path must be installed, so the new program's first syscall is handled
/// rather than faulting. (The image is built into `space`; activating it and
/// installing the trap vector are the caller's responsibility because they
/// are architecture-specific and live outside `kernel/core`.)
///
/// # Errors
///
/// Returns [`SpawnCallerError::Denied`] when the capability check fails and
/// [`SpawnCallerError::Build`] when image construction fails. On success the
/// function diverges and does not return.
#[allow(clippy::too_many_arguments)]
pub unsafe fn spawn_and_enter<P, A, E>(
    caps: &dyn CapabilityQuery,
    audit: &dyn Sink,
    enter: &E,
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    request: &SpawnRequest<'_>,
    alloc_frame: A,
) -> Result<core::convert::Infallible, SpawnCallerError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
    E: EnterUser,
{
    // Step 2 (AGENTS.md §5.4) — capability check before any state touch.
    if !caps.holds(CapabilityId::PROC_SPAWN) {
        emit(audit, AuditEvent::ProcessSpawnDenied, Level::Error, &[]);
        return Err(SpawnCallerError::Denied);
    }

    let image = build_process_image(
        space,
        physmap,
        request.image,
        request.image_bytes,
        request.bias,
        &request.stack,
        request.start_block_base,
        request.args,
        request.env,
        request.canary,
        alloc_frame,
    )
    .map_err(|error| {
        emit(
            audit,
            AuditEvent::ProcessSpawnFailed,
            Level::Error,
            &[Field {
                key: "cause",
                value: spawn_error_cause(error),
            }],
        );
        SpawnCallerError::Build(error)
    })?;

    let mut entry_buf = [0u8; 16];
    emit(
        audit,
        AuditEvent::ProcessSpawned,
        Level::Info,
        &[Field {
            key: "entry",
            value: format_hex_u64(image.entry, &mut entry_buf),
        }],
    );

    // SAFETY: the function's own safety contract requires `space` to be the
    // active address space with the trap path installed. `build_process_image`
    // mapped `image.entry` as a user-accessible executable page and
    // `image.stack_top` as the exclusive top of a user-accessible writable
    // stack in `space`, so the `UserEntry` register state satisfies
    // `EnterUser::enter_user`'s precondition.
    unsafe {
        enter.enter_user(UserEntry::new(
            image.entry,
            image.stack_top,
            image.start_block,
        ))
    }
}

/// Emit one structured audit record for `event` with `fields`.
fn emit(audit: &dyn Sink, event: AuditEvent, level: Level, fields: &[Field<'_>]) {
    rustos_log::log(
        audit,
        &Event {
            level,
            id: event.id(),
            message: event.message(),
            fields,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::TestSink;
    use rustos_abi::rxe::{LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE};
    use rustos_abi::{ABI_VERSION_CURRENT, LOAD_MAGIC, SYSCALL_TABLE_HASH_LEN};
    use rustos_kernel_mem::{
        AddressSpace, HostPageTable, PhysAddr, SimPhysMap, UserStack, PAGE_SIZE,
    };
    use rustos_log::{set_max_level, Level};

    extern crate std;
    use std::boxed::Box;

    const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x33; SYSCALL_TABLE_HASH_LEN];

    /// A `CapabilityQuery` granting exactly the capabilities in its slice.
    struct Granted(&'static [CapabilityId]);
    impl CapabilityQuery for Granted {
        fn holds(&self, cap: CapabilityId) -> bool {
            self.0.contains(&cap)
        }
    }

    /// An `EnterUser` that must never be reached on the host. The deny and
    /// build-failure paths return before the transition, so a test that
    /// drives them never calls this; if one ever did, the panic flags the
    /// test bug rather than silently passing.
    struct NeverEnter;
    impl EnterUser for NeverEnter {
        unsafe fn enter_user(&self, _regs: UserEntry) -> ! {
            unreachable!("enter_user is only meaningful on the bare-metal target")
        }
    }

    /// A minimal valid single-segment PIE `rxe` blob plus the parsed image.
    fn tiny_image() -> (std::vec::Vec<u8>, LoadImage) {
        let seg = Segment {
            vaddr: 0x1000,
            file_offset: (LoadHeader::WIRE_LEN + Segment::WIRE_LEN) as u64,
            file_size: 4,
            mem_size: PAGE_SIZE as u64,
            permission: RxePermission::ReadExecute,
        };
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: 1,
            needed_count: 0,
            entry: 0x1000,
            cfi_tag: TAG,
        };
        let mut rxe = std::vec::Vec::new();
        rxe.extend_from_slice(&header.to_le_bytes());
        rxe.extend_from_slice(&seg.to_le_bytes());
        rxe.extend_from_slice(&[0x13, 0x00, 0x00, 0x00]); // 4 bytes of "code"
        let image = LoadImage::parse(&rxe, &TAG).expect("valid tiny image");
        (rxe, image)
    }

    fn host_space() -> AddressSpace<HostPageTable> {
        AddressSpace::new(HostPageTable::new())
    }

    fn sim() -> SimPhysMap {
        SimPhysMap::new(PhysAddr::new((PAGE_SIZE * 16) as u64), 64 * PAGE_SIZE)
    }

    fn request<'a>(image: &'a LoadImage, bytes: &'a [u8], stack_pages: u64) -> SpawnRequest<'a> {
        SpawnRequest {
            image,
            image_bytes: bytes,
            bias: 0,
            stack: UserStack {
                base: 0x20_0000,
                page_count: stack_pages,
            },
            start_block_base: 0x30_0000,
            args: &[],
            env: &[],
            canary: 0,
        }
    }

    #[test]
    fn denied_without_proc_spawn_capability_touches_no_state() {
        set_max_level(Level::Trace);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let (bytes, image) = tiny_image();
        let mut space = host_space();
        let physmap = sim();
        let caps = Granted(&[]); // no capabilities
        let req = request(&image, &bytes, 1);

        // SAFETY: the deny path returns before `enter_user`, so the
        // never-entering port is never invoked and the (inactive) host
        // address space is never entered.
        let result = unsafe {
            spawn_and_enter(&caps, sink, &NeverEnter, &mut space, &physmap, &req, || {
                None
            })
        };
        assert_eq!(result.err(), Some(SpawnCallerError::Denied));
        // Nothing was mapped: the check fails closed before building.
        assert_eq!(space.mapped_pages(), 0);
        let ids = sink.event_ids();
        assert!(ids.contains(&AuditEvent::ProcessSpawnDenied.id().0));
        assert!(!ids.contains(&AuditEvent::ProcessSpawned.id().0));
    }

    #[test]
    fn build_failure_is_audited_and_reported() {
        set_max_level(Level::Trace);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let (bytes, image) = tiny_image();
        let mut space = host_space();
        let physmap = sim();
        let caps = Granted(&[CapabilityId::PROC_SPAWN]);
        // A zero-page stack makes `build_process_image` fail closed with
        // `SpawnError::EmptyStack` before mapping anything.
        let req = request(&image, &bytes, 0);

        // SAFETY: `build_process_image` fails before the function reaches
        // `enter_user`, so the never-entering port is never invoked.
        let result = unsafe {
            spawn_and_enter(&caps, sink, &NeverEnter, &mut space, &physmap, &req, || {
                None
            })
        };
        assert_eq!(
            result.err(),
            Some(SpawnCallerError::Build(SpawnError::EmptyStack))
        );
        let ids = sink.event_ids();
        assert!(ids.contains(&AuditEvent::ProcessSpawnFailed.id().0));
        assert!(!ids.contains(&AuditEvent::ProcessSpawned.id().0));
    }
}

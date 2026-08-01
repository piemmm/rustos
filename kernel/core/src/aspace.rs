//! Per-task address-space registry (increment **B** of the staged
//! user-memory copy path, `PLAN.md` Stage 7).
//!
//! The kernel's `copy_from_user` / `copy_to_user` boundary
//! ([`tairix_kernel_mem::uaccess`] /
//! `tests/SECURITY.md` §5) walks the *calling task's* address space.
//! A syscall handler therefore needs to turn the caller's
//! [`tairix_kernel_sec::TaskId`] into the pair the copy path consumes:
//! the task's user [`AddressSpace`](tairix_kernel_mem::AddressSpace)
//! and the kernel [`PhysMap`] that backs it. This module owns that
//! mapping.
//!
//! # Why trait objects
//!
//! [`tairix_kernel_mem::AddressSpace`] is generic over its
//! [`PageTable`](tairix_kernel_mem::PageTable) backend, so the
//! kernel cannot hold a `BTreeMap<TaskId, AddressSpace<P>>` for a
//! single `P` — different tasks may run on different architecture page
//! tables, and the orchestrator that composes this registry into
//! `KernelState` is architecture-neutral. Each entry is therefore
//! stored behind the object-safe
//! [`UserAddressSpace`] (the read-only translate view the copy walk
//! needs) and a boxed [`PhysMap`]. The same erasure the kernel
//! already applies to the
//! direct map (`&dyn PhysMap`) is applied to the address space, so the
//! registry stays one concrete, non-generic type.
//!
//! # Lifecycle
//!
//! An entry is [`register`](AddressSpaceRegistry::register)ed when a
//! task's `rxe` image is mapped (the loader's
//! [`map_image`](tairix_kernel_mem::map_image) result handed to the
//! spawner) and [`withdraw`](AddressSpaceRegistry::withdraw)n when the
//! task exits. Both are fail-closed: registering an id that is already
//! present is refused rather than silently
//! replacing a live mapping, and resolving an unknown id yields
//! `None`. The registry is a pure data structure with no ambient
//! authority and no audit sink of its own — the call sites that drive
//! the lifecycle (the spawner, the `exit` handler) own the
//! security-relevant logging, exactly as the dispatcher audits IPC
//! endpoint lookups rather than [`PortRegistry`] doing so internally.
//!
//! [`PortRegistry`]: tairix_kernel_ipc::PortRegistry

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_abi::hwtree::{GrantedResource, HwResource, HwResourceKind};
use tairix_abi::{DescriptorTable, Errno, LimitKind, OpenFlags, ResourceLimit, STD_STREAM_COUNT};
use tairix_caps::CapabilitySet;
use tairix_kernel_mem::{Frame, MapFlags, Page, PhysMap, UserAddressSpace, PAGE_SIZE};
use tairix_kernel_sec::TaskId;

use crate::pipe::PipeEnd;
use crate::pty::{PtyMasterEnd, PtySlaveEnd};
use crate::resource::ResourceBacking;
use crate::rlimit::LimitSet;

/// Why registering a task's address space was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AspaceError {
    /// An address space is already registered for this task id. The
    /// registry never silently replaces a live mapping — the caller
    /// must [`withdraw`](AddressSpaceRegistry::withdraw) the old task
    /// first (fail closed).
    AlreadyPresent,
}

/// One task's stored user address space and the physical map backing it.
///
/// Held only inside [`AddressSpaceRegistry`]; exposed to callers solely
/// as the borrowed pair returned by
/// [`resolve`](AddressSpaceRegistry::resolve).
struct TaskAddressSpace {
    space: Box<dyn UserAddressSpace + Send + Sync>,
    physmap: Box<dyn PhysMap + Send + Sync>,
}

/// Maps each live task's [`TaskId`] to its user address space and the
/// kernel [`PhysMap`] backing it.
///
/// Composed into `KernelState` as a `RwLock`-wrapped field (mirroring
/// the `caps` and `ipc` registries — the registry
/// owns no lock of its own, so the synchronisation policy lives with
/// `KernelState`). It boots empty: entries appear only as tasks are
/// spawned and disappear as they exit.
#[derive(Default)]
pub struct AddressSpaceRegistry {
    tasks: BTreeMap<TaskId, TaskAddressSpace>,
    /// Each live task's standard-stream descriptor table. Co-located with the address space because it shares the
    /// exact per-process lifecycle — established at spawn, withdrawn at
    /// exit — and is keyed by the same [`TaskId`]; a parallel registry +
    /// lock would be near-duplicate plumbing.
    /// A task with no entry resolves to the fail-closed
    /// [`DescriptorTable::closed`] default, so an unestablished process
    /// can reach no stream backing.
    streams: BTreeMap<TaskId, DescriptorTable>,
    /// Each live task's effective resource limits. Held
    /// here for the same reason as [`Self::streams`]: it shares the exact
    /// per-process lifecycle (inherited at spawn, withdrawn at exit) and is
    /// keyed by the same [`TaskId`], so a parallel registry + lock would be
    /// near-duplicate plumbing. A task with no
    /// entry resolves to the per-boot [`Self::default_limits`] policy via
    /// [`Self::limits`].
    limits: BTreeMap<TaskId, LimitSet>,
    /// The per-boot default limit policy a task with no established set
    /// resolves to, and the default `LimitSet::inherit` intersects
    /// against. Starts at the compile-time [`LimitSet::DEFAULT`] floor;
    /// the boot path replaces it once with the hardware-derived set
    /// (today: the discovered-RAM pinned-memory bound), so every
    /// consumer — `limits`, inheritance, `rlimit_get` — reads one
    /// definition and none can drift.
    default_limits: LimitSet,
    /// The tasks whose entire anonymous memory is pinned — exempt from
    /// the compressed `ramzip` tier and any future lower swap tier
    /// (`mem_pin`, `plans/STRESSTEST.md` ST2). Process-scoped state: a
    /// task is present or absent, never partially pinned. Deliberately
    /// not inherited across spawn (a fresh task id is never in the set)
    /// and cleared by [`Self::withdraw`] on exit. The compressed tier's
    /// eligibility classifier reads this through
    /// [`Self::is_pinned`] when a candidate's owner is judged, so there
    /// is exactly one pin decision.
    pinned: BTreeSet<TaskId>,
    /// Each live task's device-resource grants (the unforgeable, kernel-issued handles a driver task may map with
    /// `mmio_map`). Co-located with the address space for the same reason
    /// as [`Self::streams`] and [`Self::limits`]: a grant shares the exact
    /// per-process lifecycle — minted when a driver is admitted, reclaimed
    /// when the task exits — and is keyed by the same [`TaskId`], so a
    /// parallel registry + lock would be near-duplicate plumbing. A task with no entry owns no grants, so
    /// [`Self::grant`] resolves to `None` — fail closed: a task can
    /// map only the windows it was actually granted.
    grants: BTreeMap<TaskId, TaskGrants>,
    /// The discovered hardware-tree node each autoloaded **driver** task was
    /// loaded for. Recorded when a driver is spawned for
    /// a matched node, beside its grants, and keyed by the same kernel-trusted
    /// [`TaskId`]; an ordinary `spawn` (no matched node) records nothing.
    ///
    /// This is the security spine of `hw_emit_node`'s tree placement: when a driver publishes a discovered child,
    /// the kernel sets the child's parent to *this* node — the emitter's own —
    /// so a driver can neither forge its position in the tree nor parent a
    /// child under a node it was not loaded for. A task with no entry resolves
    /// to `None` via [`Self::loaded_node`], so a non-driver task (or one with
    /// no matched node) cannot emit a child at all (fail closed).
    /// Dropped at [`withdraw`](Self::withdraw) so a reused id never inherits a
    /// dead driver's node.
    loaded_nodes: BTreeMap<TaskId, u32>,
    /// Each live task's open file/directory handles (the descriptors
    /// `fs_open` returns and `fs_close` releases). Co-located with the
    /// address space for the same reason as [`Self::streams`]: a handle
    /// shares the exact per-process lifecycle — allocated on `fs_open`,
    /// released on `fs_close`, and reclaimed when the task exits — and is
    /// keyed by the same [`TaskId`]. A task with no entry owns no open
    /// files, so [`Self::open_file`] resolves to `None` (fail closed: a
    /// task can only operate on a descriptor it actually opened). Dropped at
    /// [`withdraw`](Self::withdraw) so a reused id never inherits a dead
    /// task's handles.
    open_files: BTreeMap<TaskId, OpenFileTable>,
    /// Each live task's running total of mapped address space, in bytes
    /// (whole pages): anonymous memory from `mem_map` plus demand-paged
    /// file regions from `file_map`. Co-located with the address space for
    /// the same reason as [`Self::streams`]: it shares the exact
    /// per-process lifecycle — accrued on a map, released on the matching
    /// unmap, and dropped when the task exits — and is keyed by the
    /// same [`TaskId`]. This is the live usage the kernel checks the
    /// `LimitKind::AddressSpaceBytes` ceiling against so the limit is
    /// actually enforced on the allocation path (fail closed) rather than
    /// merely stored. A task with no entry has mapped nothing, so
    /// [`Self::mapped_aspace_bytes`] resolves to `0`. Dropped at
    /// [`withdraw`](Self::withdraw) so a reused id never inherits a dead
    /// task's accounting.
    mapped_aspace_bytes: BTreeMap<TaskId, u64>,
    /// Each live task's current working directory, as a normalised absolute
    /// path (the `/`-view spelling). Co-located with the address space for
    /// the same reason as [`Self::streams`]: it shares the exact per-process
    /// lifecycle — inherited from the spawner at spawn, changed by `fs_chdir`,
    /// and dropped when the task exits — and is keyed by the same [`TaskId`].
    /// A task with no entry resolves to the root `/` via [`Self::cwd`], so a
    /// process whose directory was never established resolves relative paths
    /// against the root rather than failing (a sensible, fail-safe default;
    /// the root is the least-privileged starting point). Dropped at
    /// [`withdraw`](Self::withdraw) so a reused id never inherits a dead
    /// task's directory.
    cwds: BTreeMap<TaskId, String>,
    /// Each live task's demand-paged file mappings (the regions `file_map`
    /// reserves and the fault path backs), keyed by region base. Co-located
    /// with the address space for the same reason as [`Self::open_files`]:
    /// a mapping shares the exact per-process lifecycle — recorded on
    /// `file_map`, removed on `file_unmap`, and dropped when the task exits
    /// — and is keyed by the same kernel-trusted [`TaskId`]. A task with no
    /// entry has mapped no file, so a fault outside every record resolves
    /// to `None` and the task is terminated rather than silently backed
    /// (fail closed). Dropped at [`withdraw`](Self::withdraw) so a reused
    /// id never inherits a dead task's mappings.
    file_regions: BTreeMap<TaskId, BTreeMap<u64, FileRegion>>,
    /// Each live task's reserved demand-paged **anonymous** mappings (the
    /// regions `mem_map` reserves and the anonymous fault path backs one
    /// zeroed page at a time), keyed by region base and valued by the
    /// page-rounded page count reserved. Co-located with the address space
    /// for the same reason as [`Self::file_regions`]: a mapping shares the
    /// exact per-process lifecycle — recorded on `mem_map`, removed on
    /// `mem_unmap`, and dropped when the task exits — and is keyed by the
    /// same kernel-trusted [`TaskId`]. A task with no entry has reserved no
    /// anonymous region, so a fault outside every record resolves to `None`
    /// and the task is terminated rather than silently backed (fail
    /// closed). The resident frames themselves are owned by the task's live
    /// address space and reclaimed by its drop; this map is only the
    /// fault-validation and accounting bookkeeping. Dropped at
    /// [`withdraw`](Self::withdraw) so a reused id never inherits a dead
    /// task's mappings.
    anon_regions: BTreeMap<TaskId, BTreeMap<u64, u64>>,
    /// Each live task's reserved user-stack span (the region the spawn
    /// layout placed and the stack-growth fault path backs on demand).
    /// Co-located with the address space for the same reason as
    /// [`Self::limits`]: the span shares the exact per-process lifecycle —
    /// recorded at admission, dropped when the task exits — and is keyed by
    /// the same kernel-trusted [`TaskId`]. A task with no entry has no
    /// growable stack, so a fault below its committed stack resolves to
    /// `None` and stays fatal (fail closed). Dropped at
    /// [`withdraw`](Self::withdraw) so a reused id never inherits a dead
    /// task's span.
    stack_spans: BTreeMap<TaskId, StackSpan>,
    /// The one-shot file delegations minted **to** each live task and not
    /// yet redeemed (`fd_grant`/`fd_redeem`, `plans/CAPABILITY_USE.md`
    /// CU6). Co-located with the address space for the same reason as
    /// [`Self::grants`]: a pending delegation shares the exact per-process
    /// lifecycle — minted when a grantor delegates to the task, consumed on
    /// redemption, and dropped when the recipient exits — and is keyed by
    /// the same kernel-trusted [`TaskId`]. A task with no entry holds no
    /// pending delegation, so [`Self::redeem_fd_delegation`] resolves to
    /// `NotFound` (fail closed: a task can redeem only what was actually
    /// minted to it). Dropped at [`withdraw`](Self::withdraw) so a reused
    /// id never inherits a dead task's pending delegations.
    fd_delegations: BTreeMap<TaskId, TaskFdDelegations>,
    /// Each live task's PIE load base — the lowest user virtual address
    /// its relocated program image occupies. Recorded at admission by the
    /// spawn path (the lowest relocated segment vaddr) and used only by the
    /// user-fault crash path to express a faulting `pc` and every backtrace
    /// frame as a **program-relative offset** (`addr - load_base`) instead
    /// of an absolute virtual address, so a privileged crash record never
    /// publishes the task's address-space layout and the offsets resolve
    /// offline against the unstripped binary. Co-located with the address
    /// space for the same reason as [`Self::stack_spans`]: it shares the
    /// exact per-process lifecycle and is keyed by the same kernel-trusted
    /// [`TaskId`]. A task with no entry (a kernel task, or one whose image
    /// was loaded at a base the spawn path did not record) has no load base
    /// and its offsets degrade to absolute values only inside the
    /// capability-gated record. Dropped at [`withdraw`](Self::withdraw) so
    /// a reused id never inherits a dead task's base.
    load_bases: BTreeMap<TaskId, u64>,
}

/// One live demand-paged file mapping of a task: the region `file_map`
/// reserved, the file range behind it, and the mapping-time identity the
/// fault path reads under.
///
/// `len` is the page-rounded byte length actually reserved (the figure
/// charged against `LimitKind::AddressSpaceBytes` and credited back on
/// release), and `offset` the page-aligned file byte offset of the
/// region's first page. `uid` and `caps` are the caller's kernel-attested
/// owner and effective capability snapshot at map time — the same
/// authority model as an open descriptor, so a later capability revocation
/// affects new mappings, not pages an existing mapping still faults in
/// (exactly as it does not retract an open descriptor).
#[derive(Clone, Debug)]
pub struct FileRegion {
    /// Base user virtual address of the reserved region.
    pub base: u64,
    /// Page-rounded byte length of the region.
    pub len: u64,
    /// Absolute path of the mapped file, as resolved at open time.
    pub path: String,
    /// Page-aligned byte offset into the file of the region's first page.
    pub offset: u64,
    /// The mapping caller's kernel-attested owning user id.
    pub uid: u32,
    /// The mapping caller's effective capability set at map time.
    pub caps: CapabilitySet,
}

/// One live task's reserved user-stack span: the structural bound the
/// stack may ever occupy, and the low-water mark of what is committed.
///
/// `reserve_base` is the lowest page of the whole reserved span (the
/// unmapped guard page sits immediately below it), `committed_base` the
/// lowest page currently backed by a frame, and `top` one past the
/// highest stack byte. The pages in `[reserve_base, committed_base)` are
/// the growth room the stack-growth fault path backs one page at a time,
/// bounded by the task's settable `LimitKind::StackBytes` soft bound; the
/// guard page below `reserve_base` never maps, so a true overrun still
/// faults deterministically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackSpan {
    reserve_base: u64,
    committed_base: u64,
    top: u64,
}

impl StackSpan {
    /// Build a span from its page-aligned bounds, failing closed with
    /// `None` on a malformed shape: a misaligned bound, a committed base
    /// below the reserve base, or an empty committed top
    /// (`committed_base >= top`). The committed top is never empty by
    /// construction — the layout derivation refuses a zero commit — so a
    /// refusal here signals a caller defect, not a policy choice.
    #[must_use]
    pub fn new(reserve_base: u64, committed_base: u64, top: u64) -> Option<Self> {
        let page = PAGE_SIZE as u64;
        let aligned = reserve_base.is_multiple_of(page)
            && committed_base.is_multiple_of(page)
            && top.is_multiple_of(page);
        (aligned && reserve_base <= committed_base && committed_base < top).then_some(Self {
            reserve_base,
            committed_base,
            top,
        })
    }

    /// Page-aligned user virtual address of the lowest page of the whole
    /// reserved span.
    #[must_use]
    pub fn reserve_base(&self) -> u64 {
        self.reserve_base
    }

    /// Page-aligned user virtual address of the lowest committed page.
    #[must_use]
    pub fn committed_base(&self) -> u64 {
        self.committed_base
    }

    /// One past the highest stack byte (the page-aligned span top).
    #[must_use]
    pub fn top(&self) -> u64 {
        self.top
    }

    /// Bytes of the span currently committed (`top - committed_base`).
    #[must_use]
    pub fn committed_bytes(&self) -> u64 {
        self.top - self.committed_base
    }

    /// Whether `va` lies in the uncommitted growth room — inside the span,
    /// below the committed base. Only such a fault is stack growth; the
    /// guard page below `reserve_base` and everything outside the span
    /// resolve `false` and stay fatal.
    #[must_use]
    pub fn in_growth_room(&self, va: u64) -> bool {
        va >= self.reserve_base && va < self.committed_base
    }
}

/// Bounded byte window past a region's end (or below the stack guard)
/// within which a fatal fault is described as a small, region-relative
/// offset rather than a genuinely wild access.
///
/// 64 KiB — a handful of pages: wide enough to catch a realistic buffer
/// overrun or an off-by-a-stride bug, narrow enough that the offset it
/// publishes ("0x40 past *a* region") discloses a *distance*, never a
/// location.
pub const NEAR_REGION_WINDOW: u64 = 64 * 1024;

/// Where a fatal user fault landed relative to the address space the task
/// legitimately owns, as a coarse, non-leaking descriptor.
///
/// This exists for the one diagnostics-policy line the fault path must
/// never cross: a fault-kill record may say *how far* a fault was from
/// something the task owns, but never *where* that something (or the
/// fault) lives, so the shared, hash-chained audit log never becomes an
/// address-space-layout oracle. Every variant carries at most an offset —
/// a distance from a fixed anchor (virtual address 0, the stack guard, a
/// region end) — never an absolute virtual address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultLocality {
    /// Within the first page: a null-pointer dereference. `offset` is
    /// measured from virtual address 0, so it reveals nothing about
    /// layout.
    NullPage {
        /// Distance of the fault above virtual address 0.
        offset: u64,
    },
    /// A bounded distance below the reserved stack span's guard page — a
    /// stack overflow that ran past the guard. `distance` is how far below
    /// the reserve base the fault landed, not the base itself.
    BelowStackGuard {
        /// Distance the fault landed below the stack reserve base.
        distance: u64,
    },
    /// A bounded distance past the end of a specific mapping the task
    /// owns. `offset` is that distance; the region it is relative to is
    /// deliberately not identified.
    PastRegion {
        /// Distance of the fault past the nearest owned region's end.
        offset: u64,
    },
    /// The fault landed **inside** a region the task legitimately owns (a
    /// reserved anonymous mapping, a file mapping, or the committed/growth
    /// stack span) but could not be resolved — the deterministic
    /// out-of-memory case, where a demand-paged page could not be backed.
    /// This is emphatically *not* a wild access: the address is memory the
    /// task reserved, so it carries no offset to leak and is reported as a
    /// distinct, honest "in a region you own" locality rather than the
    /// scaremongering "wild".
    InRegion,
    /// Genuinely far from every mapping and from the null page — no
    /// meaningful offset to report.
    Wild,
}

impl FaultLocality {
    /// Stable, non-leaking bucket name for the audit `fault_offset` field.
    #[must_use]
    pub fn bucket(self) -> &'static str {
        match self {
            Self::NullPage { .. } => "null_page",
            Self::BelowStackGuard { .. } => "below_stack_guard",
            Self::PastRegion { .. } => "region",
            Self::InRegion => "in_region",
            Self::Wild => "wild",
        }
    }

    /// The region-relative offset (or distance) this locality carries, or
    /// `None` for [`Self::Wild`], which has no meaningful anchor. Never an
    /// absolute address.
    #[must_use]
    pub fn offset(self) -> Option<u64> {
        match self {
            Self::NullPage { offset } | Self::PastRegion { offset } => Some(offset),
            Self::BelowStackGuard { distance } => Some(distance),
            Self::InRegion | Self::Wild => None,
        }
    }
}

/// A filesystem object delegated by another process, carrying the
/// **grantor's** captured authority (`plans/CAPABILITY_USE.md` CU6 — the
/// file picker's one-shot hand-off).
///
/// Captured at `fd_grant` time from the grantor's kernel-attested identity
/// — never from anything the recipient supplies — so every later operation
/// on the redeemed descriptor is re-authorised through the secured VFS
/// under exactly the authority the grantor held, no more. The recipient's
/// own identity and capability set never enter the check: the delegation
/// *is* the authority, established by the grantor's user-mediated choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedFile {
    /// The resolved absolute path the grantor's descriptor named.
    pub path: String,
    /// The grantor's uid, the identity every VFS re-check runs under.
    pub uid: u32,
    /// The grantor's effective capability set at grant time.
    pub caps: CapabilitySet,
}

/// What a descriptor resolves to: a filesystem path or a typed resource.
///
/// A descriptor's number is unique per process regardless of what backs it,
/// so both filesystem opens ([`SyscallNumber::FS_OPEN`](tairix_abi::SyscallNumber))
/// and resource opens
/// ([`SyscallNumber::RESOURCE_OPEN`](tairix_abi::SyscallNumber)) draw from the
/// single `OpenFileTable` allocator; the backing records which subsystem
/// serves the handle's reads and writes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenBacking {
    /// A filesystem object at the given absolute path. The path is stored,
    /// not a driver inode pointer, because the filesystem is owned by the
    /// disk-owning service the handle ops route to; the kernel re-resolves
    /// and re-authorises the path through the secured VFS on every operation
    /// under the caller's real credentials (no cached authority).
    Path(String),
    /// A typed non-filesystem resource (`plans/ALIAS.md`), resolved and
    /// authorised once at open time; its reads and writes route to the named
    /// kernel subsystem rather than the VFS.
    Resource(ResourceBacking),
    /// One counted end of a kernel pipe (`plans/SPAWN.md` SP10). Cloning
    /// the entry (a spawn wiring a child onto the end) registers one more
    /// live end; dropping it (close, exit, a failed spawn's unwind)
    /// releases it and wakes the peer side — the [`PipeEnd`] handle owns
    /// that bookkeeping.
    Pipe(PipeEnd),
    /// A filesystem object delegated one-shot by another process
    /// (`fd_grant`/`fd_redeem`), operated on under the **grantor's**
    /// captured identity rather than the holder's. Never re-delegatable:
    /// `fd_grant` accepts only [`OpenBacking::Path`], so a delegation
    /// chain cannot form and delegated authority never widens.
    Delegated(DelegatedFile),
    /// The master end of a kernel pseudo-terminal (`plans/PTY.md`): the
    /// terminal emulator's handle. A read drains the slave's cooked output;
    /// a write feeds the input discipline. Cloning the entry registers one
    /// more live master end; dropping it releases it and wakes the peer —
    /// the [`PtyMasterEnd`] handle owns that bookkeeping, exactly as
    /// [`OpenBacking::Pipe`].
    PtyMaster(PtyMasterEnd),
    /// The slave end of a kernel pseudo-terminal (`plans/PTY.md`): wired as
    /// a child shell's fd 0/1/2. A read drains the input (echoing in cooked
    /// mode); a write is cooked (`ONLCR`) onto the output. The slave is a
    /// *tty* for `stream_input_mode`/`terminal_size`/`console_foreground`.
    PtySlave(PtySlaveEnd),
}

/// A readable stream end borrowed in place for the wait-set readiness peek:
/// a pipe read end, a pty master, or a pty slave. The one shape
/// [`AddressSpaceRegistry::stream_read_member`] and
/// [`AddressSpaceRegistry::stream_readable`] resolve to, so the readiness
/// check has a single definition across every stream kind.
enum ReadStreamEnd<'a> {
    /// A pipe read end.
    Pipe(&'a PipeEnd),
    /// A pty master end (drains the slave's cooked output).
    PtyMaster(&'a PtyMasterEnd),
    /// A pty slave end (drains the input).
    PtySlave(&'a PtySlaveEnd),
}

/// One open descriptor: what it resolves to and the [`OpenFlags`] it was
/// opened with.
///
/// The flags fix the access the handle permits — a read against a handle
/// opened without [`OpenFlags::READ`], or a write without
/// [`OpenFlags::WRITE`], fails closed without ever reaching the backing.
///
/// Entries cloned from one another (a `stream_read`/`stream_write` caller's
/// snapshot, or a spawn wiring a child onto a parent descriptor) share one
/// *open-file description*: the [`Self::cursor`] the sequential stream
/// operations advance is one `Arc`'d counter, so two wired sinks on the
/// same description interleave their output at one position (the POSIX
/// dup semantics) instead of silently overwriting each other.
#[derive(Clone, Debug)]
pub struct OpenFile {
    /// What the descriptor resolves to.
    pub backing: OpenBacking,
    /// The access/behaviour flags the descriptor was opened with.
    pub flags: OpenFlags,
    /// The shared sequential-stream position (bytes from the start) the
    /// `stream_read`/`stream_write` handlers advance for a path-backed
    /// entry. Positional `fs_read`/`fs_write` never touch it; pipe and
    /// resource backings have no position and ignore it.
    cursor: Arc<AtomicU64>,
}

/// Two entries are equal when they name the same backing with the same
/// flags. The cursor is deliberately not part of equality: it is mutable
/// per-description state, not part of what the descriptor *is*.
impl PartialEq for OpenFile {
    fn eq(&self, other: &Self) -> bool {
        self.backing == other.backing && self.flags == other.flags
    }
}

impl Eq for OpenFile {}

impl OpenFile {
    /// A fresh entry over `backing` with `flags`, its stream cursor at the
    /// start.
    #[must_use]
    pub fn new(backing: OpenBacking, flags: OpenFlags) -> Self {
        Self {
            backing,
            flags,
            cursor: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The pipe end this descriptor holds, or `None` when it is backed by
    /// a path or resource.
    #[must_use]
    pub fn pipe(&self) -> Option<&PipeEnd> {
        match &self.backing {
            OpenBacking::Pipe(end) => Some(end),
            _ => None,
        }
    }

    /// The pty master end this descriptor holds, or `None` otherwise.
    #[must_use]
    pub fn pty_master(&self) -> Option<&PtyMasterEnd> {
        match &self.backing {
            OpenBacking::PtyMaster(end) => Some(end),
            _ => None,
        }
    }

    /// The pty slave end this descriptor holds, or `None` otherwise.
    #[must_use]
    pub fn pty_slave(&self) -> Option<&PtySlaveEnd> {
        match &self.backing {
            OpenBacking::PtySlave(end) => Some(end),
            _ => None,
        }
    }

    /// The description's current sequential-stream position.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// Advance the description's stream position by `n` bytes. Shared by
    /// every clone of the description, so dup'd sinks append at one
    /// position.
    pub fn advance_cursor(&self, n: u64) {
        self.cursor.fetch_add(n, Ordering::AcqRel);
    }

    /// The absolute filesystem path this descriptor resolves to, or `None`
    /// when it is backed by a resource or pipe rather than a path.
    ///
    /// A delegated backing deliberately answers `None` here: its path is
    /// operated on under the grantor's captured identity, never under the
    /// holder's own credentials, so a caller resolving "the caller's own
    /// path" must not see it.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match &self.backing {
            OpenBacking::Path(path) => Some(path),
            OpenBacking::Resource(_)
            | OpenBacking::Pipe(_)
            | OpenBacking::Delegated(_)
            | OpenBacking::PtyMaster(_)
            | OpenBacking::PtySlave(_) => None,
        }
    }

    /// The resource this descriptor resolves to, or `None` when it is backed
    /// by a filesystem path or pipe.
    #[must_use]
    pub fn resource(&self) -> Option<ResourceBacking> {
        match &self.backing {
            OpenBacking::Resource(backing) => Some(*backing),
            OpenBacking::Path(_)
            | OpenBacking::Pipe(_)
            | OpenBacking::Delegated(_)
            | OpenBacking::PtyMaster(_)
            | OpenBacking::PtySlave(_) => None,
        }
    }
}

/// One task's open file/directory descriptors.
///
/// Descriptor numbers are allocated at or above [`STD_STREAM_COUNT`] (the
/// standard streams fd 0..3 are reserved by the process ABI and never handed
/// out here) using the lowest free number, so a long-lived process that
/// opens and closes many files reuses descriptors rather than marching a
/// monotonic counter toward exhaustion (a grow-not-cap posture, never a
/// fixed ceiling). The whole record is dropped when the task is
/// [`withdraw`](AddressSpaceRegistry::withdraw)n, so a reused [`TaskId`]
/// starts from an empty descriptor set.
#[derive(Default)]
struct OpenFileTable {
    by_fd: BTreeMap<u32, OpenFile>,
}

impl OpenFileTable {
    /// Allocate the lowest free descriptor number at or above
    /// [`STD_STREAM_COUNT`].
    ///
    /// Returns [`Errno::OutOfRange`] only when every descriptor number up to
    /// [`u32::MAX`] is in use — a genuine exhaustion of the descriptor space,
    /// not a hand-picked ceiling.
    fn alloc_fd(&self) -> Result<u32, Errno> {
        // `STD_STREAM_COUNT` (4) fits a u32 with room to spare; the checked
        // conversion makes that explicit rather than truncating.
        let mut candidate = u32::try_from(STD_STREAM_COUNT).map_err(|_| Errno::OutOfRange)?;
        for &fd in self.by_fd.keys() {
            if fd < candidate {
                continue;
            }
            if fd > candidate {
                break;
            }
            // `fd == candidate`: this number is taken, try the next. Saturate
            // at the top of the descriptor space and fail closed below rather
            // than wrapping back into the reserved range.
            candidate = candidate.checked_add(1).ok_or(Errno::OutOfRange)?;
        }
        Ok(candidate)
    }
}

/// One task's device-resource grants: the handles it may pass to
/// `mmio_map`, each naming exactly one granted [`HwResource`].
///
/// Handles are minted per task, monotonically from `1` (handle `0` is the
/// reserved invalid value and is never issued), and are never reused
/// within a task's lifetime — so a stale handle from a reclaimed grant
/// can never alias a later one. The whole record is dropped when the task
/// is [`withdraw`](AddressSpaceRegistry::withdraw)n, so a reused [`TaskId`]
/// starts from an empty grant set and cannot inherit a dead task's windows
/// (fail closed).
#[derive(Default)]
struct TaskGrants {
    /// The next handle value to issue. Starts at `1`; only ever increases,
    /// so handles are unique for the task's whole lifetime.
    next_handle: u64,
    /// The granted resource behind each issued handle.
    by_handle: BTreeMap<u64, HwResource>,
}

/// The one-shot file delegations minted **to** one task and not yet
/// redeemed (`fd_grant`/`fd_redeem`, `plans/CAPABILITY_USE.md` CU6).
///
/// Handles follow the [`TaskGrants`] discipline: minted per recipient,
/// monotonically from `1` (handle `0` is the reserved invalid value and is
/// never issued), never reused within the task's lifetime, and resolvable
/// only when presented by the recipient itself. The whole record is dropped
/// when the recipient is [`withdraw`](AddressSpaceRegistry::withdraw)n, so
/// an unredeemed delegation dies with its recipient and never leaks
/// (fail closed).
#[derive(Default)]
struct TaskFdDelegations {
    /// The next handle value to issue. Starts at `1`; only ever increases.
    next_handle: u64,
    /// The pending delegation behind each issued handle, with the open
    /// flags the grantor's descriptor carried.
    by_handle: BTreeMap<u64, (DelegatedFile, OpenFlags)>,
}

/// The handle already naming `value` in `by_handle`, if any — the
/// duplicate suppression both delegation tables share.
///
/// Delegation conveys a *set* of authority, so re-granting something a
/// recipient already holds must hand back the handle it already has rather
/// than append a second, identical entry. That is what bounds these
/// kernel-side tables: without it a donor can drive an unbounded allocation
/// in a victim's address-space record simply by repeating one delegation
/// syscall. Both minting paths call this so their notion of "already held"
/// cannot drift apart.
///
/// Linear over a table whose length is, by virtue of this very check, the
/// number of *distinct* authorities the task holds — a handful for a driver,
/// and never grown by repetition.
fn existing_handle<V: PartialEq>(by_handle: &BTreeMap<u64, V>, value: &V) -> Option<u64> {
    by_handle
        .iter()
        .find(|(_, held)| *held == value)
        .map(|(&handle, _)| handle)
}

impl AddressSpaceRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            streams: BTreeMap::new(),
            limits: BTreeMap::new(),
            grants: BTreeMap::new(),
            loaded_nodes: BTreeMap::new(),
            open_files: BTreeMap::new(),
            mapped_aspace_bytes: BTreeMap::new(),
            cwds: BTreeMap::new(),
            file_regions: BTreeMap::new(),
            anon_regions: BTreeMap::new(),
            stack_spans: BTreeMap::new(),
            fd_delegations: BTreeMap::new(),
            load_bases: BTreeMap::new(),
            default_limits: LimitSet::DEFAULT,
            pinned: BTreeSet::new(),
        }
    }

    /// Register `task`'s user address space and the physical map that
    /// backs it.
    ///
    /// # Errors
    ///
    /// [`AspaceError::AlreadyPresent`] if an address space is already
    /// registered for `task`; the existing entry is left untouched.
    pub fn register(
        &mut self,
        task: TaskId,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
    ) -> Result<(), AspaceError> {
        if self.tasks.contains_key(&task) {
            return Err(AspaceError::AlreadyPresent);
        }
        self.tasks.insert(task, TaskAddressSpace { space, physmap });
        Ok(())
    }

    /// Replace `task`'s registered address-space snapshot with `space`,
    /// keeping its existing physical map, and return `true` if an entry was
    /// present to update.
    ///
    /// The registry stores a `Send + Sync`
    /// [`FrozenAddressSpace`](tairix_kernel_mem::vmm::FrozenAddressSpace)
    /// snapshot rather than the live, `!Sync` arch space (see
    /// [`tairix_kernel_mem::LiveUserSpace`]). A snapshot frozen at spawn
    /// describes only the task's spawn-time image and stack; once the task
    /// maps its own heap (`mem_map`), unmaps it, or a driver maps a granted
    /// window/DMA buffer, the snapshot is stale and the
    /// [`tairix_kernel_mem::uaccess`] copy path can no longer see the new
    /// (or freed) pages. The mutating syscall handler re-freezes the live
    /// space and calls this to publish the fresh snapshot, so the very next
    /// `copy_in` / `copy_out` reflects the current mappings (the copy path must see exactly the task's live memory; the
    /// behaviour
    /// [`FrozenAddressSpace`](tairix_kernel_mem::vmm::FrozenAddressSpace)'s
    /// docs prescribe for a remap path).
    ///
    /// The physical map is left untouched: it is the kernel direct map,
    /// identical across every snapshot of the same task, so re-freezing only
    /// the mappings is sufficient and avoids re-boxing it. A task with no
    /// registered entry is **not** created here — re-freezing concerns only
    /// a task that already has a space (a kernel task has none and reaches no
    /// user copy path), so the call is a no-op returning `false` (fail
    /// closed).
    pub fn reregister_space(
        &mut self,
        task: TaskId,
        space: Box<dyn UserAddressSpace + Send + Sync>,
    ) -> bool {
        match self.tasks.get_mut(&task) {
            Some(entry) => {
                entry.space = space;
                true
            }
            None => false,
        }
    }

    /// Apply a single-page mapping delta to `task`'s stored snapshot in
    /// place — record `page → mapping` (`Some` on a fresh backing, `None`
    /// on an unmap) — returning `true` when the snapshot absorbed it.
    ///
    /// This is the demand-fault resolver's fast path: it backs one page per
    /// fault, so updating the one entry keeps per-fault work O(log n) instead
    /// of re-freezing the whole address space (which would make touching a
    /// large mapping O(N²), tens of seconds under emulation). A snapshot that
    /// cannot absorb an in-place delta (the host double), or a task with no
    /// entry, returns `false` and the caller falls back to a full re-freeze —
    /// so this is a pure optimisation, never a correctness dependency. The
    /// physical map is untouched (it is the shared kernel direct map).
    pub fn note_faulted_page(
        &mut self,
        task: TaskId,
        page: Page,
        mapping: Option<(Frame, MapFlags)>,
    ) -> bool {
        match self.tasks.get_mut(&task) {
            Some(entry) => entry.space.apply_page_delta(page, mapping),
            None => false,
        }
    }

    /// Withdraw `task`'s entry, returning `true` if one was present.
    ///
    /// Idempotent: withdrawing a task with no entry (e.g. a kernel task
    /// that never had a user address space, or a double `exit`) is a
    /// no-op that returns `false`. The task's standard-stream descriptor
    /// table is dropped at the same time so a reused id never inherits a
    /// dead task's streams (fail closed).
    pub fn withdraw(&mut self, task: TaskId) -> bool {
        // The pin mark is per-process state: a task that exits (or is
        // killed) leaves the pinned set, so a reused id never inherits a
        // dead task's exemption and the system-wide pinned aggregate
        // drops with the task.
        let had_pin = self.pinned.remove(&task);
        let had_streams = self.streams.remove(&task).is_some();
        let had_limits = self.limits.remove(&task).is_some();
        let had_grants = self.grants.remove(&task).is_some();
        let had_node = self.loaded_nodes.remove(&task).is_some();
        let had_files = self.open_files.remove(&task).is_some();
        let had_anon = self.mapped_aspace_bytes.remove(&task).is_some();
        let had_cwd = self.cwds.remove(&task).is_some();
        let had_file_regions = self.file_regions.remove(&task).is_some();
        let had_anon_regions = self.anon_regions.remove(&task).is_some();
        let had_stack_span = self.stack_spans.remove(&task).is_some();
        let had_load_base = self.load_bases.remove(&task).is_some();
        let had_fd_delegations = self.fd_delegations.remove(&task).is_some();
        let had_task = self.tasks.remove(&task).is_some();
        // Reclaim post-condition (debug-only tripwire): every per-task map
        // has just had `task` removed, so no map may still hold it. A
        // residual entry means either a per-task map was added without a
        // matching removal above — the precursor to a reused id inheriting
        // a dead task's state — or a `remove` did not take effect, i.e. the
        // map is corrupt. Faulting here names the reclaim site deterministically
        // rather than letting the debris surface as a wedge a second later.
        // Compiled out of shippable images (`debug_assertions` off).
        debug_assert!(
            self.stale_task_entry(task).is_none(),
            "aspace: withdraw left task {task:?} in the {:?} map (reused-id debris or map corruption)",
            self.stale_task_entry(task)
        );
        had_task
            || had_pin
            || had_streams
            || had_limits
            || had_grants
            || had_node
            || had_files
            || had_anon
            || had_cwd
            || had_file_regions
            || had_anon_regions
            || had_stack_span
            || had_load_base
            || had_fd_delegations
    }

    /// The name of the first per-task map that still holds `task`, or `None`
    /// when no per-task state references it — the check
    /// [`withdraw`](Self::withdraw) asserts as its reclaim post-condition.
    ///
    /// Every field enumerated here is one [`withdraw`](Self::withdraw)
    /// clears; the two lists must stay in lockstep, so a per-task map added
    /// to the registry is added to *both*. Pure and host-tested; the caller
    /// asserts on it only in the `debug_assertions` (non-shippable) build.
    #[must_use]
    pub fn stale_task_entry(&self, task: TaskId) -> Option<&'static str> {
        if self.tasks.contains_key(&task) {
            return Some("tasks");
        }
        if self.pinned.contains(&task) {
            return Some("pinned");
        }
        if self.streams.contains_key(&task) {
            return Some("streams");
        }
        if self.limits.contains_key(&task) {
            return Some("limits");
        }
        if self.grants.contains_key(&task) {
            return Some("grants");
        }
        if self.loaded_nodes.contains_key(&task) {
            return Some("loaded_nodes");
        }
        if self.open_files.contains_key(&task) {
            return Some("open_files");
        }
        if self.mapped_aspace_bytes.contains_key(&task) {
            return Some("mapped_aspace_bytes");
        }
        if self.cwds.contains_key(&task) {
            return Some("cwds");
        }
        if self.file_regions.contains_key(&task) {
            return Some("file_regions");
        }
        if self.anon_regions.contains_key(&task) {
            return Some("anon_regions");
        }
        if self.stack_spans.contains_key(&task) {
            return Some("stack_spans");
        }
        if self.load_bases.contains_key(&task) {
            return Some("load_bases");
        }
        if self.fd_delegations.contains_key(&task) {
            return Some("fd_delegations");
        }
        None
    }

    /// Record that the autoloaded driver `task` was loaded for the discovered
    /// hardware-tree node `node_id`.
    ///
    /// Called by the privileged driver-spawn path beside
    /// [`mint_grant`](Self::mint_grant), under the same write lock, so a
    /// driver's matched node and its grants are established together. The
    /// `node_id` is kernel-sourced (the matched node the device manager
    /// resolved), never caller-supplied. The ordinary
    /// `spawn` path records nothing, so a non-driver task has no loaded node
    /// and cannot publish a child (fail closed).
    pub fn set_loaded_node(&mut self, task: TaskId, node_id: u32) {
        self.loaded_nodes.insert(task, node_id);
    }

    /// The discovered hardware-tree node `task` was loaded for, or `None`
    /// when `task` is not an autoloaded driver bound to a node.
    ///
    /// The security spine of `hw_emit_node`'s parent assignment: the kernel parents a driver's published
    /// child under *this* node, and a `task` with no loaded node may publish
    /// nothing (fail closed). The `task` argument is the kernel-trusted
    /// caller id, never caller-supplied.
    #[must_use]
    pub fn loaded_node(&self, task: TaskId) -> Option<u32> {
        self.loaded_nodes.get(&task).copied()
    }

    /// Mint a device-resource grant for `task`, returning the unforgeable,
    /// kernel-issued handle the task passes to `mmio_map` to reach exactly
    /// `resource` and nothing else (resources are
    /// capability-grant requests, never ambient handles; — a driver
    /// reaches only the resources its matched node requested).
    ///
    /// Called by the driver-admission path when a node's requested
    /// resources are granted to the driver task it loads, and by the
    /// delegation syscalls when one task passes a resource it holds to
    /// another. The returned handle is meaningful only when presented by
    /// `task` itself: [`Self::grant`] is keyed by the kernel-trusted caller
    /// id, so another task passing the same numeric value resolves to
    /// nothing (handle forgery is refused).
    ///
    /// **Idempotent.** Granting `task` a resource it already holds returns
    /// the handle it already has; only a resource new to `task` mints a
    /// fresh handle (monotonic from `1`, so a handle number never aliases a
    /// reclaimed grant). Authority is a set: repetition must not be able to
    /// grow a recipient's kernel-side table.
    pub fn mint_grant(&mut self, task: TaskId, resource: HwResource) -> u64 {
        let entry = self.grants.entry(task).or_default();
        // Authority is a set, not a multiset: a task that already holds
        // exactly this resource is handed the handle it already has rather
        // than a second entry naming the same thing. Without this, a donor
        // holding one grant could call a delegation syscall in a loop and
        // grow the *recipient's* kernel-side table without limit — an
        // unbounded kernel allocation an unprivileged peer can drive. The
        // match is exact, never `covers`: returning the handle of a *wider*
        // grant would hand back authority the donor did not name.
        if let Some(handle) = existing_handle(&entry.by_handle, &resource) {
            return handle;
        }
        // Handle 0 is the reserved invalid value; the first minted handle
        // is 1. `next_handle` only ever increases within a task's life, so
        // a handle is never reused even after its grant is reclaimed.
        entry.next_handle += 1;
        let handle = entry.next_handle;
        entry.by_handle.insert(handle, resource);
        handle
    }

    /// Withdraw every task's per-endpoint grant naming any call endpoint in
    /// `endpoints`, returning how many grants were revoked.
    ///
    /// A [`HwResourceKind::Endpoint`] grant names an endpoint by its
    /// **numeric id**, and that id is re-creatable: once an endpoint is
    /// destroyed, a *different* task may bind the same number. A grant that
    /// survived its endpoint would therefore silently retarget onto the new
    /// instance and let its holder call a service it was never granted. The
    /// endpoint teardown path calls this in the same step that destroys the
    /// endpoints, so delegated authority can never outlive the endpoint
    /// *instance* it was issued against and id reuse is safe by construction.
    /// A holder's next call fails closed (`grant_covers` no longer matches),
    /// never retargets.
    ///
    /// Deliberately a single pass over the grant tables rather than a
    /// reverse `endpoint -> holders` index: teardown is a cold path (a
    /// service process ending), while an index would be a second source of
    /// truth to keep in step across every mint, withdrawal, and revocation —
    /// and a desync in it would silently reopen exactly the hole this closes.
    pub fn revoke_endpoint_grants(&mut self, endpoints: &BTreeSet<u64>) -> usize {
        if endpoints.is_empty() {
            return 0;
        }
        let mut revoked = 0;
        for entry in self.grants.values_mut() {
            entry.by_handle.retain(|_, resource| {
                let doomed = resource.kind() == Some(HwResourceKind::Endpoint)
                    && endpoints.contains(&resource.base());
                revoked += usize::from(doomed);
                !doomed
            });
        }
        revoked
    }

    /// Resolve the device-resource grant identified by `handle` for the
    /// owning `task`, or `None` (fail closed).
    ///
    /// Returns the granted [`HwResource`] iff `handle` was minted for
    /// `task`; `None` for an unknown handle, the reserved `0` handle, a
    /// handle minted for a *different* task (forgery — a driver cannot
    /// reach another driver's window by guessing a handle value), or a
    /// grant since reclaimed on exit. The `task` argument is the
    /// kernel-trusted caller id, never a caller-supplied value, so it is
    /// the security spine of the `mmio_map` handler (no
    /// trusted-caller shortcut;).
    #[must_use]
    pub fn grant(&self, task: TaskId, handle: u64) -> Option<HwResource> {
        self.grants.get(&task)?.by_handle.get(&handle).copied()
    }

    /// Serialise `task`'s device-resource grants as consecutive
    /// [`GrantedResource`] records (each [`GrantedResource::WIRE_LEN`]
    /// bytes), in ascending handle order, for delivery to the task through
    /// the `resource_grants` syscall.
    ///
    /// Returns an empty vector for a task with no grants — a valid, empty
    /// result, not an error (an unbound node is
    /// normal). The set is bounded by construction: handles are minted only
    /// by the kernel's driver-admission path, one per [`HwResource`] the
    /// matched node requested (no ambient authority), so a
    /// node's fixed resource maximum bounds the record count. Ascending
    /// handle order makes the delivered sequence deterministic
    /// ([`BTreeMap`] iterates by key).
    #[must_use]
    pub fn grants_to_le_bytes(&self, task: TaskId) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(entry) = self.grants.get(&task) {
            out.reserve(entry.by_handle.len() * GrantedResource::WIRE_LEN);
            for (&handle, &resource) in &entry.by_handle {
                out.extend_from_slice(&GrantedResource::new(handle, resource).to_le_bytes());
            }
        }
        out
    }

    /// Returns `true` iff one of `task`'s minted device-resource grants
    /// fully covers `resource` (`HwResource::covers`).
    ///
    /// This is the security spine of `hw_emit_node`: a
    /// user-space bus driver may publish a child node requesting `resource`
    /// only when it already holds a grant covering it, so an autoloaded
    /// child can never be minted authority its emitter lacks (no ambient authority). A `task` with no grants covers nothing,
    /// so an ungranted task fails closed. The `task` argument is the
    /// kernel-trusted caller id, never a caller-supplied value.
    #[must_use]
    pub fn grant_covers(&self, task: TaskId, resource: &HwResource) -> bool {
        self.grants
            .get(&task)
            .is_some_and(|entry| entry.by_handle.values().any(|grant| grant.covers(resource)))
    }

    /// Establish `task`'s standard-stream descriptor table.
    ///
    /// Called by the spawner when it admits a process, recording which
    /// inherited streams the child may read or write. Replacing an
    /// existing table is permitted: re-establishing the streams of a live
    /// task is the spawner's prerogative, and unlike the address space
    /// there is no live mapping to protect. A task whose table is never
    /// set resolves to [`DescriptorTable::closed`] via [`Self::streams`].
    pub fn set_streams(&mut self, task: TaskId, table: DescriptorTable) {
        self.streams.insert(task, table);
    }

    /// Resolve `task`'s standard-stream descriptor table, or the
    /// fail-closed [`DescriptorTable::closed`] default when none is
    /// established.
    ///
    /// The `stream_read` / `stream_write` handlers consult this to turn a
    /// caller's `fd` into the direction its backing supports. An unregistered task (a kernel task, or one withdrawn on
    /// `exit`) has every descriptor closed, so it can reach no backing.
    #[must_use]
    pub fn streams(&self, task: TaskId) -> DescriptorTable {
        self.streams.get(&task).copied().unwrap_or_default()
    }

    /// Establish `task`'s current working directory, a normalised absolute
    /// path.
    ///
    /// Called by the spawner to inherit the parent's directory into a child,
    /// and by the `fs_chdir` handler once a target directory has been
    /// resolved and authorised. Replacing an existing value is permitted, as
    /// for [`Self::set_streams`]. A task whose directory is never established
    /// resolves to the root `/` via [`Self::cwd`].
    pub fn set_cwd(&mut self, task: TaskId, cwd: String) {
        self.cwds.insert(task, cwd);
    }

    /// Resolve `task`'s current working directory, or the root `/` when none
    /// is established.
    ///
    /// The `copy_path_in` resolver consults this to turn a caller's relative
    /// path into an absolute one, and the `fs_getcwd` handler returns it. An
    /// unregistered task (a kernel task, or one withdrawn on `exit`) resolves
    /// to the root — a safe default that grants no authority of its own,
    /// since every subsequent resolution is still authorised against the
    /// caller's real credentials.
    #[must_use]
    pub fn cwd(&self, task: TaskId) -> String {
        self.cwds
            .get(&task)
            .cloned()
            .unwrap_or_else(|| String::from("/"))
    }

    /// Establish `task`'s full effective resource-limit set.
    ///
    /// Called by the spawner when it admits a process, recording the limits
    /// the child inherited (already intersected against the system default,
    /// [`LimitSet::inherit`]). Replacing an existing set is permitted, as
    /// for [`Self::set_streams`]; a task whose set is never established
    /// resolves to the per-boot [`Self::default_limits`] policy via
    /// [`Self::limits`].
    pub fn set_limits(&mut self, task: TaskId, limits: LimitSet) {
        self.limits.insert(task, limits);
    }

    /// Update `task`'s effective limit for a single [`LimitKind`],
    /// leaving the other kinds untouched.
    ///
    /// The `rlimit_set` handler calls this once a request has been
    /// authorised ([`crate::authorize_set`]). A task with no established
    /// set starts from the per-boot [`Self::default_limits`] policy, so
    /// the first imposed bound on any kind leaves every other kind at
    /// the default policy.
    pub fn set_limit(&mut self, task: TaskId, kind: LimitKind, limit: ResourceLimit) {
        let mut set = self
            .limits
            .get(&task)
            .copied()
            .unwrap_or(self.default_limits);
        set.set(kind, limit);
        self.limits.insert(task, set);
    }

    /// Resolve `task`'s effective resource-limit set, or the per-boot
    /// default policy ([`Self::default_limits`]) when none is
    /// established.
    ///
    /// The `rlimit_get` / `rlimit_set` handlers consult this to read a
    /// caller's own effective limit. An unregistered
    /// task (a kernel task, or one withdrawn on `exit`) reads the default
    /// policy — reading one's own limit grants no authority.
    #[must_use]
    pub fn limits(&self, task: TaskId) -> LimitSet {
        self.limits
            .get(&task)
            .copied()
            .unwrap_or(self.default_limits)
    }

    /// The per-boot default limit policy (the fallback [`Self::limits`]
    /// resolves to and the default `LimitSet::inherit` intersects
    /// against).
    #[must_use]
    pub const fn default_limits(&self) -> LimitSet {
        self.default_limits
    }

    /// Install the per-boot default limit policy.
    ///
    /// Called once by the boot path with the hardware-derived set (the
    /// discovered-RAM pinned-memory bound); every later `limits` fallback
    /// and spawn inheritance then runs under it. Replacing the default
    /// never widens an already-established task set — those were
    /// intersected at spawn and stand on their own.
    pub fn set_default_limits(&mut self, default: LimitSet) {
        self.default_limits = default;
    }

    /// Mark `task`'s entire anonymous memory — current and future — as
    /// pinned (`mem_pin`). Idempotent: pinning a pinned task leaves it
    /// pinned.
    ///
    /// The handler has already enforced the caller's
    /// `PinnedMemoryBytes` bound; this is the unconditional store. The
    /// `task` argument is the kernel-trusted caller id.
    pub fn set_pinned(&mut self, task: TaskId) {
        self.pinned.insert(task);
    }

    /// Clear `task`'s pin mark (`mem_unpin`). Idempotent: unpinning an
    /// unpinned task is a no-op.
    pub fn clear_pinned(&mut self, task: TaskId) {
        self.pinned.remove(&task);
    }

    /// Whether `task`'s anonymous memory is pinned.
    ///
    /// The single pin decision every consumer reads: the compressed
    /// tier's candidate path (a pinned owner's page carries the refusing
    /// `pinned` attribute), the `mem_map`/stack-growth bounds while
    /// pinned, and the observability export.
    #[must_use]
    pub fn is_pinned(&self, task: TaskId) -> bool {
        self.pinned.contains(&task)
    }

    /// `task`'s pinned footprint in bytes: its mapped address space plus
    /// its committed stack.
    ///
    /// The one measure the `PinnedMemoryBytes` bound is enforced
    /// against — the same accounting the `AddressSpaceBytes` ceiling
    /// uses ([`Self::mapped_aspace_bytes`]) plus the demand-grown stack,
    /// so the bounds can never drift apart. File-backed pages are
    /// counted although the compressed tier never takes them: counting
    /// them only tightens the cap, and splitting the accounting would
    /// mean a second running total to keep honest. Saturating: a
    /// miscount can overstate, never understate, usage.
    #[must_use]
    pub fn pinned_footprint_bytes(&self, task: TaskId) -> u64 {
        self.mapped_aspace_bytes(task)
            .saturating_add(self.stack_committed_bytes(task))
    }

    /// The system-wide pinned aggregate: the summed
    /// [`Self::pinned_footprint_bytes`] of every pinned task.
    ///
    /// Read by the observability export (`RAMZIP_STATS.pinned_bytes`,
    /// `stats:mem/pinned`) so an operator can see how much memory
    /// pressure management may never reclaim. Walks only the pinned set
    /// (a handful of monitor-scale processes), not every task.
    #[must_use]
    pub fn pinned_total_bytes(&self) -> u64 {
        self.pinned.iter().fold(0u64, |sum, task| {
            sum.saturating_add(self.pinned_footprint_bytes(*task))
        })
    }

    /// `task`'s running total of mapped address space — anonymous memory
    /// plus demand-paged file regions — in bytes, or `0` when it has mapped
    /// none.
    ///
    /// The `mem_map` and `file_map` handlers read this to check a request
    /// against the `LimitKind::AddressSpaceBytes` ceiling before mapping.
    /// The `task` argument is the kernel-trusted caller id.
    #[must_use]
    pub fn mapped_aspace_bytes(&self, task: TaskId) -> u64 {
        self.mapped_aspace_bytes.get(&task).copied().unwrap_or(0)
    }

    /// Accrue `bytes` against `task`'s mapped-address-space total.
    ///
    /// Called by the `mem_map`/`file_map` handlers *after* a map succeeds and only once
    /// the request has been admitted against the task's
    /// `LimitKind::AddressSpaceBytes` ceiling, so the saturating add never
    /// loses accounting in practice; it saturates rather than wraps purely
    /// so a future miscount can never silently understate usage (fail
    /// closed, never a panic). The `task` argument is the kernel-trusted
    /// caller id.
    pub fn charge_aspace_bytes(&mut self, task: TaskId, bytes: u64) {
        let entry = self.mapped_aspace_bytes.entry(task).or_insert(0);
        *entry = entry.saturating_add(bytes);
    }

    /// Release `bytes` from `task`'s mapped-address-space total.
    ///
    /// Called by the `mem_unmap`/`file_unmap` handlers *after* an unmap succeeds, so
    /// `bytes` corresponds to pages that were actually backed and charged.
    /// The subtraction saturates at zero (it can never underflow into a
    /// bogus huge total that would wrongly deny later maps) and drops the
    /// entry once it reaches zero so a task that frees everything holds no
    /// residual accounting. The `task` argument is the kernel-trusted
    /// caller id.
    pub fn credit_aspace_bytes(&mut self, task: TaskId, bytes: u64) {
        if let Some(entry) = self.mapped_aspace_bytes.get_mut(&task) {
            *entry = entry.saturating_sub(bytes);
            if *entry == 0 {
                self.mapped_aspace_bytes.remove(&task);
            }
        }
    }

    /// Record `task`'s reserved user-stack span.
    ///
    /// Called by the spawner when it admits a process, recording the span
    /// the spawn layout placed (already validated by [`StackSpan::new`]).
    /// Replacing an existing record is permitted, as for
    /// [`Self::set_streams`]; a task whose span is never recorded has no
    /// growable stack and every fault below its committed stack stays
    /// fatal (fail closed). The `task` argument is the kernel-trusted id
    /// the admission path minted, never a caller-supplied value.
    pub fn set_stack_span(&mut self, task: TaskId, span: StackSpan) {
        self.stack_spans.insert(task, span);
    }

    /// Resolve `task`'s recorded stack span, or `None` when none was
    /// recorded (fail closed: no span, no growth).
    ///
    /// The stack-growth fault path reads this to decide whether a fault is
    /// growth room. The `task` argument is the kernel-trusted id of the
    /// faulting CPU's current task.
    #[must_use]
    pub fn stack_span(&self, task: TaskId) -> Option<StackSpan> {
        self.stack_spans.get(&task).copied()
    }

    /// Lower `task`'s committed stack base to `page_va` after the growth
    /// path backed that page.
    ///
    /// Called *after* the producer mapped the page, so the record only ever
    /// names frames the task actually holds. Monotonic: a `page_va` at or
    /// above the current committed base (the benign already-resident race,
    /// or a hole above the low-water mark) leaves the record unchanged, and
    /// one below the reserve base is refused — the record can never claim
    /// pages outside the span (fail closed).
    pub fn commit_stack_page(&mut self, task: TaskId, page_va: u64) {
        if let Some(span) = self.stack_spans.get_mut(&task) {
            if page_va >= span.reserve_base && page_va < span.committed_base {
                span.committed_base = page_va;
            }
        }
    }

    /// Bytes of `task`'s stack currently committed, or `0` when no span is
    /// recorded.
    ///
    /// The live usage the `LimitKind::StackBytes` report surfaces beside
    /// the effective bound, mirroring [`Self::mapped_aspace_bytes`].
    #[must_use]
    pub fn stack_committed_bytes(&self, task: TaskId) -> u64 {
        self.stack_spans
            .get(&task)
            .map_or(0, StackSpan::committed_bytes)
    }

    /// Record `task`'s PIE load base — the lowest user virtual address its
    /// relocated program image occupies.
    ///
    /// Called by the spawner when it admits a process, with the base the
    /// image builder derived (the lowest relocated segment vaddr). The
    /// `task` argument is the kernel-trusted id the admission path minted,
    /// never a caller-supplied value. Replacing an existing record is
    /// permitted, mirroring [`Self::set_stack_span`]; a task whose base is
    /// never recorded simply has crash offsets expressed absolute rather
    /// than load-relative (a diagnostics-quality degradation only, never a
    /// correctness or security one — an absent base leaks nothing).
    pub fn set_load_base(&mut self, task: TaskId, load_base: u64) {
        self.load_bases.insert(task, load_base);
    }

    /// Resolve `task`'s recorded PIE load base, or `None` when none was
    /// recorded.
    ///
    /// The user-fault crash path reads this to express a faulting `pc` and
    /// every backtrace frame as a program-relative offset. The `task`
    /// argument is the kernel-trusted id of the faulting CPU's current
    /// task.
    #[must_use]
    pub fn load_base(&self, task: TaskId) -> Option<u64> {
        self.load_bases.get(&task).copied()
    }

    /// Record `task`'s live demand-paged file mapping `region`, keyed by its
    /// base address.
    ///
    /// Called by the `file_map` handler *after* the producer has reserved
    /// the region, so every record names address space the task actually
    /// holds. The record carries the mapping-time identity (uid + effective
    /// capability snapshot) the fault path pages under — the same authority
    /// model as an open descriptor, resolved once at map time. The `task`
    /// argument is the kernel-trusted caller id.
    pub fn record_file_region(&mut self, task: TaskId, region: FileRegion) {
        self.file_regions
            .entry(task)
            .or_default()
            .insert(region.base, region);
    }

    /// Resolve `task`'s file-mapping record whose `(base, len)` matches
    /// exactly, without removing it.
    ///
    /// The `file_unmap` handler validates the caller-named pair against
    /// this before any teardown, so a mismatched or unknown pair fails
    /// closed touching nothing.
    #[must_use]
    pub fn file_region_exact(&self, task: TaskId, base: u64, len: u64) -> Option<FileRegion> {
        let region = self.file_regions.get(&task)?.get(&base)?;
        (region.len == len).then(|| region.clone())
    }

    /// Remove `task`'s file-mapping record based at `base`, returning it.
    ///
    /// Called by the `file_unmap` handler *after* the producer released the
    /// region, so record and reservation leave together.
    pub fn remove_file_region(&mut self, task: TaskId, base: u64) -> Option<FileRegion> {
        let regions = self.file_regions.get_mut(&task)?;
        let removed = regions.remove(&base);
        if regions.is_empty() {
            self.file_regions.remove(&task);
        }
        removed
    }

    /// Resolve the file-mapping record of `task`'s that covers the virtual
    /// address `va`, if any.
    ///
    /// The user-fault resolver calls this to decide whether a faulting
    /// address is demand-paged file backing (resolve and resume) or a
    /// genuine wild access (terminate, fail closed). Returns a clone so no
    /// registry lock is held across the filesystem read that follows.
    #[must_use]
    pub fn file_region_covering(&self, task: TaskId, va: u64) -> Option<FileRegion> {
        let regions = self.file_regions.get(&task)?;
        let (_, region) = regions.range(..=va).next_back()?;
        (va < region.base + region.len).then(|| region.clone())
    }

    /// Record `task`'s live demand-paged anonymous mapping: `page_count`
    /// pages reserved at `base`, keyed by base.
    ///
    /// Called by the `mem_map` handler *after* the producer has reserved
    /// the address-space range, so every record names address space the
    /// task actually holds. The record lets the anonymous fault path tell
    /// a legitimate first-touch of reserved memory apart from a wild access
    /// (fail closed on a miss). The `task` argument is the kernel-trusted
    /// caller id.
    pub fn record_anon_region(&mut self, task: TaskId, base: u64, page_count: u64) {
        self.anon_regions
            .entry(task)
            .or_default()
            .insert(base, page_count);
    }

    /// Resolve `task`'s anonymous-mapping record whose `(base, page_count)`
    /// matches exactly, without removing it.
    ///
    /// The `mem_unmap` handler validates the caller-named pair against this
    /// before any teardown, so a mismatched or unknown pair fails closed
    /// touching nothing.
    #[must_use]
    pub fn anon_region_exact(&self, task: TaskId, base: u64, page_count: u64) -> bool {
        self.anon_regions
            .get(&task)
            .and_then(|regions| regions.get(&base))
            .is_some_and(|&pages| pages == page_count)
    }

    /// Remove `task`'s anonymous-mapping record based at `base`, returning
    /// its page count.
    ///
    /// Called by the `mem_unmap` handler *after* the producer released the
    /// region, so record and reservation leave together.
    pub fn remove_anon_region(&mut self, task: TaskId, base: u64) -> Option<u64> {
        let regions = self.anon_regions.get_mut(&task)?;
        let removed = regions.remove(&base);
        if regions.is_empty() {
            self.anon_regions.remove(&task);
        }
        removed
    }

    /// Resolve whether the virtual address `va` lies inside one of `task`'s
    /// reserved anonymous regions.
    ///
    /// The anonymous user-fault resolver calls this to decide whether a
    /// faulting address is demand-paged anonymous backing (back one zeroed
    /// page and resume) or a genuine wild access (terminate, fail closed).
    #[must_use]
    pub fn anon_region_covering(&self, task: TaskId, va: u64) -> bool {
        let Some(regions) = self.anon_regions.get(&task) else {
            return false;
        };
        let Some((&base, &pages)) = regions.range(..=va).next_back() else {
            return false;
        };
        // `pages * PAGE_SIZE` cannot overflow: the reservation was validated
        // to fit the address space at `mem_map` time.
        va < base + pages * PAGE_SIZE as u64
    }

    /// Describe where the fatal fault at `va` landed relative to `task`'s
    /// own address space, as the coarse, non-leaking [`FaultLocality`] the
    /// fault-kill record carries.
    ///
    /// This is the sole place the diagnostics leak-policy is enforced for
    /// the audit record: the returned value carries at most a *distance*
    /// from a fixed anchor (virtual address 0, the stack guard, a region
    /// end), never an absolute virtual address, so the shared audit log
    /// never publishes address-space layout. Precedence is most-specific
    /// first — a null-page dereference, then a below-guard stack overflow,
    /// then a bounded run past an owned region's end, and finally a
    /// genuinely wild access. Runs on the dying-task fault path (never a
    /// hot path) and allocates nothing.
    #[must_use]
    pub fn classify_fault_locality(&self, task: TaskId, va: u64) -> FaultLocality {
        // A dereference through (or near) a null pointer: the offset from
        // virtual address 0 reveals nothing about layout.
        if va < PAGE_SIZE as u64 {
            return FaultLocality::NullPage { offset: va };
        }
        // Just below the reserved stack span's guard page: a stack
        // overflow that ran past the guard. The distance below the reserve
        // base is a relative measure, not the base itself.
        if let Some(span) = self.stack_spans.get(&task) {
            let reserve_base = span.reserve_base();
            if va < reserve_base {
                let distance = reserve_base - va;
                if distance <= NEAR_REGION_WINDOW {
                    return FaultLocality::BelowStackGuard { distance };
                }
            }
        }
        // A small bounded distance past the end of a mapping the task
        // owns; the region it is relative to is never identified.
        if let Some(end) = self.nearest_region_end_at_or_below(task, va) {
            let offset = va - end;
            if offset <= NEAR_REGION_WINDOW {
                return FaultLocality::PastRegion { offset };
            }
        }
        // Inside a region the task legitimately owns — a reserved anonymous
        // mapping, a file mapping, or its stack span — that could not be
        // resolved. This is the deterministic out-of-memory case (a
        // demand-paged page that could not be backed), not a wild pointer:
        // report the honest "in a region you own" locality, never "wild".
        let in_stack_span = self
            .stack_spans
            .get(&task)
            .is_some_and(|span| va >= span.reserve_base() && va < span.top());
        if in_stack_span
            || self.anon_region_covering(task, va)
            || self.file_region_covering(task, va).is_some()
        {
            return FaultLocality::InRegion;
        }
        FaultLocality::Wild
    }

    /// The greatest region end (`base + len`) at or below `va` across
    /// every mapping `task` owns — file mappings, anonymous mappings, and
    /// the committed stack — or `None` when the task owns nothing ending at
    /// or below `va`.
    ///
    /// Iterates the task's own regions only (bounded by its address-space
    /// limits) and runs on the dying-task fault path, never a hot path. A
    /// region that *covers* `va` (its end is strictly above `va`) is
    /// excluded — that is a miss inside a live mapping, described by the
    /// `fault_class`, not a run past a region end.
    fn nearest_region_end_at_or_below(&self, task: TaskId, va: u64) -> Option<u64> {
        let mut best: Option<u64> = None;
        let mut consider = |end: u64| {
            if end <= va {
                best = Some(best.map_or(end, |b: u64| b.max(end)));
            }
        };
        if let Some(regions) = self.file_regions.get(&task) {
            for region in regions.values() {
                consider(region.base.saturating_add(region.len));
            }
        }
        if let Some(regions) = self.anon_regions.get(&task) {
            for (&base, &pages) in regions {
                consider(base.saturating_add(pages.saturating_mul(PAGE_SIZE as u64)));
            }
        }
        if let Some(span) = self.stack_spans.get(&task) {
            consider(span.top());
        }
        best
    }

    /// Open a file/directory descriptor for `task`, recording the resolved
    /// absolute `path` and the `flags` it was opened with, and return the
    /// freshly allocated descriptor number (at or above [`STD_STREAM_COUNT`]).
    ///
    /// Called by the `fs_open` handler *after* it has resolved and authorised
    /// `path` through the secured VFS under the caller's real credentials, so
    /// this records an already-checked handle; it grants no authority of its
    /// own. The number is the lowest free descriptor, so a process that opens
    /// and closes many files reuses numbers rather than exhausting the space.
    /// The `task` argument is the kernel-trusted caller id, never a
    /// caller-supplied value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] only when `task` already holds every descriptor
    /// number up to [`u32::MAX`] (genuine exhaustion, fail closed).
    pub fn open_file(
        &mut self,
        task: TaskId,
        path: String,
        flags: OpenFlags,
    ) -> Result<u32, Errno> {
        self.open_backed(task, OpenBacking::Path(path), flags)
    }

    /// Open a descriptor for `task` backed by the resolved resource
    /// `backing`, recording the `flags` it was opened with, and return the
    /// freshly allocated descriptor number (at or above [`STD_STREAM_COUNT`]).
    ///
    /// Called by the `resource_open` handler *after* it has parsed and
    /// resolved the reference and confirmed the caller's authority, so this
    /// records an already-checked handle; it grants no authority of its own.
    /// It shares the one `OpenFileTable` allocator with [`Self::open_file`]
    /// so a resource descriptor's number cannot collide with a file's. The
    /// `task` argument is the kernel-trusted caller id.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] only when `task` already holds every descriptor
    /// number up to [`u32::MAX`] (genuine exhaustion, fail closed).
    pub fn open_resource(
        &mut self,
        task: TaskId,
        backing: ResourceBacking,
        flags: OpenFlags,
    ) -> Result<u32, Errno> {
        self.open_backed(task, OpenBacking::Resource(backing), flags)
    }

    /// Allocate the lowest free descriptor for `task` and record `backing`
    /// with `flags`.
    ///
    /// The single insertion point shared by [`Self::open_file`] and
    /// [`Self::open_resource`], so every descriptor — whatever backs it —
    /// comes from one allocator and one number space (: one definition).
    fn open_backed(
        &mut self,
        task: TaskId,
        backing: OpenBacking,
        flags: OpenFlags,
    ) -> Result<u32, Errno> {
        let table = self.open_files.entry(task).or_default();
        let fd = table.alloc_fd()?;
        table.by_fd.insert(fd, OpenFile::new(backing, flags));
        Ok(fd)
    }

    /// Mint a one-shot file delegation **to** `recipient`, returning the
    /// unforgeable handle the grantor forwards in-band
    /// (`fd_grant`, `plans/CAPABILITY_USE.md` CU6).
    ///
    /// Called by the `fd_grant` handler *after* it has resolved the
    /// grantor's own descriptor and captured the grantor's identity into
    /// `file`, so this records an already-checked delegation; it grants no
    /// authority of its own. The handle follows the [`Self::mint_grant`]
    /// discipline — idempotent (re-granting a delegation still pending
    /// returns the pending handle rather than appending a duplicate) and
    /// meaningful only when presented by `recipient` itself
    /// ([`Self::redeem_fd_delegation`] is keyed by the kernel-trusted
    /// caller id, so another task presenting the same numeric value
    /// resolves to nothing).
    pub fn mint_fd_delegation(
        &mut self,
        recipient: TaskId,
        file: DelegatedFile,
        flags: OpenFlags,
    ) -> u64 {
        let entry = self.fd_delegations.entry(recipient).or_default();
        let pending = (file, flags);
        // A delegation still pending conveys exactly one right: "open this
        // path under this captured authority". Re-granting it while the
        // first is unredeemed adds nothing — descriptors here carry no
        // position (every read names its own offset), so a second identical
        // descriptor would be indistinguishable from the first — and letting
        // it append would let a grantor grow the recipient's kernel-side
        // table without limit by repeating one call. Hand back the pending
        // handle instead; once redeemed the entry is consumed, so a later
        // grant of the same file legitimately mints afresh.
        if let Some(handle) = existing_handle(&entry.by_handle, &pending) {
            return handle;
        }
        // Handle 0 is the reserved invalid value; the first minted handle
        // is 1. `next_handle` only ever increases within a task's life.
        entry.next_handle += 1;
        let handle = entry.next_handle;
        entry.by_handle.insert(handle, pending);
        handle
    }

    /// Redeem the one-shot file delegation `handle` minted to `task`,
    /// installing it into `task`'s open table and returning the fresh
    /// descriptor number (`fd_redeem`).
    ///
    /// One-shot with fail-closed atomicity: the delegation is consumed
    /// only when the descriptor allocation succeeds, so a refused
    /// redemption (descriptor-space exhaustion) leaves the grant intact
    /// for a retry after the holder closes descriptors, and a redeemed
    /// handle can never be redeemed twice. The `task` argument is the
    /// kernel-trusted caller id, never a caller-supplied value.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no such handle minted to `task` (unknown,
    ///   already redeemed, or minted to a different task — forgery answers
    ///   exactly like absence, so the handle space leaks nothing).
    /// * [`Errno::OutOfRange`] — descriptor-space exhaustion; the grant
    ///   stays pending.
    pub fn redeem_fd_delegation(&mut self, task: TaskId, handle: u64) -> Result<u32, Errno> {
        let (file, flags) = self
            .fd_delegations
            .get(&task)
            .and_then(|entry| entry.by_handle.get(&handle))
            .cloned()
            .ok_or(Errno::NotFound)?;
        let fd = self.open_backed(task, OpenBacking::Delegated(file), flags)?;
        // The allocation succeeded; consume the grant (one-shot). The
        // entry provably exists — it was read above under the same
        // exclusive borrow — so the removes cannot miss.
        if let Some(entry) = self.fd_delegations.get_mut(&task) {
            entry.by_handle.remove(&handle);
        }
        Ok(fd)
    }

    /// Create a pipe for `task`, allocating a read-end and a write-end
    /// descriptor in its open table, and return `(read_fd, write_fd)`
    /// (`plans/SPAWN.md` SP10).
    ///
    /// Both descriptors draw from the same allocator as
    /// [`Self::open_file`] / [`Self::open_resource`]. All-or-nothing: if
    /// the second descriptor cannot be allocated the first is released
    /// (its dropped end closes the side, so the pipe never leaks a
    /// half-open pair). The `task` argument is the kernel-trusted caller
    /// id.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] only on genuine descriptor-space exhaustion
    /// (fail closed).
    pub fn open_pipe(&mut self, task: TaskId) -> Result<(u32, u32), Errno> {
        let (read_end, write_end) = crate::pipe::Pipe::create();
        let read_fd = self.open_backed(task, OpenBacking::Pipe(read_end), OpenFlags::READ)?;
        match self.open_backed(task, OpenBacking::Pipe(write_end), OpenFlags::WRITE) {
            Ok(write_fd) => Ok((read_fd, write_fd)),
            Err(err) => {
                // Unwind the half-built pair: dropping the read entry
                // closes its end through the handle's own release path.
                self.close_file(task, read_fd);
                Err(err)
            }
        }
    }

    /// Create a pseudo-terminal of geometry `size` for `task`, allocating a
    /// master-end and a slave-end descriptor in its open table, and return
    /// `(master_fd, slave_fd)` (`plans/PTY.md`).
    ///
    /// Both descriptors are opened `READ | WRITE`: the master both writes
    /// keystrokes and reads the slave's output, and the slave both reads
    /// input and writes program output (so the one slave descriptor can be
    /// wired behind a child's fd 0, 1, and 2). Both draw from the same
    /// allocator [`Self::open_pipe`] uses. All-or-nothing: if the second
    /// descriptor cannot be allocated the first is released (its dropped
    /// end closes the side, so the pty never leaks a half-open pair). The
    /// `task` argument is the kernel-trusted caller id.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] only on genuine descriptor-space exhaustion
    /// (fail closed).
    pub fn open_pty(
        &mut self,
        task: TaskId,
        size: tairix_abi::TerminalSize,
    ) -> Result<(u32, u32), Errno> {
        let (master, slave) = crate::pty::Pty::create(size);
        let rw = OpenFlags::READ.union(OpenFlags::WRITE);
        let master_fd = self.open_backed(task, OpenBacking::PtyMaster(master), rw)?;
        match self.open_backed(task, OpenBacking::PtySlave(slave), rw) {
            Ok(slave_fd) => Ok((master_fd, slave_fd)),
            Err(err) => {
                // Unwind the half-built pair: dropping the master entry
                // closes its end through the handle's own release path.
                self.close_file(task, master_fd);
                Err(err)
            }
        }
    }

    /// Install `file` as `task`'s **standard-stream** open entry at `fd`
    /// (one of fd 0–3) — the spawn wiring path placing a cloned parent
    /// descriptor behind a child's standard stream (`plans/SPAWN.md`
    /// SP10). Anything already at `fd` is replaced (and, for a pipe end,
    /// released through its handle's drop).
    ///
    /// A non-standard `fd` is refused: ordinary descriptors are allocated,
    /// never installed, so the one allocator keeps owning the ≥ 4 space.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an `fd` at or above [`STD_STREAM_COUNT`].
    pub fn install_std_entry(
        &mut self,
        task: TaskId,
        fd: u32,
        file: OpenFile,
    ) -> Result<(), Errno> {
        if fd as usize >= STD_STREAM_COUNT {
            return Err(Errno::OutOfRange);
        }
        self.open_files
            .entry(task)
            .or_default()
            .by_fd
            .insert(fd, file);
        Ok(())
    }

    /// Resolve `task`'s open descriptor `fd` to its recorded path and flags,
    /// or `None` if `fd` is not one of `task`'s open descriptors (fail
    /// closed).
    ///
    /// Returns a clone so the caller (a handle op such as `fs_read`) holds no
    /// borrow of the registry across the filesystem operation it then routes
    /// to; the clone shares the entry's open-file description (cursor, pipe
    /// end). `None` covers an unopened descriptor, a standard-stream number
    /// with no wired entry (only spawn wiring records fd 0–3 here), and a
    /// descriptor opened by a *different* task —
    /// the `task` argument is the kernel-trusted caller id, so one process
    /// cannot reach another's open file by guessing a number.
    #[must_use]
    pub fn open_file_entry(&self, task: TaskId, fd: u32) -> Option<OpenFile> {
        self.open_files.get(&task)?.by_fd.get(&fd).cloned()
    }

    /// Whether `task`'s open descriptor `fd` is a readable stream end — a
    /// pipe read end, a pty master, or a pty slave, each opened for reading
    /// — the wait-set `Stream` member's add-time owner/descriptor check
    /// (`plans/APPWIN.md` AW4, `plans/PTY.md`). `false` covers an unopened
    /// number, a descriptor of a different task, a path- or resource-backed
    /// descriptor, a pipe write end, and an entry opened without read access
    /// (fail closed — the caller cannot distinguish which). The `task`
    /// argument is the kernel-trusted caller id. Borrows the entry in
    /// place — never a clone, so the peek can never touch a stream's
    /// live-end counts.
    #[must_use]
    pub fn stream_read_member(&self, task: TaskId, fd: u32) -> bool {
        self.borrow_read_stream_end(task, fd).is_some()
    }

    /// Non-consuming readiness peek on `task`'s open descriptor `fd` for
    /// the wait-set `Stream` scan: `true` when the descriptor is a readable
    /// stream end whose read would complete without parking (buffered
    /// bytes, or end-of-stream). Anything [`Self::stream_read_member`]
    /// refuses is simply not ready — a member whose descriptor was closed
    /// or replaced mid-wait stops reporting rather than erring. Borrows in
    /// place, so the per-scan peek never clones a stream end (a clone/drop
    /// pair would spuriously wake every stream waiter).
    #[must_use]
    pub fn stream_readable(&self, task: TaskId, fd: u32) -> bool {
        match self.borrow_read_stream_end(task, fd) {
            Some(ReadStreamEnd::Pipe(end)) => end.readable(),
            Some(ReadStreamEnd::PtyMaster(end)) => end.readable(),
            Some(ReadStreamEnd::PtySlave(end)) => end.readable(),
            None => false,
        }
    }

    /// Resolve `task`'s `fd` to its readable stream end **borrowed in
    /// place**, only when the entry is opened for reading and backed by a
    /// pipe read end, a pty master, or a pty slave — the one resolution
    /// [`Self::stream_read_member`] and [`Self::stream_readable`] share.
    fn borrow_read_stream_end(&self, task: TaskId, fd: u32) -> Option<ReadStreamEnd<'_>> {
        let entry = self.open_files.get(&task)?.by_fd.get(&fd)?;
        if !entry.flags.contains(OpenFlags::READ) {
            return None;
        }
        match &entry.backing {
            OpenBacking::Pipe(end) if end.role() == crate::pipe::PipeRole::Read => {
                Some(ReadStreamEnd::Pipe(end))
            }
            OpenBacking::PtyMaster(end) => Some(ReadStreamEnd::PtyMaster(end)),
            OpenBacking::PtySlave(end) => Some(ReadStreamEnd::PtySlave(end)),
            _ => None,
        }
    }

    /// Resolve `task`'s open descriptor `fd` to the pseudo-terminal it is a
    /// **slave** end of, **borrowed in place**, or `None` when `fd` is not a
    /// pty-slave descriptor of `task` (`plans/PTY.md`).
    ///
    /// The one resolution the pty-aware `stream_input_mode` / `terminal_size`
    /// / `console_foreground` handlers share: a pty slave is a *tty* for
    /// those terminal-control calls, and its discipline lives on the [`Pty`]
    /// (not in the static console list). Borrows the entry in place — never
    /// a clone — so the lookup never touches the pty's live-end counts. The
    /// `task` argument is the kernel-trusted caller id, so one process
    /// cannot reach another's pty by guessing a number.
    ///
    /// [`Pty`]: crate::pty::Pty
    #[must_use]
    pub fn pty_slave(&self, task: TaskId, fd: u32) -> Option<&crate::pty::Pty> {
        let entry = self.open_files.get(&task)?.by_fd.get(&fd)?;
        entry.pty_slave().map(PtySlaveEnd::pty)
    }

    /// Resolve `task`'s open descriptor `fd` to the pseudo-terminal it is a
    /// **master** end of, **borrowed in place**, or `None` when `fd` is not a
    /// pty-master descriptor of `task` (`plans/PTY.md`).
    ///
    /// The resolution `pty_set_size` uses: the graphical terminal holds the
    /// master end, so setting the pty's character-cell geometry on a window
    /// resize is a master-side operation. Borrows in place — never a clone —
    /// so the lookup never touches the pty's live-end counts, and `task` is
    /// the kernel-trusted caller id, so one process cannot reach another's
    /// pty by guessing a number.
    ///
    /// [`Pty`]: crate::pty::Pty
    #[must_use]
    pub fn pty_master(&self, task: TaskId, fd: u32) -> Option<&crate::pty::Pty> {
        let entry = self.open_files.get(&task)?.by_fd.get(&fd)?;
        entry.pty_master().map(PtyMasterEnd::pty)
    }

    /// Release `task`'s open descriptor `fd`, returning `true` if it was
    /// open.
    ///
    /// Idempotent and fail-closed: closing a descriptor `task` does not hold
    /// (an unopened number, a standard stream, or another task's descriptor)
    /// is a no-op returning `false`, never an error or a panic. The `task`
    /// argument is the kernel-trusted caller id.
    pub fn close_file(&mut self, task: TaskId, fd: u32) -> bool {
        self.open_files
            .get_mut(&task)
            .is_some_and(|table| table.by_fd.remove(&fd).is_some())
    }

    /// Resolve `task` to the `(address space, physical map)` pair the
    /// [`tairix_kernel_mem::uaccess`] copy path consumes, or `None` if
    /// no entry is registered.
    #[must_use]
    pub fn resolve(&self, task: TaskId) -> Option<(&dyn UserAddressSpace, &dyn PhysMap)> {
        self.tasks.get(&task).map(|entry| {
            // Drop the `Send + Sync` auto-trait bounds the stored boxes
            // carry: the copy path only needs the bare read-only views,
            // and the registry's own `RwLock` already governs sharing.
            let space: &dyn UserAddressSpace = &*entry.space;
            let physmap: &dyn PhysMap = &*entry.physmap;
            (space, physmap)
        })
    }

    /// Whether an address space is registered for `task`.
    #[must_use]
    pub fn contains(&self, task: TaskId) -> bool {
        self.tasks.contains_key(&task)
    }

    /// Number of tasks with a registered address space.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the registry holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_kernel_mem::{
        AddressSpace, Frame, HostPageTable, MapFlags, Page, PhysAddr, SimPhysMap, VirtAddr,
        PAGE_SIZE,
    };

    fn page(n: u64) -> Page {
        Page::from_addr(VirtAddr::new(n * PAGE_SIZE as u64)).expect("aligned page")
    }

    /// Build a user address space with one mapped, user-readable page at
    /// page `n` → frame `frame`, boxed behind the object-safe trait.
    fn user_space(n: u64, frame: usize) -> Box<dyn UserAddressSpace + Send + Sync> {
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(page(n), Frame(frame), MapFlags::READ | MapFlags::USER)
            .expect("mapped");
        Box::new(space)
    }

    fn sim() -> Box<dyn PhysMap + Send + Sync> {
        Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE))
    }

    /// A **frozen** snapshot mapping one user-readable page `n` → `frame`,
    /// boxed behind the object-safe trait — the form the registry really
    /// stores in production (unlike [`user_space`]'s live `AddressSpace`,
    /// which keeps the default no-op delta).
    fn frozen_space(n: u64, frame: usize) -> Box<dyn UserAddressSpace + Send + Sync> {
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(page(n), Frame(frame), MapFlags::READ | MapFlags::USER)
            .expect("mapped");
        Box::new(space.freeze())
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = AddressSpaceRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(!reg.contains(TaskId(1)));
        assert!(reg.resolve(TaskId(1)).is_none());
    }

    #[test]
    fn register_then_resolve_returns_the_pair() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(7), user_space(1, 9), sim())
            .expect("first registration succeeds");
        assert!(reg.contains(TaskId(7)));
        assert_eq!(reg.len(), 1);

        let (space, _physmap) = reg.resolve(TaskId(7)).expect("registered task resolves");
        // The boxed trait object forwards `translate` to the underlying
        // `AddressSpace<HostPageTable>`.
        let (frame, flags) = space.translate(page(1)).expect("page resolves");
        assert_eq!(frame, Frame(9));
        assert!(flags.contains(MapFlags::USER));
    }

    #[test]
    fn duplicate_registration_is_rejected_and_keeps_first_entry() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(3), user_space(1, 100), sim())
            .expect("first registration succeeds");
        let err = reg
            .register(TaskId(3), user_space(2, 200), sim())
            .expect_err("second registration for same task is refused");
        assert_eq!(err, AspaceError::AlreadyPresent);
        // The original entry survives untouched: page 1 → frame 100 is
        // still mapped, page 2 (from the rejected entry) is not.
        let (space, _) = reg.resolve(TaskId(3)).expect("first entry intact");
        assert_eq!(space.translate(page(1)).expect("page 1").0, Frame(100));
        assert!(space.translate(page(2)).is_none());
    }

    #[test]
    fn reregister_space_swaps_the_snapshot_and_keeps_the_physmap() {
        let mut reg = AddressSpaceRegistry::new();
        // Register a snapshot that maps only page 1 (the "spawn-time" view).
        reg.register(TaskId(5), user_space(1, 100), sim())
            .expect("first registration succeeds");

        // The freeze-time snapshot cannot see page 2 yet — exactly the stale
        // state a `login` hit when its heap was mapped after spawn.
        let (space, _) = reg.resolve(TaskId(5)).expect("registered");
        assert!(space.translate(page(2)).is_none());

        // Re-freeze: a fresh snapshot that now also maps page 2 (the grown
        // heap). `reregister_space` reports the task was present.
        assert!(reg.reregister_space(TaskId(5), user_space(2, 200)));

        // The copy path now sees the newly-mapped page through the same task.
        let (space, physmap) = reg.resolve(TaskId(5)).expect("still registered");
        let (frame, flags) = space.translate(page(2)).expect("page 2 now resolves");
        assert_eq!(frame, Frame(200));
        assert!(flags.contains(MapFlags::USER));
        // The physical map survived the swap (its window still translates).
        assert!(physmap.translate(PhysAddr::new(0), PAGE_SIZE).is_some());
    }

    #[test]
    fn note_faulted_page_updates_a_frozen_snapshot_in_place() {
        let mut reg = AddressSpaceRegistry::new();
        // The stored snapshot sees only page 1 (the spawn-time view).
        reg.register(TaskId(6), frozen_space(1, 100), sim())
            .expect("registration succeeds");
        assert!(reg
            .resolve(TaskId(6))
            .expect("registered")
            .0
            .translate(page(2))
            .is_none());

        // A demand fault backs page 2; the resolver applies just that page as
        // a delta (no whole-space re-freeze) and the snapshot absorbs it.
        assert!(reg.note_faulted_page(TaskId(6), page(2), Some((Frame(200), MapFlags::USER))));

        let (space, _) = reg.resolve(TaskId(6)).expect("still registered");
        let (frame, flags) = space.translate(page(2)).expect("delta page resolves");
        assert_eq!(frame, Frame(200));
        assert!(flags.contains(MapFlags::USER));
        // The original page is untouched.
        assert_eq!(space.translate(page(1)).expect("page 1").0, Frame(100));
    }

    #[test]
    fn note_faulted_page_falls_back_when_the_snapshot_cannot_absorb_a_delta() {
        let mut reg = AddressSpaceRegistry::new();
        // A live `AddressSpace` entry keeps the default no-op delta, so the
        // registry reports `false` and the caller full-re-freezes instead.
        reg.register(TaskId(7), user_space(1, 1), sim())
            .expect("registration succeeds");
        assert!(!reg.note_faulted_page(TaskId(7), page(2), Some((Frame(2), MapFlags::USER))));
        // A task with no entry also reports `false` (fail closed), never a
        // silently-created entry.
        assert!(!reg.note_faulted_page(TaskId(99), page(0), None));
        assert!(!reg.contains(TaskId(99)));
    }

    #[test]
    fn reregister_space_of_an_unregistered_task_is_a_no_op() {
        let mut reg = AddressSpaceRegistry::new();
        // A task with no entry is never created by a re-freeze (a kernel task
        // reaches no user copy path); the call fails closed.
        assert!(!reg.reregister_space(TaskId(9), user_space(1, 1)));
        assert!(!reg.contains(TaskId(9)));
        assert!(reg.resolve(TaskId(9)).is_none());
    }

    #[test]
    fn withdraw_removes_only_the_named_task() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(1), user_space(1, 1), sim()).unwrap();
        reg.register(TaskId(2), user_space(1, 2), sim()).unwrap();

        assert!(reg.withdraw(TaskId(1)));
        assert!(!reg.contains(TaskId(1)));
        assert!(reg.resolve(TaskId(1)).is_none());
        assert!(reg.contains(TaskId(2)));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn withdrawing_unknown_task_is_a_noop() {
        let mut reg = AddressSpaceRegistry::new();
        assert!(!reg.withdraw(TaskId(42)));
        reg.register(TaskId(1), user_space(1, 1), sim()).unwrap();
        // Double withdraw: the second call finds nothing.
        assert!(reg.withdraw(TaskId(1)));
        assert!(!reg.withdraw(TaskId(1)));
    }

    #[test]
    fn re_register_after_withdraw_succeeds() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(5), user_space(1, 10), sim()).unwrap();
        assert!(reg.withdraw(TaskId(5)));
        // A new task reusing the same id (after the old one exited) can
        // register again — withdrawal fully clears the slot.
        reg.register(TaskId(5), user_space(3, 30), sim())
            .expect("re-registration after withdraw succeeds");
        let (space, _) = reg.resolve(TaskId(5)).expect("re-registered");
        assert_eq!(space.translate(page(3)).expect("page 3").0, Frame(30));
    }

    #[test]
    fn unset_streams_resolve_to_the_closed_default() {
        let reg = AddressSpaceRegistry::new();
        // A task with no established table can reach no backing.
        assert_eq!(reg.streams(TaskId(9)), DescriptorTable::closed());
    }

    #[test]
    fn set_streams_then_resolve_returns_the_table() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_streams(TaskId(2), DescriptorTable::standard());
        assert_eq!(reg.streams(TaskId(2)), DescriptorTable::standard());
        // A different task is unaffected and stays fail-closed.
        assert_eq!(reg.streams(TaskId(3)), DescriptorTable::closed());
    }

    #[test]
    fn withdraw_clears_the_stream_table() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_streams(TaskId(4), DescriptorTable::standard());
        // Withdrawing a task with streams but no address space still
        // reports the slot was present and clears the table.
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.streams(TaskId(4)), DescriptorTable::closed());
    }

    #[test]
    fn stale_task_entry_is_none_for_a_fresh_id() {
        let reg = AddressSpaceRegistry::new();
        assert_eq!(reg.stale_task_entry(TaskId(1)), None);
    }

    #[test]
    fn stale_task_entry_names_a_populated_map_then_withdraw_clears_it() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_streams(TaskId(8), DescriptorTable::standard());
        assert_eq!(reg.stale_task_entry(TaskId(8)), Some("streams"));
        // Reclaim must leave nothing behind: this is exactly the
        // post-condition `withdraw` asserts, exercised explicitly.
        assert!(reg.withdraw(TaskId(8)));
        assert_eq!(reg.stale_task_entry(TaskId(8)), None);
    }

    #[test]
    fn stale_task_entry_reports_a_registered_address_space() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(3), user_space(1, 1), sim()).unwrap();
        assert_eq!(reg.stale_task_entry(TaskId(3)), Some("tasks"));
        assert!(reg.withdraw(TaskId(3)));
        assert_eq!(reg.stale_task_entry(TaskId(3)), None);
    }

    #[test]
    fn stream_read_member_admits_only_the_owners_pipe_read_end() {
        let mut reg = AddressSpaceRegistry::new();
        let (read_fd, write_fd) = reg.open_pipe(TaskId(2)).expect("pipe minted");
        let file_fd = reg
            .open_file(TaskId(2), String::from("/Storage/x"), OpenFlags::READ)
            .expect("file opened");
        // Only the caller's own pipe read end qualifies.
        assert!(reg.stream_read_member(TaskId(2), read_fd));
        // A write end, a path-backed descriptor, an unopened number, and
        // another task's descriptor all refuse identically.
        assert!(!reg.stream_read_member(TaskId(2), write_fd));
        assert!(!reg.stream_read_member(TaskId(2), file_fd));
        assert!(!reg.stream_read_member(TaskId(2), 999));
        assert!(!reg.stream_read_member(TaskId(3), read_fd));
        // A closed descriptor stops qualifying.
        assert!(reg.close_file(TaskId(2), read_fd));
        assert!(!reg.stream_read_member(TaskId(2), read_fd));
    }

    #[test]
    fn stream_readable_peeks_bytes_and_eof_without_consuming() {
        let mut reg = AddressSpaceRegistry::new();
        let (read_fd, write_fd) = reg.open_pipe(TaskId(2)).expect("pipe minted");
        // Empty with a live writer: a read would park, so not ready.
        assert!(!reg.stream_readable(TaskId(2), read_fd));
        // Buffered bytes: ready, and the peek consumes nothing.
        let end = reg
            .open_file_entry(TaskId(2), write_fd)
            .and_then(|entry| entry.pipe().cloned())
            .expect("write end resolves");
        assert_eq!(end.try_write(b"go"), crate::pipe::WriteStep::Wrote(2));
        assert!(reg.stream_readable(TaskId(2), read_fd));
        assert!(reg.stream_readable(TaskId(2), read_fd));
        // The write end itself is never stream-readable; nor is a foreign
        // task's descriptor.
        assert!(!reg.stream_readable(TaskId(2), write_fd));
        assert!(!reg.stream_readable(TaskId(3), read_fd));
        // Closing every write end leaves the member ready for its EOF
        // read (drop the local clone too — each holds a live end).
        drop(end);
        assert!(reg.close_file(TaskId(2), write_fd));
        assert!(reg.stream_readable(TaskId(2), read_fd));
    }

    #[test]
    fn unset_cwd_resolves_to_the_root() {
        let reg = AddressSpaceRegistry::new();
        // A task whose working directory was never established resolves to
        // the root, the safe least-privileged default.
        assert_eq!(reg.cwd(TaskId(9)), "/");
    }

    #[test]
    fn set_cwd_then_resolve_returns_the_directory() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_cwd(TaskId(2), String::from("/Users/bob"));
        assert_eq!(reg.cwd(TaskId(2)), "/Users/bob");
        // A different task is unaffected and stays at the root default.
        assert_eq!(reg.cwd(TaskId(3)), "/");
    }

    #[test]
    fn withdraw_clears_the_working_directory() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_cwd(TaskId(4), String::from("/Storage/data"));
        // Withdrawing a task with a cwd but no address space still reports
        // the slot was present and resets it to the root.
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.cwd(TaskId(4)), "/");
    }

    #[test]
    fn unset_limits_resolve_to_the_default_policy() {
        let reg = AddressSpaceRegistry::new();
        // A task with no established set runs under the default policy.
        assert_eq!(reg.limits(TaskId(9)), LimitSet::DEFAULT);
    }

    #[test]
    fn set_limit_updates_one_kind_and_leaves_the_rest_at_default() {
        let mut reg = AddressSpaceRegistry::new();
        let lo = ResourceLimit::new(4, 8).expect("well-formed");
        reg.set_limit(TaskId(2), LimitKind::Processes, lo);
        let set = reg.limits(TaskId(2));
        assert_eq!(set.get(LimitKind::Processes), lo);
        // Every other kind stays at the default policy.
        assert_eq!(set.get(LimitKind::OpenStreams), ResourceLimit::UNLIMITED);
        // A different task is unaffected and stays at the default policy.
        assert_eq!(reg.limits(TaskId(3)), LimitSet::DEFAULT);
    }

    #[test]
    fn set_limits_replaces_the_full_set() {
        let mut reg = AddressSpaceRegistry::new();
        let mut wanted = LimitSet::DEFAULT;
        wanted.set(
            LimitKind::StackBytes,
            ResourceLimit::new(1024, 4096).expect("well-formed"),
        );
        reg.set_limits(TaskId(7), wanted);
        assert_eq!(reg.limits(TaskId(7)), wanted);
    }

    #[test]
    fn set_default_limits_feeds_the_fallback_and_set_limit_base() {
        let mut reg = AddressSpaceRegistry::new();
        let boot_default = LimitSet::with_pinned_default(128 << 20);
        reg.set_default_limits(boot_default);
        // An unestablished task resolves to the per-boot default …
        assert_eq!(reg.limits(TaskId(9)), boot_default);
        assert_eq!(reg.default_limits(), boot_default);
        // … and a first single-kind bound starts from it, keeping the
        // derived pinned bound rather than silently reverting to the
        // compile-time floor.
        let cap = ResourceLimit::new(4, 8).expect("well-formed");
        reg.set_limit(TaskId(9), LimitKind::Processes, cap);
        assert_eq!(reg.limits(TaskId(9)).get(LimitKind::Processes), cap);
        assert_eq!(
            reg.limits(TaskId(9)).get(LimitKind::PinnedMemoryBytes),
            boot_default.get(LimitKind::PinnedMemoryBytes)
        );
    }

    #[test]
    fn pin_state_is_per_task_idempotent_and_cleared_on_withdraw() {
        let mut reg = AddressSpaceRegistry::new();
        // Fresh tasks are unpinned — pinning is never inherited.
        assert!(!reg.is_pinned(TaskId(1)));
        reg.set_pinned(TaskId(1));
        assert!(reg.is_pinned(TaskId(1)));
        assert!(!reg.is_pinned(TaskId(2)), "pin is per-task state");
        // Idempotent both ways.
        reg.set_pinned(TaskId(1));
        assert!(reg.is_pinned(TaskId(1)));
        reg.clear_pinned(TaskId(1));
        assert!(!reg.is_pinned(TaskId(1)));
        reg.clear_pinned(TaskId(1));
        assert!(!reg.is_pinned(TaskId(1)));
        // Withdraw clears the mark, so a reused id starts unpinned and
        // the withdraw reports state was dropped.
        reg.set_pinned(TaskId(3));
        assert!(reg.withdraw(TaskId(3)));
        assert!(!reg.is_pinned(TaskId(3)));
    }

    #[test]
    fn pinned_footprint_sums_mapped_bytes_and_committed_stack() {
        let mut reg = AddressSpaceRegistry::new();
        let task = TaskId(4);
        assert_eq!(reg.pinned_footprint_bytes(task), 0);
        reg.charge_aspace_bytes(task, 3 * PAGE_SIZE as u64);
        assert_eq!(reg.pinned_footprint_bytes(task), 3 * PAGE_SIZE as u64);
        // Commit one stack page inside a recorded span: the committed
        // extent joins the footprint.
        let top = 0x8000_0000u64;
        let span = StackSpan::new(top - 16 * PAGE_SIZE as u64, top - PAGE_SIZE as u64, top)
            .expect("well-formed span");
        reg.set_stack_span(task, span);
        assert_eq!(
            reg.pinned_footprint_bytes(task),
            3 * PAGE_SIZE as u64 + PAGE_SIZE as u64
        );
    }

    #[test]
    fn pinned_total_aggregates_only_pinned_tasks() {
        let mut reg = AddressSpaceRegistry::new();
        reg.charge_aspace_bytes(TaskId(1), 2 * PAGE_SIZE as u64);
        reg.charge_aspace_bytes(TaskId(2), 5 * PAGE_SIZE as u64);
        assert_eq!(reg.pinned_total_bytes(), 0, "nothing pinned yet");
        reg.set_pinned(TaskId(1));
        assert_eq!(reg.pinned_total_bytes(), 2 * PAGE_SIZE as u64);
        reg.set_pinned(TaskId(2));
        assert_eq!(reg.pinned_total_bytes(), 7 * PAGE_SIZE as u64);
        // Unpin and exit both drop out of the aggregate.
        reg.clear_pinned(TaskId(2));
        assert_eq!(reg.pinned_total_bytes(), 2 * PAGE_SIZE as u64);
        reg.withdraw(TaskId(1));
        assert_eq!(reg.pinned_total_bytes(), 0);
    }

    #[test]
    fn withdraw_clears_the_limit_set() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_limit(
            TaskId(4),
            LimitKind::Processes,
            ResourceLimit::new(1, 2).expect("well-formed"),
        );
        // Withdrawing a task with limits but no address space still reports
        // the slot was present and resets it to the default policy.
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.limits(TaskId(4)), LimitSet::DEFAULT);
    }

    // --- device-resource grants ----------------

    /// A register window resource used across the grant tests.
    fn window() -> HwResource {
        HwResource::mmio(0xFE98_0000, 0x4000)
    }

    #[test]
    fn unminted_grant_resolves_to_none() {
        let reg = AddressSpaceRegistry::new();
        // A task with no grants can map nothing, and handle 0 (the reserved
        // invalid value) is never a live grant.
        assert_eq!(reg.grant(TaskId(9), 1), None);
        assert_eq!(reg.grant(TaskId(9), 0), None);
    }

    #[test]
    fn mint_then_grant_returns_the_resource_for_its_owner() {
        let mut reg = AddressSpaceRegistry::new();
        let handle = reg.mint_grant(TaskId(2), window());
        // The first minted handle is 1 (handle 0 stays reserved-invalid).
        assert_eq!(handle, 1);
        assert_eq!(reg.grant(TaskId(2), handle), Some(window()));
        // The reserved handle still resolves to nothing for the owner.
        assert_eq!(reg.grant(TaskId(2), 0), None);
    }

    #[test]
    fn handles_are_unique_per_task_and_name_distinct_resources() {
        let mut reg = AddressSpaceRegistry::new();
        let a = HwResource::mmio(0xFE98_0000, 0x4000);
        let b = HwResource::bus_window(0x6000_0000, 0x40_0000, 0xF800_0000);
        let h_a = reg.mint_grant(TaskId(2), a);
        let h_b = reg.mint_grant(TaskId(2), b);
        assert_ne!(h_a, h_b, "each grant gets its own handle");
        assert_eq!(reg.grant(TaskId(2), h_a), Some(a));
        assert_eq!(reg.grant(TaskId(2), h_b), Some(b));
    }

    #[test]
    fn grant_is_owner_bound_against_handle_forgery() {
        let mut reg = AddressSpaceRegistry::new();
        let handle = reg.mint_grant(TaskId(2), window());
        // The owner resolves its grant; an unknown handle value does not.
        assert_eq!(reg.grant(TaskId(2), handle), Some(window()));
        assert_eq!(reg.grant(TaskId(2), handle + 1), None);
        // A *different* task passing the same numeric handle reaches
        // nothing — a driver cannot map another driver's window by reusing
        // its handle value.
        assert_eq!(reg.grant(TaskId(3), handle), None);
    }

    #[test]
    fn withdraw_reclaims_every_grant() {
        let mut reg = AddressSpaceRegistry::new();
        let handle = reg.mint_grant(TaskId(4), window());
        assert_eq!(reg.grant(TaskId(4), handle), Some(window()));
        // Withdrawing a task with grants but no address space still reports
        // the slot was present and clears the grants (reclaimed on
        // exit).
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.grant(TaskId(4), handle), None);
    }

    /// Regression: delegation is idempotent, so repeating it cannot grow a
    /// recipient's kernel-side grant table.
    ///
    /// Minting appended a fresh entry on every call, so a donor holding one
    /// resource could drive an unbounded kernel allocation in a *victim's*
    /// record simply by calling a delegation syscall in a loop. Authority is
    /// a set: re-granting something already held returns the handle already
    /// issued.
    #[test]
    fn granting_a_held_resource_again_returns_the_same_handle() {
        let mut reg = AddressSpaceRegistry::new();
        let first = reg.mint_grant(TaskId(2), window());
        for _ in 0..1_000 {
            assert_eq!(
                reg.mint_grant(TaskId(2), window()),
                first,
                "repetition must not append a second entry naming the same resource"
            );
        }
        assert_eq!(
            reg.grants_to_le_bytes(TaskId(2)).len(),
            GrantedResource::WIRE_LEN
        );
        // A *different* resource is still new authority and mints its own
        // handle: suppression is exact, never a collapse of distinct grants.
        let other = HwResource::mmio(0x3F20_0000, 0x1000);
        assert_ne!(reg.mint_grant(TaskId(2), other), first);
    }

    /// A grant naming a *narrower* resource than one already held is still
    /// new authority in its own right and mints its own handle.
    ///
    /// Suppression matches exactly, never on coverage: handing back the
    /// handle of a wider grant would return authority the donor did not
    /// name, and the recipient's later map would reach further than the
    /// delegation said.
    #[test]
    fn a_narrower_resource_is_not_suppressed_by_a_wider_held_one() {
        let mut reg = AddressSpaceRegistry::new();
        let wide = HwResource::mmio(0xFE98_0000, 0x4000);
        let narrow = HwResource::mmio(0xFE98_0000, 0x1000);
        assert!(wide.covers(&narrow), "the wider window covers the narrower");
        let wide_handle = reg.mint_grant(TaskId(2), wide);
        let narrow_handle = reg.mint_grant(TaskId(2), narrow);
        assert_ne!(wide_handle, narrow_handle);
        assert_eq!(reg.grant(TaskId(2), narrow_handle), Some(narrow));
    }

    /// Regression: a per-endpoint grant must not outlive the endpoint
    /// instance it names.
    ///
    /// Endpoint ids are numeric and re-creatable, so a grant that survived
    /// its endpoint's destruction would silently retarget onto whatever task
    /// next binds that id. Teardown revokes every grant naming the destroyed
    /// ids — and only those.
    #[test]
    fn revoking_a_destroyed_endpoint_withdraws_only_the_grants_naming_it() {
        let mut reg = AddressSpaceRegistry::new();
        let doomed = HwResource::endpoint(0xCA11_0001);
        let survivor = HwResource::endpoint(0xCA11_0002);
        let holder_a = reg.mint_grant(TaskId(2), doomed);
        let holder_b = reg.mint_grant(TaskId(3), doomed);
        let unrelated_endpoint = reg.mint_grant(TaskId(3), survivor);
        // A same-numbered resource of a *different kind* must survive: the
        // revocation is scoped to endpoints, not to the number.
        let same_number_region = reg.mint_grant(TaskId(3), HwResource::shared(0xCA11_0001));
        let mmio = reg.mint_grant(TaskId(4), window());

        let mut destroyed = BTreeSet::new();
        destroyed.insert(0xCA11_0001_u64);
        assert_eq!(reg.revoke_endpoint_grants(&destroyed), 2);

        // Every holder of the destroyed endpoint lost it, whichever task.
        assert_eq!(reg.grant(TaskId(2), holder_a), None);
        assert_eq!(reg.grant(TaskId(3), holder_b), None);
        assert!(!reg.grant_covers(TaskId(2), &doomed));
        // Nothing else was touched.
        assert_eq!(reg.grant(TaskId(3), unrelated_endpoint), Some(survivor));
        assert_eq!(
            reg.grant(TaskId(3), same_number_region),
            Some(HwResource::shared(0xCA11_0001))
        );
        assert_eq!(reg.grant(TaskId(4), mmio), Some(window()));
        // Idempotent, and an empty set is a no-op.
        assert_eq!(reg.revoke_endpoint_grants(&destroyed), 0);
        assert_eq!(reg.revoke_endpoint_grants(&BTreeSet::new()), 0);
    }

    /// A handle number freed by revocation is never re-issued: the next
    /// grant to the same task draws a fresh number, so a holder that kept a
    /// stale handle value cannot have it alias a later grant.
    #[test]
    fn a_revoked_handle_number_is_not_reissued() {
        let mut reg = AddressSpaceRegistry::new();
        let revoked = reg.mint_grant(TaskId(2), HwResource::endpoint(0xCA11_0003));
        let mut destroyed = BTreeSet::new();
        destroyed.insert(0xCA11_0003_u64);
        assert_eq!(reg.revoke_endpoint_grants(&destroyed), 1);
        let fresh = reg.mint_grant(TaskId(2), window());
        assert_ne!(fresh, revoked);
        assert_eq!(reg.grant(TaskId(2), revoked), None);
    }

    /// Regression: re-granting a file delegation that is still pending
    /// returns the pending handle instead of appending a duplicate.
    ///
    /// Minting appended unconditionally, so a grantor could grow a
    /// *recipient's* kernel-side delegation table without limit by repeating
    /// one `fd_grant`. A pending delegation conveys exactly one right, and
    /// descriptors here carry no position, so a second identical entry
    /// conveys nothing the first does not. Once redeemed the entry is
    /// consumed, so a later grant of the same file legitimately mints afresh.
    #[test]
    fn regranting_a_pending_delegation_returns_the_pending_handle() {
        let mut reg = AddressSpaceRegistry::new();
        let file = DelegatedFile {
            path: String::from("/Users/ada/Documents/report.txt"),
            uid: 1000,
            caps: CapabilitySet::empty(),
        };
        let first = reg.mint_fd_delegation(TaskId(2), file.clone(), OpenFlags::READ);
        for _ in 0..1_000 {
            assert_eq!(
                reg.mint_fd_delegation(TaskId(2), file.clone(), OpenFlags::READ),
                first,
                "repetition must not append a second pending delegation"
            );
        }
        // A different file is a distinct right and mints its own handle.
        let other = DelegatedFile {
            path: String::from("/Users/ada/Documents/other.txt"),
            uid: 1000,
            caps: CapabilitySet::empty(),
        };
        assert_ne!(
            reg.mint_fd_delegation(TaskId(2), other, OpenFlags::READ),
            first
        );
        // One redemption consumes the one pending right; the duplicate
        // suppression never turned two grants into one *redeemable*
        // descriptor that outlives its consumption.
        assert!(reg.redeem_fd_delegation(TaskId(2), first).is_ok());
        assert_eq!(
            reg.redeem_fd_delegation(TaskId(2), first),
            Err(Errno::NotFound)
        );
        // With nothing pending, granting the same file again mints anew.
        let renewed = reg.mint_fd_delegation(TaskId(2), file, OpenFlags::READ);
        assert_ne!(renewed, first);
    }

    #[test]
    fn reused_task_id_starts_from_an_empty_grant_set() {
        let mut reg = AddressSpaceRegistry::new();
        let old = reg.mint_grant(TaskId(5), window());
        assert!(reg.withdraw(TaskId(5)));
        // A new task reusing the id mints from handle 1 again and never
        // inherits the dead task's grant.
        let fresh = reg.mint_grant(TaskId(5), HwResource::mmio(0x3F20_0000, 0x1000));
        assert_eq!(fresh, 1);
        assert_eq!(old, 1);
        assert_eq!(
            reg.grant(TaskId(5), fresh),
            Some(HwResource::mmio(0x3F20_0000, 0x1000))
        );
    }

    // --- open file/directory descriptors --------

    #[test]
    fn first_opened_descriptor_is_the_first_number_after_the_standard_streams() {
        let mut reg = AddressSpaceRegistry::new();
        let fd = reg
            .open_file(TaskId(2), String::from("/System/Logs/a"), OpenFlags::READ)
            .expect("descriptor space is not exhausted");
        // fd 0..3 are the reserved standard streams; the first file handle is 4.
        assert_eq!(fd, u32::try_from(STD_STREAM_COUNT).unwrap());
        assert_eq!(
            reg.open_file_entry(TaskId(2), fd),
            Some(OpenFile::new(
                OpenBacking::Path(String::from("/System/Logs/a")),
                OpenFlags::READ,
            ))
        );
    }

    #[test]
    fn a_resource_descriptor_shares_the_one_number_space_with_files() {
        let mut reg = AddressSpaceRegistry::new();
        // A file takes the first descriptor after the standard streams.
        let file = reg
            .open_file(TaskId(2), String::from("/Storage/x"), OpenFlags::READ)
            .expect("fits");
        // A resource open draws the *next* number from the same allocator, so
        // a resource fd can never collide with a file fd.
        let res = reg
            .open_resource(TaskId(2), ResourceBacking::Random, OpenFlags::READ)
            .expect("fits");
        assert_eq!((file, res), (4, 5));
        assert_eq!(
            reg.open_file_entry(TaskId(2), res).map(|f| f.backing),
            Some(OpenBacking::Resource(ResourceBacking::Random))
        );
        // The resource handle exposes its resource, not a path.
        assert_eq!(
            reg.open_file_entry(TaskId(2), res)
                .and_then(|f| f.resource()),
            Some(ResourceBacking::Random)
        );
        assert_eq!(
            reg.open_file_entry(TaskId(2), res)
                .and_then(|f| f.path().map(String::from)),
            None
        );
    }

    #[test]
    fn an_unopened_descriptor_resolves_to_none() {
        let reg = AddressSpaceRegistry::new();
        assert_eq!(reg.open_file_entry(TaskId(9), 4), None);
        // A standard-stream number is never recorded in the open-file table.
        assert_eq!(reg.open_file_entry(TaskId(9), 0), None);
    }

    #[test]
    fn open_descriptor_is_owner_bound_against_forgery() {
        let mut reg = AddressSpaceRegistry::new();
        let fd = reg
            .open_file(TaskId(2), String::from("/Storage/x"), OpenFlags::READ)
            .expect("fits");
        // A *different* task passing the same number reaches nothing — one
        // process cannot read another's open file by guessing the descriptor.
        assert_eq!(reg.open_file_entry(TaskId(3), fd), None);
        assert_eq!(
            reg.open_file_entry(TaskId(2), fd)
                .and_then(|f| f.path().map(String::from)),
            Some(String::from("/Storage/x"))
        );
    }

    #[test]
    fn closing_a_descriptor_frees_it_and_close_is_idempotent() {
        let mut reg = AddressSpaceRegistry::new();
        let fd = reg
            .open_file(TaskId(2), String::from("/Storage/x"), OpenFlags::READ)
            .expect("fits");
        assert!(reg.close_file(TaskId(2), fd));
        assert_eq!(reg.open_file_entry(TaskId(2), fd), None);
        // Closing again, or closing an unopened number, is a fail-closed no-op.
        assert!(!reg.close_file(TaskId(2), fd));
        assert!(!reg.close_file(TaskId(2), 999));
        // Closing another task's descriptor number is refused.
        assert!(!reg.close_file(TaskId(3), fd));
    }

    #[test]
    fn the_lowest_free_descriptor_is_reused_after_a_close() {
        let mut reg = AddressSpaceRegistry::new();
        let a = reg
            .open_file(TaskId(2), String::from("/a"), OpenFlags::READ)
            .expect("fits");
        let b = reg
            .open_file(TaskId(2), String::from("/b"), OpenFlags::READ)
            .expect("fits");
        let c = reg
            .open_file(TaskId(2), String::from("/c"), OpenFlags::READ)
            .expect("fits");
        assert_eq!((a, b, c), (4, 5, 6));
        // Free the middle one; the next open reuses the lowest free number.
        assert!(reg.close_file(TaskId(2), b));
        let reused = reg
            .open_file(TaskId(2), String::from("/d"), OpenFlags::READ)
            .expect("fits");
        assert_eq!(reused, 5);
    }

    #[test]
    fn withdraw_reclaims_every_open_descriptor() {
        let mut reg = AddressSpaceRegistry::new();
        let fd = reg
            .open_file(TaskId(4), String::from("/Storage/x"), OpenFlags::READ)
            .expect("fits");
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.open_file_entry(TaskId(4), fd), None);
        // A reused id starts from an empty descriptor set, back at 4.
        let fresh = reg
            .open_file(TaskId(4), String::from("/Storage/y"), OpenFlags::READ)
            .expect("fits");
        assert_eq!(fresh, u32::try_from(STD_STREAM_COUNT).unwrap());
    }

    // --- pipes and wired standard-stream entries --------

    #[test]
    fn open_pipe_mints_a_read_write_pair_in_the_one_number_space() {
        let mut reg = AddressSpaceRegistry::new();
        let (read_fd, write_fd) = reg.open_pipe(TaskId(2)).expect("pair fits");
        assert_eq!((read_fd, write_fd), (4, 5));
        let read = reg.open_file_entry(TaskId(2), read_fd).expect("read end");
        let write = reg.open_file_entry(TaskId(2), write_fd).expect("write end");
        assert!(read.flags.is_read() && !read.flags.is_write());
        assert!(write.flags.is_write() && !write.flags.is_read());
        let (read_end, write_end) = (read.pipe().expect("pipe"), write.pipe().expect("pipe"));
        assert!(read_end.same_pipe(write_end));
        // The pair is owner-bound like every descriptor.
        assert_eq!(reg.open_file_entry(TaskId(3), read_fd), None);
        // A later open draws the next number from the same allocator.
        let next = reg
            .open_file(TaskId(2), String::from("/x"), OpenFlags::READ)
            .expect("fits");
        assert_eq!(next, 6);
    }

    #[test]
    fn closing_a_pipe_entry_releases_its_end() {
        use crate::pipe::WriteStep;
        let mut reg = AddressSpaceRegistry::new();
        let (read_fd, write_fd) = reg.open_pipe(TaskId(2)).expect("pair fits");
        let write = reg.open_file_entry(TaskId(2), write_fd).expect("write end");
        let write_end = write.pipe().expect("pipe").clone();
        // Dropping the read entry (close) leaves no reader: the writer
        // observes broken-pipe through the shared object.
        assert!(reg.close_file(TaskId(2), read_fd));
        assert_eq!(write_end.try_write(b"x"), WriteStep::Broken);
    }

    #[test]
    fn withdraw_releases_pipe_ends_through_the_table_drop() {
        use crate::pipe::ReadStep;
        let mut reg = AddressSpaceRegistry::new();
        let (read_fd, _write_fd) = reg.open_pipe(TaskId(2)).expect("pair fits");
        let read = reg.open_file_entry(TaskId(2), read_fd).expect("read end");
        let read_end = read.pipe().expect("pipe").clone();
        // Task exit: the whole table drops, closing the write end, so the
        // surviving reader observes end-of-stream (nothing leaks).
        reg.register(TaskId(2), user_space(1, 7), sim())
            .expect("register");
        assert!(reg.withdraw(TaskId(2)));
        assert_eq!(read_end.try_read(&mut [0u8; 4]), ReadStep::Eof);
    }

    #[test]
    fn install_std_entry_accepts_only_standard_slots() {
        let mut reg = AddressSpaceRegistry::new();
        let entry = OpenFile::new(OpenBacking::Path(String::from("/log")), OpenFlags::WRITE);
        assert_eq!(reg.install_std_entry(TaskId(2), 1, entry.clone()), Ok(()));
        assert_eq!(reg.open_file_entry(TaskId(2), 1), Some(entry.clone()));
        // The reserved standard range is the only installable space.
        assert_eq!(
            reg.install_std_entry(TaskId(2), 4, entry),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn cloned_entries_share_one_stream_cursor() {
        let entry = OpenFile::new(OpenBacking::Path(String::from("/log")), OpenFlags::WRITE);
        let dup = entry.clone();
        entry.advance_cursor(10);
        // The dup observes the shared description position (POSIX dup
        // semantics), not a private copy.
        assert_eq!(dup.cursor(), 10);
        dup.advance_cursor(5);
        assert_eq!(entry.cursor(), 15);
        // A fresh entry over the same backing has its own description.
        let fresh = OpenFile::new(OpenBacking::Path(String::from("/log")), OpenFlags::WRITE);
        assert_eq!(fresh.cursor(), 0);
        assert_eq!(fresh, entry, "equality names the backing, not the cursor");
    }

    // --- mapped anonymous-memory accounting (the AddressSpaceBytes limit) --

    #[test]
    fn a_task_with_no_mapping_has_zero_mapped_anon_bytes() {
        let reg = AddressSpaceRegistry::new();
        assert_eq!(reg.mapped_aspace_bytes(TaskId(2)), 0);
    }

    #[test]
    fn charge_then_credit_tracks_the_running_total() {
        let mut reg = AddressSpaceRegistry::new();
        reg.charge_aspace_bytes(TaskId(2), 0x4000);
        assert_eq!(reg.mapped_aspace_bytes(TaskId(2)), 0x4000);
        // A second map accrues onto the existing total.
        reg.charge_aspace_bytes(TaskId(2), 0x1000);
        assert_eq!(reg.mapped_aspace_bytes(TaskId(2)), 0x5000);
        // Freeing one region credits it back.
        reg.credit_aspace_bytes(TaskId(2), 0x1000);
        assert_eq!(reg.mapped_aspace_bytes(TaskId(2)), 0x4000);
    }

    #[test]
    fn credit_saturates_at_zero_and_drops_the_entry() {
        let mut reg = AddressSpaceRegistry::new();
        reg.charge_aspace_bytes(TaskId(2), 0x2000);
        // Crediting more than is charged can never underflow into a bogus
        // huge total that would wrongly deny later maps.
        reg.credit_aspace_bytes(TaskId(2), 0x9000);
        assert_eq!(reg.mapped_aspace_bytes(TaskId(2)), 0);
        // Crediting a task that holds nothing is a no-op.
        reg.credit_aspace_bytes(TaskId(3), 0x1000);
        assert_eq!(reg.mapped_aspace_bytes(TaskId(3)), 0);
    }

    #[test]
    fn withdraw_drops_anon_accounting_so_a_reused_id_starts_clean() {
        let mut reg = AddressSpaceRegistry::new();
        reg.charge_aspace_bytes(TaskId(4), 0x8000);
        assert!(reg.withdraw(TaskId(4)));
        // A reused id never inherits the dead task's mapped-memory total.
        assert_eq!(reg.mapped_aspace_bytes(TaskId(4)), 0);
    }

    // --- demand-paged file-mapping regions (file_map / file_unmap) --------

    fn file_region(base: u64, len: u64) -> FileRegion {
        FileRegion {
            base,
            len,
            path: String::from("/big"),
            offset: 0x1000,
            uid: 7,
            caps: CapabilitySet::from_words([0; 4]),
        }
    }

    #[test]
    fn file_region_exact_matches_only_the_recorded_pair_of_the_owner() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_file_region(TaskId(2), file_region(0x10_0000, 0x4000));
        // The exact `(base, len)` of the recording task resolves; a wrong
        // base, a wrong length, and another task's lookup all fail closed.
        assert!(reg
            .file_region_exact(TaskId(2), 0x10_0000, 0x4000)
            .is_some());
        assert!(reg
            .file_region_exact(TaskId(2), 0x10_1000, 0x4000)
            .is_none());
        assert!(reg
            .file_region_exact(TaskId(2), 0x10_0000, 0x3000)
            .is_none());
        assert!(reg
            .file_region_exact(TaskId(3), 0x10_0000, 0x4000)
            .is_none());
    }

    #[test]
    fn file_region_covering_resolves_only_addresses_inside_a_live_region() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_file_region(TaskId(2), file_region(0x10_0000, 0x4000));
        reg.record_file_region(TaskId(2), file_region(0x20_0000, 0x1000));
        // Base, an interior byte, and the last byte are covered.
        assert!(reg.file_region_covering(TaskId(2), 0x10_0000).is_some());
        assert!(reg.file_region_covering(TaskId(2), 0x10_2fff).is_some());
        assert!(reg.file_region_covering(TaskId(2), 0x10_3fff).is_some());
        // The exclusive top, the gap between regions, an address below every
        // region, and another task's address space are not.
        assert!(reg.file_region_covering(TaskId(2), 0x10_4000).is_none());
        assert!(reg.file_region_covering(TaskId(2), 0x18_0000).is_none());
        assert!(reg.file_region_covering(TaskId(2), 0x0f_ffff).is_none());
        assert!(reg.file_region_covering(TaskId(3), 0x10_0000).is_none());
        // The second region resolves independently and carries its record.
        let hit = reg
            .file_region_covering(TaskId(2), 0x20_0abc)
            .expect("inside the second region");
        assert_eq!(hit.base, 0x20_0000);
        assert_eq!(hit.path, "/big");
    }

    #[test]
    fn remove_file_region_returns_the_record_and_only_once() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_file_region(TaskId(2), file_region(0x10_0000, 0x4000));
        let removed = reg
            .remove_file_region(TaskId(2), 0x10_0000)
            .expect("recorded");
        assert_eq!(removed.len, 0x4000);
        // Gone: neither an exact lookup, a covering lookup, nor a second
        // removal can see it.
        assert!(reg
            .file_region_exact(TaskId(2), 0x10_0000, 0x4000)
            .is_none());
        assert!(reg.file_region_covering(TaskId(2), 0x10_0001).is_none());
        assert!(reg.remove_file_region(TaskId(2), 0x10_0000).is_none());
    }

    #[test]
    fn withdraw_drops_file_regions_so_a_reused_id_starts_clean() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_file_region(TaskId(5), file_region(0x10_0000, 0x4000));
        assert!(reg.withdraw(TaskId(5)));
        assert!(reg.file_region_covering(TaskId(5), 0x10_0000).is_none());
    }

    // --- reserved demand-paged anonymous regions (mem_map) ----------------

    #[test]
    fn anon_region_covering_resolves_only_addresses_inside_a_live_region() {
        let mut reg = AddressSpaceRegistry::new();
        // A four-page region based at 0x20_0000.
        reg.record_anon_region(TaskId(2), 0x20_0000, 4);
        // Inside the region (first byte, last byte of the fourth page).
        assert!(reg.anon_region_covering(TaskId(2), 0x20_0000));
        assert!(reg.anon_region_covering(TaskId(2), 0x20_0000 + 4 * 0x1000 - 1));
        // One byte past the region is outside.
        assert!(!reg.anon_region_covering(TaskId(2), 0x20_0000 + 4 * 0x1000));
        // Below the base and another task both resolve to nothing.
        assert!(!reg.anon_region_covering(TaskId(2), 0x1F_FFFF));
        assert!(!reg.anon_region_covering(TaskId(3), 0x20_0000));
    }

    #[test]
    fn anon_region_exact_matches_only_the_recorded_pair_of_the_owner() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_anon_region(TaskId(2), 0x20_0000, 4);
        assert!(reg.anon_region_exact(TaskId(2), 0x20_0000, 4));
        // A wrong page count, a wrong base, or another task all fail closed.
        assert!(!reg.anon_region_exact(TaskId(2), 0x20_0000, 3));
        assert!(!reg.anon_region_exact(TaskId(2), 0x21_0000, 4));
        assert!(!reg.anon_region_exact(TaskId(3), 0x20_0000, 4));
    }

    #[test]
    fn remove_anon_region_returns_the_page_count_and_only_once() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_anon_region(TaskId(2), 0x20_0000, 4);
        assert_eq!(reg.remove_anon_region(TaskId(2), 0x20_0000), Some(4));
        // Gone: neither a covering lookup, an exact lookup, nor a second
        // removal can see it.
        assert!(!reg.anon_region_covering(TaskId(2), 0x20_0000));
        assert!(!reg.anon_region_exact(TaskId(2), 0x20_0000, 4));
        assert_eq!(reg.remove_anon_region(TaskId(2), 0x20_0000), None);
    }

    #[test]
    fn withdraw_drops_anon_regions_so_a_reused_id_starts_clean() {
        let mut reg = AddressSpaceRegistry::new();
        reg.record_anon_region(TaskId(5), 0x20_0000, 4);
        assert!(reg.withdraw(TaskId(5)));
        assert!(!reg.anon_region_covering(TaskId(5), 0x20_0000));
    }

    // --- reserved user-stack spans (demand-grown stack) -------------------

    fn stack_span() -> StackSpan {
        StackSpan::new(0x4000, 0x8000, 0xA000).expect("well-formed")
    }

    #[test]
    fn stack_span_new_fails_closed_on_a_malformed_shape() {
        // Misaligned bounds.
        assert!(StackSpan::new(0x4001, 0x8000, 0xA000).is_none());
        assert!(StackSpan::new(0x4000, 0x8010, 0xA000).is_none());
        assert!(StackSpan::new(0x4000, 0x8000, 0xA00F).is_none());
        // A committed base below the reserve base.
        assert!(StackSpan::new(0x8000, 0x4000, 0xA000).is_none());
        // An empty committed top.
        assert!(StackSpan::new(0x4000, 0xA000, 0xA000).is_none());
        // A fully committed span (reserve == committed) is legal.
        assert!(StackSpan::new(0x4000, 0x4000, 0xA000).is_some());
    }

    #[test]
    fn stack_span_reports_growth_room_and_committed_bytes() {
        let span = stack_span();
        // The growth room is `[reserve_base, committed_base)` exactly.
        assert!(span.in_growth_room(0x4000));
        assert!(span.in_growth_room(0x7fff));
        assert!(!span.in_growth_room(0x3fff));
        assert!(!span.in_growth_room(0x8000));
        assert!(!span.in_growth_room(0x9fff));
        assert_eq!(span.committed_bytes(), 0x2000);
    }

    #[test]
    fn unrecorded_stack_span_resolves_to_none() {
        let reg = AddressSpaceRegistry::new();
        assert!(reg.stack_span(TaskId(2)).is_none());
        assert_eq!(reg.stack_committed_bytes(TaskId(2)), 0);
    }

    #[test]
    fn set_stack_span_then_resolve_returns_the_owners_record() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_stack_span(TaskId(2), stack_span());
        assert_eq!(reg.stack_span(TaskId(2)), Some(stack_span()));
        // Another task's lookup resolves nothing (fail closed).
        assert!(reg.stack_span(TaskId(3)).is_none());
        assert_eq!(reg.stack_committed_bytes(TaskId(2)), 0x2000);
    }

    #[test]
    fn commit_stack_page_lowers_the_base_monotonically_within_the_span() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_stack_span(TaskId(2), stack_span());
        // A growth page lowers the committed base and grows the usage.
        reg.commit_stack_page(TaskId(2), 0x6000);
        assert_eq!(
            reg.stack_span(TaskId(2))
                .expect("recorded")
                .committed_base(),
            0x6000
        );
        assert_eq!(reg.stack_committed_bytes(TaskId(2)), 0x4000);
        // A page at/above the base (the resident race) never raises it.
        reg.commit_stack_page(TaskId(2), 0x7000);
        reg.commit_stack_page(TaskId(2), 0x9000);
        assert_eq!(
            reg.stack_span(TaskId(2))
                .expect("recorded")
                .committed_base(),
            0x6000
        );
        // A page below the reserve base is refused — the record can never
        // claim pages outside the span.
        reg.commit_stack_page(TaskId(2), 0x3000);
        assert_eq!(
            reg.stack_span(TaskId(2))
                .expect("recorded")
                .committed_base(),
            0x6000
        );
        // A task with no record is a no-op.
        reg.commit_stack_page(TaskId(3), 0x6000);
        assert!(reg.stack_span(TaskId(3)).is_none());
    }

    #[test]
    fn withdraw_drops_the_stack_span_so_a_reused_id_starts_clean() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_stack_span(TaskId(6), stack_span());
        assert!(reg.withdraw(TaskId(6)));
        assert!(reg.stack_span(TaskId(6)).is_none());
        assert_eq!(reg.stack_committed_bytes(TaskId(6)), 0);
    }

    #[test]
    fn unrecorded_load_base_resolves_to_none() {
        let reg = AddressSpaceRegistry::new();
        assert!(reg.load_base(TaskId(2)).is_none());
    }

    #[test]
    fn set_load_base_then_resolve_returns_the_owners_base() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_load_base(TaskId(2), 0x20_0000);
        assert_eq!(reg.load_base(TaskId(2)), Some(0x20_0000));
        // Keyed by the owning task; a different id has no base.
        assert!(reg.load_base(TaskId(3)).is_none());
        // Replacing is permitted (a reused id re-admitted at a new base).
        reg.set_load_base(TaskId(2), 0x40_0000);
        assert_eq!(reg.load_base(TaskId(2)), Some(0x40_0000));
    }

    #[test]
    fn withdraw_drops_the_load_base_so_a_reused_id_starts_clean() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_load_base(TaskId(6), 0x20_0000);
        assert!(reg.withdraw(TaskId(6)));
        assert!(reg.load_base(TaskId(6)).is_none());
    }

    #[test]
    fn fault_locality_accessors_carry_only_distances() {
        assert_eq!(
            FaultLocality::NullPage { offset: 0x18 }.bucket(),
            "null_page"
        );
        assert_eq!(
            FaultLocality::NullPage { offset: 0x18 }.offset(),
            Some(0x18)
        );
        assert_eq!(
            FaultLocality::BelowStackGuard { distance: 0x40 }.bucket(),
            "below_stack_guard"
        );
        assert_eq!(
            FaultLocality::BelowStackGuard { distance: 0x40 }.offset(),
            Some(0x40)
        );
        assert_eq!(FaultLocality::PastRegion { offset: 0x8 }.bucket(), "region");
        assert_eq!(
            FaultLocality::PastRegion { offset: 0x8 }.offset(),
            Some(0x8)
        );
        assert_eq!(FaultLocality::Wild.bucket(), "wild");
        assert_eq!(FaultLocality::Wild.offset(), None);
    }

    #[test]
    fn classify_fault_locality_names_a_null_page_dereference() {
        let reg = AddressSpaceRegistry::new();
        // Anywhere in the first page, offset measured from VA 0.
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0),
            FaultLocality::NullPage { offset: 0 }
        );
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x18),
            FaultLocality::NullPage { offset: 0x18 }
        );
        // The very first byte of the second page is no longer the null page.
        assert_ne!(
            reg.classify_fault_locality(TaskId(2), PAGE_SIZE as u64)
                .bucket(),
            "null_page"
        );
    }

    #[test]
    fn classify_fault_locality_names_a_below_guard_stack_overflow() {
        let mut reg = AddressSpaceRegistry::new();
        // A stack span with a high reserve base so a fault far below it can
        // be tested without underflowing.
        let reserve_base = 0x20_0000u64;
        let span = StackSpan::new(reserve_base, reserve_base + 0x4000, reserve_base + 0x8000)
            .expect("well-formed");
        reg.set_stack_span(TaskId(2), span);
        // A fault a little below the reserve base is an overflow that ran
        // past the guard page.
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), reserve_base - 0x40),
            FaultLocality::BelowStackGuard { distance: 0x40 }
        );
        // Far below the reserve base (past the window) is genuinely wild,
        // not attributed to the stack.
        assert_eq!(
            reg.classify_fault_locality(
                TaskId(2),
                reserve_base - (NEAR_REGION_WINDOW + PAGE_SIZE as u64)
            ),
            FaultLocality::Wild
        );
    }

    #[test]
    fn classify_fault_locality_measures_a_bounded_run_past_a_region() {
        let mut reg = AddressSpaceRegistry::new();
        // A file region [0x10_0000, 0x10_4000): a small run past its end
        // is reported as a region-relative offset, the region unnamed.
        reg.record_file_region(TaskId(2), file_region(0x10_0000, 0x4000));
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x10_4000 + 0x40),
            FaultLocality::PastRegion { offset: 0x40 }
        );
        // One byte past the end is offset 0.
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x10_4000),
            FaultLocality::PastRegion { offset: 0 }
        );
        // Far past the region (beyond the window) is wild.
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x10_4000 + NEAR_REGION_WINDOW + 1),
            FaultLocality::Wild
        );
        // A fault inside the live region is not a run *past* it — it is a
        // miss inside memory the task owns (the deterministic OOM case), so
        // the locality is the honest `InRegion`, never the scaremongering
        // `wild` (which is reserved for addresses outside every mapping).
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x10_2000),
            FaultLocality::InRegion
        );
    }

    #[test]
    fn classify_fault_locality_names_an_in_region_oom_not_wild() {
        let mut reg = AddressSpaceRegistry::new();
        // A reserved anonymous region [0x20_0000, 0x20_4000): a fault inside
        // it that the resolver could not back (frame exhaustion) is a
        // deterministic OOM, reported as `in_region` with no leaked offset —
        // not `wild`, which would falsely read as a stray pointer.
        reg.record_anon_region(TaskId(2), 0x20_0000, 4);
        let locality = reg.classify_fault_locality(TaskId(2), 0x20_2000);
        assert_eq!(locality, FaultLocality::InRegion);
        assert_eq!(locality.bucket(), "in_region");
        assert_eq!(locality.offset(), None, "in-region OOM leaks no offset");
    }

    #[test]
    fn classify_fault_locality_uses_the_nearest_owned_region_end() {
        let mut reg = AddressSpaceRegistry::new();
        // Two regions and an anonymous mapping; the nearest end at or below
        // the fault wins, so the reported offset is the smallest true
        // distance.
        reg.record_file_region(TaskId(2), file_region(0x10_0000, 0x4000)); // end 0x104000
        reg.record_anon_region(TaskId(2), 0x20_0000, 4); // end 0x204000
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x20_4000 + 0x10),
            FaultLocality::PastRegion { offset: 0x10 }
        );
    }

    #[test]
    fn classify_fault_locality_is_wild_with_no_regions() {
        let reg = AddressSpaceRegistry::new();
        assert_eq!(
            reg.classify_fault_locality(TaskId(2), 0x9999_0000),
            FaultLocality::Wild
        );
    }
}

//! Per-task capability state.
//!
//! Every running task has a [`TaskCapabilities`] record. The effective
//! capability set is **the intersection** of two narrower-or-equal sets:
//!
//! * the user grant attached to the task's owning [`UserId`], and
//! * the capability set the binary's signed manifest *requested*.
//!
//! Both halves enter through the verifier in `manifest.rs` and the
//! verifier in `identity.rs`; this module never widens what those modules
//! sanctioned. Delegation and revocation are forwarded to
//! `lib/caps` — the single source of truth for the subset-only delegation
//! invariant — and every transition emits exactly one audit event.
//!
//! # No ambient authority
//!
//! Nothing in this module branches on `uid == 0`. The numeric uid is
//! attached purely so the audit trail can attribute a record to a
//! principal; it confers no extra capability.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_abi::{CapabilitySummary, Errno, Origin, ProcId, TrustDomain, ORIGIN_CONSOLE_NONE};
use tairix_caps::{CapabilitySet, CapabilityToken, RevocationEpoch};
use tairix_crypto::Ed25519PublicKey;
use tairix_log::{Field, Sink};

use crate::audit::{record, AuditEvent};
use crate::identity::{format_hex_u64, format_i32, GroupId, UserId};

/// Numeric task identifier carried by audit records.
///
/// Distinct from `pid_t`: `TaskId` is the kernel's internal handle for a
/// schedulable entity. `kernel/sched` produces these; we accept them
/// verbatim for audit attribution.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TaskId(pub u64);

/// Maximum length, in bytes, of a kernel-attested process name.
///
/// Reuses the one process-name bound `tairix_abi` already defines for the
/// System Information process record, so the attested audit name and any
/// reported process name can never disagree on length — a single definition.
pub const PROC_NAME_MAX: usize = tairix_abi::sysinfo::PROCESS_NAME_MAX;

/// A kernel-attested, bounded process name.
///
/// Set kernel-side at process admission from trusted state — the resolved
/// executable path, or a fixed name for the kernel's own principals — and
/// never from caller-supplied bytes, so an audit consumer may trust it to
/// name the acting process. Stored inline (no allocation) so it can be read
/// on the audit path, and it holds only a valid-UTF-8 prefix so
/// [`as_str`](Self::as_str) is total.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcName {
    buf: [u8; PROC_NAME_MAX],
    len: usize,
}

impl ProcName {
    /// The empty name — the default for a record whose admit path attested
    /// none (kernel threads, in-kernel binder / device-host records).
    pub const EMPTY: Self = Self {
        buf: [0u8; PROC_NAME_MAX],
        len: 0,
    };

    /// Build a name from `bytes`, keeping the largest valid-UTF-8 prefix that
    /// fits in [`PROC_NAME_MAX`].
    ///
    /// A process name is display/attribution metadata, not a security
    /// decision, so an over-long or non-UTF-8-boundary input is bounded to
    /// its largest valid prefix rather than rejected — never storing invalid
    /// UTF-8 (so [`as_str`](Self::as_str) is total) and never storing a
    /// caller-trusted value (the sole callers pass kernel-resolved bytes).
    #[must_use]
    pub fn from_bytes_truncating(bytes: &[u8]) -> Self {
        let capped = if bytes.len() > PROC_NAME_MAX {
            &bytes[..PROC_NAME_MAX]
        } else {
            bytes
        };
        let valid = match core::str::from_utf8(capped) {
            Ok(s) => s.len(),
            Err(e) => e.valid_up_to(),
        };
        let mut buf = [0u8; PROC_NAME_MAX];
        buf[..valid].copy_from_slice(&capped[..valid]);
        Self { buf, len: valid }
    }

    /// Build a name from a kernel-resolved executable or bundle `path`.
    ///
    /// This is the one definition of "name a process after the path it was
    /// started from", shared by every admit path that resolves a path
    /// kernel-side (the `spawn` syscall, the driver-store spawn seam):
    ///
    /// * The generic bundle entry point (a final `Run` component,
    ///   [`tairix_abi::BundleEntry::Run`]) never names a process — every
    ///   bundle shares that leaf. The owning bundle directory's stem names
    ///   it instead, with a [`tairix_abi::BUNDLE_SUFFIX`] (`.app`) suffix
    ///   stripped, so `/Apps/Example.app/Run` attests `Example` and a
    ///   driver bundle `/System/Drivers/input/usb_kbd/Run` attests
    ///   `usb_kbd`.
    /// * Any other path names its final non-empty `/`-separated component.
    /// * A path from which no name is derivable (`"/"`, `""`, a bare
    ///   `Run` with no owning directory) keeps the whole path bytes rather
    ///   than attesting an empty name, so a process listing always has
    ///   *something* truthful to display.
    #[must_use]
    pub fn from_path(path: &[u8]) -> Self {
        let mut components = path
            .rsplit(|&b| b == b'/')
            .filter(|component| !component.is_empty());
        let Some(last) = components.next() else {
            return Self::from_bytes_truncating(path);
        };
        if last != tairix_abi::BundleEntry::Run.as_str().as_bytes() {
            return Self::from_bytes_truncating(last);
        }
        match components.next() {
            Some(parent) => {
                let stem = parent
                    .strip_suffix(tairix_abi::BUNDLE_SUFFIX.as_bytes())
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or(parent);
                Self::from_bytes_truncating(stem)
            }
            None => Self::from_bytes_truncating(path),
        }
    }

    /// Borrow the name as a string; always valid UTF-8 by construction.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// `true` if no name was attested.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for ProcName {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Per-task capability state.
///
/// The fields are private so callers cannot bypass the intersection
/// invariant by writing the effective set directly. Construct via
/// [`Self::derive`] and mutate only through [`Self::delegate`] and
/// [`Self::revoke`].
///
/// Carries no [`Eq`]/[`PartialEq`]: nothing in the kernel compares a whole
/// record, and the per-process I/O counters are live accounting state, not
/// a value with a meaningful equality. [`Clone`] is still needed — the
/// syscall dispatcher clones a snapshot of the caller's record under a
/// briefly held read lock so the rest of the call runs lock-free — so the
/// counters are [`Arc`]-shared [`AtomicU64`]s: cloning the record shares the
/// same underlying counters rather than resetting a fresh pair, so an
/// increment made through a per-syscall snapshot still lands on the one
/// total the registry's live entry (and every other outstanding snapshot)
/// reads back.
#[derive(Clone, Debug)]
pub struct TaskCapabilities {
    task: TaskId,
    owner: UserId,
    /// Primary group of the task's kernel-attested credential.
    ///
    /// Snapshotted at process creation from the authoritative identity table
    /// (a switch to a target user) or inherited from the spawning parent's own
    /// record; never derived from caller-supplied bytes. Defaults to
    /// [`GroupId::default`] (gid 0, the system group) for a record built
    /// before a credential is attached (kernel principals, a plain
    /// [`Self::derive`]). It is identity used by the filesystem permission
    /// model and reported in the attested [`Origin`]; it confers no capability
    /// (capabilities flow only through `effective`).
    primary_gid: GroupId,
    /// Supplementary groups of the task's kernel-attested credential.
    ///
    /// Bounded by the identity table's verifier when the credential is
    /// resolved, so a task can never carry an unbounded set. Snapshotted or
    /// inherited exactly like [`primary_gid`](Self::primary_gid); empty for a
    /// record with no attached credential.
    supplementary_gids: Vec<GroupId>,
    /// Maximum the owner's user grant ever allows on this task. Acts as
    /// the *upper bound* for every subsequent operation; nothing in this
    /// module can grow `effective` past this set.
    user_grant: CapabilitySet,
    /// What the binary's manifest asked for (already verified by
    /// [`crate::verify_manifest`]).
    manifest_request: CapabilitySet,
    /// Currently effective set. Always a subset of `user_grant ∩ manifest_request`.
    effective: CapabilitySet,
    /// `true` for the **system principal** — a program the kernel launches
    /// before or outside an authenticated session. Such a task has no
    /// users-db account: its registered manifest is its own ceiling, and
    /// [`Self::user_ceiling`] answers [`None`] so a child it spawns is
    /// bounded by the *child's* manifest, not a fabricated account grant.
    /// Set only by the kernel admit paths through
    /// [`Self::as_system_principal`]; defaults to `false` (a user-session
    /// task whose ceiling is its account grant).
    system_principal: bool,
    /// `true` for a **parser sandbox** process (`SPAWN_FLAG_SANDBOX`,
    /// `docs/src/security/sandbox.md`): a minimum-capability worker whose
    /// grant, manifest request, and effective set are all forced empty and
    /// stay empty for the task's whole life — [`Self::delegate`] and
    /// [`Self::apply_token`] refuse a sandboxed target outright, and the
    /// syscall dispatcher confines the task to its closed sandbox
    /// allow-list. Set only by the kernel spawn-admit path through
    /// [`Self::as_sandboxed`]; defaults to `false`.
    sandboxed: bool,
    /// Kernel-generated process-instance identity, distinct from the
    /// reusable scheduler [`TaskId`]. Defaults to [`ProcId::KERNEL`] —
    /// the sentinel for a schedulable entity that is not a distinct user
    /// process instance (kernel threads, IPC-binder/device-host records).
    /// The two process-admit paths replace it with a minted value through
    /// [`Self::with_proc_id`]; it is set kernel-side and never derived from
    /// any caller-supplied bytes, so a task can neither forge nor influence
    /// it.
    proc_id: ProcId,
    /// Process-instance identity of the task's **parent** — the process
    /// that spawned it — distinct from the parent's reusable numeric id, so
    /// parentage survives PID reuse exactly as [`proc_id`](Self::proc_id)
    /// does for the task itself. Defaults to [`ProcId::KERNEL`], the
    /// sentinel for a kernel-parented task with no distinct user-process
    /// parent (PID 1, the storage bootstrap-floor drivers, kernel threads).
    /// The spawn admit path replaces it with the spawning parent's attested
    /// identity through [`Self::with_parent_proc_id`]; like `proc_id` it is
    /// set kernel-side from the parent's own kernel-held record and never
    /// from caller-supplied bytes, so a task cannot forge or influence its
    /// recorded parentage.
    parent_proc_id: ProcId,
    /// Kernel-attested process name — the resolved executable's basename for
    /// a spawned process, the driver-store path's basename for a spawned
    /// driver, a fixed name for the kernel's own principals (PID 1).
    /// Defaults to [`ProcName::EMPTY`].
    /// The process-admit paths set it through [`Self::with_name`] from
    /// kernel-resolved state, never from caller-supplied bytes, so an audit
    /// consumer may trust it to name the acting process.
    name: ProcName,
    /// Kernel-attested program path — the exact registry or store-bundle
    /// path the `spawn` handler resolved and admitted this process from,
    /// recorded so the reserved self-spawn token (`tairix_abi::SPAWN_SELF`)
    /// can re-spawn *this same program* as a parser-sandbox worker without
    /// trusting any caller-supplied spelling (`argv[0]` is data, not
    /// authority). Empty for a process no spawnable path admitted (kernel
    /// threads, boot principals), which fails a self-spawn closed. Set only
    /// through [`Self::with_spawn_path`] from the kernel's own resolved
    /// path, never from caller-supplied bytes.
    spawn_path: Vec<u8>,
    /// Kernel-attested monotonic timestamp of the task's admission — the
    /// value the Arch HAL monotonic counter (`ticks_now`) read at the instant
    /// the process was admitted. Defaults to `0`, the sentinel for a task
    /// admitted before user-process start tracking runs (PID 1, the storage
    /// bootstrap-floor drivers, kernel threads — the boot principals), which
    /// began at boot. The production process-admit path sets it through
    /// [`Self::with_start_time`] from the kernel's own monotonic clock, never
    /// from caller-supplied bytes, so an audit or origin consumer may trust it
    /// to order and age a process instance, and to distinguish two lifetimes
    /// that reused a numeric id even within one monotonic epoch. It confers no
    /// capability.
    start_time: u64,
    /// Kernel-attested installed-console index backing the task's standard
    /// streams. Defaults to [`ORIGIN_CONSOLE_NONE`], the sentinel for a task
    /// whose streams are not console-backed (a driver process, a kernel
    /// thread). The production process-admit path sets it through
    /// [`Self::with_console`] from the console the kernel itself resolved
    /// for the child's descriptor table — never from caller-supplied bytes —
    /// so a per-console service may trust the origin to place its caller on
    /// a console. It confers no capability.
    console: u64,
    /// Bytes actually transferred by this process's own `fs_read`
    /// (and delegated-read) system calls: the count the secured VFS really
    /// moved, never the length the caller asked for. Monotonic for the
    /// process's lifetime and saturating rather than wrapping.
    ///
    /// `Arc<AtomicU64>` rather than a plain `u64` or a bare `AtomicU64`: the
    /// syscall dispatcher works off a per-call `Clone` of this record (see
    /// the struct docs), so the counter must be a shared cell every clone
    /// of one task's record points at, not a value each clone would carry
    /// its own independent copy of. [`Self::record_bytes_read`] then updates
    /// it through the shared `&TaskCapabilities` every syscall handler
    /// already holds, with no additional lock on the file I/O hot path.
    io_bytes_read: Arc<AtomicU64>,
    /// Bytes actually transferred by this process's own `fs_write` system
    /// calls, mirroring [`Self::io_bytes_read`] in every respect but
    /// direction.
    io_bytes_written: Arc<AtomicU64>,
}

/// Add `delta` to the value `counter` holds, clamping at [`u64::MAX`]
/// rather than wrapping on overflow.
///
/// Ordering is [`Ordering::Relaxed`] throughout: the counter is a pure
/// accounting total with no other memory access it must be ordered
/// against, so the compare-and-swap loop only needs to converge on a
/// single, consistent value, never to publish or observe unrelated state.
fn saturating_fetch_add(counter: &AtomicU64, delta: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(delta);
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

impl TaskCapabilities {
    /// Derive a task's effective capabilities from its user grant and the
    /// verified manifest request.
    ///
    /// The effective set is the intersection of the two inputs. Emits exactly one
    /// [`AuditEvent::TaskCapabilitiesDerived`].
    pub fn derive<S: Sink + ?Sized>(
        task: TaskId,
        owner: UserId,
        user_grant: CapabilitySet,
        manifest_request: CapabilitySet,
        audit: &S,
    ) -> Self {
        let effective = user_grant.intersection(&manifest_request);
        let mut task_buf = [0u8; 16];
        let task_field = format_hex_u64(task.0, &mut task_buf);
        let mut uid_buf = [0u8; 12];
        let uid_field = format_i32(i32::try_from(owner.0).unwrap_or(i32::MAX), &mut uid_buf);
        let mut len_buf = [0u8; 12];
        let len_field = format_i32(
            i32::try_from(effective.len()).unwrap_or(i32::MAX),
            &mut len_buf,
        );
        record(
            audit,
            AuditEvent::TaskCapabilitiesDerived,
            &[
                Field {
                    key: "task",
                    value: tairix_log::FieldValue::Str(task_field),
                },
                Field {
                    key: "uid",
                    value: tairix_log::FieldValue::Str(uid_field),
                },
                Field {
                    key: "caps",
                    value: tairix_log::FieldValue::Str(len_field),
                },
            ],
        );
        Self {
            task,
            owner,
            primary_gid: GroupId::default(),
            supplementary_gids: Vec::new(),
            user_grant,
            manifest_request,
            effective,
            system_principal: false,
            sandboxed: false,
            proc_id: ProcId::KERNEL,
            parent_proc_id: ProcId::KERNEL,
            name: ProcName::EMPTY,
            spawn_path: Vec::new(),
            start_time: 0,
            console: ORIGIN_CONSOLE_NONE,
            io_bytes_read: Arc::new(AtomicU64::new(0)),
            io_bytes_written: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Mark this record as belonging to the **system principal**: a program
    /// the kernel launches before or outside an authenticated session
    /// (PID 1 `init`, the boot services, an in-kernel driver host). The
    /// system principal has no users-db account, so it carries no account
    /// ceiling — its registered manifest *is* its ceiling — and
    /// [`user_ceiling`](Self::user_ceiling) then answers [`None`], making a
    /// child it spawns derive against the *child's own* manifest rather
    /// than inheriting a fabricated account grant.
    ///
    /// Consumed and returned so the process-admit path can set it inline,
    /// mirroring [`Self::with_proc_id`]. Only the kernel's admit sites call
    /// it, and only for a credential the kernel itself minted — never from
    /// any caller-supplied state. It does not alter the effective set.
    #[must_use]
    pub fn as_system_principal(mut self) -> Self {
        self.system_principal = true;
        self
    }

    /// Mark this record as a **parser sandbox** process and force every
    /// capability set empty.
    ///
    /// The emptiness is structural, not caller discipline: whatever grant or
    /// manifest request the record was derived with is discarded here, so a
    /// sandboxed task can never start with — or later re-derive — any
    /// capability. Consumed and returned so the spawn-admit path can set it
    /// inline, mirroring [`Self::as_system_principal`]. Only the kernel's
    /// spawn-admit site calls it, and only for a spawn whose attach block
    /// carried `SPAWN_FLAG_SANDBOX`.
    #[must_use]
    pub fn as_sandboxed(mut self) -> Self {
        self.sandboxed = true;
        self.user_grant = CapabilitySet::EMPTY;
        self.manifest_request = CapabilitySet::EMPTY;
        self.effective = CapabilitySet::EMPTY;
        self
    }

    /// Whether this record belongs to a parser-sandbox process.
    #[must_use]
    pub fn is_sandboxed(&self) -> bool {
        self.sandboxed
    }

    /// Attach the task's kernel-attested group credential (primary group and
    /// supplementary groups) to this record.
    ///
    /// Consumed and returned so the process-admit path can set the credential
    /// inline before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call it,
    /// passing groups resolved from the authoritative identity table (a
    /// spawn-as-user switch) or copied from the spawning parent's own attested
    /// record (inherit) — never a caller-supplied value. The groups are
    /// identity for the filesystem permission model; they confer no
    /// capability. A record with no attached credential keeps gid 0 and an
    /// empty supplementary set.
    #[must_use]
    pub fn with_credential(
        mut self,
        primary_gid: GroupId,
        supplementary_gids: Vec<GroupId>,
    ) -> Self {
        self.primary_gid = primary_gid;
        self.supplementary_gids = supplementary_gids;
        self
    }

    /// The primary group of the task's attested credential.
    #[must_use]
    pub fn primary_gid(&self) -> GroupId {
        self.primary_gid
    }

    /// The supplementary groups of the task's attested credential.
    #[must_use]
    pub fn supplementary_gids(&self) -> &[GroupId] {
        &self.supplementary_gids
    }

    /// Attach a minted process-instance identity to this record.
    ///
    /// Consumed and returned so the process-admit path can set the identity
    /// inline before inserting the record into the [`CapTable`]. Only the
    /// kernel's two process-admit sites call this; every other producer
    /// leaves the [`ProcId::KERNEL`] sentinel.
    #[must_use]
    pub fn with_proc_id(mut self, proc_id: ProcId) -> Self {
        self.proc_id = proc_id;
        self
    }

    /// The task's kernel-generated process-instance identity.
    ///
    /// Returns [`ProcId::KERNEL`] for a record that is not a distinct user
    /// process instance. The value is attested by the kernel — never
    /// caller-supplied — so an audit or origin consumer may trust it.
    #[must_use]
    pub fn proc_id(&self) -> ProcId {
        self.proc_id
    }

    /// Attach the spawning parent's process-instance identity to this record.
    ///
    /// Consumed and returned so the process-admit path can set the parentage
    /// inline before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call it,
    /// passing the parent's *attested* [`proc_id`](Self::proc_id) read from
    /// the parent's own kernel-held record — never a caller-supplied value.
    /// A kernel-parented task (PID 1, the storage bootstrap-floor drivers)
    /// leaves the [`ProcId::KERNEL`] sentinel.
    #[must_use]
    pub fn with_parent_proc_id(mut self, parent_proc_id: ProcId) -> Self {
        self.parent_proc_id = parent_proc_id;
        self
    }

    /// The process-instance identity of the task's parent.
    ///
    /// Returns [`ProcId::KERNEL`] for a kernel-parented task (no distinct
    /// user-process parent). The value is attested by the kernel — read from
    /// the parent's own kernel-held record, never caller-supplied — so an
    /// audit or origin consumer may trust it to attribute a task to the exact
    /// parent instance that spawned it, even across PID reuse.
    #[must_use]
    pub fn parent_proc_id(&self) -> ProcId {
        self.parent_proc_id
    }

    /// Attach the kernel-attested process name to this record.
    ///
    /// Consumed and returned so the process-admit path can set the name
    /// inline before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call it,
    /// passing a name derived from kernel-resolved state (the executable's
    /// basename, or a fixed name for a kernel principal) — never a
    /// caller-supplied value. A record with no attested name keeps
    /// [`ProcName::EMPTY`].
    #[must_use]
    pub fn with_name(mut self, name: ProcName) -> Self {
        self.name = name;
        self
    }

    /// Attach the kernel-resolved program path this process was admitted
    /// from, consumed and returned like [`Self::with_name`]. Only the
    /// kernel's process-admit sites call it, passing the exact registry or
    /// store-bundle path the spawn handler itself resolved — never a
    /// caller-supplied value — so the reserved self-spawn token can trust
    /// it to name this process's own program. A record never given one
    /// keeps the empty "no spawnable path" sentinel, which fails a
    /// self-spawn closed.
    #[must_use]
    pub fn with_spawn_path(mut self, spawn_path: Vec<u8>) -> Self {
        self.spawn_path = spawn_path;
        self
    }

    /// The kernel-resolved program path this process was admitted from
    /// (empty if no spawnable path admitted it).
    ///
    /// Read from this record's own kernel-held state — never
    /// caller-supplied — so the spawn handler may trust it to re-spawn
    /// this process's own program.
    #[must_use]
    pub fn spawn_path(&self) -> &[u8] {
        &self.spawn_path
    }

    /// Attach the kernel-attested monotonic admission timestamp to this
    /// record.
    ///
    /// Consumed and returned so the process-admit path can set it inline
    /// before inserting the record into the [`CapTable`], mirroring
    /// [`Self::with_proc_id`]. Only the kernel's process-admit path calls it,
    /// passing the value the Arch HAL monotonic counter read at admission —
    /// never a caller-supplied value. A record with no attested start time
    /// keeps the `0` boot/kernel-principal sentinel.
    #[must_use]
    pub fn with_start_time(mut self, start_time: u64) -> Self {
        self.start_time = start_time;
        self
    }

    /// Attach the kernel-resolved installed-console index backing this
    /// task's standard streams, consumed and returned like
    /// [`Self::with_proc_id`]. Only the kernel's process-admit sites call
    /// it, passing the console the spawn path itself resolved for the
    /// child's descriptor table — never a caller-supplied value. A record
    /// never given one keeps the [`ORIGIN_CONSOLE_NONE`] "not
    /// console-backed" sentinel.
    #[must_use]
    pub fn with_console(mut self, console: u64) -> Self {
        self.console = console;
        self
    }

    /// The task's kernel-attested installed-console index, or
    /// [`ORIGIN_CONSOLE_NONE`] when its standard streams are not
    /// console-backed.
    #[must_use]
    pub fn console(&self) -> u64 {
        self.console
    }

    /// Cumulative bytes this process has actually read through its own
    /// `fs_read` (and delegated-read) system calls.
    ///
    /// Zero for a task that has never read a file. Consumers derive a rate
    /// from the delta between two samples, exactly as for
    /// [`Self::start_time`]-relative CPU time.
    #[must_use]
    pub fn io_bytes_read(&self) -> u64 {
        self.io_bytes_read.load(Ordering::Relaxed)
    }

    /// Cumulative bytes this process has actually written through its own
    /// `fs_write` system calls.
    ///
    /// Zero for a task that has never written a file. Mirrors
    /// [`Self::io_bytes_read`] in every respect but direction.
    #[must_use]
    pub fn io_bytes_written(&self) -> u64 {
        self.io_bytes_written.load(Ordering::Relaxed)
    }

    /// Attribute `n` more bytes transferred by this process's own
    /// `fs_read` (or delegated-read) call to its running total.
    ///
    /// Called once, from the one shared descriptor-read path in
    /// `kernel/core`, with the byte count the secured VFS actually moved —
    /// never the length the caller requested. Saturates at [`u64::MAX`]
    /// rather than wrapping. Takes `&self` (not `&mut self`): the counter
    /// is a plain [`Ordering::Relaxed`] atomic with no ordering relationship
    /// to any other memory, so every concurrent reader of this task's
    /// record — including the syscall path itself — can update it without
    /// taking a write lock on the surrounding [`CapTable`].
    pub fn record_bytes_read(&self, n: u64) {
        saturating_fetch_add(&self.io_bytes_read, n);
    }

    /// Attribute `n` more bytes transferred by this process's own
    /// `fs_write` call to its running total. Mirrors
    /// [`Self::record_bytes_read`] in every respect but direction.
    pub fn record_bytes_written(&self, n: u64) {
        saturating_fetch_add(&self.io_bytes_written, n);
    }

    /// Continue `previous`'s I/O totals in this record instead of starting
    /// fresh ones.
    ///
    /// A spawned child's record is installed twice under one task id — the
    /// empty-set placeholder at admit, then the effective set once its image
    /// verifies — and each derivation mints its own counter cells. Without
    /// this, the second install would silently restart the task's totals at
    /// zero mid-life, so a byte the loading slice moved would vanish and the
    /// counters would not be monotonic for the process's lifetime as their
    /// contract promises. [`CapTable::insert`] applies it at the one place a
    /// record can replace a live entry, so the continuity holds for every
    /// install path without any caller having to remember it, and only ever
    /// between records of the *same* task (the registry keys on
    /// [`Self::task`], which no setter can change after derivation).
    fn adopt_io_counters(&mut self, previous: &Self) {
        self.io_bytes_read = Arc::clone(&previous.io_bytes_read);
        self.io_bytes_written = Arc::clone(&previous.io_bytes_written);
    }

    /// The task's kernel-attested monotonic admission timestamp.
    ///
    /// Returns `0` for a boot or kernel principal admitted before
    /// user-process start tracking runs. The value is attested by the kernel
    /// — read from the monotonic clock at admission, never caller-supplied —
    /// so an audit or origin consumer may trust it to order and age the
    /// process instance.
    #[must_use]
    pub fn start_time(&self) -> u64 {
        self.start_time
    }

    /// The task's kernel-attested process name (empty if none was attested).
    ///
    /// Read from this record's own kernel-held state — never caller-supplied
    /// — so an audit consumer may trust it to name the acting process.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Produce the kernel-attested [`Origin`] of this task.
    ///
    /// Every field is read from this record's own kernel-held state — never
    /// from anything a caller supplied — so the result is authoritative: a
    /// task can neither forge another principal's origin nor inflate its own.
    /// The trust domain is [`TrustDomain::Kernel`] for the
    /// [`ProcId::KERNEL`] sentinel (kernel threads and in-kernel binder /
    /// device-host records) and [`TrustDomain::User`] for a minted process
    /// instance. The capability summary is the effective set's wire image —
    /// the non-secret membership bitmap, carrying no capability *tokens*.
    #[must_use]
    pub fn attest_origin(&self) -> Origin {
        let trust_domain = if self.proc_id.is_kernel() {
            TrustDomain::Kernel
        } else {
            TrustDomain::User
        };
        let capabilities = CapabilitySummary::from_raw(self.effective.to_le_bytes());
        Origin::new(
            trust_domain,
            self.owner.0,
            self.primary_gid.0,
            self.task.0,
            self.proc_id,
            capabilities,
            self.console,
        )
    }

    /// Currently effective capability set.
    #[must_use]
    pub fn effective(&self) -> &CapabilitySet {
        &self.effective
    }

    /// User grant the task is bounded by.
    #[must_use]
    pub fn user_grant(&self) -> &CapabilitySet {
        &self.user_grant
    }

    /// The account ceiling this task hands to a child it spawns, or
    /// [`None`] for the system principal.
    ///
    /// A user-session task passes its account's grant on: an inherit-spawned
    /// child derives `its own manifest ∩ this ceiling`, so delegation only
    /// narrows. The system principal (see
    /// [`as_system_principal`](Self::as_system_principal)) has no users-db
    /// account and answers [`None`]: each system program's registered
    /// manifest is its own ceiling, so a boot service spawned by PID 1 is
    /// bounded by *its* manifest, not by PID 1's.
    #[must_use]
    pub fn user_ceiling(&self) -> Option<&CapabilitySet> {
        if self.system_principal {
            None
        } else {
            Some(&self.user_grant)
        }
    }

    /// Original manifest request.
    #[must_use]
    pub fn manifest_request(&self) -> &CapabilitySet {
        &self.manifest_request
    }

    /// Owning user identifier.
    #[must_use]
    pub fn owner(&self) -> UserId {
        self.owner
    }

    /// Task identifier carried in audit records.
    #[must_use]
    pub fn task(&self) -> TaskId {
        self.task
    }

    /// `true` if the task's effective set holds `cap`.
    ///
    /// This is the per-syscall predicate every privileged operation must
    /// consult; it never emits audit traffic itself so callers can cheaply
    /// probe membership without filling the log. The *decision* an
    /// IPC/syscall site takes after consulting this predicate is the
    /// thing recorded — that lives in the dispatch layer (Stage 2.5).
    #[must_use]
    pub fn has(&self, cap: tairix_abi::CapabilityId) -> bool {
        self.effective.contains(cap)
    }

    /// Install a delegated subset on the task.
    ///
    /// Returns [`Errno::PermissionDenied`] (and emits
    /// [`AuditEvent::TaskCapabilitiesDelegateWiden`]) for a sandboxed
    /// target — a sandbox may never be handed capabilities — and
    /// [`Errno::DelegationWiden`] (same audit event) if `requested`
    /// would widen the current effective set. On success the effective
    /// set is **replaced** with the delegated subset and one
    /// [`AuditEvent::TaskCapabilitiesDelegated`] is emitted. The
    /// upstream `user_grant` and `manifest_request` are not touched, so
    /// a later [`Self::derive`]-equivalent refresh is still possible by
    /// re-intersecting them.
    pub fn delegate<S: Sink + ?Sized>(
        &mut self,
        requested: &CapabilitySet,
        audit: &S,
    ) -> Result<(), Errno> {
        if self.sandboxed {
            // A sandbox holds nothing and may never be handed anything —
            // even the empty set is refused so the answer never depends on
            // the payload.
            let mut buf = [0u8; 16];
            record(
                audit,
                AuditEvent::TaskCapabilitiesDelegateWiden,
                &[Field {
                    key: "task",
                    value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
                }],
            );
            return Err(Errno::PermissionDenied);
        }
        match self.effective.delegate(requested) {
            Ok(narrowed) => {
                self.effective = narrowed;
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegated,
                    &[Field {
                        key: "task",
                        value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
                    }],
                );
                Ok(())
            }
            Err(err) => {
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegateWiden,
                    &[Field {
                        key: "task",
                        value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
                    }],
                );
                Err(err)
            }
        }
    }

    /// Apply a signed [`CapabilityToken`] to this task.
    ///
    /// The token is verified against `authority`, the current effective
    /// set (which acts as the parent), and **this task's id as the
    /// subject** — a token minted for another task is refused here, so a
    /// stolen or misdirected token cannot be replayed onto an unrelated
    /// principal. On success the task's effective set
    /// is replaced with the token's payload (always a subset of the
    /// current set by [`CapabilityToken::verify`]'s own invariant).
    /// Failure modes are mapped to the same audit event as a
    /// direct [`Self::delegate`]: a forged or stale token is *security*
    /// information, not crypto trivia, and the audit trail records the
    /// security decision rather than which validation step failed
    /// (matching the rationale in `lib/caps/token.rs`).
    ///
    /// # Errors
    ///
    /// Returns [`Errno::PermissionDenied`] for a sandboxed target before
    /// any verification (a sandbox may never be handed capabilities);
    /// otherwise forwards [`CapabilityToken::verify`]'s error verbatim.
    /// Either failure emits [`AuditEvent::TaskCapabilitiesDelegateWiden`].
    pub fn apply_token<S: Sink + ?Sized>(
        &mut self,
        token: &CapabilityToken,
        authority: &Ed25519PublicKey,
        epoch: RevocationEpoch,
        audit: &S,
    ) -> Result<(), Errno> {
        if self.sandboxed {
            // Same rule as `delegate`: no token — however well signed — may
            // ever land capabilities on a sandbox.
            let mut buf = [0u8; 16];
            record(
                audit,
                AuditEvent::TaskCapabilitiesDelegateWiden,
                &[Field {
                    key: "task",
                    value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
                }],
            );
            return Err(Errno::PermissionDenied);
        }
        match token.verify(authority, &self.effective, epoch, self.task.0) {
            Ok(()) => {
                self.effective = token.caps;
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegated,
                    &[Field {
                        key: "task",
                        value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
                    }],
                );
                Ok(())
            }
            Err(err) => {
                let mut buf = [0u8; 16];
                record(
                    audit,
                    AuditEvent::TaskCapabilitiesDelegateWiden,
                    &[Field {
                        key: "task",
                        value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut buf)),
                    }],
                );
                Err(err)
            }
        }
    }

    /// Revoke a single capability from the task.
    ///
    /// Idempotent; if the capability was not held, the call is still
    /// audited (the *attempt* is the security event) but `false` is
    /// returned. Emits one [`AuditEvent::TaskCapabilitiesRevoked`].
    pub fn revoke<S: Sink + ?Sized>(&mut self, cap: tairix_abi::CapabilityId, audit: &S) -> bool {
        let was_present = self.effective.revoke(cap);
        let mut task_buf = [0u8; 16];
        let mut cap_buf = [0u8; 12];
        record(
            audit,
            AuditEvent::TaskCapabilitiesRevoked,
            &[
                Field {
                    key: "task",
                    value: tairix_log::FieldValue::Str(format_hex_u64(self.task.0, &mut task_buf)),
                },
                Field {
                    key: "cap",
                    value: tairix_log::FieldValue::Str(format_i32(
                        i32::from(cap.as_u16()),
                        &mut cap_buf,
                    )),
                },
            ],
        );
        was_present
    }
}

/// The task's effective set, read through the ABI-level query seam so a
/// capability rule defined once (the spawn-mode gate in `kernel/core`) can be
/// applied to a live caller without naming this type.
impl tairix_abi::CapabilityQuery for TaskCapabilities {
    fn holds(&self, cap: tairix_abi::CapabilityId) -> bool {
        self.has(cap)
    }
}

/// Per-task capability registry — the `TaskId → TaskCapabilities` lookup
/// the syscall dispatcher consults to recover a caller's effective
/// capability set after the per-CPU current-task slot
/// (`Scheduler::current_task`, Stage 2.7 follow-up (f1)) has named the
/// caller.
///
/// The registry owns the per-task records: callers pass a freshly
/// derived [`TaskCapabilities`] in via [`Self::insert`] (at task
/// creation, after `TaskCapabilities::derive` has audited the
/// intersection) and pull it back out via [`Self::remove`] when the
/// task exits. Lookups go through [`Self::caps_for`].
///
/// # Synchronisation
///
/// `CapTable` carries no interior mutability. The owning scope —
/// `KernelState` in `kernel/core::init` — is responsible for whatever
/// lock policy is appropriate (a reader-preferring `RwLock` mirrors
/// what `Scheduler::tasks` already uses for the same shape of access
/// pattern: many concurrent syscall-context readers, occasional
/// task-creation writers). Pushing the lock outside the type keeps
/// the borrow `caps_for(&self, _) -> Option<&TaskCapabilities>`
/// natural and lets `KernelState` compose this registry with the
/// scheduler under a single lock-ordering policy
/// (no hidden global state, no interface
/// creep).
///
/// # No ambient authority
///
/// Inserts never widen capabilities. The caller-supplied
/// [`TaskCapabilities`] has already passed through the
/// intersection-on-derive invariant in [`TaskCapabilities::derive`];
/// the registry simply stores it. There is no "make this task root"
/// shortcut and no implicit grant on lookup.
#[derive(Debug, Default)]
pub struct CapTable {
    entries: BTreeMap<TaskId, TaskCapabilities>,
}

impl CapTable {
    /// Construct an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Register a task's capabilities. The [`TaskId`] is taken from the
    /// record (`caps.task()`); callers do not pass it separately so the
    /// id and the body cannot diverge.
    ///
    /// Returns the previously-registered record, if any. A non-`None`
    /// return is an unusual condition — task ids are not recycled
    /// within a single scheduler instance (see
    /// `kernel/sched::scheduler` invariants) — but is surfaced rather
    /// than silently dropped so callers can audit / refuse it. The one
    /// routine replacement is a spawned child's effective record taking
    /// over from its admit-time placeholder; because the entry is keyed on
    /// the record's own [`TaskCapabilities::task`], a replacement always
    /// concerns that same task, so the incoming record continues the
    /// outgoing one's per-process I/O totals rather than restarting them.
    pub fn insert(&mut self, mut caps: TaskCapabilities) -> Option<TaskCapabilities> {
        let task = caps.task();
        if let Some(previous) = self.entries.get(&task) {
            caps.adopt_io_counters(previous);
        }
        self.entries.insert(task, caps)
    }

    /// Borrow the registry entry for `task` immutably.
    ///
    /// Used by the syscall dispatcher's `cap_query` / `cap_revoke`
    /// paths: the caller's effective set is read but not mutated.
    #[must_use]
    pub fn caps_for(&self, task: TaskId) -> Option<&TaskCapabilities> {
        self.entries.get(&task)
    }

    /// Borrow the registry entry for `task` mutably. Used by the
    /// syscall dispatcher's `cap_delegate` / `cap_revoke` paths,
    /// which call `TaskCapabilities::{delegate,revoke,apply_token}`
    /// directly on the borrowed record.
    pub fn caps_for_mut(&mut self, task: TaskId) -> Option<&mut TaskCapabilities> {
        self.entries.get_mut(&task)
    }

    /// Remove the registry entry for `task`, returning it.
    ///
    /// Called by the syscall dispatcher's `exit` handler after
    /// `Scheduler::exit` has flipped the task's state; the returned
    /// record can be inspected by tests, then dropped. Returning the
    /// record (instead of swallowing it) lets the caller zero out any
    /// capability material in line with the kernel allocator's
    /// "zero-on-free for credential-holding memory" requirement.
    pub fn remove(&mut self, task: TaskId) -> Option<TaskCapabilities> {
        self.entries.remove(&task)
    }

    /// Iterate every registered task's attested capability record, in
    /// ascending [`TaskId`] order.
    ///
    /// The order is the `BTreeMap` key order, so it is stable across calls
    /// as long as the registry is unchanged — letting the System Information
    /// introspection source page a consistent process list. Every field of
    /// each [`TaskCapabilities`] is kernel-attested (minted at admit), so a
    /// consumer building a process view reads authoritative identity, never
    /// a caller claim.
    pub fn iter(&self) -> impl Iterator<Item = &TaskCapabilities> {
        self.entries.values()
    }

    /// Number of tasks currently registered. Primarily for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no task is currently registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use ed25519_dalek::{Signer, SigningKey};
    use tairix_abi::{CapabilityId, ABI_VERSION_CURRENT};
    use tairix_crypto::Ed25519Signature;

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    #[test]
    fn derive_is_intersection() {
        let user_grant = caps_of(&[
            CapabilityId::FS_MOUNT,
            CapabilityId::NET_RAW,
            CapabilityId::AUDIT_READ,
        ]);
        let manifest_request = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::DRV_LOAD]);
        let sink = RecordingSink::new();
        let t =
            TaskCapabilities::derive(TaskId(1), UserId(1000), user_grant, manifest_request, &sink);
        // Intersection: only FS_MOUNT is in both.
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::NET_RAW));
        assert!(!t.has(CapabilityId::DRV_LOAD));
        assert_eq!(sink.ids(), [AuditEvent::TaskCapabilitiesDerived.id().0]);
    }

    #[test]
    fn proc_id_defaults_to_kernel_sentinel_and_with_proc_id_attaches() {
        use tairix_abi::ProcId;
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(7), UserId(1000), grant, grant, &sink);
        // A freshly-derived record carries the kernel sentinel: it is not a
        // distinct user process instance until a minted id is attached.
        assert_eq!(base.proc_id(), ProcId::KERNEL);
        assert!(base.proc_id().is_kernel());

        let minted = ProcId::from_raw([0xAB; 16]);
        let admitted = base.with_proc_id(minted);
        assert_eq!(admitted.proc_id(), minted);
        assert!(!admitted.proc_id().is_kernel());
        // Attaching the identity changes nothing about the capability set.
        assert!(admitted.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn parent_proc_id_defaults_to_kernel_sentinel_and_with_parent_attaches() {
        use tairix_abi::ProcId;
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(9), UserId(1000), grant, grant, &sink);
        // A freshly-derived record is kernel-parented until the admit path
        // attaches the spawning parent's attested identity.
        assert_eq!(base.parent_proc_id(), ProcId::KERNEL);
        assert!(base.parent_proc_id().is_kernel());

        let parent = ProcId::from_raw([0xC3; 16]);
        let child = ProcId::from_raw([0xAB; 16]);
        let admitted = base.with_proc_id(child).with_parent_proc_id(parent);
        // The task's own identity and its parentage are independent fields:
        // attaching one never disturbs the other or the capability set.
        assert_eq!(admitted.proc_id(), child);
        assert_eq!(admitted.parent_proc_id(), parent);
        assert!(!admitted.parent_proc_id().is_kernel());
        assert!(admitted.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn proc_name_defaults_empty_and_with_name_attaches() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(11), UserId(1000), grant, grant, &sink);
        // A freshly-derived record has no attested name.
        assert_eq!(base.name(), "");

        let named = base.with_name(ProcName::from_bytes_truncating(b"sysinfod"));
        assert_eq!(named.name(), "sysinfod");
        // Attaching the name changes nothing about the capability set.
        assert!(named.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn start_time_defaults_to_boot_sentinel_and_with_start_time_attaches() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(15), UserId(1000), grant, grant, &sink);
        // A freshly-derived record carries the `0` boot/kernel-principal
        // sentinel until an admission timestamp is attested.
        assert_eq!(base.start_time(), 0);

        let started = base.with_start_time(0x1234_5678_9abc_def0);
        assert_eq!(started.start_time(), 0x1234_5678_9abc_def0);
        // Attaching the start time is identity only: it changes nothing about
        // the capability set, and it is independent of the other attested
        // fields (proc_id / name stay at their defaults).
        assert!(started.has(CapabilityId::FS_MOUNT));
        assert!(started.proc_id().is_kernel());
        assert_eq!(started.name(), "");
    }

    #[test]
    fn proc_name_bounds_to_largest_valid_utf8_prefix() {
        // The empty name is empty and its default matches.
        assert_eq!(ProcName::EMPTY.as_str(), "");
        assert!(ProcName::EMPTY.is_empty());
        assert_eq!(ProcName::default(), ProcName::EMPTY);

        // An over-long input is bounded to PROC_NAME_MAX bytes.
        let long = [b'a'; PROC_NAME_MAX + 8];
        let name = ProcName::from_bytes_truncating(&long);
        assert_eq!(name.as_str().len(), PROC_NAME_MAX);
        assert!(name.as_str().bytes().all(|b| b == b'a'));

        // A cut that would land inside a multi-byte code point keeps only
        // the largest valid prefix, so `as_str` never observes invalid UTF-8.
        // "é" (0xC3 0xA9) repeated fills 2 bytes each; PROC_NAME_MAX (32) is
        // even, so the boundary lands cleanly, but a trailing lone lead byte
        // must be dropped:
        let mut bytes = alloc::vec![0u8; PROC_NAME_MAX];
        for chunk in bytes.chunks_mut(2) {
            chunk[0] = 0xC3;
            if chunk.len() > 1 {
                chunk[1] = 0xA9;
            }
        }
        // Append a stray lead byte past the cap; it is dropped with the cap.
        bytes.push(0xC3);
        let accented = ProcName::from_bytes_truncating(&bytes);
        // Valid UTF-8 by construction, and every code point is "é".
        assert!(accented.as_str().chars().all(|c| c == 'é'));
        assert_eq!(accented.as_str().len(), PROC_NAME_MAX);

        // A trailing lone lead byte within the cap is dropped, not stored.
        let truncated = ProcName::from_bytes_truncating(&[b'h', b'i', 0xC3]);
        assert_eq!(truncated.as_str(), "hi");
    }

    #[test]
    fn proc_name_from_path_keeps_the_final_non_empty_component() {
        // The ordinary case: an absolute path names its final component.
        let name = ProcName::from_path(b"/System/Drivers/input/usb_kbd");
        assert_eq!(name.as_str(), "usb_kbd");

        // A trailing slash never attests an empty name: the last non-empty
        // component still names the process.
        let trailing = ProcName::from_path(b"/System/Drivers/input/usb_kbd/");
        assert_eq!(trailing.as_str(), "usb_kbd");

        // A bare name (no separator) is its own basename.
        let bare = ProcName::from_path(b"virtio_blk");
        assert_eq!(bare.as_str(), "virtio_blk");

        // A path with no non-empty component keeps the whole path bytes so
        // a process listing always has something truthful to display.
        assert_eq!(ProcName::from_path(b"/").as_str(), "/");
        assert_eq!(ProcName::from_path(b"//").as_str(), "//");
        assert_eq!(ProcName::from_path(b"").as_str(), "");

        // The basename is bounded exactly like any other attested name.
        let mut long = alloc::vec![b'/'];
        long.extend_from_slice(&[b'x'; PROC_NAME_MAX + 8]);
        let bounded = ProcName::from_path(&long);
        assert_eq!(bounded.as_str().len(), PROC_NAME_MAX);
        assert!(bounded.as_str().bytes().all(|b| b == b'x'));
    }

    #[test]
    fn proc_name_from_path_names_a_bundle_by_its_directory_stem_never_run() {
        // Regression: a driver-store bundle's entry point is the generic
        // `Run` leaf every bundle shares, so a process listing showed `Run`
        // for every autoloaded driver. The owning bundle directory names
        // the process instead.
        let driver = ProcName::from_path(b"/System/Drivers/input/usb_kbd/Run");
        assert_eq!(driver.as_str(), "usb_kbd");

        // An application bundle's `.app` suffix is stripped: the stem is
        // the command/program name, matching the spawn syscall's bundle
        // naming.
        let app = ProcName::from_path(b"/Apps/Example.app/Run");
        assert_eq!(app.as_str(), "Example");
        let store = ProcName::from_path(b"/System/Commands/ps.app/Run");
        assert_eq!(store.as_str(), "ps");

        // Empty components never hide the owning directory.
        let doubled = ProcName::from_path(b"/System/Drivers//input/usb_kbd//Run/");
        assert_eq!(doubled.as_str(), "usb_kbd");

        // A directory named exactly `.app` has an empty stem; the suffix is
        // then kept rather than attesting an empty name.
        let bare_suffix = ProcName::from_path(b"/Apps/.app/Run");
        assert_eq!(bare_suffix.as_str(), ".app");

        // A `Run` with no owning directory keeps the whole path bytes so
        // the listing still shows something truthful.
        assert_eq!(ProcName::from_path(b"Run").as_str(), "Run");
        assert_eq!(ProcName::from_path(b"/Run").as_str(), "/Run");
    }

    #[test]
    fn credential_defaults_empty_and_with_credential_attaches() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let base = TaskCapabilities::derive(TaskId(13), UserId(1000), grant, grant, &sink);
        // A freshly-derived record carries the system group and no
        // supplementary groups until a credential is attached.
        assert_eq!(base.primary_gid(), GroupId::default());
        assert!(base.supplementary_gids().is_empty());

        let cred = base
            .clone()
            .with_credential(GroupId(50), alloc::vec![GroupId(60), GroupId(70)]);
        assert_eq!(cred.primary_gid(), GroupId(50));
        assert_eq!(cred.supplementary_gids(), &[GroupId(60), GroupId(70)]);
        // Attaching the credential changes nothing about the capability set.
        assert!(cred.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn attest_origin_carries_the_attested_primary_gid() {
        use tairix_abi::ProcId;
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let proc = TaskCapabilities::derive(TaskId(44), UserId(1000), grant, grant, &sink)
            .with_proc_id(ProcId::from_raw([0x5A; 16]))
            .with_credential(GroupId(77), alloc::vec![]);
        assert_eq!(proc.attest_origin().gid(), 77);
    }

    #[test]
    fn attest_origin_is_built_from_kernel_state() {
        use tairix_abi::{ProcId, TrustDomain};
        let grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        // A kernel-domain record (no minted proc_id) attests as Kernel.
        let kernel_task = TaskCapabilities::derive(TaskId(3), UserId(0), grant, grant, &sink);
        let kernel_origin = kernel_task.attest_origin();
        assert_eq!(kernel_origin.trust_domain(), TrustDomain::Kernel);
        assert!(kernel_origin.proc_id().is_kernel());

        // A minted process instance attests as User, carrying its own uid,
        // pid, proc_id, and a capability summary mirroring its effective set.
        let minted = ProcId::from_raw([0x5A; 16]);
        let proc = TaskCapabilities::derive(TaskId(42), UserId(1000), grant, grant, &sink)
            .with_proc_id(minted);
        let origin = proc.attest_origin();
        assert_eq!(origin.trust_domain(), TrustDomain::User);
        assert_eq!(origin.uid(), 1000);
        assert_eq!(origin.pid(), 42);
        assert_eq!(origin.proc_id(), minted);
        assert!(origin.capabilities().holds_cap(CapabilityId::FS_MOUNT));
        assert!(origin
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));
        assert!(!origin.capabilities().holds_cap(CapabilityId::NET_RAW));
        // The summary is exactly the effective set's wire image.
        assert_eq!(
            origin.capabilities().as_bytes(),
            &proc.effective().to_le_bytes()
        );
    }

    #[test]
    fn delegate_subset_succeeds_and_replaces_effective() {
        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let manifest_request = user_grant; // identical → effective == both.
        let sink = RecordingSink::new();
        let mut t =
            TaskCapabilities::derive(TaskId(2), UserId(1), user_grant, manifest_request, &sink);
        let narrower = caps_of(&[CapabilityId::FS_MOUNT]);
        assert_eq!(t.delegate(&narrower, &sink), Ok(()));
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::NET_RAW));
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesDelegated.id().0,
            ]
        );
    }

    #[test]
    fn delegate_widening_is_refused_with_audit() {
        let user_grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let manifest_request = user_grant;
        let sink = RecordingSink::new();
        let mut t =
            TaskCapabilities::derive(TaskId(3), UserId(1), user_grant, manifest_request, &sink);
        let wider = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::DRV_KERNEL]);
        assert_eq!(t.delegate(&wider, &sink), Err(Errno::DelegationWiden));
        // The effective set is unchanged.
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::DRV_KERNEL));
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesDelegateWiden.id().0,
            ]
        );
    }

    #[test]
    fn revoke_removes_capability_and_returns_previous_state() {
        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(4), UserId(1), user_grant, user_grant, &sink);
        assert!(t.revoke(CapabilityId::FS_MOUNT, &sink));
        assert!(!t.has(CapabilityId::FS_MOUNT));
        // Revoking again is idempotent (returns false) but still
        // produces an audit record.
        assert!(!t.revoke(CapabilityId::FS_MOUNT, &sink));
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesRevoked.id().0,
                AuditEvent::TaskCapabilitiesRevoked.id().0,
            ]
        );
    }

    #[test]
    fn token_application_accepts_signed_subset() {
        let signing = SigningKey::from_bytes(&[0x11; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();

        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::AUDIT_READ]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(5), UserId(1), user_grant, user_grant, &sink);

        let epoch = RevocationEpoch(3);
        let narrowed = caps_of(&[CapabilityId::FS_MOUNT]);
        let body =
            CapabilityToken::signing_input(ABI_VERSION_CURRENT, t.task().0, epoch, &narrowed);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: t.task().0,
            epoch,
            caps: narrowed,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(t.apply_token(&token, &authority, epoch, &sink), Ok(()));
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(!t.has(CapabilityId::AUDIT_READ));
    }

    #[test]
    fn as_sandboxed_forces_every_capability_set_empty() {
        // Whatever the record was derived with, marking it sandboxed strips
        // the grant, the manifest request, and the effective set — the
        // emptiness is structural, so nothing can later re-derive from the
        // discarded sets.
        let grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let sink = RecordingSink::new();
        let t =
            TaskCapabilities::derive(TaskId(11), UserId(1000), grant, grant, &sink).as_sandboxed();
        assert!(t.is_sandboxed());
        assert!(t.effective().is_empty());
        assert!(t.user_grant().is_empty());
        assert!(t.manifest_request().is_empty());
        assert!(!t.has(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn delegate_refuses_a_sandboxed_target() {
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(
            TaskId(12),
            UserId(1000),
            caps_of(&[CapabilityId::FS_MOUNT]),
            caps_of(&[CapabilityId::FS_MOUNT]),
            &sink,
        )
        .as_sandboxed();
        // Even the empty set is refused: the answer never depends on the
        // payload, and the attempt is audited as a widening attempt.
        assert_eq!(
            t.delegate(&CapabilitySet::empty(), &sink),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            t.delegate(&caps_of(&[CapabilityId::FS_MOUNT]), &sink),
            Err(Errno::PermissionDenied)
        );
        assert!(t.effective().is_empty());
        assert_eq!(
            sink.ids(),
            [
                AuditEvent::TaskCapabilitiesDerived.id().0,
                AuditEvent::TaskCapabilitiesDelegateWiden.id().0,
                AuditEvent::TaskCapabilitiesDelegateWiden.id().0,
            ]
        );
    }

    #[test]
    fn apply_token_refuses_a_sandboxed_target() {
        // A correctly-signed, current-epoch token whose payload is even the
        // empty set is refused on a sandboxed record before verification.
        let signing = SigningKey::from_bytes(&[0x44; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(
            TaskId(13),
            UserId(1000),
            caps_of(&[CapabilityId::FS_MOUNT]),
            caps_of(&[CapabilityId::FS_MOUNT]),
            &sink,
        )
        .as_sandboxed();
        let epoch = RevocationEpoch(1);
        let payload = CapabilitySet::empty();
        let body = CapabilityToken::signing_input(ABI_VERSION_CURRENT, t.task().0, epoch, &payload);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: t.task().0,
            epoch,
            caps: payload,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(
            t.apply_token(&token, &authority, epoch, &sink),
            Err(Errno::PermissionDenied)
        );
        assert!(t.effective().is_empty());
    }

    #[test]
    fn token_for_another_task_is_refused() {
        // A correctly-signed, current-epoch, subset token issued to a
        // *different* task must not apply here: binding to the subject
        // forecloses replaying a stolen token onto another principal. The effective set must be left untouched.
        let signing = SigningKey::from_bytes(&[0x33; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();

        let user_grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::AUDIT_READ]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(9), UserId(1), user_grant, user_grant, &sink);

        let epoch = RevocationEpoch(3);
        let narrowed = caps_of(&[CapabilityId::FS_MOUNT]);
        // Sign the token for some other task, not `t`.
        let other_subject = t.task().0 ^ 0x1;
        let body =
            CapabilityToken::signing_input(ABI_VERSION_CURRENT, other_subject, epoch, &narrowed);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: other_subject,
            epoch,
            caps: narrowed,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(
            t.apply_token(&token, &authority, epoch, &sink),
            Err(Errno::NotFound),
        );
        // The task keeps its full grant; the foreign token changed nothing.
        assert!(t.has(CapabilityId::FS_MOUNT));
        assert!(t.has(CapabilityId::AUDIT_READ));
        assert!(sink
            .ids()
            .contains(&AuditEvent::TaskCapabilitiesDelegateWiden.id().0));
    }

    #[test]
    fn token_with_revoked_epoch_is_refused() {
        let signing = SigningKey::from_bytes(&[0x22; 32]);
        let authority = Ed25519PublicKey::from_bytes(signing.verifying_key().as_bytes()).unwrap();

        let user_grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(6), UserId(1), user_grant, user_grant, &sink);

        // Sign for epoch 1 but verify under epoch 2 — mass revocation.
        let issued_at = RevocationEpoch(1);
        let current = RevocationEpoch(2);
        let body =
            CapabilityToken::signing_input(ABI_VERSION_CURRENT, t.task().0, issued_at, &user_grant);
        let sig = signing.sign(&body);
        let token = CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject: t.task().0,
            epoch: issued_at,
            caps: user_grant,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        };
        assert_eq!(
            t.apply_token(&token, &authority, current, &sink),
            Err(Errno::NotFound),
        );
        // Audit records the refusal under the delegation-widen event id
        // (single failure path; see docstring for rationale).
        assert!(sink
            .ids()
            .contains(&AuditEvent::TaskCapabilitiesDelegateWiden.id().0));
    }

    #[test]
    fn uid_zero_gets_no_extra_powers() {
        // A uid==0 task with an empty user grant ends up with an empty
        // effective set, even when the manifest requests the universe.
        let manifest_request = caps_of(&[
            CapabilityId::FS_MOUNT,
            CapabilityId::DRV_KERNEL,
            CapabilityId::USER_ADMIN,
        ]);
        let sink = RecordingSink::new();
        let t = TaskCapabilities::derive(
            TaskId(7),
            UserId(0),
            CapabilitySet::empty(), // ambient powers? no.
            manifest_request,
            &sink,
        );
        assert!(t.effective().is_empty());
    }

    #[test]
    fn io_counters_start_at_zero_and_add_the_bytes_moved() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let t = TaskCapabilities::derive(TaskId(60), UserId(1000), grant, grant, &sink);
        assert_eq!(t.io_bytes_read(), 0);
        assert_eq!(t.io_bytes_written(), 0);

        // A short transfer credits what actually moved: the caller asked for
        // 4096 bytes and the VFS returned 100, so 100 is what lands.
        t.record_bytes_read(100);
        t.record_bytes_written(7);
        assert_eq!(t.io_bytes_read(), 100);
        assert_eq!(t.io_bytes_written(), 7);

        // Successive transfers accumulate, and each direction is separate.
        t.record_bytes_read(1);
        assert_eq!(t.io_bytes_read(), 101);
        assert_eq!(t.io_bytes_written(), 7);

        // A transfer that moved nothing leaves the total untouched.
        t.record_bytes_read(0);
        t.record_bytes_written(0);
        assert_eq!(t.io_bytes_read(), 101);
        assert_eq!(t.io_bytes_written(), 7);
    }

    #[test]
    fn io_counters_saturate_rather_than_wrapping() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let t = TaskCapabilities::derive(TaskId(61), UserId(1000), grant, grant, &sink);
        t.record_bytes_read(u64::MAX - 1);
        t.record_bytes_written(u64::MAX);

        t.record_bytes_read(10);
        t.record_bytes_written(1);
        assert_eq!(t.io_bytes_read(), u64::MAX);
        assert_eq!(t.io_bytes_written(), u64::MAX);

        // Clamped, never wrapped back to a small (and so misleadingly
        // idle-looking) total.
        t.record_bytes_read(u64::MAX);
        assert_eq!(t.io_bytes_read(), u64::MAX);
    }

    #[test]
    fn io_counters_are_independent_between_tasks() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let parent = TaskCapabilities::derive(TaskId(62), UserId(1000), grant, grant, &sink);
        let child = TaskCapabilities::derive(TaskId(63), UserId(1000), grant, grant, &sink);
        parent.record_bytes_read(4096);
        parent.record_bytes_written(512);
        // Each task's record is derived on its own, so one task's I/O can
        // never be credited to another's.
        assert_eq!(child.io_bytes_read(), 0);
        assert_eq!(child.io_bytes_written(), 0);
        child.record_bytes_read(8);
        assert_eq!(parent.io_bytes_read(), 4096);
        assert_eq!(child.io_bytes_read(), 8);
    }

    #[test]
    fn cloned_snapshots_of_one_task_share_its_counters() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT]);
        let sink = RecordingSink::new();
        let live = TaskCapabilities::derive(TaskId(64), UserId(1000), grant, grant, &sink);
        // The dispatcher answers each syscall off a clone of the live record;
        // an increment made through that snapshot must land on the one total
        // the registry's entry reads back.
        let snapshot = live.clone();
        snapshot.record_bytes_read(2048);
        snapshot.record_bytes_written(64);
        assert_eq!(live.io_bytes_read(), 2048);
        assert_eq!(live.io_bytes_written(), 64);
        // And the reverse direction: a later snapshot observes earlier bytes.
        live.record_bytes_read(1);
        assert_eq!(live.clone().io_bytes_read(), 2049);
    }

    #[test]
    fn narrowing_a_record_leaves_its_io_totals_alone() {
        let grant = caps_of(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let sink = RecordingSink::new();
        let mut t = TaskCapabilities::derive(TaskId(65), UserId(1000), grant, grant, &sink);
        t.record_bytes_read(300);
        t.record_bytes_written(30);
        t.delegate(&caps_of(&[CapabilityId::FS_MOUNT]), &sink)
            .expect("narrowing is permitted");
        assert!(t.revoke(CapabilityId::FS_MOUNT, &sink));
        // Capability changes are authority, not accounting: the process's
        // transferred-byte facts are untouched by either.
        assert_eq!(t.io_bytes_read(), 300);
        assert_eq!(t.io_bytes_written(), 30);
    }

    // ---------------------------------------------------------------
    // Stage 2.7 follow-up (f2): per-task CapTable registry.
    // ---------------------------------------------------------------

    fn make_caps(task: u64, caps: &[tairix_abi::CapabilityId]) -> TaskCapabilities {
        let grant = caps_of(caps);
        let sink = RecordingSink::new();
        TaskCapabilities::derive(TaskId(task), UserId(1000), grant, grant, &sink)
    }

    #[test]
    fn captable_is_empty_when_constructed() {
        let table = CapTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.caps_for(TaskId(1)).is_none());
    }

    #[test]
    fn captable_insert_then_lookup_returns_record() {
        let mut table = CapTable::new();
        let caps = make_caps(7, &[tairix_abi::CapabilityId::FS_MOUNT]);
        assert!(table.insert(caps).is_none());
        assert_eq!(table.len(), 1);
        let got = table.caps_for(TaskId(7)).expect("registered");
        assert!(got.has(tairix_abi::CapabilityId::FS_MOUNT));
        assert_eq!(got.task(), TaskId(7));
    }

    #[test]
    fn captable_lookup_miss_returns_none() {
        let mut table = CapTable::new();
        let caps = make_caps(1, &[tairix_abi::CapabilityId::FS_MOUNT]);
        table.insert(caps);
        assert!(table.caps_for(TaskId(2)).is_none());
    }

    #[test]
    fn captable_insert_returns_previous_record_on_duplicate_id() {
        // Task ids are not recycled in `kernel/sched`, so a duplicate
        // insert is a real anomaly. Surface it via the return value so
        // a caller can audit / refuse rather than silently lose state.
        let mut table = CapTable::new();
        table.insert(make_caps(3, &[tairix_abi::CapabilityId::FS_MOUNT]));
        let displaced = table.insert(make_caps(3, &[tairix_abi::CapabilityId::NET_RAW]));
        let prior = displaced.expect("first record returned");
        assert!(prior.has(tairix_abi::CapabilityId::FS_MOUNT));
        // The registry now reflects the second insert only.
        assert_eq!(table.len(), 1);
        let current = table.caps_for(TaskId(3)).expect("present");
        assert!(current.has(tairix_abi::CapabilityId::NET_RAW));
        assert!(!current.has(tairix_abi::CapabilityId::FS_MOUNT));
    }

    #[test]
    fn captable_remove_returns_and_evicts_record() {
        let mut table = CapTable::new();
        table.insert(make_caps(9, &[tairix_abi::CapabilityId::FS_MOUNT]));
        let evicted = table.remove(TaskId(9)).expect("present before remove");
        assert!(evicted.has(tairix_abi::CapabilityId::FS_MOUNT));
        assert!(table.is_empty());
        assert!(table.caps_for(TaskId(9)).is_none());
        // Idempotent: a second remove returns None and leaves the
        // registry empty.
        assert!(table.remove(TaskId(9)).is_none());
        assert!(table.is_empty());
    }

    #[test]
    fn captable_caps_for_mut_supports_revoke_in_place() {
        // The dispatcher's `cap_revoke` handler reaches `TaskCapabilities`
        // through `caps_for_mut`; this test exercises that path so the
        // mutable lookup is covered by the same security-relevant
        // assertions as `caps_for`.
        let mut table = CapTable::new();
        table.insert(make_caps(
            11,
            &[
                tairix_abi::CapabilityId::FS_MOUNT,
                tairix_abi::CapabilityId::NET_RAW,
            ],
        ));
        let sink = RecordingSink::new();
        let entry = table.caps_for_mut(TaskId(11)).expect("present");
        assert!(entry.revoke(tairix_abi::CapabilityId::FS_MOUNT, &sink));
        let after = table.caps_for(TaskId(11)).expect("still present");
        assert!(!after.has(tairix_abi::CapabilityId::FS_MOUNT));
        assert!(after.has(tairix_abi::CapabilityId::NET_RAW));
    }

    #[test]
    fn captable_stores_multiple_tasks_independently() {
        let mut table = CapTable::new();
        table.insert(make_caps(1, &[tairix_abi::CapabilityId::FS_MOUNT]));
        table.insert(make_caps(2, &[tairix_abi::CapabilityId::NET_RAW]));
        table.insert(make_caps(3, &[tairix_abi::CapabilityId::DRV_LOAD]));
        assert_eq!(table.len(), 3);
        assert!(table
            .caps_for(TaskId(1))
            .expect("1")
            .has(tairix_abi::CapabilityId::FS_MOUNT));
        assert!(table
            .caps_for(TaskId(2))
            .expect("2")
            .has(tairix_abi::CapabilityId::NET_RAW));
        assert!(table
            .caps_for(TaskId(3))
            .expect("3")
            .has(tairix_abi::CapabilityId::DRV_LOAD));
        // Removing one leaves the others intact (no aliasing).
        table.remove(TaskId(2));
        assert_eq!(table.len(), 2);
        assert!(table.caps_for(TaskId(2)).is_none());
        assert!(table.caps_for(TaskId(1)).is_some());
        assert!(table.caps_for(TaskId(3)).is_some());
    }

    #[test]
    fn captable_replacement_continues_the_task_io_totals() {
        // A spawned child's effective record replaces its admit-time
        // placeholder under the same id; the bytes the placeholder accounted
        // must survive that handover rather than restarting at zero.
        let mut table = CapTable::new();
        let placeholder = make_caps(21, &[]);
        placeholder.record_bytes_read(4096);
        placeholder.record_bytes_written(512);
        table.insert(placeholder);

        let effective = make_caps(21, &[tairix_abi::CapabilityId::FS_MOUNT]);
        assert_eq!(effective.io_bytes_read(), 0);
        let displaced = table.insert(effective).expect("placeholder returned");
        let current = table.caps_for(TaskId(21)).expect("present");
        assert_eq!(current.io_bytes_read(), 4096);
        assert_eq!(current.io_bytes_written(), 512);
        // The new authority is the effective set, not the placeholder's.
        assert!(current.has(tairix_abi::CapabilityId::FS_MOUNT));
        // One shared pair of counters, so a byte moved after the handover is
        // visible through the displaced record too — it is the same task.
        current.record_bytes_read(4);
        assert_eq!(displaced.io_bytes_read(), 4100);
    }

    #[test]
    fn captable_replacement_never_shares_across_tasks() {
        let mut table = CapTable::new();
        let first = make_caps(31, &[]);
        first.record_bytes_read(1024);
        table.insert(first);
        table.insert(make_caps(32, &[]));
        // A different id is a different task: it starts its own accounting
        // and observing one never reveals the other's activity.
        assert_eq!(
            table.caps_for(TaskId(32)).expect("present").io_bytes_read(),
            0
        );
        table
            .caps_for(TaskId(32))
            .expect("present")
            .record_bytes_read(7);
        assert_eq!(
            table.caps_for(TaskId(31)).expect("present").io_bytes_read(),
            1024
        );
        assert_eq!(
            table.caps_for(TaskId(32)).expect("present").io_bytes_read(),
            7
        );
    }

    #[test]
    fn captable_reused_id_after_exit_starts_fresh_counters() {
        // Exit evicts the entry, so a later task that happens to reuse the
        // numeric id finds no predecessor to continue and starts at zero —
        // the dead task's activity can never be attributed to it.
        let mut table = CapTable::new();
        let first = make_caps(41, &[]);
        first.record_bytes_read(9999);
        table.insert(first);
        table.remove(TaskId(41)).expect("evicted on exit");
        table.insert(make_caps(41, &[]));
        assert_eq!(
            table.caps_for(TaskId(41)).expect("present").io_bytes_read(),
            0
        );
    }
}

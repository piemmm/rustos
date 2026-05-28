//! Generated `abi-v1` dispatch table.
//!
//! Every syscall entering the kernel — from any architecture port —
//! lands in [`Dispatcher::dispatch`]. The dispatcher performs the
//! five steps mandated by `AGENTS.md` §5.4 and forwards the call to
//! the owning subsystem via the [`SyscallHandlers`] trait. The trait
//! is implemented in `kernel/core`'s wiring layer so this crate stays
//! decoupled from `kernel/ipc`, `kernel/sched`, and friends
//! (`AGENTS.md` §2.3 — no bloat).

use rustos_abi::{
    spec_for, AbiType, CapabilityId, Errno, SyscallNumber, SyscallSpec, ENCODED_TABLE,
    SYSCALL_MAX_ARGS,
};
use rustos_crypto::{sha256, Sha256Digest};
use rustos_kernel_sec::{TaskCapabilities, TaskId};
use rustos_log::{Field, Sink};
use rustos_util::fmt::{format_hex_u64, format_i32};

use crate::audit::{record, AuditEvent};

/// SHA-256 fingerprint of [`rustos_abi::ENCODED_TABLE`].
///
/// `cargo xtask abi-check` independently computes the same digest and
/// fails the build if it disagrees with the literal here. The kernel
/// itself re-checks the value via [`verify_table_hash`] at the
/// syscall-registration phase of `kernel_main`; refusal to boot beats
/// silently dispatching against an ABI the user space never agreed to.
pub const SYSCALL_TABLE_HASH: Sha256Digest = [
    0xca, 0x91, 0x9c, 0x1d, 0xb0, 0x58, 0x7d, 0xc8, 0x4e, 0x6e, 0xd8, 0x2f, 0x49, 0xae, 0x9f, 0xa5,
    0x52, 0x0e, 0xdc, 0x5a, 0xc2, 0xe8, 0xcf, 0x4f, 0xba, 0xd1, 0x80, 0x39, 0xe0, 0x5e, 0x30, 0xfb,
];

/// Re-compute the SHA-256 of [`rustos_abi::ENCODED_TABLE`] and compare it
/// to [`SYSCALL_TABLE_HASH`].
///
/// # Errors
///
/// Returns [`Errno::AbiVersionUnsupported`] when the two diverge, which
/// can only happen if the dependency graph contains a `rustos-abi`
/// older or newer than the one this crate was built against.
pub fn verify_table_hash() -> Result<(), Errno> {
    if sha256(&ENCODED_TABLE) == SYSCALL_TABLE_HASH {
        Ok(())
    } else {
        Err(Errno::AbiVersionUnsupported)
    }
}

/// Register-passed argument tuple delivered to [`Dispatcher::dispatch`].
///
/// Architecture entry stubs build this from the caller's argument
/// registers (Stage 3). Unused slots must be zero — the dispatcher
/// rejects anything else with [`Errno::LengthOutOfRange`] so a buggy
/// trampoline cannot smuggle data past a syscall's declared
/// `arg_count`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RawArgs(pub [u64; SYSCALL_MAX_ARGS]);

// SAFETY-INVARIANT: `RawArgs` is `#[repr(transparent)]` over
// `[u64; SYSCALL_MAX_ARGS]`. The x86_64 syscall trampoline
// (`kernel/arch/x86_64::syscall_entry`) builds the argument frame as a
// raw `[u64; SYSCALL_MAX_ARGS]` on the kernel stack and the binding
// kernel binary reinterprets that frame as a `RawArgs` via the public
// tuple-struct constructor `RawArgs(arr)`. Locking the layout here
// keeps that bridge sound under future field additions: any change
// that breaks size, alignment, or representation will fail the build
// at the call site rather than silently desync the ABI. `AGENTS.md`
// §2.4 (no interface creep) — this is a compile-time invariant
// assertion, not a new public surface.
const _RAW_ARGS_LAYOUT_MATCHES_ARRAY: () = {
    assert!(core::mem::size_of::<RawArgs>() == core::mem::size_of::<[u64; SYSCALL_MAX_ARGS]>());
    assert!(core::mem::align_of::<RawArgs>() == core::mem::align_of::<[u64; SYSCALL_MAX_ARGS]>());
};

impl RawArgs {
    /// All-zero argument tuple. Used by argument-less syscalls and as
    /// the default in tests.
    pub const ZERO: Self = Self([0; SYSCALL_MAX_ARGS]);

    /// Borrow the underlying register array.
    #[must_use]
    pub const fn as_array(&self) -> &[u64; SYSCALL_MAX_ARGS] {
        &self.0
    }
}

/// Identification of the task that issued a syscall.
///
/// The dispatcher never trusts caller-supplied identity — `task_id`
/// and `caps` are produced by the per-CPU current-task slot owned by
/// `kernel/sched`. The architecture stub passes both through verbatim;
/// the dispatcher uses them for capability checking and audit
/// attribution only.
pub struct CallerContext<'a> {
    /// Identifier carried in audit records.
    pub task_id: TaskId,
    /// Effective capability set, already intersected with the user
    /// grant and manifest request (see `kernel/sec`).
    pub caps: &'a TaskCapabilities,
}

/// Return type of a single dispatched syscall.
///
/// `Ok(value)` is the unsigned return register; the architecture stub
/// maps it back onto the ABI return type declared in
/// [`SyscallSpec::ret`]. `Err(errno)` is delivered to user space as a
/// negative integer in the standard ABI form.
pub type SyscallResult = Result<u64, Errno>;

/// Pluggable subsystem hooks.
///
/// One implementation per kernel build — `kernel/core` provides the
/// production wiring; tests substitute a mock. Each method receives
/// the [`CallerContext`] and the already-validated arguments, never
/// the raw [`RawArgs`].
///
/// Methods return a [`SyscallResult`]; the dispatcher converts an
/// `Err` into the [`AuditEvent::SyscallHandlerRejected`] audit record
/// for security-relevant calls before returning the same `Err` to the
/// caller.
pub trait SyscallHandlers {
    /// Voluntarily yield the CPU.
    fn yield_now(&self, caller: &CallerContext<'_>) -> SyscallResult;
    /// Terminate the calling process with `code`.
    fn exit(&self, caller: &CallerContext<'_>, code: i32) -> SyscallResult;
    /// Send a message to an IPC endpoint.
    fn ipc_send(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
    ) -> SyscallResult;
    /// Receive a message from an IPC endpoint.
    fn ipc_recv(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
    ) -> SyscallResult;
    /// Query whether the caller holds `cap`.
    fn cap_query(&self, caller: &CallerContext<'_>, cap: CapabilityId) -> SyscallResult;
    /// Delegate a capability set to another task.
    fn cap_delegate(&self, caller: &CallerContext<'_>, target: u64, set_ptr: u64) -> SyscallResult;
    /// Revoke `cap` from the task identified by `target`.
    fn cap_revoke(
        &self,
        caller: &CallerContext<'_>,
        target: u64,
        cap: CapabilityId,
    ) -> SyscallResult;
    /// Read the monotonic clock (nanoseconds since boot).
    fn clock_get(&self, caller: &CallerContext<'_>) -> SyscallResult;
}

/// Architecture-neutral syscall dispatcher.
///
/// Borrows its dependencies for the lifetime of one syscall:
///
/// * `handlers` — the [`SyscallHandlers`] implementation owned by
///   `kernel/core`.
/// * `audit`   — the [`Sink`] every security record flows through.
///
/// The struct holds no mutable state; one [`Dispatcher`] may be
/// constructed per syscall or shared across an entire CPU as the
/// kernel sees fit.
pub struct Dispatcher<'a, H: SyscallHandlers + ?Sized, S: Sink + ?Sized> {
    handlers: &'a H,
    audit: &'a S,
}

impl<'a, H: SyscallHandlers + ?Sized, S: Sink + ?Sized> Dispatcher<'a, H, S> {
    /// Construct a new dispatcher bound to `handlers` and `audit`.
    pub const fn new(handlers: &'a H, audit: &'a S) -> Self {
        Self { handlers, audit }
    }

    /// Run the full §5.4 sequence for one syscall and return its
    /// result.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `raw_number` is not a valid
    ///   [`SyscallNumber`] (above [`SyscallNumber::MAX`]).
    /// * [`Errno::NotFound`] — the number is in range but no entry of
    ///   [`rustos_abi::SYSCALLS`] is assigned at that index (no gaps in
    ///   `abi-v1` today).
    /// * [`Errno::PermissionDenied`] — the caller lacks the syscall's
    ///   `required_capability`.
    /// * [`Errno::LengthOutOfRange`] — the argument tuple carries data
    ///   in a slot past the syscall's declared `arg_count`.
    /// * [`Errno::BadAlignment`] — a `UserPtr` argument is null.
    /// * Any other [`Errno`] returned by the handler verbatim.
    pub fn dispatch(
        &self,
        caller: &CallerContext<'_>,
        raw_number: u16,
        args: RawArgs,
    ) -> SyscallResult {
        let Ok(number) = SyscallNumber::from_raw(raw_number) else {
            self.audit_unknown(caller, raw_number);
            return Err(Errno::OutOfRange);
        };
        let Some(spec) = spec_for(number) else {
            self.audit_unknown(caller, raw_number);
            return Err(Errno::NotFound);
        };

        // §5.4 step 2: capability check.
        if let Some(required) = spec.required_capability {
            if !caller.caps.has(required) {
                self.audit_denied(caller, spec);
                return Err(Errno::PermissionDenied);
            }
        }

        // §5.4 step 3: argument validation. Trailing slots must be
        // zero — a buggy trampoline that leaks register state past
        // `arg_count` is a security defect, not "harmless".
        for slot in &args.0[spec.arg_count as usize..] {
            if *slot != 0 {
                self.audit_bad_args(caller, spec);
                return Err(Errno::LengthOutOfRange);
            }
        }
        for i in 0..spec.arg_count as usize {
            if let Err(err) = validate_arg(spec.args[i], args.0[i]) {
                self.audit_bad_args(caller, spec);
                return Err(err);
            }
        }

        // §5.4 step 4: dispatch.
        let outcome = self.invoke(caller, spec, &args);

        // §5.4 step 5: audit emission for security-relevant calls.
        match &outcome {
            Ok(_) if spec.audit => self.audit_invoked(caller, spec),
            Err(_) if spec.audit => self.audit_rejected(caller, spec, outcome.as_ref().err()),
            _ => {}
        }
        outcome
    }

    fn invoke(
        &self,
        caller: &CallerContext<'_>,
        spec: &SyscallSpec,
        args: &RawArgs,
    ) -> SyscallResult {
        match spec.number {
            SyscallNumber::YIELD => self.handlers.yield_now(caller),
            SyscallNumber::EXIT => {
                // `validate_arg` guarantees the value is a
                // sign-extended `i32`. Recover the original `i32` by
                // truncating the low 32 bits — equivalent to `as i32`
                // but without the lint-flagged truncation cast.
                #[allow(clippy::cast_possible_wrap)]
                let code = (args.0[0] & 0xFFFF_FFFF) as i32;
                self.handlers.exit(caller, code)
            }
            SyscallNumber::IPC_SEND => {
                let len = decode_len(args.0[2])?;
                self.handlers.ipc_send(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::IPC_RECV => {
                let len = decode_len(args.0[2])?;
                self.handlers.ipc_recv(caller, args.0[0], args.0[1], len)
            }
            SyscallNumber::CAP_QUERY => {
                let cap = decode_capability(args.0[0])?;
                self.handlers.cap_query(caller, cap)
            }
            SyscallNumber::CAP_DELEGATE => self.handlers.cap_delegate(caller, args.0[0], args.0[1]),
            SyscallNumber::CAP_REVOKE => {
                let cap = decode_capability(args.0[1])?;
                self.handlers.cap_revoke(caller, args.0[0], cap)
            }
            SyscallNumber::CLOCK_GET => self.handlers.clock_get(caller),
            _ => Err(Errno::NotFound),
        }
    }

    fn audit_unknown(&self, caller: &CallerContext<'_>, number: u16) {
        let mut t = [0u8; 16];
        let mut n = [0u8; 12];
        // The number always fits in `u32` (it is a `u16`); `format_usize`
        // saturates above `i32::MAX` which never trips for a `u16`.
        record(
            self.audit,
            AuditEvent::SyscallUnknown,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "no",
                    value: rustos_util::fmt::format_usize(usize::from(number), &mut n),
                },
            ],
        );
    }

    fn audit_denied(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        record(
            self.audit,
            AuditEvent::SyscallPermissionDenied,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_bad_args(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        record(
            self.audit,
            AuditEvent::SyscallBadArguments,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_invoked(&self, caller: &CallerContext<'_>, spec: &SyscallSpec) {
        let mut t = [0u8; 16];
        record(
            self.audit,
            AuditEvent::SyscallInvoked,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
            ],
        );
    }

    fn audit_rejected(&self, caller: &CallerContext<'_>, spec: &SyscallSpec, err: Option<&Errno>) {
        let mut t = [0u8; 16];
        let mut e = [0u8; 12];
        let err_field = match err {
            Some(e_ref) => format_i32(e_ref.as_i32(), &mut e),
            None => "?",
        };
        record(
            self.audit,
            AuditEvent::SyscallHandlerRejected,
            &[
                Field {
                    key: "task",
                    value: format_hex_u64(caller.task_id.0, &mut t),
                },
                Field {
                    key: "sc",
                    value: spec.name,
                },
                Field {
                    key: "err",
                    value: err_field,
                },
            ],
        );
    }
}

fn validate_arg(ty: AbiType, raw: u64) -> Result<(), Errno> {
    match ty {
        AbiType::Unit => {
            if raw == 0 {
                Ok(())
            } else {
                Err(Errno::LengthOutOfRange)
            }
        }
        AbiType::I32 => {
            // Upper 32 bits must equal the sign extension of the low
            // 32 bits — anything else is a malformed trampoline value.
            // Truncating to `i32` and zero-extending back to a `u64`
            // gives the canonical sign-extended representation; the
            // raw value must equal it exactly.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let low = (raw & 0xFFFF_FFFF) as i32;
            #[allow(clippy::cast_sign_loss)]
            let extended = i64::from(low) as u64;
            if raw == extended {
                Ok(())
            } else {
                Err(Errno::OutOfRange)
            }
        }
        AbiType::U32 => {
            if raw >> 32 == 0 {
                Ok(())
            } else {
                Err(Errno::OutOfRange)
            }
        }
        AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint => Ok(()),
        AbiType::Cap => decode_capability(raw).map(|_| ()),
        AbiType::UserPtr => {
            if raw == 0 {
                Err(Errno::BadAlignment)
            } else {
                Ok(())
            }
        }
        AbiType::Len => decode_len(raw).map(|_| ()),
        AbiType::Errno => {
            // Errno is a return type; never appears as an input.
            Err(Errno::OutOfRange)
        }
    }
}

fn decode_capability(raw: u64) -> Result<CapabilityId, Errno> {
    if raw >> 16 != 0 {
        return Err(Errno::OutOfRange);
    }
    // The `>> 16 != 0` check above guarantees `raw` fits in `u16`.
    let narrowed = u16::try_from(raw).map_err(|_| Errno::OutOfRange)?;
    CapabilityId::from_raw(narrowed)
}

fn decode_len(raw: u64) -> Result<usize, Errno> {
    usize::try_from(raw).map_err(|_| Errno::LengthOutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::{CapabilityId, SyscallNumber};
    use rustos_caps::CapabilitySet;
    use rustos_kernel_sec::{TaskCapabilities, TaskId, UserId};
    use rustos_log::{set_max_level, Event, Level};

    /// Single-threaded sink that records every event identifier.
    struct RecordingSink {
        events: RefCell<Vec<u32>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            set_max_level(Level::Trace);
            Self {
                events: RefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.events.borrow().clone()
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(event.id.0);
        }
    }

    fn caps_of(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn build_caps(items: &[CapabilityId], sink: &RecordingSink) -> TaskCapabilities {
        let set = caps_of(items);
        let t = TaskCapabilities::derive(TaskId(0xA), UserId(1000), set, set, sink);
        // Drop the derivation event so tests can assert against dispatcher
        // events alone.
        sink.events.borrow_mut().clear();
        t
    }

    /// Handler that records each invocation and returns canned results.
    #[derive(Default)]
    struct MockHandlers {
        calls: RefCell<Vec<&'static str>>,
        force_err: Option<Errno>,
    }
    impl MockHandlers {
        fn record(&self, name: &'static str) {
            self.calls.borrow_mut().push(name);
        }
        fn last(&self) -> Option<&'static str> {
            self.calls.borrow().last().copied()
        }
    }
    impl SyscallHandlers for MockHandlers {
        fn yield_now(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("yield");
            Ok(0)
        }
        fn exit(&self, _c: &CallerContext<'_>, code: i32) -> SyscallResult {
            self.record("exit");
            #[allow(clippy::cast_sign_loss)]
            let bits = code as u32;
            Ok(u64::from(bits))
        }
        fn ipc_send(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, len: usize) -> SyscallResult {
            self.record("ipc_send");
            if let Some(err) = self.force_err {
                Err(err)
            } else {
                Ok(len as u64)
            }
        }
        fn ipc_recv(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, len: usize) -> SyscallResult {
            self.record("ipc_recv");
            Ok(len as u64)
        }
        fn cap_query(&self, c: &CallerContext<'_>, cap: CapabilityId) -> SyscallResult {
            self.record("cap_query");
            Ok(u64::from(c.caps.has(cap)))
        }
        fn cap_delegate(&self, _c: &CallerContext<'_>, _t: u64, _p: u64) -> SyscallResult {
            self.record("cap_delegate");
            Ok(0)
        }
        fn cap_revoke(&self, _c: &CallerContext<'_>, _t: u64, _cap: CapabilityId) -> SyscallResult {
            self.record("cap_revoke");
            Ok(0)
        }
        fn clock_get(&self, _c: &CallerContext<'_>) -> SyscallResult {
            self.record("clock_get");
            Ok(42)
        }
    }

    #[test]
    fn hash_matches_lib_abi() {
        assert_eq!(verify_table_hash(), Ok(()));
        assert_eq!(sha256(&ENCODED_TABLE), SYSCALL_TABLE_HASH);
    }

    #[test]
    fn every_syscall_is_reachable_with_required_capability() {
        let sink = RecordingSink::new();
        // Hold every capability the abi-v1 table requires.
        let caps = build_caps(&[CapabilityId::USER_ADMIN], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        for spec in rustos_abi::SYSCALLS {
            let mut args = RawArgs::ZERO;
            populate_valid_args(spec, &mut args);
            let r = d.dispatch(&ctx, spec.number.as_u16(), args);
            assert!(r.is_ok(), "{} returned {r:?}", spec.name);
        }
    }

    fn populate_valid_args(spec: &SyscallSpec, args: &mut RawArgs) {
        for i in 0..spec.arg_count as usize {
            args.0[i] = match spec.args[i] {
                AbiType::U32 | AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint => 1,
                AbiType::Cap => u64::from(CapabilityId::FS_MOUNT.as_u16()),
                AbiType::UserPtr => 0x1000,
                AbiType::Len => 64,
                AbiType::I32 | AbiType::Unit | AbiType::Errno => 0,
            };
        }
    }

    #[test]
    fn missing_capability_is_refused_and_audited() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink); // empty effective set
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 1; // handle
        args.0[1] = u64::from(CapabilityId::FS_MOUNT.as_u16());
        let r = d.dispatch(&ctx, SyscallNumber::CAP_REVOKE.as_u16(), args);
        assert_eq!(r, Err(Errno::PermissionDenied));
        // Handler must NOT have been invoked.
        assert_eq!(h.last(), None);
        // Exactly one denied event.
        assert_eq!(sink.ids(), [AuditEvent::SyscallPermissionDenied.id().0]);
    }

    #[test]
    fn unknown_syscall_is_rejected_with_audit() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // Past MAX = OutOfRange.
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::MAX + 1, RawArgs::ZERO),
            Err(Errno::OutOfRange)
        );
        // In range but unassigned = NotFound.
        let unassigned = u16::try_from(rustos_abi::SYSCALLS.len()).unwrap();
        assert_eq!(
            d.dispatch(&ctx, unassigned, RawArgs::ZERO),
            Err(Errno::NotFound)
        );
        assert_eq!(h.last(), None);
        // Two SyscallUnknown audit records.
        let ids = sink.ids();
        assert_eq!(ids.len(), 2);
        for id in ids {
            assert_eq!(id, AuditEvent::SyscallUnknown.id().0);
        }
    }

    #[test]
    fn trailing_argument_must_be_zero() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // YIELD declares zero arguments; smuggling 1 in slot 1 must fail.
        let mut args = RawArgs::ZERO;
        args.0[1] = 1;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::YIELD.as_u16(), args),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(h.last(), None);
        assert_eq!(sink.ids(), [AuditEvent::SyscallBadArguments.id().0]);
    }

    #[test]
    fn user_ptr_null_is_rejected() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        let mut args = RawArgs::ZERO;
        args.0[0] = 1; // endpoint
        args.0[1] = 0; // user ptr — null, must be refused
        args.0[2] = 8; // len
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::BadAlignment)
        );
        assert_eq!(h.last(), None);
    }

    #[test]
    fn u32_argument_high_bits_must_be_zero() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // No abi-v1 syscall takes a U32 today; exercise the validator
        // through cap_query's Cap argument, which forbids the high
        // bits set (raw >> 16 != 0).
        let mut args = RawArgs::ZERO;
        args.0[0] = 1u64 << 20; // out of cap-id range
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::CAP_QUERY.as_u16(), args),
            Err(Errno::OutOfRange)
        );
        assert_eq!(h.last(), None);
    }

    #[test]
    fn i32_argument_must_be_sign_extended() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        // -1 properly sign-extended.
        let mut args = RawArgs::ZERO;
        args.0[0] = u64::MAX;
        assert!(d.dispatch(&ctx, SyscallNumber::EXIT.as_u16(), args).is_ok());

        // High bits set without negative low — invalid.
        let mut bad = RawArgs::ZERO;
        bad.0[0] = 0x1_0000_0000;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::EXIT.as_u16(), bad),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn handler_error_is_audited_for_security_relevant_calls() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers {
            force_err: Some(Errno::NotFound),
            ..Default::default()
        };
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 1;
        args.0[1] = 0x2000;
        args.0[2] = 4;
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::IPC_SEND.as_u16(), args),
            Err(Errno::NotFound)
        );
        assert_eq!(sink.ids(), [AuditEvent::SyscallHandlerRejected.id().0]);
    }

    #[test]
    fn observers_do_not_emit_invoked_audit_records() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);

        for &n in &[
            SyscallNumber::YIELD,
            SyscallNumber::CAP_QUERY,
            SyscallNumber::CLOCK_GET,
        ] {
            let mut args = RawArgs::ZERO;
            if n == SyscallNumber::CAP_QUERY {
                args.0[0] = u64::from(CapabilityId::FS_MOUNT.as_u16());
            }
            assert!(d.dispatch(&ctx, n.as_u16(), args).is_ok());
        }
        // No security-relevant audit traffic.
        assert!(sink.ids().is_empty());
    }

    #[test]
    fn audited_call_emits_invoked_on_success() {
        let sink = RecordingSink::new();
        let caps = build_caps(&[], &sink);
        let ctx = CallerContext {
            task_id: TaskId(7),
            caps: &caps,
        };
        let h = MockHandlers::default();
        let d = Dispatcher::new(&h, &sink);
        let mut args = RawArgs::ZERO;
        args.0[0] = 0xDEAD_BEEF;
        assert!(d
            .dispatch(&ctx, SyscallNumber::EXIT.as_u16(), {
                let mut a = RawArgs::ZERO;
                a.0[0] = 0u64.wrapping_sub(2); // -2 sign-extended
                a
            })
            .is_ok());
        let _ = args;
        assert_eq!(sink.ids(), [AuditEvent::SyscallInvoked.id().0]);
    }
}

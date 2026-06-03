//! Deterministic fuzz target for the dispatcher's argument decoder.
//!
//! Stage 2.7 requires a fuzz harness for the per-syscall argument
//! validation path (`AGENTS.md` §7 / PLAN Stage 2.7 brief). We do not
//! pull in an external fuzz runner: a deterministic LCG with a fixed
//! seed exercises 100 000 random `(syscall, RawArgs)` pairs on every
//! `cargo test` run and asserts the two invariants the dispatcher must
//! uphold no matter what bits a caller crafts:
//!
//! 1. Dispatching never panics.
//! 2. The dispatcher accepts an input *iff* every argument matches its
//!    declared `AbiType` (validated by the local mirror in
//!    [`would_accept`]). If the dispatcher disagrees with the mirror,
//!    the test fails and prints the offending input.
//!
//! The deterministic seed makes failures reproducible — a flaky fuzz
//! target is a bug per `AGENTS.md` §7.
//!
//! ## Wall-clock budget (`AGENTS.md` §19.6)
//!
//! A plain `cargo test` runs the fixed [`ITERATIONS`] sweep. When
//! `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS`, the harness keeps
//! drawing fresh `(syscall, RawArgs)` pairs from the *same continuing*
//! PRNG stream until the budget elapses — the §19.6 "run each harness for
//! ≥ 60 s" contract — while the fixed seed keeps any crash reproducible.

use core::cell::RefCell;
use rustos_abi::{
    spec_for, AbiType, CapabilityId, Errno, IrqHandle, RandomFlags, SyscallNumber,
    ENCODED_TABLE_LEN, SYSCALLS, SYSCALL_MAX_ARGS,
};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{TaskCapabilities, TaskId, UserId};
use rustos_kernel_syscall::{CallerContext, Dispatcher, RawArgs, SyscallHandlers, SyscallResult};
use rustos_log::{set_max_level, Event, Level, Sink};

/// Iteration count of one sweep. Pinned at 100 000 to match the
/// abi-decode fuzz harness in `lib/abi/tests/fuzz_decode.rs` (Stage 1).
const ITERATIONS: u64 = 100_000;

/// Deadline for the current run, or `None` for the fixed smoke sweep.
///
/// `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS` (`AGENTS.md`
/// §19.6); a positive value turns the harness into a wall-clock loop. An
/// unset, empty, zero, or unparsable value preserves the deterministic
/// single-sweep behaviour.
fn fuzz_deadline() -> Option<std::time::Instant> {
    let secs: u64 = std::env::var("RUSTOS_FUZZ_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(std::time::Instant::now() + std::time::Duration::from_secs(secs))
}

/// `true` while the wall-clock budget has time left; always `false` for
/// the fixed smoke sweep so the loop body runs exactly once.
fn within_budget(deadline: Option<std::time::Instant>) -> bool {
    matches!(deadline, Some(end) if std::time::Instant::now() < end)
}

/// xor-shift* PRNG. Deterministic, fast, and zero-allocation; not used
/// for anything except generating fuzz inputs.
struct Rng(u64);
impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Handler that always succeeds, returning the first argument verbatim.
/// The fuzz target only cares about whether the dispatcher *reaches*
/// the handler; success/failure beyond that is the handler's business.
#[derive(Default)]
struct AcceptingHandlers {
    invocations: RefCell<u64>,
}
impl SyscallHandlers for AcceptingHandlers {
    fn yield_now(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn exit(&self, _c: &CallerContext<'_>, _code: i32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn ipc_send(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, _l: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn ipc_recv(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, _l: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn cap_query(&self, _c: &CallerContext<'_>, _cap: CapabilityId) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn cap_delegate(&self, _c: &CallerContext<'_>, _t: u64, _p: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn cap_revoke(&self, _c: &CallerContext<'_>, _t: u64, _cap: CapabilityId) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn clock_get(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn irq_bind(&self, _c: &CallerContext<'_>, _line: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn irq_wait(&self, _c: &CallerContext<'_>, _h: IrqHandle, _timeout_ns: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn random_get(
        &self,
        _c: &CallerContext<'_>,
        _buf: u64,
        _len: usize,
        _flags: RandomFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
}

/// Silent sink — fuzz output must not pollute test stdout. Capacity
/// constraints are deliberately ignored.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Mirror of the dispatcher's argument-acceptance predicate. Kept here
/// so the fuzz harness cross-checks the public API against an
/// independent implementation; if the two diverge, the test fails.
fn would_accept(spec_idx: usize, raw_number: u16, args: &[u64; SYSCALL_MAX_ARGS]) -> bool {
    if raw_number as usize != spec_idx {
        return false; // number must be in the populated abi-v1 range
    }
    let Some(spec) = spec_for(SyscallNumber::from_raw(raw_number).ok().unwrap()) else {
        return false;
    };
    // Trailing slots must be zero.
    for slot in &args[spec.arg_count as usize..] {
        if *slot != 0 {
            return false;
        }
    }
    for (i, &slot) in args.iter().enumerate().take(spec.arg_count as usize) {
        if !arg_is_well_typed(spec.args[i], slot) {
            return false;
        }
    }
    // `random_get`'s flags argument carries an extra semantic check the
    // per-`AbiType` validator cannot express: the dispatcher runs the raw
    // `U32` through `RandomFlags::from_bits`, which rejects any reserved
    // bit. Mirror that here (the only defined bit today is `NON_BLOCKING`).
    if spec.number == SyscallNumber::RANDOM_GET {
        let allowed = u64::from(RandomFlags::NON_BLOCKING.bits());
        if args[2] & !allowed != 0 {
            return false;
        }
    }
    true
}

/// Narrow `raw` to a plausibly-valid value for `ty`. The result is not
/// guaranteed valid (e.g. a narrowed `Cap` may still exceed
/// `CAPABILITY_ID_MAX`), so the dispatcher's rejection paths are still
/// exercised; it just lifts the validity probability above the
/// 1-in-2³² floor a pure `next_u64()` would give for `I32`/`U32`.
fn narrow_for(ty: AbiType, raw: u64) -> u64 {
    match ty {
        AbiType::Unit | AbiType::Errno => 0,
        AbiType::I32 => {
            // Sign-extend the low 32 bits into the high 32.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let low = (raw & 0xFFFF_FFFF) as i32;
            #[allow(clippy::cast_sign_loss)]
            let extended = i64::from(low) as u64;
            extended
        }
        AbiType::U32 => raw & 0xFFFF_FFFF,
        AbiType::Cap => raw & 0xFF, // keep within CAPABILITY_ID_MAX (255) most of the time
        AbiType::UserPtr => {
            if raw == 0 {
                0x1000
            } else {
                raw
            }
        }
        AbiType::Len | AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint => raw,
    }
}

fn arg_is_well_typed(ty: AbiType, raw: u64) -> bool {
    match ty {
        AbiType::Unit => raw == 0,
        AbiType::I32 => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let low = (raw & 0xFFFF_FFFF) as i32;
            #[allow(clippy::cast_sign_loss)]
            let extended = i64::from(low) as u64;
            raw == extended
        }
        AbiType::U32 => raw >> 32 == 0,
        // U64, Handle, IpcEndpoint and Len all accept any 64-bit
        // value verbatim on a 64-bit host — the dispatcher's
        // `usize::try_from` for Len is infallible there.
        AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint | AbiType::Len => true,
        AbiType::Cap => raw >> 16 == 0 && raw <= u64::from(rustos_abi::CAPABILITY_ID_MAX),
        AbiType::UserPtr => raw != 0,
        AbiType::Errno => false,
    }
}

#[test]
fn fuzz_dispatcher_matches_mirror() {
    // Compile-time sanity check that the layout we're fuzzing has not
    // shifted under us.
    assert_eq!(ENCODED_TABLE_LEN % SYSCALLS.len(), 0);

    set_max_level(Level::Error);
    let sink = NullSink;
    let handlers = AcceptingHandlers::default();
    let dispatcher = Dispatcher::new(&handlers, &sink);

    // Build a capability set holding every required capability in the
    // table so the cap-check step never short-circuits the validator.
    let mut caps_set = CapabilitySet::empty();
    for spec in SYSCALLS {
        if let Some(c) = spec.required_capability {
            caps_set.insert(c);
        }
    }
    let caps = TaskCapabilities::derive(TaskId(0xF), UserId(42), caps_set, caps_set, &sink);
    let ctx = CallerContext {
        task_id: TaskId(0xF),
        caps: &caps,
    };

    let mut rng = Rng::new(0xCAFE_F00D_DEAD_BEEF);
    let mut accepted = 0u64;
    let mut rejected = 0u64;
    let deadline = fuzz_deadline();
    loop {
        for _ in 0..ITERATIONS {
            // Bias the syscall number towards the populated range so the
            // happy path receives meaningful coverage; reserve 1/8 of
            // iterations for completely random `u16` values to also fuzz
            // the unknown-number paths.
            let raw_no = if rng.next_u64().trailing_zeros() >= 3 {
                // Fully random `u16` — fuzzes the unknown-number paths.
                #[allow(clippy::cast_possible_truncation)]
                let n = (rng.next_u64() & 0xFFFF) as u16;
                n
            } else {
                // Inside the populated range.
                let bucket = rng.next_u64() % (SYSCALLS.len() as u64);
                #[allow(clippy::cast_possible_truncation)]
                let narrowed = bucket as u16;
                narrowed
            };

            // Per-slot input generator: half-fuzzy. Half the time we hand
            // the dispatcher fully random bits (the "anything goes" path);
            // half the time we narrow the bits to a plausibly-valid value
            // for the slot's declared `AbiType`. This is what gives the
            // accepted-input counter meaningful coverage at the same time
            // as exercising every rejection path.
            let mut args = [0u64; SYSCALL_MAX_ARGS];
            let valid_spec = if (raw_no as usize) < SYSCALLS.len() {
                Some(&SYSCALLS[raw_no as usize])
            } else {
                None
            };
            for (slot_idx, slot) in args.iter_mut().enumerate() {
                let raw = rng.next_u64();
                let coin = rng.next_u64() & 1 == 0;
                *slot = match (valid_spec, coin) {
                    (Some(spec), true) if slot_idx < spec.arg_count as usize => {
                        narrow_for(spec.args[slot_idx], raw)
                    }
                    (Some(spec), true) if slot_idx >= spec.arg_count as usize => 0,
                    _ => raw,
                };
            }

            let expected = if (raw_no as usize) < SYSCALLS.len() {
                would_accept(raw_no as usize, raw_no, &args)
            } else {
                false
            };
            let result = dispatcher.dispatch(&ctx, raw_no, RawArgs(args));
            match (expected, &result) {
                (true, Ok(_)) => accepted += 1,
                (false, Err(_)) => rejected += 1,
                (true, Err(e)) => {
                    panic!(
                        "dispatcher rejected well-typed input: no={raw_no} args={args:?} err={e:?}"
                    )
                }
                (false, Ok(v)) => {
                    panic!(
                        "dispatcher accepted ill-typed input: no={raw_no} args={args:?} -> {v:?}"
                    )
                }
            }
            // The dispatcher must reject every well-known unexpected `Errno`
            // variant cleanly — we never see a stray success.
            if let Err(e) = &result {
                assert!(matches!(
                    e,
                    Errno::OutOfRange
                        | Errno::NotFound
                        | Errno::PermissionDenied
                        | Errno::LengthOutOfRange
                        | Errno::BadAlignment
                ));
            }
        }
        if !within_budget(deadline) {
            break;
        }
    }

    // Sanity bound: with 100 000 iterations we expect both sides of the
    // dispatcher's predicate to be exercised non-trivially.
    assert!(accepted > 0, "fuzz produced no accepted inputs");
    assert!(rejected > 0, "fuzz produced no rejected inputs");
}

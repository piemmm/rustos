//! Stateful property model for the syscall dispatch path (`AGENTS.md`
//! §19.7 Bronze).
//!
//! [`Dispatcher::dispatch`] is the single place the kernel runs the five
//! §5.4 steps for every privileged entry point. §19.7 requires it to carry a
//! `proptest`-style stateful model alongside its unit tests and the §19.6
//! argument fuzz harness (`tests/fuzz_args.rs`).
//!
//! Where the fuzz harness focuses on **argument typing** under raw random
//! bits, this model focuses on the **capability gate and dispatch
//! accounting**: a randomised sequence of calls — each with a randomly
//! drawn caller capability set and a syscall selector (a known number, an
//! in-range-unassigned number, or an out-of-range number) — is replayed
//! against a live dispatcher whose mock handlers count invocations. After
//! every call the result is checked against an independent oracle of the
//! §5.4 precedence, and the running handler-invocation count is checked
//! against the model's count of calls that should have reached a handler.
//! Arguments are always well-typed, so for a known syscall the *only*
//! rejection a known caller can provoke is the capability gate — the
//! property this model exists to pin down.
//!
//! ## Wall-clock budget (`AGENTS.md` §19.7)
//!
//! A plain `cargo test` runs [`SMOKE_CASES`] sequences from proptest's fixed
//! deterministic RNG; `cargo xtask proptest` exports
//! `RUSTOS_PROPTEST_BUDGET_SECS` and [`drive`] repeats batches until the budget
//! elapses. The orchestrator also exports `RUSTOS_PROPTEST_SEED`
//! ([`seeded_rng`]): a fresh seed each run so soaks draw new programs (§2.1),
//! or a logged value via `--seed` to reproduce one.

use core::cell::RefCell;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};
use rustos_abi::{
    AbiType, CapabilityId, Errno, IrqHandle, RandomFlags, SyscallNumber, SyscallSpec, SYSCALLS,
    SYSCALL_MAX_ARGS,
};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{TaskCapabilities, TaskId, UserId};
use rustos_kernel_syscall::{CallerContext, Dispatcher, RawArgs, SyscallHandlers, SyscallResult};
use rustos_log::{set_max_level, Event, Level, Sink};

/// Sequences run by a plain `cargo test` (no budget set).
const SMOKE_CASES: u32 = 256;
/// Sequences per batch under a wall-clock budget.
const BUDGET_BATCH_CASES: u32 = 256;

struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Handlers that always succeed and count how often they are reached.
#[derive(Default)]
struct CountingHandlers {
    invocations: RefCell<u64>,
}
impl CountingHandlers {
    fn count(&self) -> u64 {
        *self.invocations.borrow()
    }
    /// Record one handler entry. Handlers return `Ok(0)` themselves so the
    /// counter helper does not become a `Result`-always-`Ok` wrapper.
    fn bump(&self) {
        *self.invocations.borrow_mut() += 1;
    }
}
impl SyscallHandlers for CountingHandlers {
    fn yield_now(&self, _c: &CallerContext<'_>) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn exit(&self, _c: &CallerContext<'_>, _code: i32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn ipc_send(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, _l: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn ipc_recv(&self, _c: &CallerContext<'_>, _e: u64, _p: u64, _l: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn cap_query(&self, _c: &CallerContext<'_>, _cap: CapabilityId) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn cap_delegate(&self, _c: &CallerContext<'_>, _t: u64, _p: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn cap_revoke(&self, _c: &CallerContext<'_>, _t: u64, _cap: CapabilityId) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn clock_get(&self, _c: &CallerContext<'_>) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn irq_bind(&self, _c: &CallerContext<'_>, _line: u32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn irq_wait(&self, _c: &CallerContext<'_>, _h: IrqHandle, _timeout_ns: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn random_get(
        &self,
        _c: &CallerContext<'_>,
        _buf: u64,
        _len: usize,
        _flags: RandomFlags,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn console_write(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn spawn(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn console_read(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
}

fn budget_deadline() -> Option<Instant> {
    let secs: u64 = std::env::var("RUSTOS_PROPTEST_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(Instant::now() + Duration::from_secs(secs))
}

/// The `ChaCha` RNG `drive` runs from.
///
/// `cargo xtask proptest` exports `RUSTOS_PROPTEST_SEED` so each soak run
/// draws fresh programs (`AGENTS.md` §19.7 / §2.1) while a logged seed still
/// reproduces a counterexample; a plain `cargo test` leaves it unset and uses
/// proptest's fixed deterministic RNG, keeping the smoke sweep reproducible.
fn seeded_rng() -> TestRng {
    match std::env::var("RUSTOS_PROPTEST_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(seed) => TestRng::from_seed(RngAlgorithm::ChaCha, &expand_seed(seed)),
        None => TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    }
}

/// Expand a 64-bit seed into proptest's 32-byte `ChaCha` seed via `SplitMix64`.
fn expand_seed(seed: u64) -> [u8; 32] {
    let mut state = seed;
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    bytes
}

fn drive<S: Strategy>(strategy: S, check: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let deadline = budget_deadline();
    let cases = if deadline.is_some() {
        BUDGET_BATCH_CASES
    } else {
        SMOKE_CASES
    };
    let config = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner = TestRunner::new_with_rng(config, seeded_rng());
    loop {
        if let Err(err) = runner.run(&strategy, &check) {
            panic!("proptest stateful model found a counterexample: {err}");
        }
        if !matches!(deadline, Some(end) if Instant::now() < end) {
            break;
        }
    }
}

/// The capabilities the `abi-v1` table actually gates on, in ascending id
/// order — the universe the model draws caller capability sets from.
fn required_universe() -> Vec<CapabilityId> {
    let mut caps: Vec<CapabilityId> = SYSCALLS
        .iter()
        .filter_map(|s| s.required_capability)
        .collect();
    caps.sort_unstable_by_key(|c| c.as_u16());
    caps.dedup();
    caps
}

/// Fill `args` with a well-typed value for every declared slot, mirroring
/// the dispatcher's own validator (trailing slots stay zero).
fn populate_valid_args(spec: &SyscallSpec, args: &mut [u64; SYSCALL_MAX_ARGS]) {
    for (i, slot) in args.iter_mut().enumerate().take(spec.arg_count as usize) {
        *slot = match spec.args[i] {
            AbiType::U32 | AbiType::U64 | AbiType::Handle | AbiType::IpcEndpoint => 1,
            AbiType::Cap => u64::from(CapabilityId::FS_MOUNT.as_u16()),
            AbiType::UserPtr => 0x1000,
            AbiType::Len => 64,
            AbiType::I32 | AbiType::Unit | AbiType::Errno => 0,
        };
    }
}

/// One dispatch call: the caller's capability mask and a syscall selector.
#[derive(Clone, Debug)]
struct Call {
    cap_mask: u32,
    /// 0..len → that known syscall; len → in-range-unassigned; len+1 →
    /// out-of-range.
    selector: usize,
}

fn program(universe_len: usize) -> impl Strategy<Value = Vec<Call>> {
    let selector_max = SYSCALLS.len() + 1;
    let cap_bits: u32 = (1u32 << universe_len) - 1;
    let call = ((0u32..=cap_bits), (0usize..=selector_max))
        .prop_map(|(cap_mask, selector)| Call { cap_mask, selector });
    prop::collection::vec(call, 0..=48)
}

#[test]
fn dispatch_capability_gate_tracks_oracle() {
    set_max_level(Level::Error);
    let universe = required_universe();
    let unassigned = u16::try_from(SYSCALLS.len()).expect("table length fits u16");

    drive(program(universe.len()), move |calls| {
        let sink = NullSink;
        let handlers = CountingHandlers::default();
        let dispatcher = Dispatcher::new(&handlers, &sink);
        let mut expected_invocations = 0u64;

        for call in &calls {
            // Build the caller's capability set from the mask.
            let mut cap_set = CapabilitySet::empty();
            for (bit, cap) in universe.iter().enumerate() {
                if call.cap_mask & (1 << bit) != 0 {
                    cap_set.insert(*cap);
                }
            }
            let caps = TaskCapabilities::derive(TaskId(7), UserId(1), cap_set, cap_set, &sink);
            let ctx = CallerContext {
                task_id: TaskId(7),
                caps: &caps,
            };

            let mut args = [0u64; SYSCALL_MAX_ARGS];
            let (raw_number, want) = match call.selector {
                s if s < SYSCALLS.len() => {
                    let spec = &SYSCALLS[s];
                    populate_valid_args(spec, &mut args);
                    let want = match spec.required_capability {
                        Some(required) if !cap_set.contains(required) => {
                            Err(Errno::PermissionDenied)
                        }
                        _ => {
                            expected_invocations += 1;
                            Ok(0)
                        }
                    };
                    (spec.number.as_u16(), want)
                }
                // In range but unassigned (no gaps in abi-v1 today).
                s if s == SYSCALLS.len() => (unassigned, Err(Errno::NotFound)),
                // Out of range.
                _ => (SyscallNumber::MAX + 1, Err(Errno::OutOfRange)),
            };

            let got = dispatcher.dispatch(&ctx, raw_number, RawArgs(args));
            prop_assert_eq!(got, want);
            // The handler is reached exactly on the calls the oracle accepts.
            prop_assert_eq!(handlers.count(), expected_invocations);
        }
        Ok(())
    });
}

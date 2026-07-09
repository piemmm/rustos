//! Stateful property model for the syscall dispatch path (Bronze).
//!
//! [`Dispatcher::dispatch`] is the single place the kernel runs the five
//! steps for every privileged entry point. The charter requires it to carry a
//! `proptest`-style stateful model alongside its unit tests and the
//! argument fuzz harness (`tests/fuzz_args.rs`).
//!
//! Where the fuzz harness focuses on **argument typing** under raw random
//! bits, this model focuses on the **capability gate and dispatch
//! accounting**: a randomised sequence of calls — each with a randomly
//! drawn caller capability set and a syscall selector (a known number, an
//! in-range-unassigned number, or an out-of-range number) — is replayed
//! against a live dispatcher whose mock handlers count invocations. After
//! every call the result is checked against an independent oracle of the
//! precedence, and the running handler-invocation count is checked
//! against the model's count of calls that should have reached a handler.
//! Arguments are always well-typed, so for a known syscall the *only*
//! rejection a known caller can provoke is the capability gate — the
//! property this model exists to pin down.
//!
//! ## Wall-clock budget
//!
//! The shared `rustos_fuzzseed::prop::drive` runner owns the seed/budget
//! policy (one definition): a plain `cargo test` runs [`SMOKE_CASES`]
//! sequences **once** from a fresh, logged seed; `cargo xtask proptest --soak`
//! exports `RUSTOS_PROPTEST_BUDGET_SECS` and the runner repeats
//! [`BUDGET_BATCH_CASES`] batches off the same continuing RNG until the
//! deadline. The seed is logged at the start of each run (pinnable via
//! `--seed`), so a fresh-seed counterexample is still reproducible.

use core::cell::RefCell;

use proptest::prelude::*;
use rustos_abi::{
    AbiType, CapabilityId, Errno, IrqHandle, OpenFlags, RandomFlags, SyscallNumber, SyscallSpec,
    UnlinkFlags, SYSCALLS, SYSCALL_MAX_ARGS,
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
    fn stream_write(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    // Mirrors the trait's register-shaped signature (see the trait's
    // justification).
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _attach: u64,
        _attach_len: usize,
        _strings: u64,
        _strings_len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn pipe_create(&self, _c: &CallerContext<'_>, _out: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn console_count(&self, _c: &CallerContext<'_>) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn stream_input_mode(&self, _c: &CallerContext<'_>, _fd: u32, _mode: u32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn console_foreground(&self, _c: &CallerContext<'_>, _fd: u32, _pid: i32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn stream_read(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _buf: u64,
        _len: usize,
        _timeout_ns: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn mem_map(
        &self,
        _c: &CallerContext<'_>,
        _len: usize,
        _flags: rustos_abi::MapFlags,
        _addr_hint: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn mem_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn wait(
        &self,
        _c: &CallerContext<'_>,
        _pid: i32,
        _status: u64,
        _flags: rustos_abi::WaitFlags,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn signal(
        &self,
        _c: &CallerContext<'_>,
        _pid: i32,
        _signal: rustos_abi::Signal,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn rlimit_get(&self, _c: &CallerContext<'_>, _kind: u32, _out: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn rlimit_set(&self, _c: &CallerContext<'_>, _kind: u32, _value: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn users_db_read(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn users_admin(
        &self,
        _c: &CallerContext<'_>,
        _req: u64,
        _req_len: usize,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn seat_switch(&self, _c: &CallerContext<'_>, _seat_id: u64, _console: u32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn seat_revoke(&self, _c: &CallerContext<'_>, _seat_id: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn key_inject(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn display_acquire(&self, _c: &CallerContext<'_>, _seat: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn display_release(&self, _c: &CallerContext<'_>, _seat: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn keyboard_read(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn mmio_map(
        &self,
        _c: &CallerContext<'_>,
        _handle: u64,
        _offset: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn dma_alloc(
        &self,
        _c: &CallerContext<'_>,
        _handle: u64,
        _len: usize,
        _device_out: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn dma_free(&self, _c: &CallerContext<'_>, _handle: u64, _cpu_va: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn resource_grants(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn hw_tree_read(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn hw_tree_wait(
        &self,
        _c: &CallerContext<'_>,
        _last_generation: u64,
        _timeout_ns: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn users_db_wait(&self, _c: &CallerContext<'_>, _timeout_ns: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn ipc_call(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _request: u64,
        _request_len: usize,
        _reply: u64,
        _reply_cap: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    #[allow(clippy::too_many_arguments)] // Matches the trait declaration's justified count.
    fn call_create(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _send_caps: u64,
        _recv_caps: u64,
        _max_request: usize,
        _max_reply: usize,
        _capacity: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn call_recv(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _buf: u64,
        _buf_cap: usize,
        _ticket_out: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn call_reply(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _reply: u64,
        _reply_len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn call_peer_origin(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _origin: u64,
        _origin_cap: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn wall_time_get(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn wall_time_set(
        &self,
        _c: &CallerContext<'_>,
        _time: u64,
        _time_len: usize,
        _state: u32,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn boot_id_get(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn self_origin(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn sysinfo_introspect(
        &self,
        _c: &CallerContext<'_>,
        _domain: u32,
        _arg: u64,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn terminal_size(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn log_emit(&self, _c: &CallerContext<'_>, _record: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn hw_emit_node(&self, _c: &CallerContext<'_>, _node: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn hw_remove_node(&self, _c: &CallerContext<'_>, _node_id: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn msi_alloc(&self, _c: &CallerContext<'_>, _out: u64, _out_len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn shm_create(&self, _c: &CallerContext<'_>, _len: usize, _id_out: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn shm_map(&self, _c: &CallerContext<'_>, _handle: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn shm_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn waitset_create(&self, _c: &CallerContext<'_>) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn waitset_ctl(
        &self,
        _c: &CallerContext<'_>,
        _set: u64,
        _op: u32,
        _kind: u32,
        _id: u64,
        _token: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn waitset_wait(
        &self,
        _c: &CallerContext<'_>,
        _set: u64,
        _timeout_ns: u64,
        _token_out: u64,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_open(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _flags: OpenFlags,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_close(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_read(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _offset: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_write(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _offset: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_readdir(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_stat(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _out: u64,
        _out_len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_truncate(&self, _c: &CallerContext<'_>, _fd: u32, _size: u64) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_sync(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_mkdir(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_unlink(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _flags: UnlinkFlags,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_rename(
        &self,
        _c: &CallerContext<'_>,
        _src: u64,
        _src_len: usize,
        _dst: u64,
        _dst_len: usize,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_set_mode(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _mode: u32,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_chdir(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn fs_getcwd(&self, _c: &CallerContext<'_>, _buf: u64, _buf_cap: usize) -> SyscallResult {
        self.bump();
        Ok(0)
    }
    fn resource_open(
        &self,
        _c: &CallerContext<'_>,
        _reference: u64,
        _reference_len: usize,
        _flags: OpenFlags,
    ) -> SyscallResult {
        self.bump();
        Ok(0)
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

    rustos_fuzzseed::prop::drive(
        "dispatch_capability_gate_tracks_oracle",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        program(universe.len()),
        move |calls| {
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
        },
    );
}

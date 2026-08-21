//! Deterministic fuzz target for the dispatcher's argument decoder.
//!
//! Stage 2.7 requires a fuzz harness for the per-syscall argument
//! validation path (PLAN Stage 2.7 brief). We do not
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
//! target is a bug.
//!
//! ## Wall-clock budget
//!
//! A plain `cargo test` runs the [`ITERATIONS`] sweep once from a fresh, logged
//! seed. When
//! `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS`, the harness keeps
//! drawing fresh `(syscall, RawArgs)` pairs from the *same continuing*
//! PRNG stream until the budget elapses — the "run each harness for its
//! wall-clock budget" contract — while the logged seed keeps any crash reproducible.

use core::cell::RefCell;
use tairix_abi::seat::ReleaseSurface;
use tairix_abi::{
    spec_for, AbiType, CapabilityId, Errno, IrqHandle, LinkFlags, MapFlags, OpenFlags, PowerAction,
    RandomFlags, SyscallNumber, UnlinkFlags, WaitFlags, ENCODED_TABLE_LEN, FS_ATTR_KEY_MAX,
    FS_ATTR_VALUE_MAX, FS_MODE_MASK, SYSCALLS, SYSCALL_MAX_ARGS,
};
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::{ProcessId, TaskCapabilities, TaskId, UserId};
use tairix_kernel_syscall::{CallerContext, Dispatcher, RawArgs, SyscallHandlers, SyscallResult};
use tairix_log::{set_max_level, Event, Level, Sink};

/// Iteration count of one sweep. Pinned at 100 000 to match the
/// abi-decode fuzz harness in `lib/abi/tests/fuzz_decode.rs` (Stage 1).
const ITERATIONS: u64 = 100_000;

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
    fn ipc_recv(
        &self,
        _c: &CallerContext<'_>,
        _e: u64,
        _p: u64,
        _l: usize,
        _sender_out: u64,
    ) -> SyscallResult {
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
    fn stream_write(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn pipe_create(&self, _c: &CallerContext<'_>, _out: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn pty_create(
        &self,
        _c: &CallerContext<'_>,
        _out: u64,
        _rows: u32,
        _cols: u32,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn pty_set_size(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _rows: u32,
        _cols: u32,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn console_count(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn stream_input_mode(&self, _c: &CallerContext<'_>, _fd: u32, _mode: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn console_foreground(&self, _c: &CallerContext<'_>, _fd: u32, _pid: i32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn terminal_purge(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn thread_create(
        &self,
        _c: &CallerContext<'_>,
        _entry: u64,
        _arg: u64,
        _stack_len: usize,
        _tls_base: u64,
        _clear_on_exit: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn thread_exit(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn futex_wait(
        &self,
        _c: &CallerContext<'_>,
        _uaddr: u64,
        _expected: u32,
        _timeout_ns: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn futex_wake(&self, _c: &CallerContext<'_>, _uaddr: u64, _count: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn mem_map(
        &self,
        _c: &CallerContext<'_>,
        _len: usize,
        _flags: MapFlags,
        _addr_hint: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn mem_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn mem_pin(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn mem_unpin(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn wait(
        &self,
        _c: &CallerContext<'_>,
        _pid: i32,
        _status: u64,
        _flags: WaitFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn signal(
        &self,
        _c: &CallerContext<'_>,
        _pid: i32,
        _signal: tairix_abi::Signal,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn signal_intake(
        &self,
        _c: &CallerContext<'_>,
        _op: tairix_abi::SignalIntakeOp,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn sched_set_realtime(&self, _c: &CallerContext<'_>, _realtime: bool) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn sched_set_priority(
        &self,
        _c: &CallerContext<'_>,
        _pid: i32,
        _priority: tairix_abi::SchedPriority,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn rlimit_get(&self, _c: &CallerContext<'_>, _kind: u32, _out: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn rlimit_set(&self, _c: &CallerContext<'_>, _kind: u32, _value: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn users_db_read(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn seat_switch(&self, _c: &CallerContext<'_>, _seat_id: u64, _console: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn seat_revoke(&self, _c: &CallerContext<'_>, _seat_id: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn key_inject(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn display_acquire(&self, _c: &CallerContext<'_>, _seat: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn display_release(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _next: ReleaseSurface,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn keyboard_read(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn pointer_inject(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn pointer_read(
        &self,
        _c: &CallerContext<'_>,
        _seat: u64,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn call_grant(&self, _c: &CallerContext<'_>, _endpoint: u64, _recipient: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn shm_grant(&self, _c: &CallerContext<'_>, _region: u64, _endpoint: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn call_peer_seat(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _seat: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn mmio_map(
        &self,
        _c: &CallerContext<'_>,
        _handle: u64,
        _offset: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn dma_alloc(
        &self,
        _c: &CallerContext<'_>,
        _handle: u64,
        _len: usize,
        _device_out: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn dma_free(&self, _c: &CallerContext<'_>, _handle: u64, _cpu_va: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn resource_grants(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn hw_tree_read(&self, _c: &CallerContext<'_>, _buf: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn hw_tree_wait(
        &self,
        _c: &CallerContext<'_>,
        _last_generation: u64,
        _timeout_ns: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn users_db_wait(&self, _c: &CallerContext<'_>, _timeout_ns: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn call_recv(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _buf: u64,
        _buf_cap: usize,
        _ticket_out: u64,
        _flags: tairix_abi::CallRecvFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn call_post(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _request: u64,
        _request_len: usize,
        _ticket_out: u64,
        _deadline_ns: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn call_reap(
        &self,
        _c: &CallerContext<'_>,
        _endpoint: u64,
        _ticket: u64,
        _reply: u64,
        _reply_cap: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn call_cancel(&self, _c: &CallerContext<'_>, _endpoint: u64, _ticket: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn wall_time_get(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn wall_time_set(
        &self,
        _c: &CallerContext<'_>,
        _time: u64,
        _time_len: usize,
        _state: u32,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn boot_id_get(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn boot_facts_get(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn boot_session_get(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn self_origin(&self, _c: &CallerContext<'_>, _out: u64, _out_cap: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn terminal_size(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _out: u64,
        _out_cap: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn log_emit(&self, _c: &CallerContext<'_>, _record: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn hw_emit_node(&self, _c: &CallerContext<'_>, _node: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn hw_remove_node(&self, _c: &CallerContext<'_>, _node_id: u64, _flags: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn hw_node_health(&self, _c: &CallerContext<'_>, _health: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn hw_self_node(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn msi_alloc(&self, _c: &CallerContext<'_>, _out: u64, _out_len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn shm_create(&self, _c: &CallerContext<'_>, _len: usize, _id_out: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn shm_map(&self, _c: &CallerContext<'_>, _handle: u64, _len_out: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn shm_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn waitset_create(&self, _c: &CallerContext<'_>) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn waitset_wait(
        &self,
        _c: &CallerContext<'_>,
        _set: u64,
        _timeout_ns: u64,
        _token_out: u64,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_open(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _flags: OpenFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_close(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_readdir(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _buf: u64,
        _len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_stat(
        &self,
        _c: &CallerContext<'_>,
        _fd: u32,
        _out: u64,
        _out_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_truncate(&self, _c: &CallerContext<'_>, _fd: u32, _size: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_sync(&self, _c: &CallerContext<'_>, _fd: u32) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_mkdir(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_unlink(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _flags: UnlinkFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
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
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_symlink(
        &self,
        _c: &CallerContext<'_>,
        _target: u64,
        _target_len: usize,
        _link: u64,
        _link_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_readlink(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _out: u64,
        _out_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_link(
        &self,
        _c: &CallerContext<'_>,
        _existing: u64,
        _existing_len: usize,
        _link: u64,
        _link_len: usize,
        _flags: LinkFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_set_mode(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _mode: u32,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_set_owner(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _uid: u32,
        _gid: u32,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_attr_get(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _key: u64,
        _key_len: usize,
        _value_out: u64,
        _value_out_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_attr_set(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _key: u64,
        _key_len: usize,
        _value: u64,
        _value_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_attr_list(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _index: u64,
        _key_out: u64,
        _key_out_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_attr_remove(
        &self,
        _c: &CallerContext<'_>,
        _path: u64,
        _path_len: usize,
        _key: u64,
        _key_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn port_resolve(&self, _c: &CallerContext<'_>, _name: u64, _name_len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn port_bind(&self, _c: &CallerContext<'_>, _e: u64, _mp: usize, _cap: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn file_map(&self, _c: &CallerContext<'_>, _fd: u32, _offset: u64, _len: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn file_unmap(&self, _c: &CallerContext<'_>, _base: u64, _len: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn volume_attach(
        &self,
        _c: &CallerContext<'_>,
        _request: u64,
        _request_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn volume_detach(
        &self,
        _c: &CallerContext<'_>,
        _request: u64,
        _request_len: usize,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn system_power(&self, _c: &CallerContext<'_>, _action: PowerAction) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_chdir(&self, _c: &CallerContext<'_>, _path: u64, _path_len: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fs_getcwd(&self, _c: &CallerContext<'_>, _buf: u64, _buf_cap: usize) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fd_grant(&self, _c: &CallerContext<'_>, _fd: u32, _pid: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn fd_redeem(&self, _c: &CallerContext<'_>, _handle: u64) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(0)
    }
    fn resource_open(
        &self,
        _c: &CallerContext<'_>,
        _reference: u64,
        _reference_len: usize,
        _flags: OpenFlags,
    ) -> SyscallResult {
        *self.invocations.borrow_mut() += 1;
        Ok(5)
    }
}

/// Silent sink — fuzz output must not pollute test stdout. Capacity
/// constraints are deliberately ignored.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Which argument slot carries a flags word, and the predicate that accepts
/// it: the same `from_bits` the dispatcher runs, never a re-listed bit set.
type FlagsArg = (usize, fn(u32) -> bool);

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
    // `call_recv`'s flags argument (arg 4) carries the same extra semantic
    // check: the dispatcher runs the raw `U32` through
    // `CallRecvFlags::from_bits`, which rejects any reserved bit (the only
    // defined bit today is `NON_BLOCKING`). Mirror that here.
    if spec.number == SyscallNumber::CALL_RECV {
        let allowed = u64::from(tairix_abi::CallRecvFlags::NON_BLOCKING.bits());
        if args[4] & !allowed != 0 {
            return false;
        }
    }
    // `mem_map`'s flags argument carries the same extra semantic check:
    // the dispatcher runs the raw `U32` through `MapFlags::from_bits`,
    // which rejects any reserved bit (the only defined bit today is
    // `FIXED`). Mirror that here.
    if spec.number == SyscallNumber::MEM_MAP {
        let allowed = u64::from(MapFlags::FIXED.bits());
        if args[1] & !allowed != 0 {
            return false;
        }
    }
    // `wait`'s flags argument (arg 2) carries the same extra semantic check:
    // the dispatcher runs the raw `U32` through `WaitFlags::from_bits`, which
    // rejects any reserved bit (the defined bits today are `NONBLOCK` and
    // `STOPPED`). Mirror that here.
    if spec.number == SyscallNumber::WAIT {
        let allowed = u64::from(WaitFlags::NONBLOCK.bits() | WaitFlags::STOPPED.bits());
        if args[2] & !allowed != 0 {
            return false;
        }
    }
    // `signal`'s signal argument (arg 1) carries an extra semantic check the
    // per-`AbiType` validator cannot express: the dispatcher runs the raw
    // `U32` through `Signal::from_u32`, which rejects any value outside the
    // closed signal set (including the reserved 0). Mirror that here.
    if spec.number == SyscallNumber::SIGNAL {
        let raw = u32::try_from(args[1] & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        if tairix_abi::Signal::from_u32(raw).is_err() {
            return false;
        }
    }
    // `signal_intake`'s op argument (arg 0) carries the same extra semantic
    // check: the dispatcher runs the raw `U32` through
    // `SignalIntakeOp::from_u32`, which rejects any value outside the
    // closed op set. Mirror that here.
    if spec.number == SyscallNumber::SIGNAL_INTAKE {
        let raw = u32::try_from(args[0] & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        if tairix_abi::SignalIntakeOp::from_u32(raw).is_err() {
            return false;
        }
    }
    // `sched_set_priority`'s level argument (arg 1) carries the same extra
    // semantic check: the dispatcher runs the raw `U32` through
    // `SchedPriority::from_u32`, which rejects any value outside the closed
    // level set (including the reserved 0). Mirror that here.
    if spec.number == SyscallNumber::SCHED_SET_PRIORITY {
        let raw = u32::try_from(args[1] & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        if tairix_abi::SchedPriority::from_u32(raw).is_err() {
            return false;
        }
    }
    // `system_power`'s action argument (arg 0) carries the same extra
    // semantic check: the dispatcher runs the raw `U32` through
    // `PowerAction::from_u32`, which rejects any value outside the closed
    // action set (including the reserved 0). Mirror that here.
    if spec.number == SyscallNumber::SYSTEM_POWER {
        let raw = u32::try_from(args[0] & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        if PowerAction::from_u32(raw).is_err() {
            return false;
        }
    }
    // `fs_open`'s flags argument (arg 2) runs through `OpenFlags::from_bits`,
    // which rejects both a reserved bit and an illegal dependent combination
    // (TRUNCATE/APPEND without WRITE, EXCLUSIVE without CREATE, DIRECTORY with
    // WRITE). That decode is canonical ABI logic, so mirror it through the same
    // predicate the dispatcher uses rather than re-deriving the rule set.
    // `resource_open`'s flags argument is also arg 2 and runs through the same
    // `OpenFlags::from_bits` decode.
    if spec.number == SyscallNumber::FS_OPEN || spec.number == SyscallNumber::RESOURCE_OPEN {
        let raw = u32::try_from(args[2] & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        if OpenFlags::from_bits(raw).is_err() {
            return false;
        }
    }
    // A filesystem flags word runs through its own type's `from_bits`, which
    // rejects any reserved bit. Mirror the dispatcher through those same
    // predicates rather than re-listing each bit set here.
    let flags_arg: Option<FlagsArg> = match spec.number {
        SyscallNumber::FS_UNLINK => Some((2, |b| UnlinkFlags::from_bits(b).is_ok())),
        SyscallNumber::FS_LINK => Some((4, |b| LinkFlags::from_bits(b).is_ok())),
        _ => None,
    };
    if let Some((slot, accepts)) = flags_arg {
        let raw = u32::try_from(args[slot] & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        if !accepts(raw) {
            return false;
        }
    }
    // `fs_set_mode`'s mode argument (arg 2) carries the same extra semantic
    // check: the dispatcher refuses any bit above `FS_MODE_MASK` (the
    // permission triads plus setuid/setgid/sticky) rather than masking it.
    // Mirror that here.
    if spec.number == SyscallNumber::FS_SET_MODE && args[2] & !u64::from(FS_MODE_MASK) != 0 {
        return false;
    }
    // The attribute calls bound their key length (arg 3) to
    // `1..=FS_ATTR_KEY_MAX` at dispatch, and `fs_attr_set` additionally
    // bounds its value length (arg 5) to `FS_ATTR_VALUE_MAX`. Mirror both.
    if matches!(
        spec.number,
        SyscallNumber::FS_ATTR_GET | SyscallNumber::FS_ATTR_SET | SyscallNumber::FS_ATTR_REMOVE
    ) && (args[3] == 0 || args[3] > FS_ATTR_KEY_MAX as u64)
    {
        return false;
    }
    if spec.number == SyscallNumber::FS_ATTR_SET && args[5] > FS_ATTR_VALUE_MAX as u64 {
        return false;
    }
    // `display_release`'s disposition argument (arg 1) carries the same extra
    // semantic check: the dispatcher runs the raw `U64` through
    // `ReleaseSurface::from_u64`, which rejects any value outside the closed
    // set. Mirror it through that same predicate.
    if spec.number == SyscallNumber::DISPLAY_RELEASE
        && tairix_abi::seat::ReleaseSurface::from_u64(args[1]).is_err()
    {
        return false;
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
        AbiType::Cap => raw >> 16 == 0 && raw <= u64::from(tairix_abi::CAPABILITY_ID_MAX),
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
    let caps = TaskCapabilities::derive(ProcessId(0xF), UserId(42), caps_set, caps_set, &sink);
    let ctx = CallerContext {
        task_id: TaskId(0xF),
        caps: &caps,
    };

    let mut rng = Rng::new(tairix_fuzzseed::start(
        "fuzz_dispatcher_matches_mirror",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut accepted = 0u64;
    let mut rejected = 0u64;
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
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
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }

    // Sanity bound: with 100 000 iterations we expect both sides of the
    // dispatcher's predicate to be exercised non-trivially.
    assert!(accepted > 0, "fuzz produced no accepted inputs");
    assert!(rejected > 0, "fuzz produced no rejected inputs");
}

/// A pointer-shaped value valid for any non-`UserPtr` argument slot.
///
/// Every non-pointer argument type the `UserPtr`-bearing syscalls take
/// (`IpcEndpoint`, `Len`, `U32`, `U64`, `Handle`, `Unit`) accepts `0`:
/// it is a well-typed, in-range value, so the dispatcher reaches the
/// `UserPtr` check rather than rejecting on a sibling argument.
fn benign_non_ptr_value() -> u64 {
    0
}

/// `tests/SECURITY.md` §5 / CWE-367 / CWE-822: drive every `UserPtr`-
/// bearing syscall with deliberately *pointer-shaped* adversarial bases
/// — null, a kernel-half address, the 48-bit non-canonical hole, and the
/// top of the address space — and assert the dispatcher's argument
/// decoder is deterministic and never panics.
///
/// The architecture-neutral dispatcher cannot itself reject a kernel-
/// range or non-canonical user pointer (canonicality is an x86_64
/// property; that check is `tairix_arch_x86_64::syscall_entry::
/// validate_user_buffer`, exercised at the `copy_from_user` boundary in
/// Stage 6). What it *must* uphold today is the null rejection
/// (`Errno::BadAlignment`) and the no-panic / no-spurious-success
/// invariant for any bit pattern. This pins both so a Stage-6 change that
/// wires in the per-access pointer validator cannot silently regress the
/// boundary.
#[test]
fn pointer_shaped_user_ptr_inputs_are_handled_deterministically() {
    set_max_level(Level::Error);
    let sink = NullSink;
    let handlers = AcceptingHandlers::default();
    let dispatcher = Dispatcher::new(&handlers, &sink);

    let mut caps_set = CapabilitySet::empty();
    for spec in SYSCALLS {
        if let Some(c) = spec.required_capability {
            caps_set.insert(c);
        }
    }
    let caps = TaskCapabilities::derive(ProcessId(0x7), UserId(7), caps_set, caps_set, &sink);
    let ctx = CallerContext {
        task_id: TaskId(0x7),
        caps: &caps,
    };

    // Pointer-shaped bases: each row is `(value, is_null)`.
    let adversarial: [(u64, bool); 6] = [
        (0, true),                      // null
        (0x1000, false),                // a plausible user page
        (0x0000_8000_0000_0000, false), // first non-canonical address
        (0xFFFF_8000_0010_0000, false), // canonical kernel-half address
        (0x0001_0000_0000_0000, false), // inside the 48-bit hole
        (u64::MAX, false),              // top of the address space
    ];

    let mut saw_ptr_slot = false;
    for (spec_idx, spec) in SYSCALLS.iter().enumerate() {
        // A syscall may carry more than one `UserPtr` argument (e.g.
        // `ipc_call` takes both a request and a reply pointer). Drive *every*
        // pointer slot to the adversarial base together, so a non-null case
        // reaches the handler rather than tripping the null check on a sibling
        // pointer left at zero.
        let has_ptr = spec.args[..spec.arg_count as usize].contains(&AbiType::UserPtr);
        if !has_ptr {
            continue;
        }
        saw_ptr_slot = true;

        #[allow(clippy::cast_possible_truncation)]
        let raw_number = spec_idx as u16;
        for (base, is_null) in adversarial {
            let mut args = [benign_non_ptr_value(); SYSCALL_MAX_ARGS];
            // Trailing slots past arg_count must stay zero.
            for slot in &mut args[spec.arg_count as usize..] {
                *slot = 0;
            }
            for (i, ty) in spec.args[..spec.arg_count as usize].iter().enumerate() {
                if *ty == AbiType::UserPtr {
                    args[i] = base;
                }
            }
            // The attribute calls bound their key length (arg 3) to
            // `1..=FS_ATTR_KEY_MAX` at dispatch; seed the zeroed slot with
            // the minimal in-bounds value so the pointer contract stays the
            // thing under test.
            if matches!(
                spec.number,
                SyscallNumber::FS_ATTR_GET
                    | SyscallNumber::FS_ATTR_SET
                    | SyscallNumber::FS_ATTR_REMOVE
            ) {
                args[3] = 1;
            }

            let result = dispatcher.dispatch(&ctx, raw_number, RawArgs(args));
            if is_null {
                assert_eq!(
                    result,
                    Err(Errno::BadAlignment),
                    "null user pointer must be rejected: no={raw_number} args={args:?}"
                );
            } else {
                // Today the dispatcher accepts any non-null pointer (the
                // per-access canonical/range check is Stage-6). The
                // contract we pin is: it never panics and never returns a
                // spurious error for an otherwise well-typed call.
                assert!(
                    result.is_ok(),
                    "non-null pointer-shaped input must reach the handler today: \
                     no={raw_number} base={base:#x} -> {result:?}"
                );
            }
        }
    }
    assert!(
        saw_ptr_slot,
        "no UserPtr-bearing syscall found — the abi-v1 table changed shape"
    );
}

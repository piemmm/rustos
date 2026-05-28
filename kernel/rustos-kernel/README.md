# rustos-kernel

Stage 3a (c7-bin) of [`PLAN.md`](../../PLAN.md). The freestanding
`x86_64-unknown-none` kernel binary that wires the existing
architecture port (`kernel/arch/x86_64`) to the architecture-neutral
[`kernel_core::kernel_main`](../core/src/init.rs) entry point.

## Layout

The crate ships a hybrid `[lib]` + `[[bin]]`:

| Path                  | Role                                                              |
|-----------------------|-------------------------------------------------------------------|
| `src/lib.rs`          | Library half — boot pipeline reused by both bins.                 |
| `src/main.rs`         | Production `rustos-kernel` binary.                                |
| `src/bumpalloc.rs`    | Forward-only bump allocator + `GlobalAlloc` impl.                 |
| `src/arch_wrapper.rs` | `BinArch` — local `KernelArch` impl around `X86_64Arch`.          |
| `src/dispatch.rs`     | Production syscall-dispatch callback + `DISPATCH_SLOT` (Stage 2.7 (f5)). |
| `src/serial_sink.rs`  | COM1-backed `rustos_log::Sink`.                                   |
| `src/panic_ctx.rs`    | Bridge between `#[panic_handler]` and `kernel_core::handle_panic`. |
| `src/boot.rs`         | `boot(multiboot_info, log_sink, audit_sink) -> !`.                |
| `build.rs`            | Hands the kernel linker script to `rustc` on bare metal.          |

The QEMU integration test
[`tests/integration/kernel_arch_boot`](../../tests/integration/kernel_arch_boot)
re-uses the library half with its own audit sink (the one that flips
QEMU's `isa-debug-exit` device to success on observing
`AuditEvent::BootCompleted`).

## Allocator

The crate uses a forward-only **bump allocator** with a static
16 MiB heap (`src/bumpalloc.rs`). The allocator never frees; its
sole purpose is to support the `Arc`/`Vec`/`BTreeMap` traffic the
`kernel/core` init sequence produces during boot. A real
slab/per-process allocator lands in the `kernel/mem` sub-stage that
activates the scheduler dispatch loop.

Documented limits:

* **No reclamation** — `GlobalAlloc::dealloc` is a no-op.
* **Hard cap** — exhausting the 16 MiB heap returns `null_mut` per the
  `GlobalAlloc` contract; the caller (and `panic = "abort"`) reports
  the failure.
* **Thread-safe** — the cursor is a CAS-driven `AtomicUsize`.
* **One `static mut`** — the heap arena. `AGENTS.md` §2 reserves
  `static mut` for the per-CPU bootstrap area; the boot heap is the
  documented exception until the production allocator lands.

## Production dispatch callback

`src/dispatch.rs::production_dispatch` is installed via
`syscall_entry::set_dispatch_callback` before `init_local_syscalls`
enables `syscall` on any CPU — the arch-level fail-closed ordering
contract (see `rustos_arch_x86_64::syscall_entry` rustdoc and
`AGENTS.md` §5.4.5).

The callback's job is split into two stages (Stage 2.7 follow-up
(f4)+(f5)):

1. **Stage A — arch publication.** `boot.rs::try_boot` installs
   `production_dispatch` so the trampoline has a callback before any
   syscall can fire. `production_dispatch` is `extern "C" fn(u64,
   *const [u64; SYSCALL_MAX_ARGS]) -> u64` and its ABI is pinned at
   compile time by `_DISPATCH_SIGNATURE_PINNED`.
2. **Stage B — kernel publication.** `kernel_main` runs a new
   `Phase::Syscall` step between Sched and Ipc, building a
   `KernelDispatchHook` around `KernelState` (scheduler + capability
   table + arch + audit) and calling
   `DISPATCH_SLOT.install_dispatcher(&hook)`. `production_dispatch`
   reads the slot via `DISPATCH_SLOT.get()` on every syscall and
   forwards.

`production_dispatch` halts the CPU forever via
`kernel_arch::halt` in exactly two situations — the same fail-closed
posture the pre-(f5) `fail_closed_dispatch` shipped:

* **Empty slot**: a syscall fired before `kernel_main` ran the
  `Syscall` phase. Impossible if BSP boot ordering is correct, but
  the callback must not assume so.
* **`DispatchOutcome::NoCallerContext`**: `Scheduler::current_task`
  returned `None` for the issuing CPU, or no `TaskCapabilities`
  record exists for the running task. The hook has already emitted
  `AuditEvent::SyscallNoCallerContext` (id 4021) by the time we
  halt.

Normal `Errno` returns are encoded back into `%rax` via
`encode_result`: `Ok(v) → v`; `Err(e) → (-(e.as_i32() as i64)) as
u64`, the conventional Linux-style negation that userland recovers
with `(rax as i64) < 0` → `(-(rax as i64)) as i32`. The encoding is
covered by `encode_result_err_encodes_as_negative_i64`.

## `KernelArch::halt` proof

`src/arch_wrapper.rs::BinArch` implements
`rustos_kernel_core::KernelArch::halt` by forwarding to the free
function `rustos_arch_x86_64::kernel_arch::halt()`. The `-> !`
signature is pinned at compile time by
`const _BIN_ARCH_HALT_RETURNS_NEVER: fn(&BinArch) -> ! =
<BinArch as KernelArch>::halt;` — the same pattern (c7-arch) uses on
the arch crate's free `halt` function.

## Panic handler

Each binary's `#[panic_handler]` is a one-liner that calls
`rustos_kernel::handle_panic_via_kernel_core` (defined in
`src/panic_ctx.rs`). The bridge loads a `Arc<BinArch>` pointer
published by `boot()` (via `panic_ctx::PANIC_ARCH_PTR`) and forwards
to `rustos_kernel_core::handle_panic` with a `PanicContext { arch,
audit_sink: &SERIAL_SINK }`. A pre-init panic (before `boot()`
finishes building the arch handle) emits a single "panic before init"
record on COM1 and halts.

## Tests

* **Host unit tests** (`cargo test -p rustos-kernel --lib`):
  bump-allocator semantics, `BinArch` delegation to `X86_64Arch`'s
  host counters, `RawArgs` reinterpretation through the `extern "C"`
  shim, `production_dispatch` signature pin, `encode_result` Ok/Errno
  round-trips, and `dispatch_via_slot` happy-path / no-task /
  empty-slot branches.
* **QEMU integration test** (`cargo xtask test --qemu` →
  `rustos-test-kernel-arch-boot`): boot the kernel image under
  QEMU on `-smp 1`; the audit-observer sink flips
  `qemu_exit::exit_success` on observing
  `AuditEvent::BootCompleted` (`EventId(4004)`).

## Stage 2.7 follow-up

Stage 2.7 follow-up (f4)+(f5) are now landed. `src/dispatch.rs`
ships `production_dispatch`, `DISPATCH_SLOT`, and `encode_result`;
`boot.rs` installs `production_dispatch` (not `fail_closed_dispatch`)
and hands `&DISPATCH_SLOT` to `BootInfo::new`. The slot itself
remains a `static` (not `static mut`); its set-once publication is
protected by `kernel/sync::OnceCell` (`AGENTS.md` §2.1).

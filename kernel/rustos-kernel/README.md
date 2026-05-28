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
| `src/dispatch.rs`     | Fail-closed syscall-dispatch callback (Stage 2.7 will replace).   |
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

## Syscall-dispatch callback

`src/dispatch.rs` installs a **fail-closed** dispatch callback via
`syscall_entry::set_dispatch_callback` before `init_local_syscalls`
enables `syscall` on any CPU. If the trampoline ever forwards a real
syscall to the callback (which the (c7-bin) boot path never does —
there is no user space yet), the callback parks the CPU forever via
`kernel_arch::halt`.

Stage 2.7 will replace the body with a forwarder to
`rustos_kernel_syscall::Dispatcher::dispatch` once
`kernel/core::kernel_main` gains the syscall-registration phase. The
signature is locked at compile time by `_DISPATCH_SIGNATURE_PINNED`,
so the swap is a body-only change.

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
  shim, fail-closed dispatch signature pin.
* **QEMU integration test** (`cargo xtask test --qemu` →
  `rustos-test-kernel-arch-boot`): boot the kernel image under
  QEMU on `-smp 1`; the audit-observer sink flips
  `qemu_exit::exit_success` on observing
  `AuditEvent::BootCompleted` (`EventId(4004)`).

## Stage 2.7 follow-up

The fail-closed syscall-dispatch callback in `src/dispatch.rs` is the
documented Stage 2.7 hook. When `kernel_core::kernel_main` gains the
syscall-registration phase, the callback body is replaced with a
forwarder to `Dispatcher::dispatch` built against the then-available
`SyscallHandlers` impl and per-CPU `CallerContext` plumbing. No
other piece of this crate is intended to change.

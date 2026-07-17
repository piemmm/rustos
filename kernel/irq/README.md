# `kernel/irq` — kernel IRQ table and per-handle wait queue

Stage 4.D Item 2-tail. Kernel-side plumbing that backs the
`irq_bind` / `irq_wait` syscall pair gated by `CAP_IRQ_BIND`. The
user/kernel contract this crate implements is locked down in
[`docs/src/security/irq.md`](../../docs/src/security/irq.md).

## Stability tier

**experimental** — the public Rust API of this crate is not yet
frozen. The `abi-v1` syscall surface it backs (`lib/abi`,
`kernel/syscall/src/table.rs`) **is** frozen and this crate's
implementation may not depart from it.

## Surface

| Item              | Role                                                       |
| ----------------- | ---------------------------------------------------------- |
| `IrqTable<H>`     | Per-kernel singleton: `(line, IrqEntry)` map + wait queue. |
| `IrqEntry`        | One row per `(task, line)` binding.                        |
| `IrqHost` trait   | Composition seam: clock, park/unpark, controller mask.    |
| `IrqError`        | Internal failure surface; mapped to ABI `Errno` at the     |
|                   | syscall boundary.                                          |
| `MockIrqHost`     | Deterministic test-only host (feature `test-host`).        |

## Invariants

1. **Mask-before-wake.** `IrqTable::fire(line)` calls
   `IrqHost::mask(line)` **before** it sets the `ready` flag and
   **before** it wakes a parked waiter. The unit test
   `mask_is_observed_before_wake` exercises this ordering against a
   deterministic mock host whose `mask` records its call order
   relative to `unpark`.
2. **At most one waiter per binding.** Per the contract in
   `docs/src/security/irq.md`, a binding is `(task, line)`; the
   table refuses a second concurrent `wait` against the same
   handle with `Errno::OutOfRange`.
3. **Forgery defence.** Every `wait` re-verifies the
   `(sec_task_id, handle)` mapping before any state transition. A
   handle minted for another task returns `Errno::NotFound` (the
   kernel-handler-side audit emits `SyscallHandlerRejected`
   through the dispatcher).
4. **Idempotent release.** `IrqTable::release_for(task)` is
   cancellation-safe: a second call after a task exits is a no-op.

## Composition

The crate is `no_std` and holds no global mutable state. The
production wiring lives in `kernel/core::init::KernelState`, which
constructs one `IrqTable` and one `IrqHost` impl composed from
the scheduler, the architecture handle, and the per-platform
interrupt controller (where one exists — `kernel/tairix-kernel`
wires the IO-APIC redirection-entry mask on x86_64; on
aarch64 / riscv64 / wasm32 the production host's `mask` returns
`IrqError::ArchUnsupported`, surfaced at the syscall boundary as
`Errno::NotImplemented`, with one kernel-init audit record
naming the architecture per AGENTS.md §5.4.4 — fail closed).

## Tests

Host-side `cargo test -p tairix-kernel-irq` covers:

* `bind_mints_handle_and_records_owner`
* `bind_refuses_duplicate_line`
* `bind_refuses_out_of_range_line`
* `wait_returns_immediately_if_ready_flag_set`
* `wait_parks_when_no_ready_then_fire_wakes_one`
* `wait_returns_not_found_on_forged_handle`
* `wait_returns_not_found_on_handle_minted_for_another_task`
* `wait_returns_timed_out_when_deadline_elapses`
* `mask_is_observed_before_wake`
* `release_for_evicts_bindings_and_returns_waiters_with_not_found`

The QEMU integration test that arms a real IRQ at the controller
level lands in a follow-up session together with the x86_64 IDT
external-vector / IO-APIC routing work (the per-arch trap glue is
the prerequisite — see `.junie/next-session-prompt.md`).

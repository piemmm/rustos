# Kernel panic diagnostics (registers + backtrace)

A TAIRiX kernel panic is fatal, non-recoverable, and halts the offending
CPU (fail closed, never a silent reset). Because it is a post-mortem, its
dump carries everything an investigator needs: the source location, a
snapshot of the CPU register file, and a bounded stack backtrace. This
page describes what a panic record contains, which images produce which
parts, the deliberate address-leak policy, and how to resolve raw
addresses offline.

Two things end a kernel this way: a Rust `panic!`, and a **CPU exception
taken in kernel mode** that the port's vector has no fix-up for. Both go
through the one report path (`kernel_core::panic_dump` /
`kernel_core::fault_dump`), so both carry the same register block and
backtrace; only the cause fields and the audit event id differ.

## What a panic record contains

The panic handler emits exactly one `AuditEvent::Panic` record through the
audit sink, then halts. Its fields:

| Field           | Meaning                                                        |
| --------------- | -------------------------------------------------------------- |
| `cpu`           | Decimal id of the CPU that panicked.                           |
| `file`          | Source file of the panic (`<unknown>` if absent).             |
| `line` `column` | Decimal source position.                                       |
| `pc` `sp` `fp`  | `0x`-prefixed 64-bit hex of the captured program counter, stack pointer, and frame pointer. |
| `<reg>`         | One field per captured named general-purpose register (`rax`, `x0`, `ra`, …), each 64-bit hex. |
| `frame_0`       | The captured program counter — the top of the call chain.      |
| `frame_1..`     | The return addresses recovered by walking the frame-pointer chain, in caller order. |
| `peers_asked`   | Online CPUs other than this one that the stop-the-world asked to halt. |
| `peers_stopped` | How many of them acknowledged.                                 |
| `peer_unresponsive` | Present only when one did not — see "Stopping the world" below. |
| `root`          | The active translation root (`TTBR0_EL1` / `CR3` / `satp`), when the port can read it. |

A panic taken *inside* the report path (a re-entrant panic, or a fault
raised while a report is being written) is caught by a per-boot guard: it
emits a single terse record naming its own cause and halts immediately,
without re-entering the register/backtrace machinery — a corrupt walk can
never fault the fault handler. The guard is shared by both causes, so a
fault during a panic dump cannot recurse either.

## What a kernel-fault record contains

The production boot installs the fatal-fault handler on the boot CPU as
soon as its exception vector is live, so a kernel-mode exception reaches
the same report rather than parking the CPU in silence. Its record is
`AuditEvent::KernelFault` (`4011`):

| Field        | Meaning                                                     |
| ------------ | ----------------------------------------------------------- |
| `cpu`        | Decimal id of the CPU that faulted.                         |
| `syndrome`   | 64-bit hex of the port's exception syndrome — `ESR_EL1`, the `#PF` error code, `scause`. |
| `fault_addr` | 64-bit hex of the address the access could not reach — `FAR_EL1`, `CR2`, `stval`. |
| `fault_pc`   | 64-bit hex of the faulting instruction — `ELR_EL1`, `RIP`, `sepc`. |

plus, on a port with a non-faulting translation probe:

| Field        | Meaning                                                     |
| ------------ | ----------------------------------------------------------- |
| `fault_maps` | `yes` / `no` / `unsupported` — whether `fault_addr` translates under the active root *at report time*. |
| `fault_par`  | The probe's raw result: the physical address when it maps, the port's fault status when it does not. |
| `fault_hole` | Present only when it does not map: `page`, `block`, or `gigapage` — the coarsest granule around the address that is *also* absent. |
| `maps_after_tlbi` | Present only when it does not map: whether it translates once this CPU's cached translations are discarded. `yes` means the tables were right and the TLB was not. |
| `par_after_tlbi` | The post-flush probe's raw result, read like `fault_par`. |
| `desc_0..`   | Present only when it does not map: the raw translation descriptors the active regime holds for it, root-downward. |

followed by the same register and `frame_N` blocks as a panic. `fault_pc`
is the *interrupted* instruction; the register block's `pc` is where the
handler shim itself was captured, so the two are deliberately distinct
keys. A fault carries no `file`/`line`/`column`, and a panic carries no
syndrome — neither is fabricated for the other, which is why the two are
distinct event ids rather than one record with optional halves.

Coverage per port follows each port's fatal tail. aarch64 and riscv64 fan
*every* unhandled synchronous exception (plus FIQ / `SError` / AArch32
entries on aarch64) into one tail, so every kernel-mode exception is
reported. On x86_64 only the dedicated page-fault entry reaches the
handler today: the other exception vectors still point at the fail-closed
default IDT thunk, which has no per-vector stub and so cannot name a
syndrome — tracked as an open defect in `plans/OPEN-DEFECTS.md`.

### Why the report names the active root and re-probes

Whether an address translates is a property of the **active root**, not of the
machine: the same physical page can be mapped in one address space and absent
in another, and kernel code runs on whatever root was last activated (a
kernel thread does not switch roots). A fault report that named only the
address would therefore leave the essential question — *which* address space
refused it — unanswered.

`desc_0..` answers the question behind the absence: whether the *hierarchy*
is intact. A well-formed table descriptor pointing at a plausible table, with
one invalid leaf, is a mapping never made or since removed; a descriptor that
is arbitrary data means the table page itself has been clobbered or reused —
a page-table use-after-free, and a far worse defect. Each table is proved
translatable with the non-faulting probe before it is read, so a clobbered or
unmapped table ends the walk rather than faulting inside the fault handler,
and the walk stops at an invalid entry or a leaf.

A third reading is possible, and the descriptors alone cannot separate it from
a clobbered table: a *valid* descriptor for an address the probe refused. That
means the tables are right and the **TLB** is not — a translation changed
without the invalidation it owed, or a granule changed without
break-before-make. `maps_after_tlbi` is what distinguishes it. The probe is
permitted to answer from the TLB, so discarding this CPU's cached translations
and re-probing forces the answer to come from the tables themselves: `yes`
convicts TLB maintenance, `no` sends you back to the tables. Discarding them is
safe here precisely because the report has already read the tables it is about
to re-walk, and the port that flushes is the same one that probes — a port
without a probe reports no verdict rather than one it cannot support.

`fault_hole` separates two unrelated defects a syndrome cannot tell apart: a
leaf that something unmapped (`page`) versus a region that was never mapped at
all (`block` or `gigapage`). It is derived in the neutral layer by probing the
address's containing 2 MiB and 1 GiB bases, so it needs no extra port surface.

Re-probing the same address buys a second distinction the syndrome cannot
make. If the probe says `no`, the address is persistently unmapped under that
root. If it says `yes`, the mapping *changed* between the faulting access and
the report — a racing map/unmap, or a break-before-make that was not — which
is a wholly different defect. The probe uses only a non-faulting
address-translation instruction (aarch64's `AT`), never a hand walk of the
page tables: a report must not dereference the very memory it cannot vouch
for. x86_64 and riscv64 have no such instruction, so they report the root and
an honest `unsupported` for the probe rather than risk faulting inside the
fault handler.

Reporting a fault does **not** make it survivable. The report ends in
`KernelArch::halt`, exactly as a panic does.

## Stopping the world

A kernel-mode panic or fault means a kernel invariant is already broken and
the reporting CPU cannot resume. Leaving the other CPUs running is therefore
not a smaller failure, it is a worse one: they either deadlock on a guard the
dying core abandoned — one core's fault becoming a system-wide wedge with no
diagnosis — or they proceed over half-updated shared state, which is silent
corruption. So the report stops the world.

It reuses the one cross-CPU stop protocol the kernel already has
(`tairix_arch_api::quiesce`), not a second one: the same latched request, the
same per-port IPI poke (`SchedulerArch::send_ipi`), and the same per-port
receive path that acknowledges and parks the core masked. Only the *patience*
differs, which is why there are two initiators:

| Initiator | Caller | On an unresponsive peer |
| --- | --- | --- |
| `quiesce_others` | The pre-boot Supervisor's whole-RAM takeover | Fails closed after a long budget; the machine is left running and recoverable. |
| `stop_others_best_effort` | A panic or fatal-fault report | Gives up promptly and *names the peer in the record*; the report proceeds. |

A fatal report cannot fail closed — the kernel is already dying — and must not
stall the one diagnosis the machine will ever produce behind a core that
cannot answer. A healthy peer only has to take an already-pending IPI, so it
acknowledges far inside the budget; a peer that does not is wedged with
interrupts masked, and `peer_unresponsive` naming it is itself the finding.

**Order: stop, then read, then write.** The world is stopped before anything
about the machine is read. Stopping before the write is what leaves the
console queue — an `IrqSafeSpinLock`-held ring — uncontended for the report,
and stops a peer from advancing shared state while the report is assembled. It
costs at most the stop budget before the first byte appears, which is why that
budget is small.

Stopping before the *readings* is what makes them describe one machine. The
`fault_maps` / `fault_par` probe and the `desc_0..` walk interrogate memory a
running peer can still be editing, and taken either side of a concurrent
page-table update they disagree — which reads as a defect in the tables rather
than in the reading. A Pi 4 capture whose probe reported the faulting address
absent and whose walk then produced a valid descriptor for that same address
cost a diagnosis exactly this way. Because the stop is best effort, a reader
compares `peers_stopped` against `peers_asked` before trusting the readings:
a partial stop means a peer may still have moved underneath them.

**The report then drains its own bytes.** Stopping the world removes the
buffered console's drainer — `pump_console_tx` runs from the dispatch loop,
and there is none left — so the report ends with
`KernelArch::flush_console_blocking` after the record and before the halt.
Without it a report on a port with a queued console truncates mid-record and
the machine reads as having died silently. Ports whose console transmit is
synchronous (riscv64 SBI, x86_64 COM1) inherit the no-op default because their
bytes are already on the wire.

For the same reason nothing that can itself fault sits between capturing the
core's state and writing the record: the display-surface reclaim happens once,
ahead of the re-entrancy guard, and is deliberately not repeated after the
stop — a repaint can fault on a scan-out the active root does not map, and
losing the screen copy of a report is a far smaller failure than losing the
report.

`MachineTakeover` is deliberately *not* reused for this. That slice is the
Supervisor's irreversible tear-down: it flattens paging, which would pull the
ground out from under the very dump being written. The stop half was already
factored out of it, which is what makes it reusable here.

Per port: aarch64, riscv64, and x86_64 all latch, poke, and park through the
paths above. wasm32 has no `KernelArch` implementation and so no in-guest
fatal-report path at all — a trapping instance is terminated by the host
harness, which tears down every worker — so there is nothing for it to
initiate or receive, and it wires neither.

## Which images produce which parts

The rule is: verbose panic is on **everywhere** when it costs no steady-state
CPU time, and the parts that cost image *size* are debug-only.

- **Register snapshot + raw (hex-address) backtrace — every image.** Frame
  pointers are already forced on all three bare-metal targets
  (`.cargo/config.toml`'s `-C force-frame-pointers=yes`), so the frame chain
  is maintained at runtime regardless. Walking it is a handful of loads, only
  at panic time when the CPU is halting anyway; reading the registers is a
  read-only snapshot. Neither adds a cycle to any hot path, so both ship in
  every image.
- **On-target symbolication (turning `0xffff…1234` into `fn_name+0x40`) —
  debug images only, and not yet implemented.** Its cost is image *size* (a
  symbol table shipped in the kernel), not CPU. The zero-image-cost default
  everywhere is to print **raw addresses** and resolve them offline against
  the unstripped kernel ELF, exactly as Linux's `decode_stacktrace.sh` does.
  An on-target `(addr, name)` table looked up by binary search on the panic
  path, gated on `cfg!(debug_assertions)`, is a staged future addition
  (`PLAN.md`); the offline workflow below is the complete story today.

Per architecture:

| Arch      | Register capture | Frame-pointer unwind |
| --------- | :--------------: | :------------------: |
| x86_64    | ✓ (`rip`/`rsp`/`rbp` + GP regs) | ✓ (saved `rbp` at `[rbp]`, return at `[rbp+8]`) |
| aarch64   | ✓ (`pc`/`sp`/`x29` + `x0..x7`/`x30`) | ✓ (saved `x29` at `[x29]`, `lr` at `[x29+8]`) |
| riscv64   | ✓ (`pc`/`sp`/`s0` + `ra`/`a0..a7`) | ✓ (saved `s0` at `[s0-16]`, `ra` at `[s0-8]`) |
| wasm32    | — (host-managed) | — (traps to the JS harness) |

Active root: aarch64 `TTBR0_EL1`, x86_64 `CR3`, riscv64 `satp`. Non-faulting
translation probe and descriptor walk: aarch64 only (`AT S1E1R`/`AT S1E1W` +
`PAR_EL1`, the one probe the watchdog's stack-link check also uses). A port
without a probe it can run without faulting reports neither rather than
risking a fault inside the fault handler.

wasm32 declares both capabilities an honest `Unsupported`: WebAssembly
exposes no readable register file and no in-memory frame chain, and a panic
traps to the JavaScript harness, which surfaces the host's own stack trace.

## Safety of the unwinder

A stack unwinder that faults on a corrupt chain is a triple fault. The
walker (`tairix_arch_api::backtrace::walk`, one arch-neutral definition
shared by every port) therefore:

- allocates nothing (the panic may itself be a heap failure);
- validates every candidate frame pointer before dereferencing it —
  non-null, 8-byte aligned, strictly greater than the previous one
  (monotonic, which kills cycles), and with both read words wholly inside
  the current CPU's kernel-stack bounds;
- reads memory only through a bounds-checked reader, so the single unsafe
  dereference site lives in one audited place, never one per architecture;
- is hard-capped at 64 frames, so even a corrupt-but-plausible chain
  terminates.

The kernel-stack bounds come from the port
(`CpuStateCapture::stack_bounds`): it returns real bounds when the captured
stack pointer is on a stack it can vouch for (the boot stack), and `None`
otherwise — in which case the dump degrades to the registers plus the
captured program counter (`frame_0`) rather than reading memory it cannot
guarantee is mapped. The walker is fuzzed
(`kernel/arch/api/tests/fuzz_backtrace.rs`): it must always terminate and
never read outside the bounds it was given, for any adversarial input.

## Address-leak policy

Panic dumps print **kernel** addresses deliberately: the event is fatal,
non-recoverable, and halting, so leaking kernel layout to the halted
console is not an exploit vector — the machine is stopping. This is a
different situation from `AuditEvent::TaskFaultKilled`, a *survivable*,
per-task event, which continues to omit the raw *user* faulting address so
a running process cannot probe kernel/user layout through repeated faults.
Panic dumps carry kernel addresses; user-fault kills still do not.

A kernel-fault record's `fault_addr` / `fault_pc` follow the same rule for
the same reason: they are on the halting path, so there is no repeatable
oracle to probe. They are whatever the CPU reported. One edge is worth
naming: a *lower*-EL (user) exception that cannot be attributed to any
running task — no current task, or no published user kthread — is a
kernel-level failure by construction, and falls through to this fatal tail
carrying the user address the CPU latched. A running user task always has
a current task and so always takes the survivable `TaskFaultKilled` path
instead, which still omits its address.

## Resolving addresses offline

The raw `frame_N` / `pc` addresses are resolved against the **unstripped**
kernel ELF (the debug build, or the linker's map for a release build):

```console
# One address:
$ llvm-addr2line -e target/<triple>/debug/tairix-kernel -f -C -i 0xffffffff80001234

# Several at once (paste the frame_N values):
$ llvm-symbolizer --obj=target/<triple>/debug/tairix-kernel \
    0xffffffff80001234 0xffffffff80001300
```

`addr2line` (GNU binutils) works identically. This mirrors Linux's
`scripts/decode_stacktrace.sh` — the kernel prints addresses, the developer
resolves them against the image they built.

# Kernel panic diagnostics (registers + backtrace)

A TAIRiX kernel panic is fatal, non-recoverable, and halts the offending
CPU (fail closed, never a silent reset). Because it is a post-mortem, its
dump carries everything an investigator needs: the source location, a
snapshot of the CPU register file, and a bounded stack backtrace. This
page describes what a panic record contains, which images produce which
parts, the deliberate address-leak policy, and how to resolve raw
addresses offline.

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

A panic taken *inside* the panic handler (a re-entrant panic) is caught by
a per-boot guard: it emits a single terse record
(`kernel panic (nested — re-entered panic handler)`) and halts immediately,
without re-entering the register/backtrace machinery — a corrupt walk can
never fault the fault handler.

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

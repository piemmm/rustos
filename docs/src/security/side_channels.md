# Side-channel mitigations (Arch HAL)

Microarchitectural side channels — Meltdown, Spectre v1/v2, MDS, L1TF,
MMIO stale data — are defeated by primitives that only an architecture
port can emit: kernel/user address-space separation (KPTI-equivalent),
speculation barriers on the syscall entry/exit boundary, and a flush of
microarchitectural buffers plus an indirect-branch-predictor barrier on
every context switch. `AGENTS.md` §19.1 makes this a closed trait set on
the Arch HAL. It lives in `kernel/arch/api` next to the scheduler-facing
slice, so a port acquires no new dependency to implement it.

## The surface

`rustos_arch_api::sidechannel` defines:

- `SideChannelMitigation` — the per-port handle the kernel reaches
  through. It exposes the three per-transition barrier primitives
  (`syscall_entry_barrier`, `syscall_exit_barrier`,
  `context_switch_barrier`) and a declarative `profile`.
- `MitigationProfile` — the port's honest declaration, one `Mitigation`
  per §19.1 control: kernel/user `address_space_isolation`, the
  `syscall_entry_barrier` / `syscall_exit_barrier` speculation barriers,
  the `context_switch_buffer_flush`, and the
  `context_switch_indirect_branch_barrier`.
- `Mitigation` — one of three honest positions:
  - `Applied` — the port emits the mitigation for its target.
  - `NotVulnerable(reason)` — a no-op, permitted by §19.1 **only** where
    the silicon is provably not vulnerable, with the justification
    recorded both here and in the port's source.
  - `Pending(note)` — the silicon *does* require it, but it cannot be
    built yet because it depends on a not-yet-landed subsystem (e.g.
    KPTI needs the Stage 6 user/kernel boundary). `Pending` is honest
    and tracked, but **not** release-ready.

`MitigationProfile::validate` enforces the honesty rule: every
non-applied slot must carry a non-empty explanation.
`MitigationProfile::is_release_ready` is the stricter §19.1 "a target
that does not pass cannot ship" gate — it rejects any `Pending` slot —
and is satisfied at release time, after the burn-down closes the gaps.

## The conformance vertical

`rustos_arch_api::sidechannel::conformance::run_all` is the §17.2 / §19.1
side-channel acceptance suite. It is portable — it names only the trait —
and every port runs it against its handle from a host unit test, exactly
like the `kernel/sched` policy conformance suite. It asserts the profile
is honest (validates, every omission justified) and that the barrier
primitives are callable and idempotent. Each port additionally pins the
exact profile its silicon requires, so a port cannot silently downgrade a
declaration.

The barrier instructions themselves are only meaningful on the
bare-metal target and are gated to it; they are reviewed under the
`// SAFETY:` discipline (`AGENTS.md` §2.10) and exercised for syntax by
the per-target build. The host suite checks the contract; the bare-metal
build checks the instructions.

## Per-target declarations

| Mitigation | x86_64 | aarch64 | riscv64 | wasm32 |
| --- | --- | --- | --- | --- |
| Address-space isolation (KPTI) | Pending (Stage 6) | Pending (Stage 6) | NotVulnerable (in-order) | NotVulnerable (host-owned) |
| Syscall entry barrier | Applied (`lfence`) | Applied (`csdb`) | Applied (`fence`) | NotVulnerable (no ISA barrier) |
| Syscall exit barrier | Applied (`lfence`) | Applied (`csdb`) | Applied (`fence`) | NotVulnerable (no ISA barrier) |
| Context-switch buffer flush | Applied (`verw`) | NotVulnerable (Intel-only) | NotVulnerable (Intel-only) | NotVulnerable (host-owned) |
| Context-switch IBP barrier | Pending (CPUID/IBPB) | Pending (MIDR/SMCCC) | NotVulnerable (in-order) | NotVulnerable (host-owned) |

- **x86_64** is vulnerable to the full zoo. The `lfence` speculation
  fence and the `verw` MDS buffer-clear are unconditionally safe and
  applied today. KPTI awaits the Stage 6 user/kernel page tables, and
  IBPB (`IA32_PRED_CMD`) awaits the `CPUID` feature probe — writing the
  MSR blindly would `#GP` — so both are tracked `Pending`.
- **aarch64** emits the `csdb` Spectre-v1 barrier. The MDS-class buffer
  flush is a justified no-op (those are Intel-specific buffer-sampling
  flaws); KPTI and the MIDR-specific Spectre-v2 sequence are `Pending`.
- **riscv64** emits a conservative `fence` on each boundary. The in-order
  cores RustOS targets (QEMU `virt`, SiFive U54/U74) do not speculate
  past a fault or a mispredict, so the Meltdown-, MDS-, and Spectre-v2-
  class controls are justified no-ops; the port is release-ready.
- **wasm32** delegates every microarchitectural defence to the
  Chrome-class host (site isolation, timer clamping, COOP/COEP) and
  isolates memory with one linear memory per worker. Every control is a
  justified host-owned no-op; the port is release-ready.

## Remaining §19.1 work

The KPTI and indirect-branch-predictor `Pending` gaps close with the
Stage 6 process model (the user/kernel boundary and the `CPUID`/MIDR
feature probes).

The §19.1 constant-time requirement for `lib/crypto`'s secret-handling
code is **landed**: `rustos_crypto::ct_eq` compares secret byte strings
in content-independent time, and its tests prove the no-early-exit
property without the wall-clock timing `AGENTS.md` §7 forbids — an
instrumented iterator asserts that equal, first-byte-differing,
last-byte-differing, and all-differing inputs all traverse the full
length. `cargo xtask ci` re-runs the `rustos-crypto` tests under the
release profile (`-C opt-level=3`) so an optimiser-introduced branch
would fail the gate. See [`rustos-crypto`](../lib/crypto.md).

# FIX-PROTECTION — First-class stack canaries, shadow stack, and hardware memory tagging

Binding under `AGENTS.md` (§3, §15.18). Governs the exploit-mitigation
hardening the charter mandates in §19.2 (W^X / ASLR / stack canaries /
shadow stack / CFI) and §19.10 (hardware memory tagging), landing each as a
**complete, hardware-backed defence with no no-ops** — real codegen, real
silicon enablement, and real fault handlers — or, only where the silicon
genuinely lacks the feature, a **justified** `Unsupported(reason)` / software
floor, never a silent stub. Performance is first-class (§2.16): every
mechanism is landed in its low-overhead mode and the double-cost trap is
avoided.

This plan turns §19 burn-down **Item 10** (§19.2 stack-canary / shadow-stack +
per-arch live fault fix-up) and the §19.10 **auto-enable Arm MTE** item from
`[DO IMMEDIATELY ON UNBLOCK]` into concrete, ordered work. Both were
stage-blocked on the Stage 6 user/kernel boundary, which is complete, so both
are now unblockable.

## Status

`planned` — none of P1–P5 below is started.

## Scope decision required before P4 (§15.7)

The **shadow-stack** work (P4) adds a *new closed trait set to the Arch HAL
surface* (§17.2), which the charter says requires a `PLAN.md` entry and a
`kernel/arch/api/` update, and must not be smuggled in as interface creep
(§2.4). Adding the `shadowstack` slice **is** in scope for this plan and is
recorded here as its §17.2 authorisation: it is a first-class HAL slice modelled
exactly on the existing `memtag` / `sidechannel` slices. If the implementer
finds the slice needs surface beyond what P4 specifies, stop and ask (§15.7)
before widening it.

## What already exists (the foundation — do not re-build)

Confirm each of these is still present before building on it; do not duplicate
any of it (§2.2).

- **HAL memory-tagging slice** — `kernel/arch/api/src/memtag.rs`: the closed
  `MemoryTagging` trait, `MemTag`, the shared `next_free_tag` rotation, honest
  `Tagging::{Supported, Unsupported(reason), Pending(note)}`, `TaggingProfile`
  with `validate()` / `is_release_ready()` / `enforces_uaf_in_hardware()`, and
  a conformance vertical (`memtag::conformance::run_all`). Per-port impls exist
  under `kernel/arch/{x86_64,aarch64,riscv64,wasm32}/src/memtag.rs`.
- **aarch64 MTE store path** — `kernel/arch/aarch64/src/memtag.rs`: the real
  `stg` (Store Allocation Tag) sequence over each 16-byte granule, 4-bit tag ==
  neutral `TAG_COUNT` (no narrowing), gated behind a per-handle `mte_enabled`
  flag that defaults **off**. Both profile slots are honestly `Pending`
  (enabling needs the `ID_AA64PFR1_EL1.MTE` probe + `Normal Tagged` stage-1
  attribute + synchronous tag-check-fault decode). Constructors
  `MemoryTags::new()` (gated off) and `MemoryTags::with_mte_enabled()` (gated
  on, never yet reached) are present.
- **Software UAF floor** — `kernel/mem/src/slab.rs`: `SlabHandle` +
  `SoftwareTagCheck`, the on-by-default use-after-free tag check on **every**
  port today, sharing the HAL's `next_free_tag`. It already **stands down**
  where `enforces_uaf_in_hardware()` is true, so there is no double cost once a
  hardware port goes live.
- **Stack-canary runtime** — `lib/rt/src/start.rs`: `__stack_chk_guard` is
  seeded once per process from the kernel-supplied per-process random canary,
  and `__stack_chk_fail` is a real fail-closed / fail-loud handler (message on
  stderr, reserved exit code `EXIT_BAD_STARTUP = 70`). `lib/crt0` mirrors this
  for non-Rust programs.
- **W^X / PIE / KASLR / CFI** — landed (§19.2 "done" in `PLAN.md`): the `rxe`
  loader (`lib/abi/src/rxe.rs`, R/RX/RW only, PIE required, CFI tag vs
  syscall-interface hash) + `kernel/mem` image build + the `EnterUser` HAL
  primitive.
- **C surface canary** — `tools/cc/src/target.rs` already passes
  `-fstack-protector-strong` for the third-party C ABI surface.
- **Side-channel slice** — `kernel/arch/api/src/sidechannel.rs`: the model to
  copy for the honest-profile discipline (`Applied` / `NotVulnerable(reason)` /
  `Pending(note)`, `validate` vs `is_release_ready`, a conformance vertical).
- Toolchain is pinned nightly `nightly-2026-07-03` with `rust-src` +
  `llvm-tools-preview` (`rust-toolchain.toml`) — which is what the
  `-Z stack-protector` / CET codegen flags require.

## The honest gaps (what this plan closes)

1. The Rust stack-protector codegen flag is **not set** anywhere in
   `.cargo/config.toml` — so `__stack_chk_guard` / `__stack_chk_fail` are
   seeded but the compiler emits no protected prologues/epilogues for
   first-party Rust; the canary is currently **inert for Rust** (only C gets it
   via the `cc` wrapper).
2. **No shadow stack** exists — no x86_64 CET, no per-arch shadow-stack HAL
   slice.
3. **No live per-arch fault fix-up** for a delivered tag-check / CET / canary
   fault: nothing turns a hardware violation into a deterministic, logged,
   fail-closed task termination.
4. **MTE is never auto-enabled** — the `FEAT_MTE` probe and `Normal Tagged`
   page attribute are unwired; `with_mte_enabled()` is unreachable.

## Cross-cutting invariants (apply to every step)

- **No no-ops where the silicon supports the feature** (§19.2 / §19.10). A
  no-op / `Unsupported` / `Pending` is permitted **only** with a justification
  recorded in the port's `README.md`, exactly as the `sidechannel` /
  `memtag` slices already require.
- **Honest per-port profiles.** Follow the existing `Applied` /
  `NotVulnerable(reason)` / `Pending(note)` discipline; a port that claims a
  defence it does not have is a defect. `validate()` (per-PR honesty gate) stays
  green throughout the burn-down; `is_release_ready()` flips only when the port
  genuinely enforces in hardware.
- **No double cost** (§2.16). The `kernel/mem` software slab check already
  stands down under `enforces_uaf_in_hardware()`; keep that wiring — never pay
  twice once MTE is live.
- **Fail closed, fail loud** (§5.4, §2.24). Every fault handler terminates the
  offending task deterministically and emits a stable `lib/log` §19.4 audit
  event; it never widens authority, never silently continues, never panics the
  kernel on a *user* violation (§2.9).
- **Measurement-backed performance claims** (§2.16 / §23.2). The closing report
  MUST quote a before/after microbenchmark of syscall/IPC dispatch and context
  switch with the flags enabled; a claim of "no regression" without a
  measurement is a review blocker.
- **Every step lands with tests + docs + conformance** (§7, §13, §19) and ends
  with the whole-project validation gate green (§2.15 / §7). No `#[ignore]`, no
  weakened assertion, no "tests later".
- **`cfg(target_arch …)` stays inside `kernel/arch/<target>/`** (§17.2,
  enforced by `cargo xtask cfg-check`); the `.cargo/config.toml` and
  `tools/*` allow-list is the only other home for target-conditional build glue.

## P1 — Rust stack canaries live on all four Tier-1 ports (§19.2, Item 10 canary half)

Goal: the seeded `__stack_chk_guard` / `__stack_chk_fail` machinery becomes
effective for first-party Rust, matching the `-fstack-protector-strong`
already used for C.

- Add `-Z stack-protector=strong` to the `rustflags` block of **each**
  bare-metal target in `.cargo/config.toml`
  (`x86_64-unknown-none`, `aarch64-unknown-none`,
  `riscv64gc-unknown-none-elf`, `wasm32-unknown-unknown`) **and** to the
  kernel binary build flags.
  - Use `strong`, **not** `all`. `all` protects every function including leaf
    functions with no stack buffers — the "premature pessimisation" §2.16
    warns against. `strong` is the industry norm and near-zero cost on hot/leaf
    paths.
  - Carry a justifying comment on each added flag, matching the existing
    `--cfg` comment convention in that file.
  - If `wasm32` codegen does not support `-Z stack-protector` (the host sandbox
    owns control-flow integrity there), record that honestly in a comment and
    in the relevant `docs/src/security/` page rather than forcing an unsupported
    flag — the wasm host is the CFI authority on that port (mirror the
    side-channel "host-owned" position).
- **Kernel `__stack_chk_fail`.** The userland handler exits the process; the
  kernel needs its own `__stack_chk_fail` that routes into the panic /
  backtrace path (`plans/FIX-PANICS.md`) — a deterministic, logged kernel fault,
  not a userland `exit`. Provide it once, shared, not duplicated per arch
  (§2.2).
- Tests: a canary-smashing vertical per surface (userland via `lib/rt`, kernel
  via the panic path) that proves the guard fires and the reserved
  exit/panic path is taken; the existing `lib/rt` / `lib/crt0` seeding tests
  stay green.
- Docs: update the §19.2 entry in the relevant `docs/src/security/` page and
  flip the `PLAN.md` §19 Item-10 canary half from stage-blocked to done.

## P2 — aarch64 MTE auto-enable on `FEAT_MTE` (§19.10)

Goal: on aarch64 silicon that reports `FEAT_MTE`, hardware tagging becomes the
live UAF defence; the software slab check stands down; the profile flips
`Pending → Supported`. No CPU-speed sacrifice: use asynchronous (or asymmetric)
tag-check mode on the hot path.

- **Probe** `ID_AA64PFR1_EL1.MTE` during aarch64 Stage 6 early discovery
  (`kernel/arch/aarch64/`), recording the result in the normalised discovery
  path — never a compile-time board assumption (§2.20).
- **Enable tag checking**: set `SCTLR_EL1.ATA` and the `TCF` (tag-check-fault)
  mode. Use **asynchronous** or **asymmetric** TCF for steady-state hot paths
  (negligible cost); synchronous mode may be a documented debug option but is
  not the default.
- **Map user + slab memory `Normal Tagged`** via the stage-1 page attributes
  (the Stage 6 page-table work). Thread the attribute through the existing
  `PageFlags` / MMU HAL, not a board constant.
- **Construct `MemoryTags::with_mte_enabled()`** only after a positive probe;
  on an MTE-less core keep `MemoryTags::new()` (gated off). The `stg` store path
  is already written — this step makes it reachable.
- **Flip the profile**: `tag_storage` and `tag_check_faults` from `Pending` to
  `Tagging::Supported` **only** once both the store and the fault decode are
  genuinely live. Until the fault decode (P3) lands, keep `tag_check_faults`
  honest.
- **Software floor stands down**: verify `enforces_uaf_in_hardware()` returns
  true under live MTE so `kernel/mem`'s `SlabHandle` check disables itself — no
  double cost (already wired; add a test that asserts it).
- **Rotate-on-free**: confirm the allocator stamps a freshly-rotated tag on
  every alloc and rotates on free so a dangling pointer keeps a stale tag
  (the whole point of MTE).
- Other ports keep honest `Unsupported(reason)` (no ratified/available tagging
  ISA for riscv64 / x86_64) or `host-owned` (wasm32), retaining the software
  floor — already the correct first-class answer there.
- Tests: a QEMU aarch64 MTE vertical (QEMU `virt` with MTE) proving a
  use-after-free faults deterministically under hardware tagging; the
  `memtag::conformance` vertical stays green; a test asserting the software
  check stands down. Docs + `PLAN.md` §19.10 auto-enable item → done.

## P3 — Per-arch live fault fix-up (§19.2 / §19.10, Item 10)

Goal: a delivered hardware violation (MTE tag-check fault, CET `#CP`, or a
canary-fail routed as a fault) becomes a deterministic, logged, fail-closed
task termination — never an unhandled abort, never a silent continue.

- Extend the interrupt/abort entry path in each `kernel/arch/<target>/` to
  decode its relevant synchronous fault classes:
  - **aarch64**: the MTE tag-check data abort (synchronous mode) / the
    asynchronous tag-check status; decode the ESR fault class.
  - **x86_64**: `#CP` (control-protection, CET) once P4 lands; page-fault
    decode already exists.
  - **riscv64**: the relevant exception cause once a tagging/CFI ISA is
    enabled (else nothing to decode — honest).
- The **decision + termination** logic is arch-neutral and shared (§2.21 /
  §2.2): only the fault *decode* (ESR/vector/cause) is arch-specific. On a
  user-mode violation, terminate the offending task deterministically and
  fail closed (§5.4); on a kernel-mode violation, route into the panic /
  backtrace path (`plans/FIX-PANICS.md`).
- Emit a **stable `lib/log` §19.4 audit event** with a fixed event ID for every
  such fault (protection-violation class); it must carry no secrets or
  capability tokens (§23.1) and land on the hash-chained log.
- Tests: a vertical per live fault class proving the offending task dies with
  the correct typed reason and the audit event is written; the fault never
  crashes the kernel on a user violation.

## P4 — `shadowstack` HAL slice (§19.2, Item 10 shadow-stack half)

Goal: a new closed Arch HAL slice for shadow-stack / backward-edge CFI, real and
hardware-backed on x86_64, honest elsewhere. This is the authorised §17.2
surface addition (see "Scope decision" above).

- Add `kernel/arch/api/src/shadowstack.rs` modelled on `memtag.rs` /
  `sidechannel.rs`: a `ShadowStack` trait (allocate/free a thread's
  shadow-stack region, enable/disable for a task, the fault hook), a
  `ShadowStackProfile` with the honest `Supported` / `Unsupported(reason)` /
  `Pending(note)` positions, `validate()` vs `is_release_ready()`, and a
  `conformance` vertical. Export it from `kernel/arch/api/src/lib.rs` alongside
  the other slices.
- Per-port impls under `kernel/arch/<target>/src/shadowstack.rs`:
  - **x86_64**: real Intel CET shadow stack — allocate shadow-stack pages (a new
    page attribute + `wrss` / SSP management), enable `CR4.CET` +
    `IA32_U_CET` / `IA32_S_CET`, and wire the `#CP` fault into P3. Hardware-
    accelerated: cost is a shadow-stack page per thread, not per-call software
    bookkeeping. Profile → `Supported` once live; honestly `NotVulnerable` /
    `Unsupported` on CET-less parts probed at runtime (fall back to the software
    floor, do not fake it).
  - **aarch64**: ARMv8.5 baseline has **no** shadow stack (GCS is ARMv9.4
    `FEAT_GCS`). Honest `Pending(note)` with a `FEAT_GCS` probe path, or a
    software SafeStack-equivalent if chosen — do not fake it. Record the
    justification in the port `README.md`.
  - **riscv64**: Zicfiss (shadow stack) is the target *if* the pinned
    toolchain/QEMU support it; otherwise honest `Pending(note)` with the
    probe path.
  - **wasm32**: honest `Unsupported("host-owned")` — the host sandbox owns
    control-flow integrity, exactly as the side-channel slot is on that port.
- Add the `shadowstack::conformance::run_all` call to the Arch HAL conformance
  suite each port runs (§17.2) — a port that does not pass cannot ship.
- Tests: the conformance vertical per port + an x86_64 QEMU CET vertical proving
  a backward-edge violation faults and is handled by P3. Docs: the §17.2 Arch
  HAL surface list gains the `shadowstack` slice; `docs/src/security/` gains the
  shadow-stack page.

## P5 — Close-out: profiles, docs, `PLAN.md`, README, benchmark (§2.16 / §13 / §23)

- Move §19 Item 10 (canary + shadow-stack + per-arch live fault fix-up) and the
  §19.10 MTE auto-enable item from `[DO IMMEDIATELY ON UNBLOCK]` /
  stage-blocked to **done** in `PLAN.md`, replacing the plan prose with a
  concise done-state summary (§13 — no landing log).
- Update the `docs/src/security/` pages (canaries, shadow stack, memory
  tagging) and the `README.md` feature/architecture support matrix (one terse
  row per feature per target reflecting `Supported` / `Pending` / `host-owned`).
- Update this file's Status to `done` and collapse P1–P4 detail to their
  done-state guarantees.
- **Performance evidence (mandatory, §2.16 / §23.2)**: run and quote a
  before/after microbenchmark of syscall/IPC dispatch and context switch with
  the canary flag, MTE (async), and CET enabled, in the completion report. A
  blown latency/throughput budget is a defect fixed or reverted in the same
  change (§2.5).

## Recommended sequencing

`P1 → P2 → P3 → P4 → P5`. P1 is the smallest self-contained win (flip the
flag + kernel handler). P2 makes MTE live and needs the Stage 6 page-table
attribute. P3 is the shared fault decision plus per-arch decode that both P2
(MTE fault) and P4 (CET `#CP`) route into, so it lands between them; land the
MTE fault decode with P2/P3 and the CET decode with P4. P5 is the measured,
documented close-out.

## Non-goals (do not build here — §2.4)

- A software shadow stack on x86_64 where CET exists (use the hardware).
- Tagging ISAs on riscv64 / x86_64 before they are ratified/available — honest
  `Unsupported(reason)`, keep the software floor.
- Any speculative HAL surface beyond the `shadowstack` slice this plan
  authorises.

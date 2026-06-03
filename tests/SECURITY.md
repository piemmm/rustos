# RustOS Security Tests

This document is the adversarial-test charter for RustOS's memory subsystem
and CPU privilege boundary. It records (a) the deliberate
**memory-corruption / fault-injection** tests that exercise our *detectors*,
and (b) the **memory and CPU/ring-boundary** adversarial tests modelled on
named CVE classes.

It is binding under `AGENTS.md`. Every test described here obeys §2.5
(all tests pass, none deferred), §2.9 (no `panic!`/`unwrap`/`expect` in
production paths), §5.4 (identify caller, check capability, validate input,
log, fail closed), §7 (tests live next to code; whole-project Definition of
Done), §17.2 (no `cfg(target_arch …)` outside `kernel/arch/<target>/`),
§19.6 (fuzzing), §19.7 (verified capability core), and §19.10 (memory
tagging). Nothing here may be landed as an `#[ignore]`, `todo!()`, or
"tests to be added later" stub (§2.5, §15.1, §15.3).

## 1. Scope and ground truth

### 1.1 Surfaces under test

- **Memory:** `kernel/mem/{slab.rs, ptr.rs, sensitive.rs, vmm.rs, frame.rs,
  phys.rs, dma, mmio}`.
- **CPU / ring boundary (x86_64):**
  `kernel/arch/x86_64/{syscall_entry.rs, gdt.rs, idt.rs, paging.rs,
  sidechannel.rs, memtag.rs}`.
- **Audit log integrity (§19.4):** the hash-chained log under `/System/Logs`.
- **DMA / virtio:** `kernel/mem/dma`, `kernel/sec/dma`, `lib/virtio/queue.rs`.

### 1.2 Existing coverage we extend, never duplicate (§2.2)

- `slab.rs` already tests: use-after-free → realloc tag mismatch, double-free,
  guard-page violations at check/alloc/free, and zero-on-free, plus the
  `poke_for_test` single-byte corruption trapdoor (`#[cfg(test)] pub(crate)`).
- `ptr.rs` already tests: one-past-the-end rejection, `usize` overflow, and
  out-of-bounds slice windows.
- Integration tests `memory_isolation_qemu_aarch64` and `syscall_dispatch_qemu`
  exist under `tests/integration/`.
- `lib/caps`, `kernel/{sec, ipc, syscall}` already carry proptest models and
  fuzz harnesses.

New work targets the CVE classes these do **not** yet hit.

### 1.3 Honest reach (§2.1, §2.6)

RustOS today has **no SMEP/SMAP/`clac`/`stac`** and **no live ring-3 user
mode**; KPTI/IBPB are `Pending` and tied to **Stage 6** (`sidechannel.rs`
states this). Tests for the full ring-break classes (SMAP bypass,
`copy_from_user` faults) are therefore mostly Stage-6 work. Today we land:

1. host-level invariant tests against code that already exists,
2. QEMU CPU-control-register / entry-path tests that **gate** Stage 6, and
3. negative tests asserting the boundary is correctly absent / fail-closed.

A Stage-6-gated test is written **now as a real conformance target the feature
must pass**, never as an `#[ignore]` stub (§2.5).

## 2. Test forms (where each test lives, §7)

- **Host unit tests** — next to the code under `#[cfg(test)] mod tests`, or a
  `_tests.rs` sibling if the file would exceed 500 lines (§7).
- **QEMU integration tests** — under `tests/integration/<name>/`, mirroring
  `memory_isolation_qemu_aarch64` and `syscall_dispatch_qemu`. Used for
  anything needing real hardware semantics (control registers, `swapgs`,
  `sysret`, page faults).
- **Property tests** — `proptest`-style, folded into `cargo xtask proptest`
  (`--quick` per PR, `--soak` nightly, §19.7).
- **Fuzz harnesses** — `cargo-fuzz`/in-tree, folded into `cargo xtask fuzz`
  (`--quick` per PR, `--soak` nightly, §19.6). Crashing inputs join the
  regression corpus with an accompanying unit test (§19.6, §7).

## 3. Deliberate corruption / fault-injection tests

These exercise the **detector**, not the preventer: assume prevention already
failed, scribble over a sensitive structure through a sanctioned `#[cfg(test)]`
trapdoor, and assert the OS notices and **fails closed** (`Result::Err`, never
UB, never `panic!`).

### 3.1 The sanctioned mechanism

`slab.rs::poke_for_test(&mut self, offset, byte)` is the model: a
`#[cfg(test)] pub(crate)` one-byte trapdoor into raw storage, documented as
test-only. **Every** deliberate-corruption capability follows this pattern:

- gated behind `#[cfg(test)]`, never present in a release build,
- never leaks `unsafe` across a crate boundary (§2.10),
- never a production code path.

### 3.2 Slab metadata corruption (CWE-787 / CWE-416 / CWE-415)

- **Tag tamper.** Add a `#[cfg(test)]` trapdoor over `tags[]`; flip the tag of
  an in-use slot, then call `slot_mut`/`free` with the original handle and
  assert `SlabError::TagMismatch`. (Today `slab.rs` only tests the *rotation*
  path; this tests metadata tampering directly.)
- **`in_use[]` bitmap tamper.** Flip a freed slot's `in_use` bit to `true`
  (simulated freelist-corruption primitive); assert `alloc`/`free`/guard-check
  stays consistent and never hands out an aliased live slot.
- **Inter-slot / off-by-one guard scribble.** Extend the guard tests to poke at
  `object_size`, `object_size - 1`, `slot_stride ± 1`, the inter-slot guard, and
  the trailing guard; assert `GuardViolation` at the next `check_guards` /
  `alloc` / `free`.
- **Double-free / cross-cache confusion.** Free a handle, reallocate the slot,
  then free the *old* handle: must be `TagMismatch`/`UnknownHandle`, never a
  real free. Extend to interleaved multi-slab scenarios.

### 3.3 Stale-data / info-leak after corruption (CWE-908 / CWE-200)

- In `sensitive.rs`: write a recognisable credential pattern, free, then **poke
  the freed region back to non-zero** (simulate "zero-on-free skipped/
  corrupted"), realloc the same slot, and assert the reuse path returns
  all-zero (or detects the dirty slot). Proves zero-on-free is an *enforced*
  invariant, not incidental (§4).

### 3.4 Audit-log tampering (§19.4, CWE-345 / CWE-347)

Highest-value "corrupt a sensitive location and catch the fallout" target:

- Build a short hash-chained log under `/System/Logs` semantics; **mutate one
  entry's bytes in place** and assert chain verification reports a discontinuity
  (itself a security event, §19.4) and identifies the broken link.
- **Truncate the chain** without `CAP_LOG_ROTATE` and assert it is
  detected/rejected (fail-closed). Pure host tests; directly model log-forging
  CVEs.

### 3.5 CFI / descriptor-table corruption (§19.2, §10)

- Corrupt a CFI type-tag (derived from the §9 syscall-interface hash) and assert
  indirect-call dispatch refuses the target at the boundary — a load/dispatch-
  time refusal, not a runtime crash.
- For `gdt.rs`/`idt.rs`: corrupt a descriptor field in the in-memory table
  (kernel-CS DPL, an IST pointer) and assert the validator / `Idt::load` rejects
  the malformed table before it is installed.

### 3.6 DMA descriptor corruption (CWE-1257, Thunderclap-class)

- In `kernel/mem/dma` / `lib/virtio/queue.rs`: corrupt a device-supplied
  descriptor so its address/length escapes the granted region; assert it is
  rejected (fail-closed, §5.4). Fold malicious ring indices/lengths into a
  `cargo-fuzz` harness (§19.6) — the natural home for randomized corruption.

### 3.7 Property invariant over single-byte corruption

`proptest`: for any single-byte corruption at any offset in
`{guards, tags, in_use, log entry}`, the next operation either rejects or
repairs, and **never** returns a live aliased object or a verified-but-forged
log. Folds into `cargo xtask proptest --quick`/`--soak` (§19.7).

## 4. Memory-corruption CVE classes → tests

- **Heap/slab overflow & guard bypass (CWE-787; SLUB OOB writes).** Adversarial
  writes at slot/guard boundaries (see §3.2) plus a property test over random
  `(object_size, slot_count, offset, len)`: any write escaping `[offset,
  offset+len)` of a slot is detected and never silently corrupts a neighbour.
- **UAF under tag exhaustion (CWE-416, §19.10).** Allocate/free longer than
  `tag_count`; assert a stale handle's tag *collides* (document software tagging
  is probabilistic, §19.10) and that the deterministic **double-free** and
  **unknown-handle** backstops still catch it.
- **Integer-overflow → undersized allocation (CWE-190; `kmalloc(n*size)`).**
  Fuzz/property coverage on every size-computing path in `frame.rs`, `dma`,
  `vmm.rs`: `checked_*` everywhere, `OutOfRange`/`Result` never panic (§4).
- **Uninitialised-memory leak on reuse (CWE-908/200).** See §3.3; pair with a
  `kernel/mem` test that any allocation that ever held credentials/keys/
  capability tokens routes through the zero-on-free path (§4).
- **DMA/IOMMU escape (CWE-1257; Thunderclap).** See §3.6.

## 5. Privilege-boundary / ring-break CVE classes → tests

- **RFLAGS not sanitised on `syscall` entry (CWE-696; AC/DF/TF class).** Host
  test against `syscall_entry.rs::fmask_value()`: `IA32_FMASK` clears at minimum
  TF, DF, IF, AC, NT and the IOPL bits — a malicious user RFLAGS cannot carry
  `AC=1` (SMAP bypass) or `DF=1` (string direction) into the kernel. *Landable
  today; pure host test.*
- **Stack-pivot / missing GS swap (CVE-2019-1125, `swapgs`).** QEMU test
  (sibling to `syscall_dispatch_qemu`): enter via a real `syscall` from a
  low-privilege context; assert the kernel ran on `rsp0`, not the user stack,
  and `gs` was kernel-side during dispatch. Host test: `install_kernel_rsp0`
  rejects a non-canonical / user-range `rsp0` (fail-closed).
- **`sysret` non-canonical RIP → #GP in ring 0 (CVE-2012-0217).** QEMU test:
  return to user with a non-canonical saved RIP; assert the fault is taken in
  user context / handled, never as a user-controlled kernel #GP. *Dedicated
  integration test before Stage-6 user mode lands.*
- **GDT/TSS/IDT integrity (call-gate / IST CVEs).** Host tests in
  `gdt.rs`/`idt.rs`: kernel CS is DPL0; user CS/SS are DPL3; TSS `rsp0`/IST
  point at guard-paged kernel stacks; `Idt::load` rejects a malformed table;
  double-fault/NMI use a separate IST stack (a kernel-stack overflow cannot
  recurse).
- **SMEP/SMAP/UMIP/WP control bits (CWE-862; ret2usr / SMAP-bypass).** *Not yet
  implemented.* Add the bits to the Arch-HAL CPU-control surface and write the
  §17.2/§19.1 QEMU conformance test asserting `CR4.SMEP`/`CR4.SMAP` and
  `CR0.WP` are set and that a kernel read/exec of a user page faults. Until the
  bits land this is the **spec that gates the feature** — a real failing-until-
  implemented target, **not** `#[ignore]` (§2.5).
- **`copy_from_user` TOCTOU & non-canonical user pointers (CWE-367 / CWE-822).**
  Land host validators now (`ptr.rs`-style: reject non-canonical, reject
  kernel-range, reject wrap) and extend `kernel/syscall/tests/fuzz_args.rs` with
  pointer-shaped adversarial inputs. Full per-access fault handling is Stage-6.
- **Side-channel transition barriers (§19.1; Spectre/MDS/L1TF).** Conformance
  assertions the charter already mandates: syscall-entry barrier present
  (`lfence`), context-switch buffer flush present (`verw`), and
  `MitigationProfile::is_release_ready()` stays **false** while KPTI/IBPB are
  `Pending` (a negative honesty test that the port reports itself
  non-shippable). These guard against regression; they do not "find" a
  microarchitectural leak in a unit test.

## 6. Effectiveness — honest assessment (§2.6)

- **Highest yield, landable today (host + QEMU):** RFLAGS/FMASK sanitisation,
  `swapgs`/stack-pivot, `sysret` non-canonical RIP, GDT/TSS/IST invariants,
  slab overflow/guard adversarial writes, integer-overflow allocation fuzzing,
  zero-on-free info-leak, DMA descriptor fuzzing, slab metadata fault-injection,
  audit-log chain-tamper. All map to named CVE classes against already-landed
  code.
- **Highest yield but Stage-6-gated:** SMEP/SMAP/WP enforcement,
  `copy_from_user` TOCTOU/pointer validation, KPTI. Write the
  conformance/spec tests now as the gate; implement in Stage 6.
- **Regression-guard, not bug-finding:** the side-channel barrier /
  `is_release_ready` honesty tests.
- **What these tests cannot prove:** they validate the *detector*, not that
  corruption is impossible; software tagging is probabilistic (tag wrap,
  §19.10). State this in the test. Hardware-enforced detection (MTE faults,
  `CR0.WP`, SMEP/SMAP page faults on a corrupted PTE) is Arch-HAL/Stage-6-gated.

## 7. Definition of Done (§7)

Every test here must go green under the **whole-project** gate, never a `-p`
subset:

1. `cargo fmt --all` (verify with `cargo fmt --all --check`).
2. `cargo xtask ci` — clippy `-D warnings`, deps-check, cfg-check, the QEMU
   test matrix, docs-check, `cargo deny`, the `--quick` fuzz/proptest gates,
   model-check, spec-review, crypto constant-time, abi-check.
3. `cargo xtask fuzz --secs 5` (on top of the `--quick` gate).
4. Anything else `.github/workflows/ci.yml` runs (e.g. `tools/ci/soak.sh`).

New fuzz/proptest harnesses fold automatically into the `--quick` PR gate and
the nightly `--soak`. Any failure found — yours or pre-existing — is fixed or
reverted before the task is done (§2.5, §7).

## 8. Suggested landing order

1. **Adversarial memory tests** (already-landed code, no Stage-6 deps): slab
   guard/UAF-wrap/overflow adversarial writes, zero-on-free info-leak, DMA
   descriptor fuzz, integer-overflow allocation fuzz.
2. **Metadata fault-injection** (extends `poke_for_test`): `tags[]`/`in_use[]`
   corruption trapdoors, tag-tamper and dirty-slot-reuse tests, single-byte
   corruption `proptest`.
3. **Audit-log chain-tamper** tests (§19.4).
4. **Ring-boundary QEMU tests:** `fmask`/RFLAGS, `swapgs`/stack-pivot, `sysret`
   non-canonical RIP, GDT/TSS/IST invariants.
5. **Stage-6-gated conformance specs:** SMEP/SMAP/`CR0.WP`, `copy_from_user`
   TOCTOU, KPTI — written now as the gate the feature must pass.

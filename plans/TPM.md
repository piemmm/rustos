# TPM.md — Trusted Platform Module support (all versions) + a no-TPM boot fallback

This is a staged build plan for adding **Trusted Platform Module** support to
TAIRiX across **every known TPM version and transport**, plus a **fail-closed
fallback for machines without a TPM** (an at-boot "enter your volume
key/passphrase" prompt). It is **binding under `AGENTS.md`**; read `AGENTS.md`
and `PLAN.md` first — every rule in both applies here without exception.

This plan exists because the charter requires a new driver class, a new
`drivers/` top-level subtree, new capabilities, a new versioned service ABI,
and (for the headline use case) a new boot-time key-provider seam to be
**proposed and approved in a plan file before any of it is built**
(`AGENTS.md` §3, §8, §9, §16.4, §15.2). The plan drives those `PLAN.md` /
`AGENTS.md` / §3 amendments as its stages land — it does not pre-write them.

## 0. Scope and decisions (binding for this plan)

- **A TPM protects keys; it does not bulk-encrypt disks.** The headline use
  (the encrypted boot/root volume) is "**seal the volume key to the TPM, gated
  on a measured boot chain**" (`TPM2_Seal`/`Unseal` bound to PCRs), *not* "the
  TPM encrypts the disk". The encryption already exists and is `lib/crypto`-
  backed (`drivers/filesystem/arxfs/src/crypto.rs`); the TPM is a **key
  protector** that plugs into the existing caller-supplied `VolumeKey` seam.
- **Measured boot is a hard prerequisite for the sealing use case.** Sealing a
  key to a TPM **without** measuring the boot chain into PCRs is theatre: an
  attacker who swaps the kernel still gets the key released. So the boot-volume
  value comes from the *pair* "measured boot into PCRs" + "key sealed to a PCR
  policy" + "fail-closed unseal", with a recovery fallback. Stages are ordered
  to reflect this (T6 depends on T5).
- **TPM lives in user space (`AGENTS.md` §4, microkernel-leaning).** The TPM
  command stack (marshalling, sessions, PCR/NV logic, sealing policy) is a
  user-space system service under `/System/Services/`, reached through a
  **versioned, hashed, capability-checked IPC/ABI** held to the exact discipline
  of the syscall table (§9) and the System Information API (§16.6). No ad-hoc
  syscalls are added.
- **The TPM response parser is untrusted input (`AGENTS.md` §19.5/§19.6).**
  Everything the device returns is hostile until proven otherwise: response
  decoding runs in a minimum-capability sandbox process and carries a fuzz
  harness + regression corpus from the stage it is introduced.
- **Roll our own command marshalling; never hand-roll crypto (`AGENTS.md`
  §2.12).** The TCG command/response codec, sessions, and policy logic are
  first-party Rust. Any cryptography (HMAC sessions, KDFs, parameter
  encryption, the unsealed-key handling) goes through the audited `lib/crypto`,
  never a new primitive.
- **`abi-v1` is not frozen yet (`AGENTS.md` §9, §2.13).** New `CAP_TPM_*`
  capability rows, the TPM service ABI types, and any `lib/abi` additions are
  added **in place** now; no `v2`-beside-`v1`, no compat shim. They freeze only
  at the first release.
- **No stubs (`AGENTS.md` §15.1).** Each stage ships code **plus** tests **plus**
  docs and is only "done" when the whole-project gate (§7) is green.

## 1. The version/transport matrix (what "all versions known" means)

The cost of "all versions" is in the **transports and the command set**, not in
the OS-facing service (which is largely version-agnostic at the top). The
matrix this plan must ultimately cover:

- **TPM 2.0** — the TCG TPM-2.0 command set. The dominant target. Transports:
  - **TIS** (TPM Interface Specification) over MMIO, with the locality model.
  - **CRB** (Command/Response Buffer) over MMIO.
- **TPM 1.2** — the legacy TCG 1.2 command set and structures (a *different,
  incompatible* payload format from 2.0), typically over the same **TIS**
  transport.
- **fTPM / firmware TPM** (ARM TrustZone fTPM, Intel PTT, AMD fTPM) — reached
  via a **secure-monitor / firmware call**, not MMIO.
- **Discrete TPM over a serial bus** — **LPC**, **SPI**, or **I²C** dTPM parts.
- **Emulated** — the QEMU `tpm-tis` / `tpm-crb` device backed by **swtpm**, the
  conformance and CI target where real silicon is not emulable (§8).

Each transport is its own driver crate against the §8 driver-class trait, each
routes through hardware-tree discovery (§18: `lib/abi/src/hwtree.rs`, ACPI on
x86_64, FDT on aarch64/riscv64, via the Arch HAL — **no** `cfg(target_arch)`
outside `kernel/arch/<target>`), and each gets a QEMU integration test where
emulable (§8). "All versions" roughly multiplies the driver/transport/test work
by the number of transports; the service, capability, sandbox, and fallback
work is shared and done once.

## 2. Current state (verified in-tree — the seams that exist)

- **No TPM of any kind.** The only trace is a comment in
  `lib/rng/src/hardware.rs` naming a TPM as one *possible* hardware entropy
  source. There is no `drivers/security/`, no `tpm` driver class in
  `lib/abi/src/driver/` (the classes are block/bus/display/dma/filesystem/
  input/mmio/msix/net/port_io/virtio*), and no `CAP_TPM_*`.
- **No measured boot, no PCRs, no secure-boot chain.** TAIRiX does per-boot
  KASLR (§19.2) but does not measure or verify its own boot stages into a
  hardware root of trust.
- **The reusable seams that *do* exist:**
  - The `HardwareRng` trait (`lib/rng/src/hardware.rs`) — a TPM's
    `TPM2_GetRandom` can feed `HardwareEntropy` as **one** XOR-mixed input,
    never trusted alone (§22, and the file's own "a vendor RNG could be weak or
    backdoored" note).
  - `lib/crypto` AEAD/KDF/HMAC/hash primitives (§2.12).
  - The caller-supplied `VolumeKey` injection point at the composition root
    (`drivers/filesystem/arxfs/src/crypto.rs` — ARXFS stores only the
    AEAD-wrapped master key; a `VolumeKey` is supplied to unseal it, the same DI
    seam encrypted swap and `init`/`login` use). This is exactly where a
    TPM-backed key provider attaches.
  - The capability table (`lib/abi/src/capability.rs`, currently ending at
    `RLIMIT_RAISE = Self(20)`) — new `CAP_TPM_*` rows extend it.

## 3. Work breakdown (stages)

Each stage is a reviewable chunk. **At the end of each stage, write the
continuation prompt for the next stage to `.junie/next-tpm-prompt.md`**
(overwrite each time — git is the history, §13), recording what landed and the
exact next work, in the style of the other `.junie/next-*-prompt.md` files. Do
**not** create that file until the first stage actually lands; this plan only
references it. Do not start a stage before its predecessor is green on the
whole-project gate (§7).

### Stage T0 — RNG-entropy slice (smallest, highest value, no service yet)

**Status: planned.**

The natural first increment, deliverable on its own: feed a *present* TPM's
`TPM2_GetRandom` into the kernel RNG as one additional `HardwareRng` /
`HardwareEntropy` input. This proves the discovery + a minimal TIS transport
end-to-end without any of the service/sealing machinery, and it is genuinely
low-effort, high-value.

**Deliverables**
- A minimal TPM 2.0 TIS transport sufficient to issue `TPM2_GetRandom` and read
  the response, behind the §8 driver trait introduced in T1 (so T0 may land
  *after* T1's trait, or introduce a deliberately minimal internal seam that T1
  generalises — sequence decided when T1 lands; do not duplicate, §2.2).
- Wire its output as one XOR-mixed `HardwareRng` source (`lib/rng`), **never**
  trusted alone, **never** passed through to callers as final output (§22).
- The response read is treated as untrusted input (§19.5/§19.6).

**Tests** — host test of the GetRandom response decode (fail-closed, no panic);
a QEMU `swtpm`/`tpm-tis` integration test that the source contributes entropy;
the RNG mixing test that a weak/empty TPM source cannot weaken output (§22).

**Docs** — `docs/src/security/` entropy-sources note; cross-link `lib/rng`.

### Stage T1 — Driver class trait + `drivers/security/` subtree + capabilities

**Status: planned.**

**Deliverables**
- A new driver-class trait in `lib/abi/src/driver/tpm.rs` (POD command/response
  buffers + the transport-level operations), with drivers exposing the single
  `register(host) -> Result<DriverHandle, DriverError>` entry (§8). Get the
  interface right first time (§2.4 no interface creep).
- A new `drivers/security/` top-level subtree (the home for `tpm_tis/`,
  `tpm_crb/`, `tpm_ftpm/`, `tpm_spi/`, … per-transport crates). **Amends
  `AGENTS.md` §3 and `PLAN.md`** in the same change (§3).
- New capabilities in `lib/abi/src/capability.rs` (next free ids after
  `RLIMIT_RAISE = Self(20)`): at minimum a *use* capability and an
  *owner/admin* capability — e.g. `CAP_TPM_USE`, `CAP_TPM_ADMIN` (final split
  decided in review). Regenerate the C header (`cargo xtask c-header --write`);
  `abi-check`/`c-header` enforce drift (§9).

**Tests** — trait/host tests against a mock TPM host; capability table tests
(ids, names, `from_raw`) mirroring the existing ones; the generated-header
completeness test.

**Docs** — `docs/src/drivers/tpm.md` (class overview, supported hardware
matrix, required capabilities); per-crate `README.md` stability tiers (§6).

### Stage T2 — TPM 2.0 command codec + user-space TPM service + sandboxed parser

**Status: planned.** Depends on T1.

**Deliverables**
- A first-party TPM 2.0 command/response codec (TCG marshalling), HMAC/policy
  sessions over `lib/crypto`, and the core command surface
  (startup, `GetCapability`, PCR read/extend, `GetRandom`, NV, create/load,
  seal/unseal) — version-agnostic at the service boundary.
- A user-space TPM service under `/System/Services/`, exposed through a
  **versioned, hashed, capability-checked** IPC/ABI in `lib/abi`
  (same discipline as §9/§16.6). Every method declares its required capability;
  the kernel enforces at dispatch (§5.2/§5.4); the service fails closed.
- The response parser runs in a §19.5 **minimum-capability sandbox process**
  (only its device/IPC endpoint, nothing else) with a §19.6 fuzz harness +
  regression corpus.
- **Secret hygiene (§4, §23.1):** auth values, session secrets, sealed blobs,
  and any unsealed key material are zeroed on free, never hit unencrypted swap
  (swap is already encrypted, §4), never reach logs or `stdinfo` (§20).
- **Audit (§19.4):** ownership/policy/seal/unseal operations emit stable
  hash-chained `lib/log` event IDs.

**Tests** — host codec round-trips (every command/response, fail-closed on
malformed responses); sandbox containment (a crashing parse returns an error,
replaces the sandbox, logs — never tears down the service); fuzz registration;
capability-gate + audit tests; a QEMU `swtpm` round-trip for the core commands.

**Docs** — `docs/src/security/tpm.md` (service model, capability map, sandbox);
the service crate `README.md`.

### Stage T3 — TPM 1.2 command set

**Status: planned.** Depends on T1, T2.

**Deliverables**
- The legacy TPM 1.2 command set/structures as a distinct codec (incompatible
  payloads from 2.0) behind the same service ABI, over the TIS transport. The
  service negotiates the device version and selects the codec; no caller-facing
  ABI fork (§2.13).

**Tests** — host 1.2 codec round-trips; a QEMU `swtpm --tpm2=off` (1.2)
integration test; the version-negotiation test.

**Docs** — extend `docs/src/security/tpm.md` with the 1.2 support matrix +
known limitations.

### Stage T4 — Remaining transports (CRB, fTPM firmware-call, SPI/I²C/LPC dTPM)

**Status: planned.** Depends on T1, T2 (each transport is an independent
increment; land them one at a time, each with its own tests/docs).

**Deliverables**
- `tpm_crb/` (MMIO CRB), `tpm_ftpm/` (secure-monitor/firmware call — the
  firmware-call primitive lives behind the Arch HAL §17.2, never
  `cfg(target_arch)` outside `kernel/arch/<target>`), and the serial-bus dTPM
  crates (`tpm_spi/`, `tpm_i2c/`, LPC), each against the §8 trait, each matched
  via the hardware tree (§18).

**Tests** — a QEMU integration test per transport where emulable (CRB via
`tpm-crb`; document where a transport is not QEMU-emulable and how it is
otherwise exercised, §8).

**Docs** — extend the support matrix; per-crate `README.md` (supported
hardware, required caps, limitations).

### Stage T5 — Measured boot (the prerequisite for sealing)

**Status: planned.** Depends on T2. **This is the hard part and the gate for
T6.** Without it, T6 (TPM-sealed volume key) is not worth doing.

**Deliverables**
- A per-arch Arch HAL primitive (§17.2) to **extend PCRs** from the boot chain —
  firmware → bootloader → kernel → critical config — building the measured-boot
  hash chain into the TPM. Architecture-specific measurement lives only under
  `kernel/arch/<target>/`; the rest of the kernel sees a normalised contract.
- The PCR-extend path is fail-closed and audited (§19.4); a measurement
  discontinuity is itself a security event.

**Tests** — host tests of the measurement contract; a QEMU `swtpm` test that
PCRs reflect the expected measured chain and that tampering changes them; the
Arch HAL conformance vertical for the primitive (§17.2).

**Docs** — `docs/src/security/measured-boot.md`; cross-link the threat model
(§19) and `docs/src/security/tpm.md`.

### Stage T6 — TPM-sealed `VolumeKey` provider for the encrypted boot volume

**Status: planned.** Depends on T2 (service) and T5 (measured boot).

**Deliverables**
- A **TPM-backed `VolumeKey` provider** that plugs into the existing
  composition-root seam (`drivers/filesystem/arxfs` crypto + the DI point):
  seal the volume key to a PCR policy at install time (§11 installer flow),
  unseal at boot, **fail closed** to the recovery path on PCR mismatch — never
  fall back to an unprotected key (§5.4).
- Seal/unseal and any PCR-policy change are capability-gated (`CAP_TPM_*`) and
  audited (§19.4); the unsealed `VolumeKey` is zeroed on free and never reaches
  swap/logs/`stdinfo` (§4, §23.1).
- Installer integration (§11): expert mode never lays out plaintext; the
  default seals to TPM when present and always provisions the T7 recovery
  passphrase.

**Tests** — unseal succeeds on matching PCRs; **fails closed** on mismatch and
hands off to recovery (T7); QEMU `swtpm` end-to-end seal-at-install /
unseal-at-boot; the §21 timestamp discipline where applicable.

**Docs** — `docs/src/security/encrypted-boot.md` (the measured-boot + sealed-key
+ recovery model); update the arxfs/installer pages.

### Stage T7 — No-TPM fallback: at-boot passphrase/volume-key prompt

**Status: planned.** Depends on the `VolumeKey` seam (independent of T5/T6; can
land alongside T6). This is the requested fallback for machines **without** a
TPM **and** the fail-closed recovery path when a TPM unseal is refused (e.g. a
legitimate firmware update changed the PCRs — the LUKS+TPM2 / BitLocker model).

**Deliverables**
- A boot-time **passphrase/recovery-key prompt** key provider implementing the
  same composition-root `VolumeKey` seam: prompt the user over the standard
  streams (fd 0/1/2, §20 — never a device syscall), derive the wrapping key via
  `lib/crypto` KDF, and unwrap the master key. A wrong key fails the AEAD auth
  and the mount is **refused, fail-closed** (§5.4) — it never falls back to an
  unprotected key; it re-prompts within a bounded retry policy (no busy-loop,
  §2.1).
- Selection logic at the composition root: **TPM-sealed provider when a TPM is
  present and unseal succeeds; otherwise the passphrase provider** (no TPM, or
  TPM unseal refused → recovery). The choice is explicit, audited (§19.4), and
  never silently weakens (§2.17).
- Installer (§11): always provisions a recovery passphrase, even when sealing to
  a TPM, so a PCR change cannot brick the machine.

**Tests** — correct passphrase unlocks; wrong passphrase is refused and
re-prompts within the retry bound; no-TPM machine uses the prompt; TPM-present
machine that fails unseal falls through to the prompt; the prompt reads/writes
only inherited standard streams (§20), never a console syscall.

**Docs** — `docs/src/security/encrypted-boot.md` recovery section; the installer
page.

## 4. Security posture (binding for every stage)

- **TPM responses are hostile (`AGENTS.md` §19.5/§19.6).** All device output is
  parsed in a minimum-capability sandbox, fail-closed, fuzzed, with a regression
  corpus. A parser crash never brings down the TPM service.
- **No new ambient authority (`AGENTS.md` §4, §5.2/§5.4).** Every TPM service
  method is capability-gated and checked kernel-side at IPC dispatch; the
  service holds only the device/IPC capabilities it needs.
- **Secrets are zeroed on free and never leak (`AGENTS.md` §4, §23.1, §20).**
  Auth values, session secrets, sealed blobs, unsealed volume keys: zero on
  free, no unencrypted swap, never in logs or `stdinfo`.
- **The TPM never bulk-encrypts and is never trusted as a sole RNG
  (`AGENTS.md` §22).** It protects keys (sealing) and contributes one mixed
  entropy input; final randomness is the kernel CSPRNG's.
- **Fail closed, never defer a defence (`AGENTS.md` §2.9, §2.17, §5.4).** A
  failed unseal, a PCR mismatch, a malformed response, or a missing capability
  denies and routes to recovery — it never widens authority or weakens to make
  a path "work".
- **Audit (`AGENTS.md` §19.4).** Ownership, policy changes, seal/unseal, and
  measured-boot extensions emit stable hash-chained log event IDs.

## 5. Charter amendments this plan drives (as stages land, §3/§16.4)

- **`AGENTS.md` §3:** add the `drivers/security/` subtree (and its TPM transport
  crates) to the authoritative repository layout — lands with T1.
- **`lib/abi/src/driver/`:** the new `tpm.rs` driver class — lands with T1.
- **`lib/abi/src/capability.rs`:** new `CAP_TPM_*` rows — land with T1
  (regenerate the C header).
- **`PLAN.md`:** a TPM stage entry referencing this plan, advanced as T0–T7
  land — touched by every stage.
- **`AGENTS.md` §16.4:** only if any TPM client library is offered as a curated
  `/System/Libraries/` class (decide in T2; if so, amend §16.4 + `PLAN.md` in
  the same change). The TPM *service* itself is a `/System/Services/` component,
  not a shared-library class.

## 6. Definition of done (per stage and overall)

Per `AGENTS.md` §7, over the **whole project** (never `-p`), and quote the
output:
1. `cargo fmt --all` (verify `cargo fmt --all --check`).
2. `cargo xtask ci` (clippy `-D warnings`, deps-check, cfg-check, the test
   matrix, docs-check, `cargo deny`, supply-chain, the `--quick` fuzz/proptest
   gates, model-check, spec-review, crypto constant-time, abi-check, c-header).
3. `cargo xtask fuzz --secs 5`.
4. Anything else `.github/workflows/ci.yml` runs (e.g. `tools/ci/soak.sh`). On a
   developer machine (that's us) `tools/ci/soak.sh` runs for a **maximum of 20
   seconds** (`tools/ci/soak.sh both --secs 20`); the unbounded 24 h soak is for
   the CI/soak host only, never a developer machine.

Any failure found — new or pre-existing — is fixed or reverted before the stage
is done (§2.5, §7). Update `PLAN.md` and this file's stage statuses as stages
advance, and refresh `.junie/next-tpm-prompt.md` for the next chunk (overwrite;
state only the current plan/state, not history, §13).

## 7. Charter cross-references

§1 (Rust-only OS; the TPM stack is Rust, the per-arch firmware-call / MMIO
primitives use the §17.2 Arch HAL), §2.2 (one definition, no duplication, no
sibling-collapsing), §2.4 (no interface creep — get the driver trait right),
§2.9 (no panic / fail closed), §2.12 (roll our own codec; never hand-roll
crypto — use `lib/crypto`), §2.13 (no compat shim; `abi-v1` mutable pre-release),
§2.17 (never defer/weaken a defence), §3 (`drivers/security/` layout), §4 (no
ambient authority; encrypted swap; zero-on-free), §5.2/§5.4 (capabilities,
kernel-side checks, fail closed), §6 (lib/crate registration + stability tiers),
§7 (whole-project gate; coverage), §8 (driver class, `register`, QEMU tests),
§9 (versioned/hashed ABI), §11 (installer: seal-or-passphrase, no plaintext),
§16.4 (curated libs — only if a TPM client lib is offered), §16.6 (sysinfo-style
ABI discipline), §17.2 (Arch HAL: firmware-call/measurement primitives), §18
(hardware-tree discovery, no `cfg(target_arch)` leakage), §19.2 (boot
hardening), §19.4 (hash-chained audit), §19.5/§19.6 (parser sandbox + fuzzing),
§20 (standard-stream prompt for the fallback), §21 (`Time64`), §22 (kernel RNG;
TPM as one mixed entropy input only), §23 (review/acceptance gate).

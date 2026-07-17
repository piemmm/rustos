# UNIVERSAL.md — Universal app distribution: multi-slice bundles + a Wasm app tier

This is a staged plan for letting a vendor publish **one** artifact that runs
on every TAIRiX architecture, instead of per-arch downloads. It is **binding
under `AGENTS.md`**; read `AGENTS.md` and `PLAN.md` first, and `plans/APPS.md`
for the application-bundle model this plan extends. Every rule in both applies
here without exception. This plan exists because the charter requires a new
interface surface to be proposed in a plan file before it is built
(`AGENTS.md` §15.2), and because the Wasm tier adds a large new trusted
component that must be scoped before any of it lands.

Status: **planned** (nothing in this plan has been built).

## 0. Scope and decisions (binding for this plan)

The design question — a multi-arch "fat" `rxe`, a platform-neutral
pseudo-compiled format, or an in-OS Rust compiler — collapses onto facts
already in the tree, and this plan fixes the answers:

- **The `.app` bundle, not the `rxe` file, is the universal unit.** `rxe`
  stays exactly one ELF-derived format with a signed manifest (§9); we do
  **not** add a Mach-O-style fat container as a second format layer. A fat
  *file* complicates the load-time signature/interface-hash checks (which
  slice was signed? which hash applies?), bloats the loader, and carries
  foreign-arch dead weight on disk. The bundle (§16.5) is already the signed,
  self-contained distribution unit; "universal" is a property of the bundle.
- **`abi-v1` is one architecture-neutral contract.** The syscall table is
  generated from the single `lib/abi/src/syscalls.rs` source for every target
  (§9) and `AppInfo` already pins ABI version + interface hashes. A universal
  app is therefore several machine-code encodings of the *same* contract —
  the ABI itself needs no change.
- **The platform-neutral format is WebAssembly, never a bespoke bytecode.**
  `wasm32-unknown-unknown` is already Tier-1 (§1), so a portable executable
  format is a second consumer of an existing commitment, not an invention. A
  hand-rolled VM/verifier is precisely the security-critical machinery where
  an existing, formally specified, massively fuzzed design beats a first-party
  one — the same reasoning that exempts cryptography from roll-your-own
  (§2.12). Wasm has a small formal spec, deterministic validation,
  linear-memory sandboxing, a mature `rustc` backend, and CPU-neutrality by
  construction.
- **Execution of the Wasm tier is install-time AOT, never a runtime
  in-process JIT as the default execution mode.** `CAP_JIT_MAP_EXEC` and the
  W^X invariants (§19.2) are the exact primitives an install-time compiler
  needs: translate once at install in a sandboxed service, cache the native
  artifact, keep the hot path fully native (§2.16), keep codegen out of every
  app's address space, stay deterministic/auditable, and honour "no
  post-install network fetches" (§19.3) because translation is local. This is
  the Android-ART / .NET-crossgen model.
- **No in-OS Rust compiler as the distribution channel.** The charter scopes
  the compiler out of roll-your-own (§2.12 does not extend to `rustc`/LLVM,
  §19.9). Source distribution makes the on-device TCB enormous, install times
  hostile, does nothing for closed-source vendors, and buys no security:
  `unsafe` exists, and TAIRiX already refuses to trust language-level claims —
  third-party native code is hostile by assumption (§16.4, §19); the real
  boundary is hardware isolation + capabilities (§4, §5), not the compiler
  that produced the binary.
- **Safety is not a format property.** Native slices get no trust from being
  "written in Rust": they are hostile code confined by the MMU, the capability
  manifest intersection, PIE/W^X/CFI (§19.2), and fail-closed load gates. A
  Wasm slice adds a genuine *extra* in-process layer (validated control flow,
  bounded linear memory) on top of the same capability confinement — defence
  in depth, never a replacement for it.
- **Evolve in place (§2.13).** TAIRiX is pre-release: `AppInfo` and the fixed
  §16.5 bundle layout gain the architecture dimension by changing the single
  living definition and every consumer in the same change — no `v2` beside a
  `v1`, no compatibility shim.
- **No stubs (§15.1).** Each stage ships code **plus** tests **plus** docs
  and is only done when the whole-project gate (§7) is green.

## 1. Stage U1 — multi-slice bundles (native fat bundles + install-time thinning)

The small, first stage. Delivers "one download, every architecture" for
native apps, and is a prerequisite for the Wasm tier (U2 is just one more
slice kind).

- **Manifest.** `AppInfo` (`lib/abi`) maps each supported target to its `rxe`
  slice — `Run` resolved per-arch, or arch-keyed entries under `Code/` — with
  per-slice interface hashes. The one `AppInfo` signature covers all slices;
  there is no per-slice signing ambiguity.
- **Load gate.** Slice selection lives in the single load gate
  (`lib/appload`), used by both the kernel boot-floor spawn path and `appmgr`
  (§2.2): pick the slice for the running architecture, then apply the existing
  checks (signature, capability intersection, interface-hash verification,
  PIE/W^X/CFI refusal rules) to the chosen slice unchanged. No native slice
  and no U2 fallback slice ⇒ refuse the bundle, fail closed (§5.4).
- **Thinning.** The installer (and `appmgr` on install) may strip foreign-arch
  slices at install time, Apple-app-thinning style, so "universal" costs
  download size, not resident disk. Thinning re-verifies what remains against
  the manifest; a thinned bundle is still a valid, verifiable bundle.
- **This is the only correct answer for hot-path code.** High-intensity games
  and performance-critical apps ship native, per-arch, LLVM-optimised slices
  (SIMD, target features); no portable format pretends to replace that
  (§2.16).
- **Tests/docs.** Unit tests on slice selection (present/absent/foreign
  slices, tie rules, refusal paths), a QEMU vertical loading one universal
  bundle on at least two arches, `docs/src/abi/` + `docs/src/userland/` pages
  updated in the same change (§13).

## 2. Stage U2 — the Wasm app tier (universal fallback slice, install-time AOT)

The large, second stage. **Do not start before U1 lands.** The AOT engine is
the largest new trusted component this plan adds and needs conservative
scoping.

- **Bundle policy.** A bundle may ship native slices, a `wasm32` slice, or
  both; native is preferred where present, the Wasm slice runs on any
  architecture without one. The future-proofing payoff: a fifth architecture
  (or the CHERI Tier-2 target, §19.8) runs the existing catalogue on day one
  with no vendor re-releases.
- **Import surface.** A WASI-style mapping of the Wasm module's imports onto
  `abi-v1` — defined in `lib/abi`, versioned/hashed like every ABI surface
  (§9). No ambient authority: the instantiated module receives only what the
  manifest capability intersection grants (§4, §5.2).
- **AOT service.** A sandboxed (§19.5 minimum-capability) system service
  validates and compiles the Wasm slice **once at install** under
  `CAP_JIT_MAP_EXEC` W^X discipline (§19.2), caching the native artifact in
  the app-scoped cache keyed and bound to the bundle's content hash, so it is
  re-verified on launch and invalidated when the bundle changes. Start with
  validation + a simple, correct compiler; optimise later with measurement
  (§2.16).
- **Hardening.** The validator/compiler parses hostile input: fuzz harnesses
  for it are part of the stage (§19.6), a malformed module fails closed to a
  refusal (§2.9, §5.4), and a compiler crash is contained by the sandbox
  (§19.5). Compiled output obeys the same `rxe`-load invariants (PIE, W^X,
  CFI tag) as any native slice.
- **Tests/docs.** Conformance tests that the same Wasm slice produces the same
  observable behaviour on every Tier-1 arch, refusal tests for malformed /
  hash-mismatched modules and stale cache artifacts, and a
  `docs/src/abi/`/`docs/src/security/` page for the import surface and the
  AOT trust story.

## 3. Explicitly out of scope (never build)

- A bespoke TAIRiX bytecode or first-party VM/verifier.
- A multi-arch fat `rxe` *file* format (the bundle is the fat unit).
- A runtime in-process JIT as the default execution mode for the Wasm tier.
- An in-OS Rust compiler / source distribution as the app channel.

# Memory tagging (Arch HAL)

Use-after-free is a *temporal* memory-safety bug: a pointer is used after
the object it points at has been freed and, usually, reallocated for
something else. Hardware **memory tagging** turns that into a
deterministic fault. Every aligned granule of memory carries a small tag,
every pointer carries a matching tag, and an access whose pointer tag does
not match the granule tag faults. Rotating the tag when a region is freed
— so a dangling pointer keeps the *old* tag — is what hardens
use-after-free: the next access through the stale pointer mismatches and
faults instead of silently reading or corrupting the reallocated object.

Only an architecture port can drive the tag-storage and tag-check silicon
(Arm MTE, SPARC ADI, the RISC-V tagging proposals), so `AGENTS.md` §19.10
makes this a closed trait set on the Arch HAL, modelled on the §19.1
side-channel surface. It lives in `kernel/arch/api` next to the
scheduler-facing and side-channel slices, so a port acquires no new
dependency to implement it.

## The surface

`rustos_arch_api::memtag` defines:

- `MemTag` — a memory tag (`0..TAG_COUNT`). `TAG_COUNT` is 16, mirroring
  Arm MTE's 4-bit tag.
- `next_free_tag(previous, tag_count)` — the architecture-neutral tag
  rotation. Pure and `const`, it returns a tag guaranteed to differ from
  `previous` whenever at least two tags exist. This is the one definition
  of the rotation; both the hardware ports and the software
  tag-checking allocator in `kernel/mem` use it, so they agree on the tag
  space (`AGENTS.md` §2.2 — no duplicated tag algebra).
- `MemoryTagging` — the per-port handle: the tag granule geometry
  (`granule_bytes`, `tag_count`), the `rotate_tag` rotation, and the
  capability-checked region-retag primitive `set_region_tag`.
- `TaggingProfile` — the port's honest declaration, one `Tagging` per
  feature: `tag_storage` (the CPU can store a per-granule tag) and
  `tag_check_faults` (a pointer/granule mismatch faults — the property
  that actually catches a UAF in hardware).
- `Tagging` — one of three honest positions:
  - `Supported` — the port drives the feature on its silicon.
  - `Unsupported(reason)` — permitted **only** where the silicon
    genuinely lacks tagging, with the justification recorded both here
    and in the port's source.
  - `Pending(note)` — the silicon *does* support it, but it cannot be
    enabled yet because it depends on a not-yet-landed subsystem (the
    Stage 6 `Normal Tagged` page attribute and the tag-check-fault
    decode). `Pending` is honest and tracked, but **not** release-ready.

`TaggingProfile::validate` enforces the honesty rule: every non-supported
slot must carry a non-empty explanation. `is_release_ready` is the
stricter gate — it rejects any `Pending` slot.

## The conformance vertical

`rustos_arch_api::memtag::conformance::run_all` is the §17.2 / §19.10
acceptance suite. It is portable — it names only the trait — and every
port runs it against its handle from a host unit test, exactly like the
side-channel suite. It asserts the profile is honest, the tag geometry is
sane (granule a power of two, tag count in range), the rotation actually
produces a distinct tag on a multi-tag port (the UAF-hardening property),
and the retag primitive is callable. Each port additionally pins the
exact profile and geometry its silicon requires.

## Per-target declarations

| Feature | x86_64 | aarch64 | riscv64 | wasm32 |
| --- | --- | --- | --- | --- |
| Tag storage | Unsupported (no MTE) | Pending (Stage 6 enable) | Unsupported (no ext.) | Unsupported (host) |
| Tag-check faults | Unsupported (no MTE) | Pending (Stage 6 attr.) | Unsupported (no ext.) | Unsupported (host) |
| Tag granule | 1 byte | 16 bytes (MTE) | 1 byte | 1 byte |
| Tag values | 1 | 16 (MTE) | 1 | 1 |

- **aarch64** is the canonical hardware-tagging target via the Memory
  Tagging Extension (FEAT_MTE, ARMv8.5-A): a 4-bit tag per 16-byte
  granule, carried in pointer bits `[59:56]`. The port implements the
  `stg` (Store Allocation Tag) store sequence, gated behind a per-handle
  `mte_enabled` flag that defaults **off** — the sequence is compiled and
  reviewed but never executed before Stage 6 probes `ID_AA64PFR1_EL1.MTE`
  and maps `Normal Tagged` memory. Both slots are therefore honestly
  `Pending`, exactly as the §19.1 KPTI / Spectre-v2 slots are.
- **x86_64** has no per-granule memory tagging. Intel LAM and AMD UAI
  mask high pointer bits but store no granule tag and raise no tag-check
  fault, so both slots are a justified `Unsupported`.
- **riscv64** targets (QEMU `virt`, SiFive U54/U74) implement no ratified
  memory-tagging extension — a justified `Unsupported`.
- **wasm32** has no host tagging primitive; spatial safety is the
  sandbox's per-worker bounds-checked linear memory — a justified
  `Unsupported`.

## Software tag check in the slab allocator

Hardware enforcement of UAF (`tag_check_faults`) needs the Stage 6
page-table work on every target, and most targets have no tagging
silicon at all. So `kernel/mem`'s slab allocator hardens UAF **today**,
on every target, in software using the *same* tag rotation. `SlabHandle`
carries the `MemTag` its slot held when the handle was issued; the slab
rotates the slot's tag on every allocation (`next_free_tag`). A handle
that outlives its allocation — used after the slot was freed and
reallocated — carries the stale tag, mismatches the slot's rotated tag,
and is rejected with `SlabError::TagMismatch`. This is the architecture-
neutral analogue of what Arm MTE faults on, and it slots in front of the
existing guard pages and zero-on-free.

## Remaining §19.10 work

The aarch64 `Pending` slots close with the Stage 6 process model: the
`ID_AA64PFR1_EL1.MTE` feature probe, enabling tag checking
(`SCTLR_EL1.ATA`/`TCF`), and the `Normal Tagged` stage-1 page attribute
plus the synchronous-tag-check abort decode. At that point the
`set_region_tag` store path is switched live (`MemoryTags::with_mte_enabled`)
and the allocator's tagged regions are checked by the hardware as well as
the software path.

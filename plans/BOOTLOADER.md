# BOOTLOADER.md — the TAIRiX first-party Rust boot chain

This is the staged, **binding** plan (under `AGENTS.md`) for the TAIRiX
boot chain: the first-party, **Rust-only** loader that finds, verifies,
loads, and starts the TAIRiX kernel on real BIOS/UEFI-class hardware, so
the §12 shippable `images/tairix-x86_64.iso` / disk boots on its own
firmware — not only under QEMU's `-kernel` PVH shortcut.

Read `AGENTS.md` first (especially §1 Rust-only, §2, §5, §9, §11, §12,
§17, §18, §19), then `plans/ARCHSUPPORT.md` (the x86_64 product-parity
plan this unblocks), `plans/TPM.md` (measured boot), and
`plans/DEVICES.md`. Every rule in all of them applies here without
exception.

## 0. Scope and binding decisions

- **Rust only (§1).** The boot chain is TAIRiX code, so it is Rust. We do
  **not** ship or depend on GRUB, and we author no C. The UEFI path needs
  no assembly at all — UEFI firmware hands control to a normal 64-bit PE
  application with paging and long mode already up, and exposes memory,
  disk, and console through boot services. The legacy-BIOS path (B5) is the
  only place the §1 assembly carve-out applies: the irreducible real-mode →
  protected → long-mode bring-up the silicon cannot express in Rust, each
  fragment header-justified and reviewed.
- **Reuse the kernel's existing entry — invent no third protocol (§2.3).**
  `kernel/arch/x86_64/src/boot.s` already accepts **multiboot2** (the
  real-bootloader path) and PVH. The loader hands the kernel off through
  **multiboot2**: it builds a multiboot2 *information* structure (memory
  map, boot command line, ACPI RSDP, and — when a GOP framebuffer is up —
  the framebuffer tag) and enters the kernel's 32-bit protected-mode
  `_start` with `EAX` = the multiboot2 magic and `EBX` = the info pointer.
  QEMU's `-kernel` PVH boot stays exactly as it is for the fast, firmware-
  free test path; the loader is what real firmware uses.
- **The multiboot2 tag layout is one shared definition (§2.2).** The kernel
  *parses* it (`kernel/arch/x86_64/src/multiboot2.rs`); the loader *builds*
  it. Producer and consumer share one `lib/*` definition of the header/tag
  wire format so they can never drift — the loader work extracts that
  shared home and refactors the kernel parser onto it (B2), it does not add
  a second copy.
- **Pure core in `lib/*`, thin firmware shells in `boot/` (§17.4, §2.20).**
  Everything that is firmware-neutral and host-testable — the ELF→load
  plan, the multiboot2 info builder, the FAT/GPT read path, signature and
  measurement policy — lives in `no_std` `lib/*` crates with full host
  tests. Only the genuinely firmware-specific glue (calling UEFI boot
  services; the BIOS real-mode stub) lives in the per-firmware binaries
  under the new top-level `boot/` directory. No board/SoC coupling reaches
  the shared crates.
- **Security is not deferred (§2.17, §5.4, §9, §11).** Before it transfers
  control, the loader **verifies the kernel image's signature** against the
  installation's boot trust anchor (the §11 machine signing key) and
  **measures** it into the TPM where present (`plans/TPM.md`), failing
  closed — never launching an unverified or unmeasured image. The handoff
  honours W^X: no segment is left both writable and executable.
- **Reproducible + pinned (§19.3).** The loader binaries are built by the
  pinned toolchain, are bit-reproducible, and enter the SBOM like every
  other shipped artefact.

## 1. Repository placement (amends `AGENTS.md` §3)

```
boot/                    # First-party Rust boot-chain binaries (per firmware).
├── uefi-x86_64/         #   UEFI application (x86_64-unknown-uefi PE). B3.
└── bios-x86_64/         #   Legacy-BIOS stub + protected/long-mode bring-up. B5.

lib/bootload/            # Pure, no_std, host-tested loader core: ELF64 ->
                         #   validated LoadPlan (PT_LOAD segments, entry,
                         #   physical span, W^X flags) over lib/binfmt. B1.
lib/multiboot2/          # Shared multiboot2 wire layout: the one header/tag
                         #   definition the kernel parser and the loader
                         #   builder both depend on (§2.2). B2.
```

The pure cores are `lib/*` crates (host-testable, `no_std`, no dependency
on `kernel/*`). The `boot/*` binaries are freestanding firmware targets
that compose the pure cores; they are *not* the microkernel and are not
`kernel/*`. `AGENTS.md` §3 is amended to add `boot/` and the new `lib/*`
crates in the same change that creates each.

## 2. Increments (dependency order; each lands complete and green per §7)

### B1 — `lib/bootload` pure loader core (`done`)

The firmware-neutral heart of every TAIRiX loader: given the kernel ELF
bytes, decode them through `lib/binfmt::ElfView` and compute a validated
`LoadPlan` — the `PT_LOAD` segments to place (file source range, physical
destination, memory size with the BSS tail zero-filled, and the W^X
permission flags), the entry point, and the total physical span the
firmware must have free. Pure, `no_std`, alloc-free (a fixed, bounded
segment array — a §24.4 security bound, not a capacity), fail-closed, no
panics: a malformed or hostile image is a typed `LoadError`, never a
partial trust. Host-tested to the §7 bar. This is the input both the UEFI
(B3) and BIOS (B5) shells consume; it decides *what* to place, the shells
decide *how* to place it.

### B2 — shared multiboot2 info builder (`done`)

`lib/multiboot2` is the one `no_std` definition of the multiboot2
header/tag wire layout. The parser (`BootInfo`, borrow-only, allocation-free,
fail-closed) was moved out of `kernel/arch/x86_64/src/multiboot2.rs` — which
is now a thin re-export, so there is no second copy and no dead code — and
the crate adds the loader-side **builder** (`InfoBuilder`): it assembles the
information structure (basic memory, the type-6 memory map, the boot command
line, the ACPI RSDP tag, and the framebuffer tag) into a caller-provided
buffer, bounded and fail-closed (a `BuildError` on overflow or an interior
NUL, never a partial structure). The parser gained the command-line (type 1)
and framebuffer (type 8) tags the builder emits, so the round-trip host tests
assemble every tag and read it back, proving producer and consumer agree; a
fuzz harness (`fuzz_info`) drives both the parse and build surfaces. The
builder does not emit an EFI memory map (type 17) — firmware supplies its own
map, forwarded as type 6.

### B3 — `boot/uefi-x86_64` UEFI application (`planned`)

A Rust `x86_64-unknown-uefi` PE application that, using UEFI boot services
only: opens the EFI System Partition, reads the kernel image and the
`root.unlock` descriptor, **verifies the kernel signature** (B6 policy) and
measures it, obtains the memory map, computes the `LoadPlan` (B1) and the
multiboot2 info (B2), places the segments, calls `ExitBootServices`, and
enters the kernel's multiboot2 `_start`. Adds the `x86_64-unknown-uefi`
target to the toolchain/CI matrix and `cargo xtask` build/deps/cfg checks.
Vertical: an **OVMF** QEMU boot that loads the kernel from the ESP with no
`-kernel` in the argv and reaches the same first-boot witness the PVH path
does.

### B4 — `tools/mkimage` GPT + ESP whole-disk image (`planned`)

GPT-encode in `lib/partition` (parser exists, encoder is new), then the
`--target x86_64` builder in `tools/mkimage`: a GPT disk with an ESP
carrying the UEFI loader (`\EFI\BOOT\BOOTX64.EFI`) + kernel + `root.unlock`,
the read-only `/System` ARXFS store, and the encrypted ARXFS root — reusing
the shared `build_system_partition`/rootfs/appload planting code unchanged
(§2.2), with `installer`/`debug` profiles matching the Pi builder. Host
tests over the layout; the whole-disk OVMF fixture boots the produced image
end to end. This is the `plans/ARCHSUPPORT.md` A1 deliverable, now built on
a real loader instead of the `-kernel` shortcut.

### B5 — `boot/bios-x86_64` legacy BIOS path (`planned`)

The hybrid-BIOS half of the §12 target: an MBR/VBR Rust stub with the §1
real-mode → protected → long-mode assembly carve-out, chaining into the
shared cores (B1/B2) and the same kernel handoff. Hybrid MBR/GPT layout so
one image boots on both BIOS and UEFI firmware. Vertical: a SeaBIOS QEMU
boot of the whole-disk image.

### B6 — verified + measured boot, reproducibility (`planned`)

The signature-verification policy (against the §11 boot trust anchor) and
TPM measurement (`plans/TPM.md`) the loader enforces before launch, as a
shared `no_std` policy the UEFI and BIOS shells both call, fail-closed;
reproducible-build + SBOM entry for the loader binaries (§19.3). Tracked
here so the chain is not "done" while an unverified image can launch.

## 3. Invariants (hold across every increment)

- Rust only; the sole non-Rust text is the B5 BIOS assembly carve-out,
  header-justified and reviewed (§1).
- No board/SoC/`cfg(target_arch)` coupling in the shared `lib/*` cores
  (§2.20); firmware specifics live in `boot/*` and the target's own crate.
- Every consumer of untrusted on-disk/firmware input is bounded and
  fail-closed (§5.4, §24.4); the loader never launches an image it could
  not verify and measure (§2.17).
- Producer/consumer contracts (the multiboot2 wire layout) are one shared
  definition, never duplicated (§2.2); superseded code is deleted, not left
  dead (§2.14).
- Each increment runs the full §7 gate and updates this plan's statuses to
  the done-state summary form (§13).

## 4. Status

- **B1 `done`** (host-gate-green): `lib/bootload` loader core.
- **B2 `done`** (host-gate-green): `lib/multiboot2` shared wire layout —
  parser (kernel re-exports it) + loader `InfoBuilder`, round-trip-tested.
- **B3, B4, B5, B6 `planned`.**

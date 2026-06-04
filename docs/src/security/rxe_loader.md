# The rxe loader: W^X, PIE, KASLR, CFI

`AGENTS.md` §19.2 freezes four exploit-mitigation invariants on the
`rxe` executable format. They are enforced *before* a single page of an
image is mapped, by the `rustos_abi::rxe` module (the format owner) and
the `kernel/mem` loader that consumes its output.

## The load image

An `rxe` binary carries the signed `ManifestHeader` (capabilities,
signature — see [security](../architecture/security.md)) and a **load
image**: a fixed `LoadHeader` followed by a table of `Segment` records.

`LoadHeader` (`abi-v1`, 56 bytes) declares the magic word, ABI version,
flags, segment count, entry point, and the **CFI type-tag** — the
SHA-256 of the syscall interface the binary was linked against. Each
`Segment` (40 bytes) declares an image-relative virtual address, the
file/memory sizes, and a permission flag word.

`LoadImage::parse(bytes, expected_cfi_tag)` is the single,
fail-closed entry point. Holding a `LoadImage` is proof that every
invariant below holds.

## W^X

`RxePermission::from_segment_flags` is the only constructor of a
segment permission, and it admits exactly three shapes: read-only,
read-execute, and read-write. A writable-and-executable segment is
refused (`RxeError::WriteExecSegment`); a non-readable segment is
refused (`RxeError::SegmentNotReadable`); unknown flag bits are refused
(`RxeError::UnknownSegmentFlags`). A writable-executable mapping is
therefore unrepresentable.

The `kernel/mem` `map_flags_for` translation reinforces this: it never
emits `MapFlags::WRITE | MapFlags::EXEC`, and the underlying
`PageTableOps` independently rejects that combination, so W^X holds
twice over.

## PIE + KASLR

`parse` refuses any image whose `LoadHeader` lacks `LOAD_FLAG_PIE`
(`RxeError::NotPositionIndependent`), so every binary is relocatable.
`kaslr_bias(seed, window_pages)` derives a page-aligned, bounded load
bias from a per-boot entropy seed (a `splitmix64` mixing, deterministic
in the seed so a boot can be reproduced from its recorded seed). The
loader applies the bias through `Segment::relocated_vaddr` and
`LoadImage::relocated_entry`; both use checked arithmetic and report
`RxeError::AddressOverflow` rather than wrapping.

## CFI type-tag

`parse` compares the header's `cfi_tag` against the kernel's
compiled-in syscall-interface hash in constant time. A mismatch is a
load-time refusal (`RxeError::InterfaceHashMismatch`), never a runtime
crash — the same discipline `ManifestHeader::syscall_table_hash` applies
to the manifest (§9).

## Mapping

`kernel/mem::map_image` consumes a validated `LoadImage`, relocates it
by a KASLR bias, and maps every segment page into an `AddressSpace`
with the permissions from `map_flags_for`, returning the relocated
entry point. Frame allocation is injected as a closure, so the loader is
allocator-agnostic and host-testable; out-of-frames surfaces as
`LoadError::OutOfFrames` rather than a panic (§4). Both `map_image` and
the spawn builder below share one page-mapping loop (`map_region`), so
the relocation/allocation arithmetic exists in exactly one place
(`AGENTS.md` §2.2).

## Building a runnable process image

`kernel/mem::build_process_image` turns a validated `LoadImage` into a
runnable user address space — the kernel-side step a spawn must perform
before it can drop to U-mode/EL0. It:

1. maps every segment page (R/RX/RW + USER) **and** fills it with the
   segment's file content, zeroing the BSS tail past `file_size`;
2. maps a zeroed user stack (U|R|W); and
3. serialises the `rustos_abi::process` startup-vector block (the
   arguments, environment, and §19.2 stack-canary seed) and writes it
   into the new address space (U|R|W).

It returns a `ProcessImage` — the relocated entry point, the initial
user stack pointer, and the user address of the startup block — i.e. the
register state an Arch HAL "enter U-mode/EL0" primitive consumes.

Content is written through the kernel's `PhysMap` directly to the
freshly allocated frame, **not** through `copy_out`: a read-execute code
page must hold its bytes before it runs, yet must never be user-writable
(W^X). The page is still mapped R/RX/RW in user space, never RWX. Every
input is validated and the builder fails closed with a `SpawnError`
(misaligned bases, a segment file range outside the image, an
over-limit startup block) rather than panicking (`AGENTS.md` §2.9).

The startup-vector block is produced by `rustos_abi::process::write_into`
(sized by `process::encoded_len`) — the production, allocation-free
builder that `lib/abi` exposes for the kernel and that round-trips
through the untrusted-input `ProcessStart::parse` crt0 uses.

The capability gate that authorises a spawn and its `lib/log` audit
record live in the higher-level spawn path that calls this builder, not
in `kernel/mem` (the §17.4 layering keeps the memory subsystem free of
the security policy).

## What is not yet enforced here

The Arch HAL "enter U-mode/EL0" primitive (which consumes the
`ProcessImage`) and stack-canary / shadow-stack selection in the arch
`unsafe` cores depend on the real arch page tables; they build on this
validated `LoadImage` and `ProcessImage` without relaxing any invariant
above.

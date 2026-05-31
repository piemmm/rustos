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
`LoadError::OutOfFrames` rather than a panic (§4).

## What is not yet enforced here

Copying segment file contents into the mapped frames, stack-canary /
shadow-stack selection in the arch `unsafe` cores, and the live
process-creation path depend on the Stage 6 process model and the real
arch page tables; they build on this validated `LoadImage` without
relaxing any invariant above.

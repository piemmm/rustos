# tairix-abi

The one definition of the interface between the kernel and everything it runs:
syscall numbers and argument layouts, error codes, capability ids, the `rxe`
manifest, the hardware tree, the driver class traits, the standard streams,
time, and the wire protocol of every system service.

## Design

- `no_std`, no dependencies, no allocation. Encoders and decoders work over
  borrowed byte slices, so the same code runs in the kernel, a freestanding
  driver and a wasm program.
- Every wire type has a fixed `#[repr(C)]` layout. The C headers under
  `include/` are generated from this crate (`cargo xtask c-header --write`),
  and `cargo xtask c-header` fails on drift; `cargo xtask abi-check` holds the
  kernel's syscall table to `syscalls.rs`.
- Decoders treat their input as hostile and fail closed; the fuzz harnesses
  are under `tests/`.

## Stability

Tier: `experimental`. `abi-v1` changes in place until the first release, when
it freezes and new behaviour ships as `abi-v2`.

# System overview

RustOS is organised as a microkernel-leaning Rust workspace. This page is the
single-screen map of the system; deeper subsystem pages land alongside the
stages of [`PLAN.md`][plan] that introduce them.

## Layers

```
┌────────────────────────────────────────────────────────────────────┐
│ userland/                                                          │
│   system/ │ session/ │ shell/ │ gui/ │ apps/                       │
│     init, installer │ login │ shell │ wm, iconbar │ …              │
├────────────────────────────────────────────────────────────────────┤
│ drivers/                                                           │
│   display/ │ filesystem/ │ bus/ │ input/ │ network/ │ storage/     │
├────────────────────────────────────────────────────────────────────┤
│ kernel/                                                            │
│   core │ mem │ sched │ sync │ ipc │ sec │ syscall │ arch/<target>  │
├────────────────────────────────────────────────────────────────────┤
│ lib/   (shared, no_std)                                            │
│   abi │ caps │ collections │ crypto │ log │ util                   │
└────────────────────────────────────────────────────────────────────┘
```

## Principles

- **Microkernel-leaning.** Only scheduling, memory, IPC, capabilities, and
  minimal architecture glue live in privileged mode. Drivers run in user
  space unless the hardware forbids it.
- **Security is the default.** Every syscall, IPC endpoint, and filesystem
  operation is capability-checked. `uid = 0` is not all-powerful; powers
  come from capabilities granted by signed manifests.
- **SMP from day one.** Shared state uses explicit primitives from
  `lib/sync`; there is no "single-CPU first" path.
- **One executable format.** The `rxe` envelope wraps ELF with a signed
  manifest declaring required capabilities and ABI version.

## Targets

| Triple                          | Boot path           | Stage |
| ------------------------------- | ------------------- | ----- |
| `x86_64-unknown-none`           | BIOS + UEFI         | 3a    |
| `aarch64-unknown-none`          | Raspberry Pi 3/4/5  | 3b    |
| `riscv64gc-unknown-none-elf`    | QEMU virt, SiFive   | 3c    |
| `wasm32-unknown-unknown`        | Browser sandbox     | 3d    |

## Status

Stage 0 (this stage) delivers the workspace skeleton, `cargo xtask`, the
mdBook scaffold, and CI. The crates listed in the diagram exist as
placeholders that compile but expose no public items; their implementations
are scheduled by [`PLAN.md`][plan].

[plan]: https://github.com/rustos-project/rustos/blob/main/PLAN.md

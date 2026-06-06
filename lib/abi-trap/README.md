# rustos-abi-trap

The raw `abi-v1` user→kernel syscall trap primitive: the single
per-architecture `syscall` (x86_64) / `svc` (AArch64) / `ecall` (RISC-V)
instruction plus the register marshalling the kernel's per-arch entry path
reads. It exposes one function, `raw_syscall(number, args) -> result`.

This crate exists so the trap assembly lives in **exactly one place**
(`AGENTS.md` §2.2). Two userland runtimes build on it:

- `rustos-abi-sys` — the C-callable `ros_sys_<name>` stub runtime that a
  program **not** written in Rust links (the curated *System runtime / C ABI*
  class, `AGENTS.md` §9, §16.4).
- `rustos-rt` — the pure-Rust userland runtime that first-party RustOS
  programs link. RustOS code is Rust-only and never routes through the C ABI
  meant for third parties (`AGENTS.md` §1).

## Security

`raw_syscall` adds **no** authority (`AGENTS.md` §5.4). Every capability check
and input validation happens kernel-side, on the far side of the trap; a
caller reaches no syscall it could not reach otherwise. The kernel
re-validates every argument and fails closed.

## Targets

The trap instruction is compiled in only for the three native Tier-1 targets
(`x86_64-unknown-none`, `aarch64-unknown-none`, `riscv64gc-unknown-none-elf`),
selected by a build-script-emitted cfg (`abi_trap_<arch>`) rather than
`cfg(target_arch)` so the instruction-set choice stays out of the source the
§17.2 `cfg-check` guards. `wasm32` has no trap instruction and is out of scope
(`plans/CCOMPAT.md` §1). On the host there is no kernel: `raw_syscall` fails
closed with `HOST_NO_TRAP`, optionally routed through the `host-seam` test
scaffolding (a thread-local seam, enabled only through a `dev-dependencies`
edge so a shipping build stays `no_std`).

## Stability tier

`experimental` — `abi-v1` is **not** frozen yet (`plans/CCOMPAT.md` §0). The
register calling convention becomes part of the frozen ABI at the first
release (`AGENTS.md` §9).

# rustos-crt0

The C-callable `abi-v1` program startup/teardown object (crt0). On each native
Tier-1 target it provides the program's `_start` entry symbol: it sets up the C
runtime environment (stack alignment per the platform C ABI; the `argc` /
`argv` / `envp` a C `main` expects, marshalled from the kernel's
position-independent startup-vector block, `rustos_abi::process`), installs the
per-process stack canary, calls the program's `main`, and routes its return
value through the `exit` syscall (`rustos_abi_sys::sys_exit`, the `ros_sys_exit`
stub). A program **not** written in Rust links it as its startup object.

Together with [`rustos-abi-sys`](../abi-sys) (the `ros_sys_<name>` syscall
stubs) it forms the curated `/System/Libraries/` class *System runtime / C ABI*
(`AGENTS.md` §16.4): the minimal libc-equivalent that lets a non-Rust program
run on RustOS. It is dynamically linked like every curated library, so one
security update covers every consumer. The staged build plan is
`plans/CCOMPAT.md` (stage CC3).

## Host-testable core vs. target trampoline

The startup vector is **untrusted input** (`AGENTS.md` §19.5/§19.6), so the
security-relevant logic — validating the block and laying out the C `argv` /
`envp` (copying each NUL-free string and NUL-terminating it, building the two
NULL-terminated pointer arrays) — lives in the allocation-free, fail-closed
`build_c_runtime`, which is unit-tested on the host. The per-architecture
`_start` assembly carve-out (the §1-sanctioned trap-free entry trampoline) is
the thin glue that aligns the stack, carves a bounded scratch region from it,
calls `build_c_runtime`, installs the canary, and drives `main` / `exit`; it is
selected by a build-script-emitted `crt0_native_<arch>` cfg rather than
`cfg(target_arch)` so the instruction-set choice stays out of the source the
§17.2 `cfg-check` guards (mirroring `lib/abi-sys`). The trampoline itself is
exercised under QEMU.

## Security

crt0 adds **no** authority (`AGENTS.md` §5.4 / `plans/CCOMPAT.md` §4): it only
marshals the startup vector and starts/stops the program. Every capability and
input check happens kernel-side. A malformed or oversized startup vector fails
closed — crt0 terminates the program with a reserved non-zero exit code rather
than indexing out of range or truncating the runtime (`AGENTS.md` §2.9). The
`rxe` hardening invariants for the hosted image (PIE, `R`/`RX`/`RW`-only
segments, the syscall-hash CFI tag, stack canaries; `AGENTS.md` §9/§19.2) are
enforced at load time by `rustos_abi::rxe::LoadImage::parse` — a non-conforming
image is refused, not patched — and crt0 seeds the compiler's
`__stack_chk_guard` from the per-process random canary the kernel placed in the
startup vector.

## Targets

The `_start` trampoline and the `exit` teardown path are compiled in only for
the three native Tier-1 targets (`x86_64-unknown-none`, `aarch64-unknown-none`,
`riscv64gc-unknown-none-elf`). `wasm32` has no trap instruction and a different
linking story, so it is out of scope (`plans/CCOMPAT.md` §1). On the host the
crate keeps only `build_c_runtime` / `read_total_len`, which are unit-tested
directly.

## Stability tier

`experimental` — new in stage CC3, and `abi-v1` is **not** frozen yet
(`plans/CCOMPAT.md` §0). The kernel → `_start` contract and the `__stack_chk_*`
symbols become part of the frozen surface at the first release (`AGENTS.md`
§9); until then they may change in step with `lib/abi`.

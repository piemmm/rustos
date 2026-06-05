# Calling RustOS from C (a worked example)

This page is the practical, end-to-end companion to the
[C development header](./c-abi.md) reference: it walks through building a real
program written in **C** — not Rust — that talks to the kernel only through the
generated `abi-v1` headers and the curated *System runtime / C ABI* class. It
is the worked example behind `plans/CCOMPAT.md` stage CC5.

RustOS itself stays Rust-only (`AGENTS.md` §1). Nothing here adds C to the OS;
it shows how a *hosted* third-party program in any C-FFI language reaches the
RustOS kernel, and how the in-tree CC5 integration test proves the header, the
syscall stub runtime, and crt0 all agree with the Rust side.

## The three pieces a C program links

A non-Rust program needs exactly three things from RustOS, all already on the
system:

1. **The headers** under `include/rustos/` — the C view of every `abi-v1`
   type, constant, and `ros_sys_<name>` prototype. They are generated from
   `lib/abi` (`cargo xtask c-header --write`) and never hand-edited.
2. **The syscall stub runtime** (`lib/abi-sys`) — one `extern "C"`,
   export-name-pinned `ros_sys_<name>` per syscall that marshals registers and
   issues the trap. It adds no authority; every check happens kernel-side.
3. **The startup object, crt0** (`lib/crt0`) — provides `_start`, marshals the
   kernel's [startup vector](./c-abi.md#process-startup-vector) into C
   `argc`/`argv`/`envp`, installs the stack canary, calls `main`, and routes
   its return through `ros_sys_exit`.

The runtime and crt0 are the curated `/System/Libraries/` *System runtime /
C ABI* class (`AGENTS.md` §16.4): an installed app links them dynamically, so
one security update covers every consumer.

## A minimal program

The CC5 fixture (`tests/integration/cc5_program/csrc/main.c`) is a complete
example. It includes the headers and exercises a representative slice of
`abi-v1`:

```c
#include <stdint.h>
#include "rustos/rustos_capability.h"
#include "rustos/rustos_syscall.h"
#include "rustos/rustos_time.h"
#include "rustos/rustos_ipc.h"
#include "rustos/rustos_sysinfo.h"

int main(int argc, char **argv, char **envp) {
    (void)argc; (void)argv; (void)envp;

    /* A Time64 spans the §21 pre-1970 / post-2038 boundaries in 64-bit secs. */
    ros_time64_t pre  = { .secs = -2208988800LL, .nanos = 500000000u };
    ros_time64_t post = { .secs =  4102444800LL, .nanos = 1u };
    if (pre.secs >= 0 || post.secs <= (int64_t)INT32_MAX) {
        return 81;
    }

    /* abi-v1 wire types are usable directly from C. */
    ros_ipc_message_header_t msg = { .magic = ROS_IPC_MESSAGE_HEADER_MAGIC };
    ros_sysinfo_request_header_t req = { .magic = ROS_SYSINFO_REQUEST_MAGIC };
    if (msg.magic != 0x31435049u || req.magic != 0x31495953u) {
        return 82;
    }

    /* Two real syscalls. The kernel re-checks every argument on the far side.
       The capability id is a named constant from the header, not a bare literal. */
    if (ros_sys_cap_query(ROS_CAP_DRV_KERNEL) != 1u) {
        return 84;
    }
    return (ros_sys_clock_get() != 0u) ? 0 : 85;
}
```

`main` returns an exit code; crt0 routes it through the `exit` syscall.

## Building: the audited toolchain wrapper

RustOS compiles the C program with `clang` and links it with `ld.lld`, but
never as an unaudited shell-out (`AGENTS.md` §12). The single gateway is the
`rustos-cc` crate (`tools/cc`), which:

- resolves `clang` / `ld.lld` (overridable with `RUSTOS_CC_CLANG` /
  `RUSTOS_CC_LLD`),
- runs `--version` and fails closed unless the tool reports the pinned
  `rustos_cc::REQUIRED_CLANG_VERSION` / `REQUIRED_LLD_VERSION`,
- SHA-256-hashes each binary with the audited `lib/crypto` and records it for
  the build transcript (and verifies it against `RUSTOS_CC_CLANG_SHA256` /
  `RUSTOS_CC_LLD_SHA256` when a digest is pinned).

The compile recipe is a freestanding, position-independent, canary-protected
object; the link recipe is a hardened PIE (`-pie --gc-sections -z noexecstack`)
laid out by a W^X page-granular link script that roots crt0's `_start`. A
build script drives it:

```rust
use rustos_cc::{CTarget, CompileRequest, LinkRequest, Toolchain};

let cc = Toolchain::discover()?;                 // version-pinned + checksummed
cc.compile(&CompileRequest {                     // clang -> main.o
    target: CTarget::Riscv64,
    source: c_source, object: &object,
    include_dirs: &[include_dir],
})?;
cc.link(&LinkRequest {                            // ld.lld -> PIE ELF
    target: CTarget::Riscv64,
    objects: &[&object],
    archives: &[&runtime_archive],               // crt0 + ros_sys_* runtime
    linker_script: link_script, output: &elf,
})?;
```

## Loading: PIE, W^X, and the rxe envelope

A program runs as a signed `/Apps/<Name>.app/` bundle whose `Run` binary is an
`rxe` PIE image. The hardening invariants are enforced at one point,
`rustos_abi::rxe::LoadImage::parse`, on a C binary exactly as on a Rust one:
position-independence (PIE), `R`/`RX`/`RW`-only segments (no `RWX`), and the
syscall-hash CFI tag (`AGENTS.md` §9 / §19.2). A linked C image that carries
anything other than `R_*_RELATIVE` relocations, or a writable-executable
segment, is **refused at load — not patched** (the CC5 build links a clean PIE
so only relative relocations remain; the converter rejects GOT/PLT/symbolic
forms). See [the rxe loader](../security/rxe_loader.md) for the full policy and
[the C header reference](./c-abi.md#bundles-and-the-dynamic-loader-policy) for
how the runtime library is resolved.

## Proving it end to end

The CC5 QEMU tests run the program above on **all three native Tier-1
targets**: `rustos-test-c-program-qemu-riscv64` (riscv64 `virt`, U-mode),
`rustos-test-c-program-qemu-aarch64` (aarch64 `virt`, EL0), and
`rustos-test-c-program-qemu-x86_64` (x86_64, ring 3). Each build script builds
the crt0 + `ros_sys_*` runtime shim (`tests/integration/cc5_program`) as a
position-independent `staticlib`, compiles `main.c` with `rustos-cc`, links one
PIE image (`CTarget::Riscv64` / `Aarch64` / `X86_64`), and converts it to an
`rxe` blob carrying the kernel's compiled-in syscall CFI tag. On boot the test
spawns the program through the production capability-checked, audited
`rustos_kernel_core::spawn_and_enter` and installs a dispatch callback that
services the program's `cap_query` / `clock_get` round-trips and asserts the
`exit` code. A pass means the generated header, the syscall stub runtime, and
crt0 agree with the kernel — for genuinely C-compiled code.

The aarch64 vertical first enables `CPACR_EL1.FPEN` (the NEON-vectorised
decoder runs at EL1); the x86_64 vertical boots the production kernel pipeline
(GDT user selectors / TSS / `IA32_LSTAR`) and enables `IA32_EFER.NXE` so the
W^X No-Execute leaf bit is honoured. They run under `cargo xtask test --qemu`
(not the host-only `cargo xtask ci` gate).

## Security posture

A C program is **not** a privileged path (`AGENTS.md` §5.4; `plans/CCOMPAT.md`
§4). The `ros_sys_*` stubs only marshal registers and trap; the kernel
re-validates every argument and capability on the far side, exactly as for a
Rust caller. A hosted program receives only the intersection of its signed
`AppInfo` request and the launching user's grants — being written in C grants
no ambient authority.

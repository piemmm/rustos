# RustOS development headers

C-language development headers for the RustOS ABI. These let a program
written in a language other than Rust (C, C++, anything with a C FFI) call
the RustOS kernel/user interface.

- `rustos/rustos_abi.h` — the `abi-v1` surface: the ABI version, the stable
  error codes, the capability identifiers, the syscall numbers, and a
  prototype for each syscall entry point.

## These files are generated

Every header here is **generated** from the single source of truth in
`lib/abi` (`AGENTS.md` §2.2, §9). Do not edit them by hand. To regenerate
after changing `lib/abi`:

```
cargo xtask c-header --write
```

`cargo xtask ci` runs `cargo xtask c-header` (no `--write`), which fails
closed if a committed header has drifted from `lib/abi`.

## Calling convention

Each syscall is exported by the user-space stub library under the symbol
`ros_sys_<name>` (for example `ros_sys_ipc_send`). The stub library
implements each entry with an explicit `#[export_name = "ros_sys_<name>"]`
so the Rust compiler does not mangle the symbol. The short `ros_` / `ROS_`
prefix namespaces the C-visible surface so it survives C's single flat symbol
namespace (`AGENTS.md` §9). Link against that library and include
`rustos/rustos_abi.h` to call the kernel.

`abi-v1` is not frozen yet; once it is released its layout, numbers, and
symbol names become immutable and new behaviour ships as `abi-v2`.

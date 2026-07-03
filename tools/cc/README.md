# `rustos-cc` — audited, version-pinned, checksummed C toolchain wrapper

Host-only build glue (`AGENTS.md` §12) that lets a single QEMU integration test
*host* a small C program, proving the generated `abi-v1` C header, the
`ros_sys_*` syscall stub runtime (`lib/abi-sys`), and the crt0 startup object
(`lib/crt0`) agree with the Rust side end to end (`plans/CCOMPAT.md` stage CC5).
RustOS itself stays Rust-only — this crate does **not** add C to the OS.

**Stability tier:** experimental.

## Pinned versions

The wrapper drives exactly the `clang` / `ld.lld` versions pinned in
`rustos_cc::REQUIRED_CLANG_VERSION` / `REQUIRED_LLD_VERSION`. A tool that
reports any other version is refused (supply-chain integrity); bumping the pin
is a deliberate, reviewed change, like `rust-toolchain.toml`.

## Finding the toolchain (no configuration needed)

`Toolchain::discover()` locates each tool automatically, so a plain
`cargo xtask ci` / `cargo xtask test --qemu` works once the pinned LLVM is
installed — no environment variables, no hunting for the right path. Resolution
order per tool:

1. **The explicit override** — `RUSTOS_CC_CLANG` / `RUSTOS_CC_LLD`. Authoritative:
   if set, it must point at a file of the pinned version, else discovery fails
   closed (an override exists to be obeyed).
2. **Well-known locations for the pinned major version**, in order:
   - the versioned name on `PATH` (`clang-<major>`, `ld.lld-<major>`);
   - Homebrew prefixes — `/opt/homebrew/opt/{llvm,lld}[@<major>]/bin` (Apple
     silicon) and `/usr/local/opt/{llvm,lld}[@<major>]/bin` (Intel);
   - Debian/apt.llvm.org prefixes — `/usr/lib/llvm-<major>/bin`;
   - the bare name on `PATH` (`clang`, `ld.lld`) as a last resort.

   The first candidate whose reported version is **exactly** the pin is chosen;
   a system/Apple `clang` of the wrong version is skipped, never accepted. All
   platforms' paths are listed unconditionally — missing ones are simply
   skipped — so the crate carries no `cfg(target_os)` fork.

If nothing matches, the error names every location searched and how to install
the toolchain. `clang` ships in the `llvm` package/formula; `ld.lld` ships in
the separate Homebrew `lld` formula (and inside `llvm` on Debian):

```sh
# macOS
brew install llvm lld
# Debian / Ubuntu (from apt.llvm.org)
apt install clang-22 lld-22
```

## Auditing and pinning a digest

Every resolved binary is SHA-256-hashed with the audited `lib/crypto` and
recorded in the build transcript. To pin an expected digest as well, set
`RUSTOS_CC_CLANG_SHA256` / `RUSTOS_CC_LLD_SHA256` to the hex digest; a mismatch
fails closed.

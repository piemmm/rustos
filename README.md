# RustOS - read the [documentation](./docs/book/index.html)

A security-first, multi-user, multi-core operating system written in Rust,
targeting bare-metal x86_64, AArch64, RISC-V 64, and the browser via
`wasm32-unknown-unknown`.

This file is intentionally brief. Authoritative documents live alongside the
code:

- [`AGENTS.md`](./AGENTS.md) — binding engineering charter.
- [`PLAN.md`](./PLAN.md) — staged delivery plan.
- [`docs/`](./docs) — long-form architecture, security, and platform book
  (built with mdBook).

## Status
**Work in progress.** - There is a long way to go before this project is ready
for prime time, if ever. <span style="color:red">**Do not expect anything to work yet.**</span>

## Building

```sh
cargo xtask ci          # Full pipeline a PR must pass
cargo xtask test        # Host-side unit and integration tests
cargo xtask docs-check  # rustdoc + mdBook (with link checking)
cargo xtask --help      # All subcommands
```

The pinned nightly toolchain in [`rust-toolchain.toml`](./rust-toolchain.toml)
is installed automatically when `rustup` is present. External tools used by
`cargo xtask ci` are:

```sh
cargo install --locked cargo-deny mdbook
```

## Licence

Licensed under the [GNU General Public License v2.0 or later](./LICENSE)
(GPL-2.0-or-later), with an additional syscall / ABI exception
(`RustOS-syscall-note`) that keeps user-space programs which merely use the
kernel's system calls or its published syscall / ABI interface definitions
from being treated as derived works. See [`LICENSE`](./LICENSE) for the full
text.

RustOS is an independent, open-source hobby project. It is not affiliated with, endorsed by, or supported by the Rust Project or the Rust Foundation.

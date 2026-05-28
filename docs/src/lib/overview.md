# Shared libraries (Stage 1)

The `lib/` workspace tree is the only place RustOS publishes code that is
shared by more than one component. Every crate here is `no_std`, performs
no allocation, and exposes a deliberately narrow public surface.

| Crate                  | Purpose                                                          | Stability |
|------------------------|------------------------------------------------------------------|-----------|
| [`rustos-abi`](abi.md) | Frozen `abi-v1` types crossing the kernel/user boundary.         | frozen    |
| [`rustos-caps`](caps.md) | Capabilities, capability sets, and signed delegation tokens.   | stable    |
| [`rustos-collections`](collections.md) | `no_std` collections (currently `BitSet256`).        | stable    |
| [`rustos-crypto`](crypto.md) | Audited wrappers (SHA-256, Ed25519 verification).          | stable    |
| [`rustos-log`](log.md) | Structured, level-filtered, alloc-free logging.                  | stable    |
| [`rustos-util`](util.md) | Reserved destination for ≥ 2-use helpers — empty in Stage 1.   | experimental |

These crates are the foundation Stage 2 (the kernel core) builds on. They
are intentionally small: every addition must justify its existence per
`AGENTS.md` §2.3.

//! riscv64 device-tree access.
//!
//! The flattened-device-tree parser itself is architecture-neutral and
//! lives once in [`rustos_fdt`] (`AGENTS.md` §2.2 — no duplication); this
//! module re-exports it so the riscv64 boot path and the QEMU integration
//! tests keep naming `rustos_arch_riscv64::fdt::Fdt`. The riscv64-specific
//! normalisation of the tree into [`rustos_abi::hwtree`] nodes lives in
//! [`crate::platform`].

pub use rustos_fdt::{Fdt, FdtError};

#[cfg(test)]
pub(crate) mod tests {
    //! Re-export of the shared DTB test fixture so the `platform`
    //! discovery tests and the conformance handle drive the same builder
    //! as the parser's own tests (`AGENTS.md` §2.2). Enabled by the
    //! `rustos-fdt/test-fixtures` feature this crate turns on in its
    //! `[dev-dependencies]`.
    pub(crate) use rustos_fdt::fixture::virt_like;
}

//! Re-export of the shared RAID on-disk metadata layer (`tairix_raidmeta`).
//!
//! The array-member superblock format and the fail-closed reassembly logic
//! live in the `lib/raidmeta` crate so the composition engines here and the
//! storage-discovery probe (`lib/fsprobe`/volmgr) share one definition
//! (`AGENTS.md` §2.2) without a `drivers/*`->`drivers/*` edge (§17.4). The
//! engine modules reach those types through `crate::superblock::*`, so this
//! module keeps that path stable while the definition lives in the library.

pub use tairix_raidmeta::*;

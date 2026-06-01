//! RustOS `mount` — list mounted filesystems and attach new ones (Stage 6,
//! `AGENTS.md` §3 `userland/apps/`, §16.6).
//!
//! With no operands `mount` lists the current mount table; with a
//! `SOURCE TARGET` pair it attaches a filesystem. The two halves use
//! different paths, both kernel-plumbing-free behind injected seams:
//!
//! * **Listing** is a *read* of live system state, so — like `ps` — it goes
//!   through the System Information API (`AGENTS.md` §16.6): RustOS has no
//!   `/proc` and no mount-table file. `mount` issues the typed, versioned
//!   `sysinfo-v1` `MOUNT_LIST` query served by `/System/Services/sysinfod`
//!   and renders one `source on target type fstype (options)` line per
//!   mount. The query is ungated; the shared paging and rendering live in
//!   `lib/procinfo` so `mount`, `ps`, and `sysinfo` never duplicate them
//!   (`AGENTS.md` §2.2).
//! * **Attaching** is privileged — it needs `CAP_FS_MOUNT` (`AGENTS.md`
//!   §5.2) — and the **kernel** makes that decision (`AGENTS.md` §5.4), not
//!   this tool. `mount` parses and presents a request to the injected
//!   [`Mounter`] seam; an unauthorised or invalid attempt is refused there
//!   and surfaced as [`MountError::Mount`].
//!
//! # What this crate is
//!
//! A command-line parser and presenter, not a policy point. The operations
//! that touch the outside world are the injected seams:
//!
//! * [`Transport`](rustos_procinfo::Transport) / [`Output`](rustos_procinfo::Output)
//!   — issue the `MOUNT_LIST` query and write each rendered line (shared
//!   with the other `sysinfo` clients).
//! * [`Mounter`] — perform the privileged attach.
//!
//! The binary that ships as `mount` wires the real syscall-backed transport,
//! console output, and mount syscall; tests wire in-memory fixtures, mirroring
//! the seam discipline of the other userland crates (`ps`'s `Transport`,
//! `useradd`'s `UserDb`).
//!
//! # Fail closed
//!
//! An unknown option, a missing option value, or a number of operands other
//! than zero or two is a [`MountError::Usage`]; an unknown `-o`/`-t` value is
//! a [`MountError::BadOption`]. A listing transport failure surfaces the
//! underlying [`Errno`](rustos_abi::Errno) as [`MountError::Service`], a
//! refused or failed attach as [`MountError::Mount`], and a terminal write
//! failure as [`MountError::Output`]. There is no panic (`AGENTS.md` §2.9).
//!
//! # Module map
//!
//! * [`error`] — [`MountError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`MountRequest`] shapes and the [`parse`]r.
//! * [`io`] — the [`Mounter`] seam and the [`MountSpec`] it carries.
//! * [`client`] — the [`run`] entry point.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6); the only dependencies are the
//! audited `lib/abi` and `lib/procinfo` crates, so this userland tool never
//! links a kernel or driver crate (`AGENTS.md` §17.4). No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, MountRequest};
pub use error::MountError;
pub use io::{MountSpec, Mounter};

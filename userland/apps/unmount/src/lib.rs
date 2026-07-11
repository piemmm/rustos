//! RustOS `unmount` — detach a runtime-attached volume (Stage 6
//! `userland/apps/`, `plans/DEVICES.md` D4b).
//!
//! `unmount NAME` takes the volume mounted under `NAME` (its catalog
//! name or its mount-point path) out of service: the kernel flushes the
//! filesystem and the device, retracts the mount, and withdraws the
//! volume's durable `id::` root. A volume whose serving device vanished
//! with uncommitted writes (*unavailable-dirty*/*unavailable-lost*, the
//! surprise-removal states the mount listing marks) refuses a plain
//! detach — its retained data is held for verified re-insert.
//! `unmount --force NAME` is the deliberate exit: the kernel discards
//! the retained set, retracts the volume, and logs the data loss with
//! its own audit event.
//!
//! The tool is a resolver and presenter, not a policy point:
//!
//! * **Resolving** `NAME` to the volume's stable 16-byte identity goes
//!   through the ungated `sysinfo-v1` `MOUNT_LIST` query (the shared
//!   `lib/procinfo` paging walk `mount` and `df` use too), whose records
//!   carry each mount's volume identity and availability.
//! * **Detaching** is privileged — the kernel's `volume_detach` path
//!   requires `CAP_FS_MOUNT` and re-validates everything; this tool only
//!   presents the request through the injected [`Detacher`] seam and
//!   reports the kernel's decision.
//!
//! # Fail closed
//!
//! An unknown option or a number of operands other than one is a
//! [`UnmountError::Usage`]. A name matching no mount, or a mount with no
//! detachable volume identity (the boot volumes, the in-RAM bindings),
//! is a typed error with its reason on standard error — never a guessed
//! detach. A refused detach surfaces the kernel's exact `Errno`; when
//! the volume is unavailable the tool adds the `--force` consequence to
//! the diagnostic and emits an fd-3 `suggestion` record (additive,
//! ignorable). There is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`UnmountError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and the [`parse`]r.
//! * [`io`] — the [`Detacher`] and [`Output`] seams.
//! * [`client`] — the [`run`] engine.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited
//! `lib/abi`, `lib/help`, and `lib/procinfo` crates, so this userland
//! tool never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command};
pub use error::UnmountError;
pub use io::{Detacher, Output};

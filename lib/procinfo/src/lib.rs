//! RustOS shared System Information API client helpers (Stage 6).
//!
//! RustOS has no `/proc` and no `/sys`: every piece of live system
//! information is read through the typed, versioned, capability-checked
//! `sysinfo-v1` API served by `/System/Services/sysinfod.app/Run`. Several terminal tools speak that API — the umbrella `sysinfo`
//! command, the POSIX-named `ps`, and the `mount` listing — and they share
//! the same request envelope, the same capability-aware call mapping, and
//! the same paged-list walk and row rendering. Sibling userland crates may
//! not depend on one another, so that shared shape lives
//! here, in one place, rather than being copied.
//!
//! # What this crate is
//!
//! A small, dependency-light client toolkit, **not** a data source. It
//! provides:
//!
//! * The [`Transport`] and [`Output`] seams through which a tool issues a
//!   request and writes a line. Keeping them behind object-safe traits is
//!   what lets the consuming tools run against in-memory fixtures with no
//!   kernel, mirroring the seam design of the other userland crates.
//! * [`encode_request`], which frames a
//!   [`SysinfoQueryId`](rustos_abi::sysinfo::SysinfoQueryId) and its typed
//!   payload into the `sysinfo-v1`
//!   [`SysinfoRequestHeader`](rustos_abi::sysinfo::SysinfoRequestHeader)
//!   envelope, and [`call`], which issues a request and maps a capability
//!   denial onto the distinguished [`CallError::PermissionDenied`].
//! * [`for_each_process`], the paged process-list walk, plus
//!   [`PROCESS_HEADER`], [`render_process`], and [`state_char`] — the
//!   shared columnar rendering.
//! * [`for_each_mount`] and [`render_mount`], the paged mount-table walk and
//!   its `source on target type fstype (options)` row rendering.
//! * [`walk_pages`](list) and the shared [`ListError`], the generic paging
//!   loop both walks are built on.
//! * [`resolve()`], the userspace `info:`/`stats:` resource-reference resolver:
//!   it maps a parsed [`ResourceRef`](rustos_resref::ResourceRef) onto a
//!   [`SysinfoQueryId`](rustos_abi::sysinfo::SysinfoQueryId), issues it over
//!   the same [`Transport`], and returns the structured [`ResourceResponse`]
//!   (`plans/ALIAS.md` §14) — the one place `info:`/`stats:` are resolved, so
//!   the shell never invents a second resolver or bypasses the System
//!   Information API.
//!
//! Each consuming tool keeps its own argument grammar, usage banner, and
//! error enum; this crate owns only the parts they would otherwise
//! duplicate.
//!
//! # Module map
//!
//! * [`transport`] — the [`Transport`] and [`Output`] seams.
//! * [`request`] — [`encode_request`], [`call`], and [`CallError`].
//! * [`hwtree`] — the shared `HARDWARE_TREE` reply decode and the
//!   stable-bus-order / topology-view walk the device-inventory listing
//!   tools (`lspci`, `lsusb`) share.
//! * [`list`] — the generic paged-list walk and the shared [`ListError`].
//! * [`process`] — the process-list paging walk and row rendering.
//! * [`mount`] — the mount-table paging walk and row rendering.
//! * [`resinfo`] — the structured `info:`/`stats:` response records
//!   ([`ResourceResponse`], [`InfoValue`], [`Metric`]).
//! * [`mod@resolve`] — the `info:`/`stats:` resource-reference resolver.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! crate and the shared reference parser `lib/resref` (so the resolver reuses
//! the one reference grammar rather than embedding a second), and this helper
//! never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

// The production client seams (`IpcTransport`, `RtOutput`) that back the
// `sysinfo`/`ps`/`top` `Run` binaries. Compiled only for a freestanding
// program that opts into the `program` feature (which pulls `rustos-rt`); the
// pure library and host builds never link the runtime.
#[cfg(all(freestanding, feature = "program"))]
pub mod client;
pub mod cputime;
pub mod hwtree;
pub mod list;
pub mod mount;
pub mod process;
pub mod request;
pub mod resinfo;
pub mod resolve;
pub mod transport;
pub mod users;

#[cfg(all(freestanding, feature = "program"))]
pub use client::{IpcTransport, RtOutput};
pub use cputime::{for_each_cpu_time, CPU_TIME_PAGE};
pub use hwtree::{bus_order, class_label, decode_tree, depth_of, keep_with_ancestors};
pub use list::{field_lossy, ListError};
pub use mount::{for_each_mount, render_mount, render_options, MOUNT_PAGE};
pub use process::{
    emit_self_scope_omission, for_each_process, render_process, state_char, PROCESS_HEADER,
    PROCESS_PAGE,
};
pub use request::{call, encode_request, CallError};
pub use resinfo::{
    render_limit_bound, Authorization, InfoValue, Metric, MetricKind, Producer, ResetBehavior,
    ResourceResponse, ResponsePayload, Sensitivity, Unit, ValueKind, MAX_INFO_VALUE_LEN,
    MAX_METRIC_NAME_LEN, MAX_QUERY_LEN, RESINFO_VERSION_CURRENT, RESINFO_VERSION_V1,
};
pub use resolve::{resolve, ResolveInfoError};
pub use transport::{Output, Transport};
pub use users::{for_each_user, user_names, USER_DIRECTORY_PAGE};

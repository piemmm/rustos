//! TAIRiX `mdadm`: inspect and administer RAID arrays (`plans/FIX-IO.md` IO6).
//!
//! The RAID composer (`drivers/storage/raid`) assembles member devices into
//! live arrays; this tool is the administrator's front end to it. It reports
//! the arrays and the devices the composer holds — read through the System
//! Information client at the same `CAP_SYSINFO_HW` bar the hardware tree is
//! read under — and drives the create/add/remove/stop mutations by posting a
//! frame to the composer's control endpoint, which the composer authorises
//! against the caller's kernel-attested origin (`CAP_STORAGE_ADMIN`). The tool
//! holds no ambient authority and re-checks nothing: a refusal is the
//! composer's answer, reported on standard error with a non-zero exit.
//!
//! The command surface tracks Linux `mdadm` — `--create`, `--detail`,
//! `--examine`, `--add`, `--remove`, `--stop`, the short forms, and `--` — so a
//! user who knows `mdadm` finds it familiar. Where TAIRiX genuinely differs it
//! diverges deliberately and documents it in the bundle's `Help/`: there is no
//! `/dev`, so a device is named by its hardware-tree node id (`node:<id>`) and
//! an array by its 128-bit identity (a full hexadecimal identity or any
//! unambiguous prefix), and there is no RAID4.
//!
//! This crate is the pure engine — parse, resolve, render, dispatch — over
//! injected seams ([`Reader`], [`Controller`], [`Output`],
//! `tairix_help::HelpSource`), host-tested with no kernel. The freestanding
//! `Run` binary in `src/run.rs` binds the production seams.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;
pub mod render;
pub mod resolve;

pub use client::{run, USAGE};
pub use command::{parse, Command, CreateArgs, ParseError};
pub use error::{MdadmError, ResolveError};
pub use io::{Controller, Output, Reader};

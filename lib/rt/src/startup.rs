//! Read access to the kernel-supplied startup vector (arguments).
//!
//! The kernel hands every spawned process a position-independent startup
//! vector ([`rustos_abi::process::ProcessStart`]) carrying its arguments,
//! environment, and stack-protector canary. The runtime's `_start` driver
//! validates the block once and publishes the parsed view here, before the
//! program's `main` runs; these accessors are how a program reads the
//! arguments its spawner chose for it (for example, the reply endpoint id
//! the driver host hands a spawned driver process — `PLAN.md` Stage 4.HW).
//!
//! # Fail-closed behaviour
//!
//! When no validated startup vector has been published (the kernel handed
//! a malformed block, or a host-side unit test never installed one) the
//! accessors report an empty argument vector rather than fabricating data. They add no authority: the arguments were placed in
//! the process's own image by its spawner, and reading one's own memory
//! grants nothing.

use core::cell::UnsafeCell;

use rustos_abi::process::ProcessStart;

/// Storage for the validated startup-vector view.
///
/// Wrapped in an [`UnsafeCell`] rather than declared `static mut` so the
/// single pre-`main` write goes through one audited path with no aliasing
/// `&mut` — the same scheme as the runtime's
/// `__stack_chk_guard`.
struct StartupVector(UnsafeCell<Option<ProcessStart<'static>>>);

// SAFETY: the vector is written exactly once, by `install`, before the
// program's `main` runs and before any thread other than the initial one
// can exist; thereafter it is only read. There is no concurrent access to
// synchronise. (Host unit tests uphold the same single-install discipline.)
unsafe impl Sync for StartupVector {}

static STARTUP: StartupVector = StartupVector(UnsafeCell::new(None));

/// Publish the validated startup-vector view for the program's lifetime.
///
/// Called exactly once by the `_start` driver after
/// [`ProcessStart::parse`] succeeded and before `main` runs.
#[cfg(any(rt_native, test))]
pub(crate) fn install(view: ProcessStart<'static>) {
    // SAFETY: see the `Sync` impl on `StartupVector`. This is the sole
    // writer, running single-threaded before any program code that could
    // read the vector, so the write cannot race a read.
    unsafe {
        *STARTUP.0.get() = Some(view);
    }
}

/// Borrow the published view, if `_start` validated one.
fn view() -> Option<&'static ProcessStart<'static>> {
    // SAFETY: after the one-shot pre-`main` `install` the cell is only
    // read; handing out a shared `'static` borrow of the immutable value
    // is sound (see the `Sync` impl on `StartupVector`).
    unsafe { (*STARTUP.0.get()).as_ref() }
}

/// Number of arguments the spawner handed this process.
///
/// Zero when no validated startup vector is available (fail closed).
#[must_use]
pub fn arg_count() -> u32 {
    view().map_or(0, ProcessStart::arg_count)
}

/// The argument at `index`, or `None` when out of range or when no
/// validated startup vector is available (fail closed).
///
/// Index 0 is conventionally the program name its spawner chose.
#[must_use]
pub fn arg(index: u32) -> Option<&'static [u8]> {
    view()?.arg(index)
}

/// Number of environment strings the spawner handed this process.
///
/// Zero when no validated startup vector is available (fail closed).
#[must_use]
pub fn env_count() -> u32 {
    view().map_or(0, ProcessStart::env_count)
}

/// The environment string at `index`, or `None` when out of range or when
/// no validated startup vector is available (fail closed).
///
/// Entries follow the conventional `NAME=value` byte spelling; use
/// [`env_var`] to look one up by name.
#[must_use]
pub fn env(index: u32) -> Option<&'static [u8]> {
    view()?.env(index)
}

/// The value of the environment variable `name`, or `None` when the
/// spawner exported no such variable (or no validated startup vector is
/// available — fail closed).
///
/// The lookup splits each `NAME=value` entry at its first `=` and compares
/// the name bytes exactly; the first match wins, mirroring the POSIX
/// convention. An entry with no `=` names no variable and never matches.
#[must_use]
pub fn env_var(name: &[u8]) -> Option<&'static [u8]> {
    let view = view()?;
    for index in 0..view.env_count() {
        let entry = view.env(index)?;
        let mut split = entry.splitn(2, |&byte| byte == b'=');
        let entry_name = split.next()?;
        let Some(value) = split.next() else {
            continue;
        };
        if entry_name == name {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test drives the full install-then-read flow so the one-shot
    /// write discipline the `Sync` impl documents is upheld even under a
    /// multi-threaded test runner: no other test calls `install`.
    #[test]
    fn accessors_fail_closed_then_serve_the_installed_vector() {
        // Before `install`: empty vector, no fabricated data.
        assert_eq!(arg_count(), 0);
        assert_eq!(arg(0), None);
        assert_eq!(env_count(), 0);
        assert_eq!(env(0), None);
        assert_eq!(env_var(b"PATH"), None);

        let args: [&[u8]; 2] = [b"drvstub", b"42"];
        let envs: [&[u8]; 3] = [b"PATH=/Users/root/tools", b"LANG=fr-FR", b"noequals"];
        let len = rustos_abi::process_start_encoded_len(&args, &envs).expect("sized");
        let buf: &'static mut [u8] = alloc_block(len);
        rustos_abi::process_start_write_into(buf, &args, &envs, 7).expect("encoded");
        let view = ProcessStart::parse(buf).expect("valid block");
        install(view);

        assert_eq!(arg_count(), 2);
        assert_eq!(arg(0), Some(&b"drvstub"[..]));
        assert_eq!(arg(1), Some(&b"42"[..]));
        assert_eq!(arg(2), None);

        assert_eq!(env_count(), 3);
        assert_eq!(env(0), Some(&b"PATH=/Users/root/tools"[..]));
        assert_eq!(env(3), None);
        // Name lookup splits at the first `=`; an entry with no `=` names
        // no variable; a missing name is `None`, never fabricated.
        assert_eq!(env_var(b"PATH"), Some(&b"/Users/root/tools"[..]));
        assert_eq!(env_var(b"LANG"), Some(&b"fr-FR"[..]));
        assert_eq!(env_var(b"noequals"), None);
        assert_eq!(env_var(b"HOME"), None);
    }

    /// Leak a zeroed block so the parsed view can be `'static`, mirroring
    /// the process-image lifetime `_start` relies on (test-only leak).
    fn alloc_block(len: usize) -> &'static mut [u8] {
        Box::leak(vec![0u8; len].into_boxed_slice())
    }
}

//! The `Run` entry-point binary of the `init` application bundle
//! (`plans/PI.md` P6b).
//!
//! This is the program the kernel spawns as PID 1 once it reaches user mode
//! (`plans/PI.md` P6c). It is a **pure-Rust** program: TAIRiX is Rust-only, so `init` links the Rust userland runtime
//! `tairix-rt` — never the C ABI (`crt0` + `abi-sys`), which exists solely
//! for programs **not** written in Rust. `tairix-rt`
//! provides `_start`, the per-process stack canary, the
//! panic handler, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` parses the compiled-in `startup::DEFAULT_CONFIG`, renders the
//! startup banner from the kernel-attested `boot_facts_get` machine
//! summary (version, installed memory, architecture, core count), writes
//! it to its inherited standard output (fd 1) through the shared
//! `tairix_rt::io` layer over the `abi-v1` `stream_write` syscall
//! (`init` binds to the stream, never a device), then **supervises** the
//! user's sessions: one session program per installed text console
//! (`console_count` / `spawn_at` — the video console when a display is
//! active, else the discovered UART, `plans/PI.md` P11), reaped with wait-any and
//! relaunched on their own consoles ([`supervisor`]). The runtime routes
//! `main`'s return value through the `exit` syscall.
//!
//! It links **only** the runtime and its own startup-config parser, never the
//! sibling `tairix-init` orchestrator library, whose `alloc`-and-crypto
//! dependency chain has no place in a banner-printing program. That parser therefore lives alongside it in [`startup`] and is
//! host-tested there. The binary is built position-independent and converted
//! to an `rxe` blob by the consuming boot path (`plans/PI.md` P6c). On the
//! host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

mod startup;
mod supervisor;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    extern crate alloc;
    use alloc::vec::Vec;

    use tairix_abi::{Duration64, Errno, Signal};
    use tairix_init::{
        AuthorityScope, Init, InitConfig, LoopReaper, Pid, ReapedChild, ServiceSpec, Spawner,
        Stopper,
    };
    use tairix_rt::io::{Stderr, Stdout, Write};
    use tairix_rt::LogSink;

    use crate::startup::{render_banner, service_name, StartupConfig, BANNER_MAX, DEFAULT_CONFIG};
    use crate::supervisor::{supervise, Launch, Outcome, Services, Sessions};

    /// Exit code when the compiled-in startup config does not parse, names a
    /// duplicate service, or forms an invalid dependency graph. A reserved,
    /// fail-closed value; the default config is well-formed and acyclic, so
    /// reaching this is a build defect, not a runtime input.
    const EXIT_CONFIG_INVALID: i32 = 70;

    /// Exit code when waiting on the sessions failed — the `wait` syscall
    /// returned a negative `-errno` (the supervisor cannot reap the children
    /// it spawned). A reserved, fail-closed value
    /// distinct from [`EXIT_CONFIG_INVALID`] so the cause is unambiguous
    /// in the audit transcript.
    const EXIT_WAIT_FAILED: i32 = 72;

    /// Exit code when no console's session could stay up and no service is
    /// running: every console consumed its relaunch budget without a session
    /// ever blocking, so the supervisor stops rather than relaunching forever.
    const EXIT_SESSION_EXHAUSTED: i32 = 73;

    /// Exit code when the kernel reports no installed console (or refuses
    /// the count): there is nothing a session could attach its standard
    /// streams to, so PID 1 reports the system unusable fail-closed rather than spawning stream-less sessions.
    const EXIT_NO_CONSOLES: i32 = 74;

    /// The primary console index services attach their standard streams to
    /// (for their fd 2 diagnostics). Sessions fan out across every console;
    /// a service has no console of its own, so it takes console 0.
    const SERVICE_CONSOLE: u64 = 0;

    /// The production [`Spawner`]: launch a service's `Run` binary on the
    /// primary console as its own service account through `spawn_as`.
    ///
    /// The kernel is the single capability authority — it verifies the signed
    /// bundle, resolves the account's ceiling, and grants
    /// `manifest ∩ ceiling` — so this seam passes only the path and the
    /// account uid, never a capability set. A refused load surfaces as the
    /// kernel's `-errno`, which the engine records as
    /// [`StartFailure::SpawnFailed`](tairix_init::StartFailure::SpawnFailed).
    struct RtSpawner;

    impl Spawner for RtSpawner {
        fn spawn(&self, spec: &ServiceSpec) -> Result<Pid, Errno> {
            let ret = tairix_rt::spawn_as(
                spec.binary_path().as_bytes(),
                SERVICE_CONSOLE,
                spec.account(),
            );
            if ret < 0 {
                Err(Errno::from_syscall(ret))
            } else {
                // A non-negative kernel result is a valid pid.
                #[allow(clippy::cast_sign_loss)]
                Ok(Pid::new(ret as u64))
            }
        }
    }

    /// Deliver `signal` to `pid`, mapping the kernel's `-errno` to a typed
    /// [`Errno`]. A pid that does not fit the syscall's `i32` argument is
    /// out of range (fail closed) rather than truncated.
    fn signal_pid(pid: Pid, signal: Signal) -> Result<(), Errno> {
        let pid_i32 = i32::try_from(pid.as_u64()).map_err(|_| Errno::OutOfRange)?;
        let ret = tairix_rt::signal(pid_i32, signal);
        if ret < 0 {
            Err(Errno::from_syscall(ret))
        } else {
            Ok(())
        }
    }

    /// The production [`Stopper`]: graceful [`Signal::Terminate`] then, only
    /// after the grace period, [`Signal::Kill`]. Never a blind kill.
    struct RtStopper;

    impl Stopper for RtStopper {
        fn request_stop(&self, pid: Pid) -> Result<(), Errno> {
            signal_pid(pid, Signal::Terminate)
        }
        fn force_terminate(&self, pid: Pid) -> Result<(), Errno> {
            signal_pid(pid, Signal::Kill)
        }
    }

    /// The [`Services`] backing over the live [`Init`] engine: PID 1's
    /// service-manager half.
    ///
    /// The session supervisor hands every reaped pid that is not one of its
    /// own login sessions to [`on_child_exit`](Services::on_child_exit),
    /// which deposits it in the engine's [`LoopReaper`] and drives one
    /// [`Init::reap`] — no second `wait` — so the engine classifies it (a
    /// known service exit applying its restart policy, or an inherited
    /// orphan). The engine and this seam share the same `reaper` by reference;
    /// single-threaded PID 1 never overlaps a borrow.
    struct EngineServices<'a, 'cfg> {
        engine: &'a mut Init<'cfg>,
        reaper: &'a LoopReaper,
    }

    impl Services for EngineServices<'_, '_> {
        fn on_child_exit(&mut self, pid: u64, exit_code: i32) {
            self.reaper.deposit(ReapedChild {
                pid: Pid::new(pid),
                exit_code,
            });
            // The monotonic clock feeds the engine's restart-backoff
            // deadlines. The floor services restart `Never`, so `now` is
            // inert for them today; it is correct as soon as a restarting
            // service is registered.
            let now = Duration64::from_nanos(tairix_rt::clock_get());
            self.engine.reap(now);
        }

        fn any_running(&self) -> bool {
            self.engine.running_count() > 0
        }
    }

    /// The production [`Sessions`] backing: the real `tairix-rt` syscall
    /// wrappers (`console_count`, the console-selecting `spawn_at`, and
    /// wait-any). Zero-sized — the per-console session table lives on
    /// `main`'s stack inside [`supervise`].
    struct RtSessions;

    impl Sessions for RtSessions {
        fn console_count(&mut self) -> i64 {
            tairix_rt::console_count()
        }
        fn spawn_at(&mut self, path: &[u8], console: u32, uid: u32) -> i64 {
            // Switch the child onto its own service account at creation
            // (there is no setuid-self): the kernel gates the switch on
            // init's `CAP_SPAWN_AS_USER` and resolves the account's group
            // set and capability ceiling from the boot-installed identity
            // table, failing closed on an unknown uid.
            tairix_rt::spawn_as(path, u64::from(console), uid)
        }
        fn wait_any(&mut self, status: &mut i32) -> i64 {
            tairix_rt::wait_exit(tairix_abi::WAIT_PID_ANY, status)
        }
        fn report_launch_failure(&mut self, path: &[u8], console: u32, err: i64) {
            // One terse line on the inherited diagnostic stream, so a
            // refused session is visible at the console instead of silently
            // absent. Best-effort: PID 1 boots on with the surviving
            // sessions whether or not the write lands, and the kernel's own
            // audit log already carries the refusal.
            let shown = core::str::from_utf8(path).unwrap_or("<non-utf8 path>");
            let _ = Stderr.write_fmt(format_args!(
                "init: launch of {shown} on console {console} refused (err {err}); continuing without it\n"
            ));
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the machine-summary
    /// banner line to its inherited standard output (fd 1), brings the
    /// boot-floor services up through the [`Init`] service-manager engine in
    /// dependency order, then supervises one login session per discovered
    /// text console for the lifetime of PID 1, routing every service/orphan
    /// exit back to the engine ([`supervise`] — `plans/PI.md` P11,
    /// `plans/NEW-SERVICEMANAGER.md` SVC-A).
    ///
    /// The banner write is *gated*: `write_all` loops over benign short writes
    /// and fails closed only when the backing accepts nothing more (a missing
    /// `CAP_CONSOLE_WRITE`, an unresolved address space, an unestablished
    /// descriptor, or a closed-fail kernel path). PID 1 cannot usefully proceed
    /// without the console it was spawned to drive, so it parks fail-closed
    /// off the run queue (`tairix_rt::park_forever`) rather than supervising
    /// sessions on a console it never reached — a terminal park consuming no
    /// CPU, not a retry loop. Only when even that park is refused does it
    /// fall to the last-resort halt spin: with no console and no wait-set
    /// there is nothing left to park on or report to.
    fn main() -> i32 {
        let Ok(config) = StartupConfig::parse(DEFAULT_CONFIG) else {
            return EXIT_CONFIG_INVALID;
        };
        // The banner's machine facts come from the kernel-attested
        // `boot_facts_get` answer. A refusal omits the machine-summary line
        // (never a fabricated machine shape) and states its reason on the
        // diagnostic stream — fail loud, degrade gracefully; PID 1 boots on
        // either way. The identity and RAM figure were already drawn by the
        // kernel's early-boot RAM self-test, so `init` never repeats them.
        let facts = match tairix_rt::boot_facts() {
            Ok(facts) => Some(facts),
            Err(err) => {
                let _ = Stderr.write_fmt(format_args!(
                    "init: boot facts unavailable (err {err}); the machine-summary line is omitted\n"
                ));
                None
            }
        };
        let mut banner_buf = [0u8; BANNER_MAX];
        let banner = render_banner(facts, &mut banner_buf);
        // The shared `tairix_rt::io` short-write loop, never an init-private
        // copy (the charter forbids that duplication).
        if Stdout.write_all(banner.as_bytes()).is_err() {
            // Terminal park off the run queue — a spinning halt would peg a
            // core for the life of the system. The spin below runs only when
            // even the park is refused (a doubly-failed boot: no console, no
            // wait-set), where nothing better remains.
            let _ = tairix_rt::park_forever();
            loop {
                core::hint::spin_loop();
            }
        }

        // Bring the boot-floor services up through the service-manager engine
        // (`plans/NEW-SERVICEMANAGER.md` SVC-A). PID 1 names only each
        // service's `Run` binary and its compiled-in service account
        // (`plans/USERS.md`); the kernel — the single capability authority —
        // verifies the signed bundle and grants `manifest ∩ ceiling` at load
        // time. The engine orders the floor by declared dependencies (the
        // floor has none, so all are immediate) and reaps and restarts them
        // per their manifest policy; the growable, discovery-registered tier
        // past the floor lands with the userland heap (SVC-3/SVC-4).
        let spawner = RtSpawner;
        let stopper = RtStopper;
        let reaper = LoopReaper::new();
        let sink = LogSink;
        let mut engine = Init::new(InitConfig {
            spawner: &spawner,
            stopper: &stopper,
            reaper: &reaper,
            sink: &sink,
            // PID 1 is the single system service manager: it holds system
            // authority and manages the boot-floor services under their own
            // system service accounts. A per-user manager instance runs at
            // the confined `AuthorityScope::User` scope instead.
            scope: AuthorityScope::System,
        });
        for entry in config.services() {
            let spec =
                ServiceSpec::new(service_name(entry.path), entry.path, entry.uid, Vec::new());
            if engine.register(spec).is_err() {
                // Two floor services resolved to the same name — a defect in
                // the compiled-in `DEFAULT_CONFIG`, not a runtime input.
                let _ = Stderr.write_fmt(format_args!(
                    "init: duplicate service name for {}; refusing to boot a surprising system\n",
                    entry.path
                ));
                return EXIT_CONFIG_INVALID;
            }
        }
        let report = match engine.start_all() {
            Ok(report) => report,
            Err(err) => {
                // A structurally invalid floor graph (missing dependency or a
                // cycle). The floor is acyclic, so this is a build defect; the
                // engine has already audited `GRAPH_REJECTED`.
                let _ = Stderr.write_fmt(format_args!(
                    "init: boot-floor service graph rejected ({err:?}); refusing to boot\n"
                ));
                return EXIT_CONFIG_INVALID;
            }
        };
        // Fail loud, degrade gracefully: state each service the kernel refused
        // to start (a stale or mis-signed bundle) and boot on with the rest —
        // one dead service must not take down the device manager, the other
        // services, or the login sessions. The kernel's audit log already
        // carries the refusal; this makes it visible at the console too.
        for failed in &report.failed {
            let _ = Stderr.write_fmt(format_args!(
                "init: service {} not started ({:?}); continuing without it\n",
                failed.name, failed.failure
            ));
        }

        // Supervise one login session per console and route every other
        // reaped child — a service the engine started, or an inherited
        // orphan — back to the engine. The session table is a fixed stack
        // array; the engine owns the (heap-backed) service state.
        let mut services = EngineServices {
            engine: &mut engine,
            reaper: &reaper,
        };
        let session = Launch {
            path: config.session().path.as_bytes(),
            uid: config.session().uid,
        };
        match supervise(&mut services, &mut RtSessions, session) {
            Outcome::NoConsoles => EXIT_NO_CONSOLES,
            Outcome::WaitFailed => EXIT_WAIT_FAILED,
            Outcome::Exhausted => EXIT_SESSION_EXHAUSTED,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// parses the compiled-in default config (touching the parser's accessors and
// the `service_name` derivation each boot-floor entry now flows through) so a
// malformed `DEFAULT_CONFIG` is caught by an ordinary `cargo build`, and
// drives the session supervisor against inert zero-console / no-service seams
// so neither is dead code on the host. It performs no I/O. The real
// engine-backed [`Services`](supervisor::Services) glue is exercised by the
// freestanding build and the QEMU boot vertical; the pure supervision policy
// and the engine itself are host-tested in their own modules.
/// An inert host-stub session seam: zero consoles, so the supervisor returns
/// [`supervisor::Outcome::NoConsoles`] without spawning or waiting.
#[cfg(not(freestanding))]
struct NoSessions;

#[cfg(not(freestanding))]
impl supervisor::Sessions for NoSessions {
    fn console_count(&mut self) -> i64 {
        0
    }
    fn spawn_at(&mut self, _path: &[u8], _console: u32, _uid: u32) -> i64 {
        -1
    }
    fn wait_any(&mut self, _status: &mut i32) -> i64 {
        -1
    }
    fn report_launch_failure(&mut self, _path: &[u8], _console: u32, _err: i64) {}
}

/// An inert host-stub service seam: no running service, so the supervisor's
/// exhaustion check depends only on the (also inert) sessions.
#[cfg(not(freestanding))]
struct NoServices;

#[cfg(not(freestanding))]
impl supervisor::Services for NoServices {
    fn on_child_exit(&mut self, _pid: u64, _exit_code: i32) {}
    fn any_running(&self) -> bool {
        false
    }
}

#[cfg(not(freestanding))]
fn main() {
    if let Ok(config) = startup::StartupConfig::parse(startup::DEFAULT_CONFIG) {
        let mut banner_buf = [0u8; startup::BANNER_MAX];
        // Touch the `service_name` derivation every boot-floor entry now flows
        // through, so a regression in it is caught by an ordinary host build.
        for entry in config.services() {
            let _ = startup::service_name(entry.path);
        }
        let _ = (
            config.session(),
            startup::render_banner(None, &mut banner_buf),
        );
    }
    assert_eq!(
        supervisor::supervise(
            &mut NoServices,
            &mut NoSessions,
            supervisor::Launch {
                path: b"session",
                uid: 0,
            },
        ),
        supervisor::Outcome::NoConsoles
    );
}

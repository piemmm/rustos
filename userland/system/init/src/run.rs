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

    use tairix_abi::service_control::{ServiceControlRequest, ServiceEnrolRequest};
    use tairix_abi::{CapabilityId, Duration64, Errno, Signal, WaitSetOp, WaitSourceKind};
    use tairix_caps::CapabilitySet;
    use tairix_init::{
        enrol, AuthorityScope, ControlError, Enrolment, EnrolmentOverride, Init, InitConfig,
        LoopReaper, Pid, ReapedChild, ServiceSpec, Spawner, Stopper,
    };
    use tairix_rt::io::{Stderr, Stdout, Write};
    use tairix_rt::LogSink;
    use tairix_util::retry::RetryLadder;

    use crate::startup::{render_banner, service_name, StartupConfig, BANNER_MAX, DEFAULT_CONFIG};
    use crate::supervisor::{supervise, Launch, Outcome, Services, Sessions, Woke};

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

    /// Exit code when PID 1 could not create the wait-set it supervises from,
    /// or could not bind the service-control endpoint. A reserved, fail-closed
    /// value distinct from the other exits so the cause is unambiguous in the
    /// transcript: without the wait-set there is no park to multiplex sessions,
    /// control, and timers over.
    const EXIT_WAITSET_FAILED: i32 = 75;

    /// The primary console index services attach their standard streams to
    /// (for their fd 2 diagnostics). Sessions fan out across every console;
    /// a service has no console of its own, so it takes console 0.
    const SERVICE_CONSOLE: u64 = 0;

    /// Register the compiled-in bootstrap floor and the enrolment-governed
    /// tier, reporting `false` (with its reason on the diagnostic stream) if
    /// either is structurally invalid.
    ///
    /// The floor is registered unconditionally: those services exist below the
    /// registration store, so no enrolment record could govern them. The
    /// enrolled tier is registered only where the effective enrolment enables
    /// it; at boot that is the image's layer alone, because the
    /// administrator's overrides live on the encrypted root, so the manager
    /// boots on the image's decision and narrows to the administrator's as soon
    /// as that document appears.
    fn register_startup_services(engine: &mut Init<'_>, config: &StartupConfig<'_>) -> bool {
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
                return false;
            }
        }
        let enrolled: Vec<ServiceSpec> = config
            .enrolled()
            .iter()
            .map(|entry| {
                ServiceSpec::new(service_name(entry.path), entry.path, entry.uid, Vec::new())
            })
            .collect();
        // The image's layer is the `enrolled` tier itself: every directive it
        // carries is enrolled by default. It cannot come off disk, because a
        // document under `/System` is not reliably readable at the instant PID
        // 1 must decide what to bring up — the writable root is not mounted
        // and the read-only volume's availability is a boot-order fact PID 1
        // has no event for. The administrator's layer *is* on disk and is
        // adopted as soon as it can be read.
        let vendor = enrolled
            .iter()
            .try_fold(Enrolment::empty(), |set, spec| enrol(&set, spec.name()));
        let Ok(vendor) = vendor else {
            let _ = Stderr.write_fmt(format_args!(
                "init: an enrolled service's name is not a valid identifier; refusing to boot\n"
            ));
            return false;
        };
        if engine
            .register_enrolled(enrolled, vendor, EnrolmentOverride::empty())
            .is_err()
        {
            let _ = Stderr.write_fmt(format_args!(
                "init: an enrolled service clashes with the boot floor or is out of scope; refusing to boot\n"
            ));
            return false;
        }
        true
    }

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

    /// How long PID 1 waits before its first attempt to read the
    /// administrator's enrolment overrides, and the base its retry doubles.
    ///
    /// The document lives on the encrypted root, which is unlocked a few
    /// seconds after PID 1 starts, and no userland event says when — so the
    /// wait is a bounded doubling one-shot ladder, never a poll.
    const OVERRIDE_RETRY_BASE: Duration64 = Duration64::from_secs(1);

    /// How many rungs that ladder climbs before it stops asking.
    ///
    /// Six doublings from one second span about a minute, which comfortably
    /// covers an unlock; a machine that never unlocks (a recovery session, a
    /// volume-less test guest) has no such document at all, and the ladder's
    /// own finite length is what bounds that case rather than a guess at the
    /// error `open` returns — an unmounted root and an absent one look
    /// identical from here.
    const OVERRIDE_RETRY_ATTEMPTS: u32 = 6;

    /// Read the administrator's override layer off the encrypted root, or
    /// `None` while the document is unreachable.
    ///
    /// A document that is present but malformed resolves to the empty layer —
    /// obey the signed image — rather than being retried for ever.
    fn read_overrides() -> Option<EnrolmentOverride> {
        let text = read_document(tairix_abi::SERVICE_OVERRIDES_PATH)?;
        Some(EnrolmentOverride::parse(&text).unwrap_or_else(|_| EnrolmentOverride::empty()))
    }

    /// Read a whole enrolment document as UTF-8 text, or `None` if it cannot
    /// be opened, read, or decoded.
    fn read_document(path: &str) -> Option<alloc::string::String> {
        let file = tairix_rt::open(path.as_bytes()).ok()?;
        let bytes = tairix_rt::read_fd_to_end(file.fd(), ENROLMENT_DOCUMENT_MAX).ok()?;
        // The reader answers *past* the cap, so an oversize document is
        // refused whole rather than parsed as a shortened one.
        (bytes.len() <= ENROLMENT_DOCUMENT_MAX)
            .then(|| alloc::string::String::from_utf8(bytes).ok())
            .flatten()
    }

    /// Byte ceiling on an enrolment document.
    ///
    /// A validation bound, not a capacity: these documents hold one short
    /// service name per line, so anything larger is a corrupt or hostile file
    /// and is refused rather than read into PID 1's heap.
    const ENROLMENT_DOCUMENT_MAX: usize = 64 * 1024;

    /// Persist the administrator's override layer, creating its directory if
    /// the volume was laid out before this manager existed.
    fn write_overrides(overrides: &EnrolmentOverride) -> Result<(), Errno> {
        let text = overrides.to_store_text();
        let path = tairix_abi::SERVICE_OVERRIDES_PATH.as_bytes();
        let file = match tairix_rt::create(path) {
            Ok(file) => file,
            // `/System/Settings` is system-owned, so an unconditional `mkdir`
            // would be refused on every provisioned machine and file a denied
            // mutation record on each request, burying a real denial in noise.
            Err(ret) if Errno::from_syscall(ret) == Errno::NotFound => {
                let made = tairix_rt::fs_mkdir(tairix_abi::SERVICE_OVERRIDES_DIR.as_bytes());
                if made != 0 && Errno::from_syscall(made) != Errno::AlreadyExists {
                    return Err(Errno::from_syscall(made));
                }
                tairix_rt::create(path).map_err(Errno::from_syscall)?
            }
            Err(ret) => return Err(Errno::from_syscall(ret)),
        };
        let bytes = text.as_bytes();
        let written = file.write_at(0, bytes).map_err(Errno::from_syscall)?;
        if written != bytes.len() {
            return Err(Errno::BufferTooSmall);
        }
        // A shorter document must not leave the tail of a longer one behind,
        // which would reparse as entries the administrator did not write.
        let truncated = tairix_rt::fs_truncate(file.fd(), written as u64);
        if truncated != 0 {
            return Err(Errno::from_syscall(truncated));
        }
        Ok(())
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
        /// The bounded one-shot schedule for reading the administrator's
        /// enrolment overrides off the encrypted root, or `None` once they
        /// have been adopted or the ladder is spent.
        override_retry: Option<RetryLadder>,
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
            self.engine.arm_watchdogs(now);
        }

        fn any_running(&self) -> bool {
            self.engine.running_count() > 0
        }

        fn next_timeout_ns(&mut self) -> u64 {
            let engine_at = self
                .engine
                .next_deadline()
                .map(|d| d.saturating_total_nanos());
            let soonest = match (engine_at, self.override_retry.map(|l| l.at)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (only, None) | (None, only) => only,
            };
            let Some(at) = soonest else {
                return tairix_abi::WAITSET_TIMEOUT_NONE;
            };
            // The deadlines are absolute monotonic instants and the park takes
            // a relative span, so an already-lapsed deadline is a zero-length
            // wait that returns at once and is served on the next turn. That
            // cannot spin: `expire_due` consumes or drops every deadline it
            // finds lapsed, and the override ladder either advances a rung or
            // disarms, so the same wakeup is never served twice.
            at.saturating_sub(tairix_rt::clock_get())
        }

        fn serve_control(&mut self) {
            let mut request = [0u8; tairix_abi::service_control::REQUEST_LEN];
            let mut ticket = 0u64;
            let Ok(len) = tairix_rt::call_recv_nonblock(
                tairix_abi::service_control::SERVICE_CONTROL_ENDPOINT,
                &mut request,
                &mut ticket,
            ) else {
                // The readiness peek raced another drain, or the kernel
                // refused. There is nothing to answer and no ticket to
                // release, so the loop simply parks again.
                return;
            };

            let now = Duration64::from_nanos(tairix_rt::clock_get());
            let mut reply = [0u8; tairix_abi::service_control::REPLY_LEN];
            // A malformed frame is answered with the decoder's own refusal
            // rather than dropped: the caller is waiting synchronously, and
            // leaving it parked would be a denial of service against a
            // principal that reached a gated endpoint legitimately.
            let outcome = ServiceControlRequest::decode(&request[..len])
                .map_err(ControlReply::Verbatim)
                .and_then(|req| self.engine.control(req, now).map_err(ControlReply::Refused));
            let written = match outcome {
                Ok(state) => tairix_abi::service_control::encode_reply(&mut reply, state),
                Err(ControlReply::Verbatim(err)) => {
                    tairix_abi::service_control::encode_error_reply(&mut reply, err)
                }
                Err(ControlReply::Refused(err)) => {
                    tairix_abi::service_control::encode_error_reply(&mut reply, control_errno(err))
                }
            };
            // The buffer is `REPLY_LEN`, which both encoders fit, so the
            // encode cannot fail — but the caller is parked on this ticket, so
            // it is answered whatever happened rather than left waiting. A
            // zero-length frame decodes as a refusal on the client side, which
            // is the fail-closed direction.
            let _ = tairix_rt::call_reply(
                tairix_abi::service_control::SERVICE_CONTROL_ENDPOINT,
                ticket,
                &reply[..written.unwrap_or(0)],
            );
            self.engine.arm_watchdogs(now);
        }

        fn serve_enrol(&mut self) {
            let mut request = [0u8; tairix_abi::service_control::REQUEST_LEN];
            let mut ticket = 0u64;
            let Ok(len) = tairix_rt::call_recv_nonblock(
                tairix_abi::service_control::SERVICE_ENROL_ENDPOINT,
                &mut request,
                &mut ticket,
            ) else {
                return;
            };

            let now = Duration64::from_nanos(tairix_rt::clock_get());
            let mut reply = [0u8; tairix_abi::service_control::ENROL_REPLY_LEN];
            // A malformed frame is answered rather than dropped, for the same
            // reason the control endpoint answers one: the caller is parked
            // synchronously and a silent drop would deny a legitimate
            // principal.
            let outcome = ServiceEnrolRequest::decode(&request[..len])
                .map_err(ControlReply::Verbatim)
                .and_then(|req| {
                    self.engine
                        .enrol_control(req, now)
                        .map_err(ControlReply::Refused)
                })
                .and_then(|report| {
                    // The decision is only durable once the document is on
                    // disk, so a failed write is reported rather than
                    // acknowledged — otherwise the next boot would silently
                    // contradict the answer the administrator was given.
                    if report.changed {
                        write_overrides(&report.overrides).map_err(ControlReply::Verbatim)?;
                    }
                    Ok(report)
                });
            let written = match outcome {
                Ok(report) => tairix_abi::service_control::encode_enrol_reply(
                    &mut reply,
                    report.enrolment,
                    report.changed,
                ),
                Err(ControlReply::Verbatim(err)) => {
                    tairix_abi::service_control::encode_error_reply(&mut reply, err)
                }
                Err(ControlReply::Refused(err)) => {
                    tairix_abi::service_control::encode_error_reply(&mut reply, control_errno(err))
                }
            };
            let _ = tairix_rt::call_reply(
                tairix_abi::service_control::SERVICE_ENROL_ENDPOINT,
                ticket,
                &reply[..written.unwrap_or(0)],
            );
            self.engine.arm_watchdogs(now);
        }

        fn expire_deadlines(&mut self) {
            let now = Duration64::from_nanos(tairix_rt::clock_get());
            self.try_adopt_overrides(now);
            let report = self.engine.expire_due(now);
            for failed in &report.failed {
                // Fail loud, degrade gracefully: a relaunch the kernel
                // refused is stated on the diagnostic stream, exactly as at
                // boot, and the rest of the system stays up.
                let _ = Stderr.write_fmt(format_args!(
                    "init: service {} not restarted ({:?}); continuing without it\n",
                    failed.name, failed.failure
                ));
            }
            self.engine.arm_watchdogs(now);
        }
    }

    impl EngineServices<'_, '_> {
        /// Try once to read the administrator's enrolment overrides, if the
        /// ladder is armed and its rung is due.
        ///
        /// Pre-unlock the manager obeys the image's layer alone, so this is
        /// where a service the administrator disabled is stopped. A rung that
        /// finds nothing advances; a spent ladder disarms and the image's layer
        /// stands for the rest of the boot, which is the honest answer for a
        /// machine that never unlocks.
        fn try_adopt_overrides(&mut self, now: Duration64) {
            let Some(mut ladder) = self.override_retry else {
                return;
            };
            if now.saturating_total_nanos() < ladder.at {
                return;
            }
            if let Some(overrides) = read_overrides() {
                self.override_retry = None;
                for name in self.engine.adopt_overrides(overrides, now) {
                    let _ = Stderr.write_fmt(format_args!(
                        "init: service {name} stopped: disabled by the administrator\n"
                    ));
                }
                return;
            }
            self.override_retry = ladder
                .advance(now.saturating_total_nanos())
                .then_some(ladder);
        }
    }

    /// Which half of the path refused a request, so the reply carries the right
    /// `Errno` without conflating a refusal that already *is* the right code
    /// with a well-formed request the manager declined.
    enum ControlReply {
        /// The reply carries this `Errno` as it stands: the decoder's own
        /// refusal of a malformed frame, or the store write's refusal of an
        /// enrolment change the manager could not persist — a decision the next
        /// boot would contradict is not a decision, so it is reported rather
        /// than acknowledged. Neither is about the caller's authority.
        Verbatim(Errno),
        /// The frame decoded but the manager declined the operation, so the
        /// refusal is mapped onto the errno the wire carries.
        Refused(ControlError),
    }

    /// Map a manager refusal onto the `Errno` the reply carries.
    ///
    /// The control wire has no room for a richer reason and does not need one:
    /// the manager has already audited the refusal with its cause, so the
    /// caller learns *that* it was refused and the operator reads *why* in the
    /// log.
    const fn control_errno(err: ControlError) -> Errno {
        match err {
            ControlError::UnknownService => Errno::NotFound,
            // Retryable: a readiness condition is unmet or the service is
            // mid-teardown, so the resource is simply not in a state to serve
            // the request.
            ControlError::Unavailable => Errno::Busy,
            // Not `PermissionDenied`: the caller's authority was sufficient —
            // it reached a gated endpoint — and it is the *target's* bundle
            // that the load gate refused. Blaming the caller would send an
            // administrator hunting the wrong problem.
            ControlError::NotStartable => Errno::NotSupported,
        }
    }

    /// Wait-set token identifying the service-control endpoint member.
    const TOKEN_CONTROL: u64 = 1;

    /// Wait-set token identifying the any-child member.
    const TOKEN_CHILD: u64 = 2;

    /// Wait-set token identifying the service-enrolment endpoint member.
    const TOKEN_ENROL: u64 = 3;

    /// How many control requests the endpoint queues before a further caller
    /// is refused by the kernel.
    ///
    /// A control tool call is synchronous and PID 1 answers one per wakeup, so
    /// the queue only absorbs callers that arrive while an earlier one is
    /// being served. A bound rather than a capacity: this is the depth at
    /// which a flood of control calls is refused instead of consuming kernel
    /// memory, and only an administrator can reach the endpoint at all.
    const CONTROL_QUEUE_DEPTH: usize = 4;

    /// The production [`Sessions`] backing: the real `tairix-rt` syscall
    /// wrappers (`console_count`, the console-selecting `spawn_at`) over the
    /// wait-set PID 1 parks on. The per-console session table lives on
    /// `main`'s stack inside [`supervise`].
    struct RtSessions {
        /// The wait-set handle carrying the control-endpoint and any-child
        /// members.
        set: u64,
    }

    impl RtSessions {
        /// Create the wait-set and enrol both members, or report the kernel's
        /// `-errno`.
        ///
        /// Both control endpoints are created here rather than by a separate
        /// service because PID 1 *is* the system service manager: each is
        /// bound restricted-sender, so the kernel refuses a call from a task
        /// without `CAP_SERVICE_CONTROL` and the engine never re-checks a
        /// caller-supplied claim.
        fn new() -> Result<Self, i64> {
            for (endpoint, reply_len) in [
                (
                    tairix_abi::service_control::SERVICE_CONTROL_ENDPOINT,
                    tairix_abi::service_control::REPLY_LEN,
                ),
                (
                    tairix_abi::service_control::SERVICE_ENROL_ENDPOINT,
                    tairix_abi::service_control::ENROL_REPLY_LEN,
                ),
            ] {
                let created = tairix_rt::call_create(
                    endpoint,
                    &control_send_caps(),
                    &CapabilitySet::empty(),
                    tairix_abi::service_control::REQUEST_LEN,
                    reply_len,
                    CONTROL_QUEUE_DEPTH,
                );
                if created != 0 {
                    return Err(created);
                }
            }
            let set = tairix_rt::waitset_create();
            if set < 0 {
                return Err(set);
            }
            // A non-negative kernel result is a valid handle.
            #[allow(clippy::cast_sign_loss)]
            let set = set as u64;
            for (kind, id, token) in [
                (
                    WaitSourceKind::Endpoint,
                    tairix_abi::service_control::SERVICE_CONTROL_ENDPOINT,
                    TOKEN_CONTROL,
                ),
                (
                    WaitSourceKind::Endpoint,
                    tairix_abi::service_control::SERVICE_ENROL_ENDPOINT,
                    TOKEN_ENROL,
                ),
                (
                    WaitSourceKind::Child,
                    tairix_abi::WAITSET_CHILD_ANY,
                    TOKEN_CHILD,
                ),
            ] {
                let added = tairix_rt::waitset_ctl(set, WaitSetOp::Add, kind, id, token);
                if added != 0 {
                    return Err(added);
                }
            }
            Ok(Self { set })
        }
    }

    /// The capability a caller must hold to reach either control endpoint.
    ///
    /// One definition, used both to bind the endpoint and by the manifest
    /// pinning tests, so the gate and the tool's request cannot drift.
    fn control_send_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::SERVICE_CONTROL);
        caps
    }

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

        fn wait_next(&mut self, timeout_ns: u64, status: &mut i32) -> Woke {
            let mut token = 0u64;
            let woke = tairix_rt::waitset_wait(self.set, timeout_ns, &mut token);
            if woke < 0 {
                return if Errno::from_syscall(woke) == Errno::TimedOut {
                    Woke::Deadline
                } else {
                    Woke::Failed
                };
            }
            match token {
                TOKEN_CONTROL => Woke::Control,
                TOKEN_ENROL => Woke::Enrol,
                TOKEN_CHILD => {
                    // A child member's readiness is a peek, so the reap is a
                    // separate non-blocking call — it must never park the one
                    // loop that also owes the control endpoint an answer. A
                    // reported-ready child that is then not reapable is a
                    // kernel-state inconsistency, not a quiet retry.
                    let reaped = tairix_rt::try_wait_exit(tairix_abi::WAIT_PID_ANY, status);
                    if reaped < 0 {
                        Woke::Failed
                    } else {
                        Woke::Child(reaped.unsigned_abs())
                    }
                }
                _ => Woke::Failed,
            }
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
        if !register_startup_services(&mut engine, &config) {
            return EXIT_CONFIG_INVALID;
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
            override_retry: RetryLadder::arm(
                tairix_rt::clock_get(),
                OVERRIDE_RETRY_BASE.saturating_total_nanos(),
                OVERRIDE_RETRY_ATTEMPTS,
                false,
            ),
        };
        let session = Launch {
            path: config.session().path.as_bytes(),
            uid: config.session().uid,
        };
        // The wait-set is PID 1's only park: the control endpoint, any-child
        // readiness, and the one-shot deadlines all wake it. Without it there
        // is nothing to supervise sessions *from*, so a refusal is fatal and
        // says why rather than silently degrading to a wait on children.
        let mut sessions = match RtSessions::new() {
            Ok(sessions) => sessions,
            Err(err) => {
                let _ = Stderr.write_fmt(format_args!(
                    "init: service-control wait-set unavailable (err {err}); refusing to boot a system it cannot supervise\n"
                ));
                return EXIT_WAITSET_FAILED;
            }
        };
        match supervise(&mut services, &mut sessions, session) {
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
// drives the session supervisor against scripted seams that replay one of
// every event, so no reactor arm is dead code on the host. It performs no
// I/O. The real engine-backed [`Services`](supervisor::Services) glue is
// exercised by the freestanding build and the QEMU boot vertical; the pure
// supervision policy and the engine itself are host-tested in their own
// modules.
/// A host-stub session seam replaying a scripted event sequence, so the host
/// build drives every arm of the reactor's dispatch.
#[cfg(not(freestanding))]
struct StubSessions {
    /// The events `wait_next` replays, in order; a spent script reports a
    /// failed park so the supervisor terminates.
    script: &'static [supervisor::Woke],
    next: usize,
}

#[cfg(not(freestanding))]
impl supervisor::Sessions for StubSessions {
    fn console_count(&mut self) -> i64 {
        1
    }
    fn spawn_at(&mut self, _path: &[u8], _console: u32, _uid: u32) -> i64 {
        1
    }
    fn wait_next(&mut self, _timeout_ns: u64, _status: &mut i32) -> supervisor::Woke {
        let woke = self
            .script
            .get(self.next)
            .copied()
            .unwrap_or(supervisor::Woke::Failed);
        self.next += 1;
        woke
    }
    fn report_launch_failure(&mut self, _path: &[u8], _console: u32, _err: i64) {}
}

/// A host-stub service seam that records which reactor callbacks the
/// supervisor drove, so the host build covers the dispatch rather than only
/// type-checking it.
#[cfg(not(freestanding))]
#[derive(Default)]
struct StubServices {
    control: usize,
    enrol: usize,
    deadlines: usize,
    exits: usize,
}

#[cfg(not(freestanding))]
impl supervisor::Services for StubServices {
    fn on_child_exit(&mut self, _pid: u64, _exit_code: i32) {
        self.exits += 1;
    }
    fn any_running(&self) -> bool {
        false
    }
    fn next_timeout_ns(&mut self) -> u64 {
        tairix_abi::WAITSET_TIMEOUT_NONE
    }
    fn serve_control(&mut self) {
        self.control += 1;
    }
    fn serve_enrol(&mut self) {
        self.enrol += 1;
    }
    fn expire_deadlines(&mut self) {
        self.deadlines += 1;
    }
}

#[cfg(not(freestanding))]
fn main() {
    if let Ok(config) = startup::StartupConfig::parse(startup::DEFAULT_CONFIG) {
        let mut banner_buf = [0u8; startup::BANNER_MAX];
        // Touch the `service_name` derivation every startup entry now flows
        // through — the floor and the enrolment-governed tier alike — so a
        // regression in it is caught by an ordinary host build.
        for entry in config.services().iter().chain(config.enrolled()) {
            let _ = startup::service_name(entry.path);
        }
        let _ = (
            config.session(),
            startup::render_banner(None, &mut banner_buf),
        );
    }
    // Drive the reactor's dispatch over a scripted seam so an ordinary host
    // build covers every arm: a control request, an enrolment request, a
    // lapsed deadline, a non-session child exit, then a failed park that ends
    // the loop.
    let mut services = StubServices::default();
    let mut sessions = StubSessions {
        script: &[
            supervisor::Woke::Control,
            supervisor::Woke::Enrol,
            supervisor::Woke::Deadline,
            supervisor::Woke::Child(4242),
            supervisor::Woke::Failed,
        ],
        next: 0,
    };
    assert_eq!(
        supervisor::supervise(
            &mut services,
            &mut sessions,
            supervisor::Launch {
                path: b"session",
                uid: 0,
            },
        ),
        supervisor::Outcome::WaitFailed
    );
    assert_eq!(
        (
            services.control,
            services.enrol,
            services.deadlines,
            services.exits
        ),
        (1, 1, 1, 1)
    );
}

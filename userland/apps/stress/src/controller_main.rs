//! The controller role's program half (`plans/STRESSTEST.md` §7.1): wire
//! the host-proven [`Controller`] state machine to the real syscalls.
//!
//! The controller pins itself (incidental — a refusal is a stderr notice,
//! never fatal), opts into the signal intake so `^C`/`Terminate` are
//! observed and torn down cleanly, prepares the scratch directory, sizes
//! the byte targets from discovered RAM and free space, spawns each
//! worker through the kernel's attested `@self` token, and then parks on
//! **one wait-set** — child exits, the signal intake, and the
//! timeout/grace deadline as the bounded wait — executing the machine's
//! actions until every child is reaped. Scratch files are removed on
//! every exit path, and the summary (stdout + the fd-3 record) is
//! reported unless `--quiet`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{
    Errno, Signal, SignalIntakeOp, WaitFlags, WaitSetOp, WaitSourceKind, WaitStatus,
    CONSOLE_INHERIT, SPAWN_SELF, SPAWN_UID_INHERIT, SYSTEM_COMMAND_STORE, WAITSET_CHILD_ANY,
    WAIT_PID_ANY,
};
use tairix_procinfo::{for_each_mount, IpcTransport};
use tairix_rt::io::{write_stderr_line, StdInfo, Stdout, Write};
use tairix_stress::{
    completion_line, dispatch_line, refusal_line, run_scratch_paths, size_targets, summary_record,
    Action, Controller, Discovered, Event, RunSpec, WorkerKind, WorkerSpec, USAGE, WORKER_FLAG,
};

extern crate alloc;

/// The teardown grace budget: how long a `Terminate`d child gets before
/// the controller escalates to `Kill`.
const GRACE_NS: u64 = 2_000_000_000;

/// The wait-set token of the child-exit member.
const TOKEN_CHILD: u64 = 1;
/// The wait-set token of the signal-intake member.
const TOKEN_SIGNAL: u64 = 2;

/// Run the controller role to completion, returning the process exit
/// status (`0`/`1`/`130`/`143`).
pub fn run(spec: &RunSpec) -> i32 {
    if spec.background {
        return background_respawn();
    }

    // Pin before dispatch: the controller must never stall on its own
    // page fault-in under the very pressure it creates. Incidental — the
    // run continues unpinned on a refusal, stated once.
    if tairix_rt::mem_pin() != 0 {
        write_stderr_line("stress: notice: running unpinned (mem_pin refused)");
    }

    // Observe Interrupt/Terminate instead of dying mid-run: the teardown
    // (workers signalled, reaped, scratch removed) runs before exit. A
    // refused opt-in degrades to the default terminate disposition,
    // stated once — the load still runs.
    let intake = tairix_rt::signal_intake(SignalIntakeOp::Enable) == 0;
    if !intake {
        write_stderr_line("stress: notice: signal observation unavailable; ^C will not tear down");
    }

    // The scratch directory, only when a disk-touching worker needs one.
    let needs_scratch = spec.workers.io + spec.workers.hdd + spec.workers.cache > 0;
    let scratch = if needs_scratch {
        match prepare_scratch(spec) {
            Ok(dir) => dir,
            Err(reason) => {
                write_stderr_line(&format!("stress: {reason}"));
                return 1;
            }
        }
    } else {
        String::new()
    };

    // Size the byte targets from what the machine actually has.
    let discovered = Discovered {
        ram_bytes: tairix_rt::boot_facts().ok().map(|facts| facts.memory_bytes),
        scratch_free_bytes: scratch_free_bytes(&scratch),
    };
    let targets = size_targets(spec, &discovered);

    let own_pid = tairix_rt::self_origin().map_or(0, |origin| origin.pid());
    if !spec.quiet {
        let _ = Stdout.write_all(dispatch_line(own_pid, &spec.workers).as_bytes());
    }

    let started_ns = tairix_rt::clock_get();
    let mut machine = Controller::new();

    // Dispatch the workers: each is this same binary re-entered through
    // the kernel's attested `@self` token with the closed worker argv.
    let plan: [(WorkerKind, u32, u64); 5] = [
        (WorkerKind::Cpu, spec.workers.cpu, 0),
        (WorkerKind::Vm, spec.workers.vm, targets.vm_bytes),
        (WorkerKind::Io, spec.workers.io, targets.io_bytes),
        (WorkerKind::Hdd, spec.workers.hdd, targets.hdd_bytes),
        (WorkerKind::Cache, spec.workers.cache, targets.cache_bytes),
    ];
    for (kind, count, bytes) in plan {
        for index in 0..count {
            let worker = WorkerSpec {
                kind,
                bytes,
                index,
                scratch: if kind.uses_scratch() {
                    scratch.clone()
                } else {
                    String::new()
                },
            };
            match spawn_worker(&worker) {
                Ok(pid) => machine.add_worker(pid, kind),
                Err(reason) => {
                    // A refused dispatch fails the run loudly; whatever
                    // was already spawned is torn down through the normal
                    // signalled path.
                    write_stderr_line(&format!("stress: {reason}"));
                    let actions = machine.on_event(Event::Signalled(Signal::Terminate));
                    let mut grace = None;
                    execute(&actions, started_ns, &mut grace);
                    drive(&mut machine, intake, None, grace, started_ns);
                    cleanup_scratch(spec, &scratch);
                    return 1;
                }
            }
        }
    }

    // `--monitor`: the installed sysmon bundle in the foreground — one
    // monitor implementation, never an embedded copy. A refused spawn is
    // a notice (the load is the purpose; the monitor is incidental).
    if spec.monitor {
        let pid = tairix_rt::spawn(format!("{SYSTEM_COMMAND_STORE}/sysmon.app/Run").as_bytes());
        if pid >= 0 {
            #[allow(clippy::cast_possible_truncation)] // The kernel bounds PIDs to i32 range.
            let pid = pid as i32;
            machine.add_monitor(pid);
            // Hand the console to the monitor so its keys (and the
            // console ^C) reach it; the console clears the grant when it
            // exits.
            let _ = tairix_rt::console_foreground(0, pid);
        } else {
            write_stderr_line("stress: notice: sysmon could not be started; running unmonitored");
        }
    }

    let deadline_ns = spec
        .timeout_secs
        .map(|secs| started_ns.saturating_add(secs.saturating_mul(1_000_000_000)));
    drive(&mut machine, intake, deadline_ns, None, started_ns);

    cleanup_scratch(spec, &scratch);

    let elapsed_secs = tairix_rt::clock_get().saturating_sub(started_ns) / 1_000_000_000;
    let code = machine.exit_code();
    if !spec.quiet {
        let tally = machine.tally();
        if let Some(line) = refusal_line(own_pid, tally.refused) {
            let _ = Stdout.write_all(line.as_bytes());
        }
        let _ = Stdout.write_all(completion_line(own_pid, code, elapsed_secs).as_bytes());
    }
    // The fd-3 summary record is advisory and best-effort, never a
    // failure path.
    let mut buf = [0u8; 512];
    let len = summary_record(&machine.tally(), code, elapsed_secs, &mut buf);
    if len > 0 {
        let _ = StdInfo.write_all(&buf[..len]);
    }
    code
}

/// Drive the machine to completion off the one wait-set.
fn drive(
    machine: &mut Controller,
    intake: bool,
    mut deadline_ns: Option<u64>,
    mut grace_deadline_ns: Option<u64>,
    started_ns: u64,
) {
    let set = tairix_rt::waitset_create();
    let set = if set >= 0 {
        #[allow(clippy::cast_sign_loss)] // A non-negative create result is the handle.
        Some(set as u64)
    } else {
        None
    };
    if let Some(set) = set {
        let _ = tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Child,
            WAITSET_CHILD_ANY,
            TOKEN_CHILD,
        );
        if intake {
            let _ = tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Signal,
                0,
                TOKEN_SIGNAL,
            );
        }
    }

    while !machine.is_done() {
        // Reap everything already waiting, then drain the intake, then
        // check the clocks — each drain feeds the machine and executes
        // its actions.
        loop {
            let mut status = WaitStatus::Exited(0);
            let ret = tairix_rt::try_wait(WAIT_PID_ANY, &mut status);
            if ret < 0 {
                break;
            }
            #[allow(clippy::cast_possible_truncation)] // The kernel bounds PIDs to i32 range.
            let pid = ret as i32;
            let code = match status {
                WaitStatus::Exited(code) => code,
                WaitStatus::Stopped(_) => continue,
            };
            let actions = machine.on_event(Event::ChildExited { pid, code });
            execute(&actions, started_ns, &mut grace_deadline_ns);
        }
        if intake {
            loop {
                let ret = tairix_rt::signal_intake(SignalIntakeOp::Take);
                if ret < 0 {
                    break;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                // A non-negative take result is the drained signal's u32 wire discriminant.
                let Ok(signal) = Signal::from_u32(ret as u32) else {
                    break;
                };
                let actions = machine.on_event(Event::Signalled(signal));
                execute(&actions, started_ns, &mut grace_deadline_ns);
            }
        }
        let now = tairix_rt::clock_get();
        if let Some(deadline) = deadline_ns {
            if now >= deadline {
                deadline_ns = None;
                let actions = machine.on_event(Event::TimeoutElapsed);
                execute(&actions, started_ns, &mut grace_deadline_ns);
            }
        }
        if let Some(grace) = grace_deadline_ns {
            if now >= grace {
                grace_deadline_ns = None;
                let actions = machine.on_event(Event::GraceElapsed);
                execute(&actions, started_ns, &mut grace_deadline_ns);
            }
        }
        if machine.is_done() {
            break;
        }

        // Park until the next event or the nearest armed deadline — the
        // controller never spins; the wait-set (or, if it could not be
        // built, a blocking reap) is the wake source.
        let timeout = next_timeout(tairix_rt::clock_get(), deadline_ns, grace_deadline_ns);
        if let Some(set) = set {
            let mut token = 0u64;
            let _ = tairix_rt::waitset_wait(set, timeout, &mut token);
        } else {
            // Fail-safe path without a wait-set: block on the next child
            // exit (the only other event source that can complete the
            // run).
            let mut status = WaitStatus::Exited(0);
            let ret = tairix_rt::wait(WAIT_PID_ANY, &mut status, WaitFlags::empty());
            if ret < 0 {
                // No children left to wait for and no set to park on:
                // nothing further can happen.
                break;
            }
            #[allow(clippy::cast_possible_truncation)] // The kernel bounds PIDs to i32 range.
            let pid = ret as i32;
            if let WaitStatus::Exited(code) = status {
                let actions = machine.on_event(Event::ChildExited { pid, code });
                execute(&actions, started_ns, &mut grace_deadline_ns);
            }
        }
    }
}

/// Execute the machine's actions: send the signals, arm the grace clock.
fn execute(actions: &[Action], _started_ns: u64, grace_deadline_ns: &mut Option<u64>) {
    for action in actions {
        match action {
            Action::Signal { pid, signal } => {
                let _ = tairix_rt::signal(*pid, *signal);
            }
            Action::ArmGrace => {
                *grace_deadline_ns = Some(tairix_rt::clock_get().saturating_add(GRACE_NS));
            }
        }
    }
}

/// The relative nanosecond budget until the nearest armed deadline, or
/// "no timeout" when none is armed.
fn next_timeout(now: u64, deadline_ns: Option<u64>, grace_ns: Option<u64>) -> u64 {
    let nearest = match (deadline_ns, grace_ns) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match nearest {
        // A deadline already due still enters the wait with a minimal
        // budget so the loop re-checks promptly without spinning.
        Some(deadline) => deadline.saturating_sub(now).max(1_000_000),
        None => u64::MAX,
    }
}

/// Spawn one worker through the `@self` token, returning its PID.
fn spawn_worker(worker: &WorkerSpec) -> Result<i32, &'static str> {
    let argv = worker.encode_argv();
    let words: Vec<&[u8]> = argv.iter().map(String::as_bytes).collect();
    let ret = tairix_rt::spawn_with(SPAWN_SELF, CONSOLE_INHERIT, SPAWN_UID_INHERIT, &words, &[]);
    if ret < 0 {
        return Err("worker spawn refused");
    }
    #[allow(clippy::cast_possible_truncation)] // The kernel bounds PIDs to i32 range.
    Ok(ret as i32)
}

/// Resolve and create the scratch directory: `--temp-path` verbatim, or
/// the app-scoped per-user cache directory `$HOME/Library/stress`.
fn prepare_scratch(spec: &RunSpec) -> Result<String, &'static str> {
    if let Some(dir) = &spec.temp_path {
        ensure_dir(dir)?;
        return Ok(dir.clone());
    }
    let home = tairix_rt::env_var(b"HOME")
        .and_then(|raw| core::str::from_utf8(raw).ok())
        .filter(|home| !home.is_empty())
        .ok_or("no scratch directory: HOME is unset and no --temp-path was given")?;
    let home = home.trim_end_matches('/');
    // The per-user cache home first, then the app-scoped directory in it.
    ensure_dir(&format!("{home}/Library"))?;
    let dir = format!("{home}/Library/stress");
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` if it does not exist; an existing directory is fine.
fn ensure_dir(dir: &str) -> Result<(), &'static str> {
    let ret = tairix_rt::fs_mkdir(dir.as_bytes());
    #[allow(clippy::cast_possible_truncation)] // The kernel encodes -errno in i32 range.
    let errno = -(ret as i32);
    if ret == 0 || errno == Errno::AlreadyExists.as_i32() {
        Ok(())
    } else {
        Err("the scratch directory could not be created")
    }
}

/// Remove every scratch file the run's workers could have created —
/// best-effort on every exit path (a file a worker never got to create
/// simply is not there).
fn cleanup_scratch(spec: &RunSpec, scratch: &str) {
    if scratch.is_empty() {
        return;
    }
    for path in run_scratch_paths(&spec.workers, scratch) {
        let _ = tairix_rt::fs_unlink(path.as_bytes(), tairix_abi::UnlinkFlags::empty());
    }
}

/// Free space on the volume backing `scratch`, discovered through the
/// unprivileged `MOUNT_LIST` query — the longest mount target that is a
/// path prefix of the scratch directory wins (the covering volume). An
/// empty scratch, a refused walk, or an uncovered path answers `None`
/// and the sizing falls back to its documented conservative figures.
fn scratch_free_bytes(scratch: &str) -> Option<u64> {
    if scratch.is_empty() {
        return None;
    }
    let transport = IpcTransport;
    let mut best: Option<(usize, u64)> = None;
    let walk = for_each_mount(&transport, |record| {
        if let Ok(target) = core::str::from_utf8(record.target_bytes()) {
            let better = best.map_or(true, |(len, _)| target.len() >= len);
            if better && path_covers(target, scratch) {
                let usage = record.usage();
                let free = u64::from(usage.block_size).saturating_mul(usage.avail_blocks);
                best = Some((target.len(), free));
            }
        }
        Ok(())
    });
    if walk.is_err() {
        return None;
    }
    best.map(|(_, free)| free)
}

/// Whether the mount target `mount` covers `path` as a directory prefix
/// (component-aligned: `/Users` covers `/Users/x` but never `/Usersx`).
fn path_covers(mount: &str, path: &str) -> bool {
    let mount = mount.trim_end_matches('/');
    if mount.is_empty() {
        return true;
    }
    path.strip_prefix(mount)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `--background`: re-spawn this same binary detached (same options,
/// `--background` dropped, `--quiet` implied), print the controller PID,
/// and return the prompt. The re-spawned controller is deliberately
/// orphaned — the kernel prunes the dead parent's bookkeeping and the run
/// carries on unsupervised.
fn background_respawn() -> i32 {
    let Some(arguments) = tairix_rt::args() else {
        write_stderr_line(USAGE);
        return 2;
    };
    let mut argv: Vec<&[u8]> = Vec::with_capacity(arguments.len() + 2);
    argv.push(b"stress");
    argv.push(b"--quiet");
    for word in &arguments {
        if *word != "--background" && *word != "--quiet" && *word != "-q" {
            argv.push(word.as_bytes());
        }
    }
    debug_assert!(!argv.contains(&(WORKER_FLAG.as_bytes())));
    // The detached controller still resolves its default scratch home
    // from `HOME`, so that one variable is threaded through; nothing
    // else of the caller's environment is authority a load run needs.
    let home = tairix_rt::env_var(b"HOME").map(|value| {
        let mut entry = alloc::vec::Vec::with_capacity(5 + value.len());
        entry.extend_from_slice(b"HOME=");
        entry.extend_from_slice(value);
        entry
    });
    let mut env: Vec<&[u8]> = Vec::new();
    if let Some(entry) = &home {
        env.push(entry.as_slice());
    }
    let ret = tairix_rt::spawn_with(SPAWN_SELF, CONSOLE_INHERIT, SPAWN_UID_INHERIT, &argv, &env);
    if ret < 0 {
        write_stderr_line("stress: the background controller could not be started");
        return 1;
    }
    let _ = Stdout.write_all(format!("{ret}\n").as_bytes());
    0
}

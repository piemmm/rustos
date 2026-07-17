//! The `Run` entry-point binary of the `Shell` application bundle — the program PID 1 `init` launches as the user's
//! session through the `spawn` syscall (`plans/SPAWN.md` `SP3b`,
//! `plans/PI.md` P6e).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so
//! it links the Rust userland runtime `tairix-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust.
//! `tairix-rt` provides `_start`, the per-process stack canary, the panic handler, the `mem_map`-backed global allocator, and the
//! syscall wrappers; `tairix_rt::entry!` names this program's `main`.
//!
//! `main` runs the [`tairix_elsh`] interpreter as a read-eval-print loop over
//! its **inherited standard streams**: it reads command
//! lines from standard input (fd 0), writes the prompt and command output to
//! standard output and standard error (fd 1 / fd 2), and emits advisory
//! metadata on the standard information stream (fd 3). It binds to those
//! descriptors only — never a console, UART, or framebuffer — because binding
//! to a device would be ambient authority and hidden coupling; the same binary therefore works whatever the spawner
//! backed the streams with.
//!
//! The interpreter is pure: it decides *what* to run but reaches the outside
//! world only through two injected seams. `RtConsole` carries its output to
//! fd 1 / fd 2, and `RtProcessHost` launches external commands through the
//! `spawn` syscall and reaps them through `wait`. A command word is resolved
//! to a runnable bundle through the shared candidate policy
//! ([`tairix_cmdres::resolution_candidates`]): the system app store first,
//! then the user's `PATH`, attempted in order. The command's words travel
//! to the child as its argument vector and the shell's exported variables
//! (with any `NAME=v cmd` prefix overrides) as its environment, through
//! the `spawn` startup-strings block. Pipes and redirections run end to
//! end (`plans/SPAWN.md` SP10): the pure `tairix_elsh::wireplan` planner
//! lowers each pipeline into pre-opened targets, per-member spawn attach
//! blocks, and the here-string / multios byte pumps this host executes
//! over `fs_open`/`resource_open`/`pipe_create`/`spawn_attached`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
extern crate alloc;

#[cfg(freestanding)]
mod program {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::elevate::{
        elevate_endpoint, ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN,
    };
    use tairix_abi::fs::{DirEntry, FS_IO_MAX};
    use tairix_abi::{Errno, FileKind, InputMode, LimitKind, ResourceLimit};
    use tairix_elsh::{
        parse_invocation, Console, DirEntryInfo, DirLister, Elevator, Environment, Invocation,
        LaunchSpec, LimitStore, Pid, PlannedOpen, PlannedWire, ProcessHost, PumpTask, ReplInput,
        ResolvedCommand, Shell, Signal, WaitOutcome, USAGE,
    };
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    /// The shell's output sink, backed by the inherited standard output (fd 1)
    /// and standard error (fd 2) through the shared `tairix_rt::io` layer — the
    /// one `Write::write_all` short-write loop, never a shell-private copy
    /// (the charter forbids that duplication).
    struct RtConsole;

    impl Console for RtConsole {
        fn write_stdout(&self, text: &str) {
            // Output is best-effort: `write_all` loops over short writes and
            // fails closed if the backing stops accepting bytes, and a dropped
            // tail must not abort the session, so the result is discarded.
            let _ = Stdout.write_all(text.as_bytes());
        }

        fn write_stderr(&self, text: &str) {
            let _ = Stderr.write_all(text.as_bytes());
        }
    }

    /// The shell's standard-input (fd 0) and standard-information (fd 3) seam,
    /// backed by `tairix_rt`.
    struct RtInput;

    impl ReplInput for RtInput {
        fn read(&mut self, buf: &mut [u8]) -> usize {
            tairix_rt::stdin(buf)
        }

        fn write_info(&mut self, bytes: &[u8]) {
            // fd 3 is best-effort and ignorable: discard the accepted count.
            let _ = tairix_rt::stdinfo(bytes);
        }

        fn set_mode(&mut self, mode: InputMode) -> Result<(), Errno> {
            let ret = tairix_rt::set_input_mode(mode);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(())
        }

        fn terminal_width(&self) -> Option<u16> {
            // The prompt renders on standard output; ask its backing.
            tairix_rt::terminal_size(tairix_abi::STDOUT)
                .ok()
                .map(tairix_abi::TerminalSize::cols)
        }
    }

    /// Initial byte size of the completion directory-listing buffer; grown on
    /// `BufferTooSmall` up to the kernel's per-call staging cap.
    const DIR_BUF_INITIAL: usize = 4096;

    /// The completion engine's read-only directory seam, backed by the
    /// kernel-authorised `fs_readdir`: every path resolution and per-inode
    /// permission check stays kernel-side, and a refusal simply yields no
    /// candidates (the engine degrades, never guesses).
    struct RtDirLister;

    impl DirLister for RtDirLister {
        fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>, Errno> {
            let handle = tairix_rt::open_dir(dir.as_bytes()).map_err(Errno::from_syscall)?;
            let mut buf = alloc::vec![0u8; DIR_BUF_INITIAL];
            let used = loop {
                match handle.read(&mut buf) {
                    Ok(used) => break used,
                    Err(ret) => match Errno::from_syscall(ret) {
                        Errno::BufferTooSmall if buf.len() < FS_IO_MAX => {
                            buf.resize((buf.len() * 2).min(FS_IO_MAX), 0);
                        }
                        other => return Err(other),
                    },
                }
            };
            let mut entries = Vec::new();
            let mut rest = &buf[..used];
            while !rest.is_empty() {
                let (entry, consumed) = DirEntry::decode(rest)?;
                rest = &rest[consumed..];
                // The ABI contract makes every entry name UTF-8; a stream
                // that is not is refused whole rather than partially listed.
                let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
                entries.push(DirEntryInfo {
                    name: String::from(name),
                    is_dir: entry.kind == FileKind::Directory,
                });
            }
            Ok(entries)
        }
    }

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`, the standard `abi-v1` convention). An unrecognised code
    /// fails closed as [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// The POSIX job-control stop signal number (SIGTSTP) the shell reports
    /// a stopped job under, so `$?` shows the familiar `128 + 20 = 148` a
    /// user scripts against — the shell-side vocabulary, deliberately the
    /// POSIX number rather than the `abi-v1` wire discriminant.
    const STOP_SIGNAL_NUMBER: i32 = 20;

    /// Byte size of the pump copy buffer — one kernel staging cap per read,
    /// so a fan-out or concatenation moves data in as few syscalls as the
    /// ABI permits.
    const PUMP_BUF: usize = FS_IO_MAX;

    /// Produce every planned handle, in plan order, returning the real
    /// descriptor numbers indexed by
    /// [`tairix_elsh::OpenId`]. All-or-nothing: any
    /// refusal closes everything already opened and surfaces the `Errno`
    /// verbatim, so a failed launch leaks no descriptor and a multios is
    /// never partially applied.
    fn open_planned(opens: &[PlannedOpen]) -> Result<Vec<u32>, Errno> {
        let mut fds: Vec<u32> = Vec::with_capacity(opens.len());
        // The write end `pipe_create` minted alongside the read end the
        // planner just placed; consumed by the paired `PipeWrite` entry.
        let mut pending_write: Option<u32> = None;
        let fail = |fds: &[u32], pending: Option<u32>, err: Errno| {
            close_fds(fds.iter().copied().chain(pending));
            Err(err)
        };
        for open in opens {
            match open {
                PlannedOpen::Path { path, flags } => {
                    let ret = tairix_rt::fs_open(path.as_bytes(), *flags);
                    if ret < 0 {
                        return fail(&fds, pending_write, errno_from(ret));
                    }
                    // A descriptor register is a small non-negative number.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    fds.push(ret as u32);
                }
                PlannedOpen::Resource { reference, flags } => {
                    let ret = tairix_rt::resource_open(reference.as_bytes(), *flags);
                    if ret < 0 {
                        return fail(&fds, pending_write, errno_from(ret));
                    }
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    fds.push(ret as u32);
                }
                PlannedOpen::PipeRead => {
                    let (read, write) = match tairix_rt::pipe_create() {
                        Ok(ends) => ends,
                        Err(ret) => return fail(&fds, pending_write, errno_from(ret)),
                    };
                    fds.push(read);
                    pending_write = Some(write);
                }
                PlannedOpen::PipeWrite { .. } => {
                    // The planner always pairs a write entry with the read
                    // entry just before it; a plan violating that shape is
                    // refused rather than guessed at.
                    let Some(write) = pending_write.take() else {
                        return fail(&fds, None, Errno::OutOfRange);
                    };
                    fds.push(write);
                }
            }
        }
        match pending_write {
            // Every minted write end must have been claimed.
            Some(write) => fail(&fds, Some(write), Errno::OutOfRange),
            None => Ok(fds),
        }
    }

    /// Close every descriptor `fds` yields (best-effort: a close of an
    /// already-released descriptor fails closed kernel-side and is ignored).
    fn close_fds(fds: impl Iterator<Item = u32>) {
        for fd in fds {
            let _ = tairix_rt::fs_close(fd);
        }
    }

    /// Forcibly terminate and reap every PID in `pids` — the launch
    /// error-path unwind, so a half-spawned pipeline never leaves a running
    /// orphan or a zombie behind. Best-effort: a child that already exited
    /// is simply reaped, and a refusal leaves nothing more to do.
    fn kill_and_reap(pids: &[i32]) {
        for &pid in pids {
            let _ = tairix_rt::signal(pid, tairix_abi::Signal::Kill);
            let mut status = tairix_abi::WaitStatus::Exited(0);
            let _ = tairix_rt::wait(pid, &mut status, tairix_abi::WaitFlags::empty());
        }
    }

    /// Spawn one pipeline member with its descriptor wires, resolving the
    /// command word through the shared candidate policy (the system app
    /// store first, then `PATH`) and attempting each candidate in order.
    /// `spawn`'s `NotFound` is a definitive "no program is registered at
    /// this path, nothing ran", so moving to the next candidate is a
    /// deterministic first-match search, never a retry; any other refusal
    /// (a permission or capability denial, a malformed image, a rejected
    /// wire) is final and reported verbatim. The kernel authorises every
    /// attempt — a candidate spelling grants nothing.
    fn spawn_member(
        spec: &LaunchSpec<'_>,
        command: &ResolvedCommand,
        wires: &[PlannedWire; tairix_abi::STD_STREAM_COUNT],
        fds: &[u32],
    ) -> Result<i32, Errno> {
        let Some(word) = command.argv.first() else {
            return Err(Errno::NotImplemented);
        };
        // The child's environment: the shell's exported variables with
        // this command's `NAME=v cmd` prefix assignments layered on top
        // (an override replaces the export of the same name), each
        // encoded in the conventional `NAME=value` spelling the child's
        // runtime splits at the first `=`.
        let mut env: Vec<(&str, &str)> = spec.env.to_vec();
        for (name, value) in &command.env_overrides {
            match env.iter_mut().find(|(seen, _)| *seen == name.as_str()) {
                Some(entry) => entry.1 = value.as_str(),
                None => env.push((name.as_str(), value.as_str())),
            }
        }
        let env_entries: Vec<String> = env
            .iter()
            .map(|(name, value)| alloc::format!("{name}={value}"))
            .collect();
        let env_bytes: Vec<&[u8]> = env_entries.iter().map(String::as_bytes).collect();
        // The child's argument vector is the command's words verbatim
        // (`argv[0]` is the typed word) — data for the child's own
        // parser, never authority.
        let arg_bytes: Vec<&[u8]> = command.argv.iter().map(String::as_bytes).collect();
        // The attach block: the caller's own credential and console, with
        // the planned wires resolved onto the descriptors just opened.
        let mut attach = tairix_abi::SpawnAttach::INHERIT;
        for (slot, wire) in wires.iter().enumerate() {
            attach.wires[slot] = match wire {
                PlannedWire::Inherit => tairix_abi::FdWire::Inherit,
                PlannedWire::InheritSlot(source) => tairix_abi::FdWire::InheritSlot(*source),
                PlannedWire::Closed => tairix_abi::FdWire::Closed,
                PlannedWire::Handle(id) => tairix_abi::FdWire::Handle(fds[id.0]),
            };
        }
        let path_var = spec
            .env
            .iter()
            .find(|(name, _)| *name == "PATH")
            .map(|(_, value)| *value);
        for candidate in tairix_cmdres::resolution_candidates(word, path_var) {
            let ret =
                tairix_rt::spawn_attached(candidate.as_bytes(), &attach, &arg_bytes, &env_bytes);
            if ret >= 0 {
                // PIDs fit an `i32` on this ABI and `ret >= 0` here, so the
                // cast preserves the PID value.
                #[allow(clippy::cast_possible_truncation)]
                return Ok(ret as i32);
            }
            let err = errno_from(ret);
            if err != Errno::NotFound {
                return Err(err);
            }
        }
        Err(Errno::NotFound)
    }

    /// Write all of `bytes` into the pipe write end `fd`, in staging-cap
    /// chunks. Returns `false` when the write cannot continue: every reader
    /// closed (`BrokenPipe` — the POSIX `yes | head` shape, silently ending
    /// the feed) or a genuine error, which is reported on standard error
    /// (fail loud) before giving up.
    fn pump_write(fd: u32, bytes: &[u8]) -> bool {
        let mut written = 0;
        while written < bytes.len() {
            let end = (written + PUMP_BUF).min(bytes.len());
            match tairix_rt::fs_write(fd, 0, &bytes[written..end]) {
                Ok(0) => {
                    // A zero-byte acceptance cannot make progress; treat it
                    // as the stream refusing further bytes.
                    report_pump_error("write stalled", Errno::NotImplemented);
                    return false;
                }
                Ok(n) => written += n,
                Err(ret) => {
                    let err = errno_from(ret);
                    if err != Errno::BrokenPipe {
                        report_pump_error("write failed", err);
                    }
                    return false;
                }
            }
        }
        true
    }

    /// Run one pump task on the shell's retained ends (`plans/SPAWN.md`
    /// `SP10b`). Pump failures do not abort the already-running job: the
    /// affected stream simply ends early (the child sees end-of-stream or
    /// the sink stops receiving) and the reason is reported on the shell's
    /// standard error — fail loud, degrade gracefully.
    fn run_pump(pump: &PumpTask, fds: &[u32]) {
        match pump {
            PumpTask::WriteContent { into, content } => {
                let _ = pump_write(fds[into.0], content.as_bytes());
            }
            PumpTask::FanOut { from, sinks } => {
                let mut buf = alloc::vec![0u8; PUMP_BUF];
                // Per-sink write offsets (append-mode sinks ignore them);
                // a failed sink is dropped from the fan-out with its error
                // reported once, the rest keep receiving.
                let mut offsets: Vec<Option<u64>> = alloc::vec![Some(0); sinks.len()];
                loop {
                    let n = match tairix_rt::fs_read(fds[from.0], 0, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(ret) => {
                            report_pump_error("read failed", errno_from(ret));
                            break;
                        }
                    };
                    for (sink, offset) in sinks.iter().zip(offsets.iter_mut()) {
                        let Some(at) = offset else { continue };
                        match write_all_at(fds[sink.0], *at, &buf[..n]) {
                            Ok(()) => *at += n as u64,
                            Err(err) => {
                                report_pump_error("write failed", err);
                                *offset = None;
                            }
                        }
                    }
                }
            }
            PumpTask::Concat { into, sources } => {
                let mut buf = alloc::vec![0u8; PUMP_BUF];
                'sources: for source in sources {
                    let mut offset: u64 = 0;
                    loop {
                        let n = match tairix_rt::fs_read(fds[source.0], offset, &mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(ret) => {
                                report_pump_error("read failed", errno_from(ret));
                                break;
                            }
                        };
                        offset += n as u64;
                        if !pump_write(fds[into.0], &buf[..n]) {
                            // No reader remains; the rest of every source
                            // is undeliverable.
                            break 'sources;
                        }
                    }
                }
            }
        }
    }

    /// Write all of `bytes` at `offset` on the (file or resource) sink
    /// `fd`, looping over short writes.
    ///
    /// # Errors
    ///
    /// The sink's [`Errno`] verbatim; a zero-byte acceptance is surfaced as
    /// [`Errno::NotImplemented`] rather than spinning.
    fn write_all_at(fd: u32, offset: u64, bytes: &[u8]) -> Result<(), Errno> {
        let mut written = 0;
        while written < bytes.len() {
            match tairix_rt::fs_write(fd, offset + written as u64, &bytes[written..]) {
                Ok(0) => return Err(Errno::NotImplemented),
                Ok(n) => written += n,
                Err(ret) => return Err(errno_from(ret)),
            }
        }
        Ok(())
    }

    /// Report a pump failure on the shell's standard error — the observing
    /// component states the reason (fail loud); the job itself keeps its
    /// own exit status.
    fn report_pump_error(action: &str, err: Errno) {
        write_stderr_line(&alloc::format!("shell: redirection: {action}: {err}"));
    }

    /// Launches and reaps external commands through the `spawn` and `wait`
    /// syscalls (`plans/SPAWN.md` SP3 / SP6 / SP10), resolving each command
    /// word to a bundle `Run` path through the shared candidate policy
    /// (`plans/APPS.md` §8: the system app store first, then `PATH`).
    ///
    /// Redirections and pipelines are lowered by the pure
    /// [`tairix_elsh::wireplan`] planner and executed here: every target is
    /// pre-opened in the shell's own descriptor table, each pipeline member
    /// spawns with its attach block, the transferred ends are closed, and
    /// the here-string / multios byte pumps run on the shell's retained
    /// pipe ends before the job is awaited.
    struct RtProcessHost {
        /// Non-leader member PIDs of each launched pipeline, keyed by the
        /// leader's PID; reaped after the leader's terminal wait so no
        /// member is left a zombie. The shell is single-threaded, so a
        /// `RefCell` suffices.
        members: RefCell<Vec<(u64, Vec<i32>)>>,
    }

    impl RtProcessHost {
        fn new() -> Self {
            Self {
                members: RefCell::new(Vec::new()),
            }
        }
    }

    impl ProcessHost for RtProcessHost {
        fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Pid, Errno> {
            // Lower the pipeline + redirections into a wiring plan first
            // (pure, fail-closed: an inexpressible redirection refuses the
            // launch before anything is opened), then execute it: open
            // every target, spawn every member with its attach block,
            // close the transferred ends, and run the byte pumps.
            let plan = tairix_elsh::lower_wire_plan(spec)?;
            let fds = open_planned(&plan.opens)?;
            let mut pids: Vec<i32> = Vec::with_capacity(plan.members.len());
            for member in &plan.members {
                let command = &spec.commands[member.command];
                match spawn_member(spec, command, &member.wires, &fds) {
                    Ok(pid) => pids.push(pid),
                    Err(err) => {
                        // Unwind whole: kill and reap what already runs,
                        // then release every descriptor the plan opened.
                        kill_and_reap(&pids);
                        close_fds(fds.iter().copied());
                        return Err(err);
                    }
                }
            }
            // The children hold their cloned ends now; release the shell's
            // copies of every transferred handle so pipe end-of-stream and
            // broken-pipe semantics see only the children as holders.
            close_fds(plan.transferred().iter().map(|id| fds[id.0]));
            // Feed and drain the retained pump ends. Pumps run to
            // completion before the wait: content is bounded (here-string)
            // or ends at the child's exit (multios fan-out), and a
            // vanished reader/writer surfaces as `BrokenPipe`/EOF rather
            // than a hang.
            for pump in &plan.pumps {
                run_pump(pump, &fds);
            }
            close_fds(plan.retained().iter().map(|id| fds[id.0]));
            // The last member is the job leader: its status becomes `$?`,
            // and the others are reaped after it (`wait`).
            let leader = *pids.last().ok_or(Errno::NotImplemented)?;
            // Spawn returned the PID as a non-negative register, so the
            // widening cast preserves the value.
            #[allow(clippy::cast_sign_loss)]
            let leader_pid = Pid::new(leader as u64);
            let others: Vec<i32> = pids[..pids.len() - 1].to_vec();
            if !others.is_empty() {
                self.members
                    .borrow_mut()
                    .push((leader_pid.as_u64(), others));
            }
            Ok(leader_pid)
        }

        fn wait(&self, pid: Pid) -> Result<WaitOutcome, Errno> {
            // PIDs fit an `i32` on this ABI; `wait` takes a signed PID.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let pid_i32 = pid.as_u64() as i32;
            // Mark the child as this console's foreground job before
            // blocking (the `tcsetpgrp` analogue): the kernel's cooked-mode
            // line discipline then delivers `^C`/`^Z` to the child while
            // the shell is parked in `wait`. A refusal (piped stdin, no
            // console backing, an unwired kernel) is not fatal — the wait
            // proceeds without interactive signal routing, exactly as a
            // non-interactive session should.
            let marked = tairix_rt::console_foreground(tairix_abi::STDIN, pid_i32) >= 0;
            let mut status = tairix_abi::WaitStatus::Exited(0);
            // `STOPPED` opts into stop reports, so a `^Z`-stopped foreground
            // job returns control to the shell instead of blocking forever.
            let ret = tairix_rt::wait(pid_i32, &mut status, tairix_abi::WaitFlags::STOPPED);
            if marked {
                // Reclaim the terminal: back at the prompt (or handling a
                // stop), bytes flow to the shell again.
                let _ = tairix_rt::console_foreground(tairix_abi::STDIN, 0);
            }
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(match status {
                tairix_abi::WaitStatus::Exited(code) => {
                    // The leader is gone for good: reap this pipeline's
                    // remaining members so none is left a zombie. Their
                    // pipe ends close as they exit, so each blocking wait
                    // terminates (`yes | head`: the broken pipe ends the
                    // producer). A stopped leader keeps its entry — `fg`
                    // resumes it and the reap happens on its final wait.
                    let entry = {
                        let mut members = self.members.borrow_mut();
                        members
                            .iter()
                            .position(|(leader, _)| *leader == pid.as_u64())
                            .map(|at| members.swap_remove(at))
                    };
                    if let Some((_, others)) = entry {
                        for member in others {
                            let mut reaped = tairix_abi::WaitStatus::Exited(0);
                            let _ = tairix_rt::wait(
                                member,
                                &mut reaped,
                                tairix_abi::WaitFlags::empty(),
                            );
                        }
                    }
                    WaitOutcome::Exited(code)
                }
                // The shell's job vocabulary speaks the POSIX numbers a
                // user scripts against: a stop reports as SIGTSTP (20), so
                // `$?` becomes the familiar 148.
                tairix_abi::WaitStatus::Stopped(_) => WaitOutcome::Stopped(STOP_SIGNAL_NUMBER),
            })
        }

        fn signal(&self, pid: Pid, signal: Signal) -> Result<(), Errno> {
            // Map the shell's own job-control signal vocabulary onto the
            // `abi-v1` signal set (one definition, no shell-private numbering)
            // and deliver it through the `signal` syscall. The kernel
            // validates that `pid` is a child the shell spawned and fails
            // closed; until its signal producer is installed the call surfaces
            // `NotImplemented` honestly rather than pretending it landed.
            let abi_signal = match signal {
                Signal::Continue => tairix_abi::Signal::Continue,
                Signal::Terminate => tairix_abi::Signal::Terminate,
                Signal::Kill => tairix_abi::Signal::Kill,
            };
            // PIDs fit an `i32` on this ABI; `signal` takes a signed PID.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let ret = tairix_rt::signal(pid.as_u64() as i32, abi_signal);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(())
        }

        fn poll(&self) -> Option<(Pid, WaitOutcome)> {
            // No asynchronous background-state notification exists yet; the
            // shell reaps foreground jobs through `wait`.
            None
        }

        fn change_directory(&self, path: &str) -> Result<String, Errno> {
            // The kernel — not the shell — resolves the path (relative to the
            // process's current working directory), re-authorises it as a
            // searchable directory, and only then moves the process. A refusal
            // surfaces as its `Errno`; the shell holds no ambient filesystem
            // authority of its own.
            let ret = tairix_rt::fs_chdir(path.as_bytes());
            if ret < 0 {
                return Err(errno_from(ret));
            }
            // Report the resolved absolute directory the kernel settled on
            // (for the prompt and `cd`'s echo). A normalised absolute path
            // never exceeds `FS_PATH_MAX`, so this buffer always holds it.
            let mut buf = alloc::vec![0u8; tairix_abi::FS_PATH_MAX];
            let n = tairix_rt::fs_getcwd(&mut buf).map_err(errno_from)?;
            core::str::from_utf8(&buf[..n])
                .map(String::from)
                .map_err(|_| Errno::OutOfRange)
        }
    }

    /// Reads and imposes resource limits through the `rlimit_get` /
    /// `rlimit_set` syscalls, backing the `ulimit`
    /// builtin.
    struct RtLimitStore;

    impl LimitStore for RtLimitStore {
        fn get(&self, kind: LimitKind) -> Result<ResourceLimit, Errno> {
            tairix_rt::rlimit_get(kind).map_err(errno_from)
        }

        fn set(&self, kind: LimitKind, value: ResourceLimit) -> Result<(), Errno> {
            let ret = tairix_rt::rlimit_set(kind, value);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(())
        }
    }

    /// The `elevate` builtin's production seam (`plans/CAPABILITY_USE.md`
    /// CU5): posts one synchronous request to this console's login
    /// supervisor and blocks until the re-authenticated command has run.
    ///
    /// The shell holds no elevation authority — it derives the rendezvous
    /// from its **own** kernel-attested console (`self_origin`, never a
    /// claim), and the supervisor re-authenticates the offered credentials
    /// before anything runs. A process with no console-backed streams has no
    /// rendezvous and fails closed before posting.
    struct RtElevator;

    impl RtElevator {
        /// Read one edited input line (without its terminator) from standard
        /// input — the read line discipline's **buffer** half
        /// ([`tairix_vt::line::LineEditor`]) over `tairix_rt::stdin`, exactly
        /// as the REPL reads a command line. A zero-length read means the
        /// stream closed and fails closed; a line longer than `buf` is
        /// refused, never truncated.
        fn read_line_raw(buf: &mut [u8]) -> Result<usize, Errno> {
            let mut editor = tairix_vt::line::LineEditor::new();
            let mut len = 0;
            let mut byte = [0u8; 1];
            loop {
                if tairix_rt::stdin(&mut byte) == 0 {
                    return Err(Errno::NotFound);
                }
                match editor.push(buf, &mut len, byte[0]) {
                    tairix_vt::line::LineFeed::Pending => {}
                    tairix_vt::line::LineFeed::Complete => return Ok(len),
                    tairix_vt::line::LineFeed::TooLong => return Err(Errno::LengthOutOfRange),
                }
            }
        }
    }

    impl Elevator for RtElevator {
        fn read_secret(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // A credential must never render: select the secret discipline
            // for the read and fail closed if it cannot be selected.
            let toggled = tairix_rt::set_input_mode(InputMode::Secret);
            if toggled < 0 {
                return Err(errno_from(toggled));
            }
            let result = Self::read_line_raw(buf);
            // Restoring the cooked default is best-effort — it cannot
            // compromise the secret already read. The un-echoed Return key
            // advanced no line, so advance one ourselves with a plain line
            // feed, which the console line discipline cooks to CR-LF.
            let _ = tairix_rt::set_input_mode(InputMode::Cooked);
            let _ = Stdout.write_all(b"\n");
            result
        }

        fn elevate(&self, username: &str, password: &str, program: &str) -> Result<i32, Errno> {
            let console = tairix_rt::self_origin().map_err(errno_from)?.console();
            // `elevate_endpoint` refuses the "no console" sentinel, so a
            // stream-fed shell (a pipe, a network session) cannot name a
            // rendezvous it is not sitting on.
            let endpoint = elevate_endpoint(console)?;
            let request = ElevateRequest {
                username,
                password,
                program,
            };
            let mut request_buf = [0u8; ELEVATE_MAX_REQUEST];
            let encoded = match request.encode(&mut request_buf) {
                Ok(len) => len,
                Err(err) => {
                    request_buf.fill(0);
                    return Err(err);
                }
            };
            let mut reply_buf = [0u8; ELEVATE_REPLY_LEN];
            let posted = tairix_rt::ipc_call(endpoint, &request_buf[..encoded], &mut reply_buf);
            // The request carries the offered password: zero it as soon as
            // the exchange resolves, before the reply is even decoded.
            request_buf.fill(0);
            let reply_len = posted.map_err(errno_from)?;
            match ElevateReply::decode(&reply_buf[..reply_len])? {
                ElevateReply::Completed { exit_code } => Ok(exit_code),
                ElevateReply::Refused(err) => Err(err),
            }
        }
    }

    /// Render `elsh`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the shell's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("elsh"), locale, "elsh")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Runs the interpreter as a read-eval-print loop over the inherited
    /// standard streams and returns the session's exit code (the `exit`
    /// builtin's code, or `0` when the input stream ends). The reserved
    /// `-h`/`-?` short-help switches render the shell's own Help document
    /// and exit `0`; any other argument is a usage error and exits `2`.
    /// The loop binds only to fd 0/1/2/3, never a device.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse_invocation(&arguments) {
            Ok(Invocation::Repl) => {}
            Ok(Invocation::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        }
        let console = RtConsole;
        let host = RtProcessHost::new();
        let limits = RtLimitStore;
        let elevator = RtElevator;
        let mut input = RtInput;
        // Seed the interactive session from the environment login exported
        // (USER, HOME, SHELL, PATH, TERM, LANG, …), filling the shell-owned
        // defaults (HOSTNAME, PWD/OLDPWD, ELSH_PROMPT) so the prompt shows
        // `user@host cwd% ` and `$USER`/`$HOME`/… are present from the first
        // line.
        let mut env = Environment::new();
        env.seed_interactive(|name| {
            tairix_rt::env_var(name.as_bytes())
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        });
        let mut shell = Shell::with_environment(&host, &console, env)
            .with_limits(&limits)
            .with_elevator(&elevator);
        tairix_elsh::run_repl(&mut shell, &console, &mut input, &RtDirLister)
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}

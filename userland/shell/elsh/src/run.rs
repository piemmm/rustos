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
//! world only through injected seams. `RtConsole` carries its output to
//! fd 1 / fd 2, `RtProcessHost` launches external commands through the
//! `spawn` syscall and reaps them through `wait`, `RtDirLister` reads
//! directories for filename completion, and `RtResourceLister` reads the live
//! names behind a resource-selector placeholder from the System Information
//! API. A command word is resolved
//! to a runnable bundle through the shared candidate policy
//! ([`tairix_cmdres::resolution_candidates`]): the two system stores, the
//! user's own two stores, then their `PATH`, attempted in order. The
//! command's words travel to the child as its argument vector and the
//! shell's exported variables (with any `NAME=v cmd` prefix overrides) as
//! its environment, through the `spawn` startup-strings block. Pipes and
//! redirections run end to end (`plans/SPAWN.md` SP10): the pure
//! `tairix_elsh::wireplan` planner lowers each pipeline into pre-opened
//! targets, per-member spawn attach blocks, and the here-string / multios
//! byte pumps this host executes over
//! `fs_open`/`resource_open`/`pipe_create`/`spawn_attached`. A read of a
//! value-backed reference (`cat < info:mem/physical`) is the one target the
//! kernel cannot open, so this host reads it over the System Information API
//! under its own attested identity and hands the child the filled pipe.
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
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::elevate::{ElevateReply, ElevateRequest};
    use tairix_abi::fs::{DirEntry, FS_IO_MAX};
    use tairix_abi::origin::{CapabilitySummary, Origin};
    use tairix_abi::sysinfo::RECLAIM_CLASS_NAMES;
    use tairix_abi::time::Time64;
    use tairix_abi::{CapabilityId, Errno, FileKind, InputMode, LimitKind, ResourceLimit};
    use tairix_elsh::{
        parse_invocation, Console, DirEntryInfo, DirLister, Elevator, Environment, Invocation,
        LaunchError, LaunchSpec, LimitStore, Pid, PlannedOpen, PlannedWire, ProcessHost, PumpTask,
        ReplInput, ResolvedCommand, ResourceLister, Shell, Signal, WaitOutcome, USAGE,
    };
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_procinfo::{
        cpu_info, for_each_irq, for_each_net_bond_member, for_each_net_interface, CallError,
        IpcTransport, ListError, ResolveInfoError, Transport, WalkStep,
    };
    use tairix_resref::SelectorDomain;
    use tairix_rt::io::{write_stderr_line, Read, StdInfo, Stderr, Stdin, Stdout, Write};

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
            match Stdin.read(buf) {
                Ok(read) => read,
                Err(err) => {
                    // The input channel failed rather than ended. The REPL
                    // has no continuation either way, but a session that
                    // vanishes must say why before it does.
                    let _ = writeln!(Stderr, "elsh: standard input failed: {}", err.as_errno());
                    0
                }
            }
        }

        fn write_info(&mut self, bytes: &[u8]) {
            // fd 3 is best-effort and ignorable: discard the accepted count.
            let _ = StdInfo.write_all(bytes);
        }

        fn set_mode(&mut self, mode: InputMode) -> Result<(), Errno> {
            let ret = tairix_rt::set_input_mode(mode);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
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

    /// The completion engine's resource-name seam, backed by the System
    /// Information API — the one broker those names come from, reached
    /// through the shared `lib/procinfo` client, never a shell-private query.
    ///
    /// # Capability-adaptive by design
    ///
    /// The shell's manifest holds `FS_ACCESS`/`PROC_SPAWN`/`CONSOLE_*` and
    /// **no** `CAP_SYSINFO_*`: those are administrative grants
    /// (`lib/users/src/grants.rs` — not in the session baseline) and the
    /// shell is the most exposed program on the machine, so widening it to
    /// make Tab prettier would be a poor trade. A session that *does* hold
    /// one (an administrator's) enumerates the domains it gates; one that
    /// does not gets no candidates there — which is the right answer, not a
    /// compromise: without `CAP_SYSINFO_HW` the session could not read
    /// `info:net/<iface>/mac` either, so there was nothing behind those names
    /// for it.
    ///
    /// A gated domain we do not hold is skipped **without issuing the
    /// query**, so a Tab press never produces a denied request or an audit
    /// refusal record. The session's own capability set is read once, from
    /// the ungated, self-scoped `PROCESS_IDENTITY` query, and cached for the
    /// process's life: `elevate` spawns a program under another identity and
    /// never re-credentials the shell, so the set cannot change under us.
    /// The *names* are never cached — each Tab press re-reads them, so a
    /// hot-plugged interface appears immediately.
    struct RtResourceLister {
        capabilities: RefCell<CapabilityCache>,
    }

    /// The session's own capability set, read at most once.
    ///
    /// A failed read is remembered as [`Unreadable`](CapabilityCache::Unreadable)
    /// rather than retried, so a broker that is refusing or absent costs one
    /// query for the life of the session instead of one per keystroke — and
    /// an unreadable identity holds nothing, so every gated domain is skipped
    /// (fail closed).
    enum CapabilityCache {
        /// Not read yet.
        Unread,
        /// Read: the caller's kernel-attested capability summary.
        Known(CapabilitySummary),
        /// Read and failed; treated as holding nothing.
        Unreadable,
    }

    impl RtResourceLister {
        const fn new() -> Self {
            Self {
                capabilities: RefCell::new(CapabilityCache::Unread),
            }
        }

        /// Whether this session holds `cap`, from the cached self-scoped
        /// identity.
        fn holds(&self, transport: &dyn Transport, cap: CapabilityId) -> bool {
            let mut cached = self.capabilities.borrow_mut();
            if matches!(*cached, CapabilityCache::Unread) {
                // The `PROCESS_IDENTITY` query is ungated and self-scoped: it
                // answers only for the asking principal, so reading our own
                // capability set costs no authority and discloses nothing.
                *cached = tairix_procinfo::call(
                    transport,
                    tairix_abi::sysinfo::SysinfoQueryId::PROCESS_IDENTITY,
                    &[],
                )
                .ok()
                .and_then(|reply| Origin::from_bytes(&reply).ok())
                .map_or(CapabilityCache::Unreadable, |origin| {
                    CapabilityCache::Known(*origin.capabilities())
                });
            }
            match &*cached {
                CapabilityCache::Known(summary) => summary.holds_cap(cap),
                CapabilityCache::Unread | CapabilityCache::Unreadable => false,
            }
        }
    }

    impl ResourceLister for RtResourceLister {
        fn list(&self, domain: SelectorDomain) -> Result<Vec<String>, Errno> {
            let transport = IpcTransport;
            match domain {
                // Two closed tables another crate already owns: the single
                // source of truth is `lib/abi`, so they are read from there
                // rather than copied into the registry — and they need no
                // capability and no IPC at all.
                SelectorDomain::LimitKind => Ok(LimitKind::ALL
                    .iter()
                    .map(|kind| String::from(kind.name()))
                    .collect()),
                SelectorDomain::ReclaimClass => Ok(RECLAIM_CLASS_NAMES
                    .iter()
                    .map(|name| String::from(*name))
                    .collect()),
                // The processor-info query is ungated (a machine fact that
                // names no principal), so every session enumerates its CPUs.
                // The records' own indices are used rather than a `0..count`
                // range, so a sparse set is reported as it is.
                SelectorDomain::Cpu => Ok(cpu_info(&transport)
                    .map_err(ResolveInfoError::to_errno)?
                    .iter()
                    .map(|record| record.cpu.to_string())
                    .collect()),
                // The interface inventory is hardware topology
                // (`CAP_SYSINFO_HW`); the bond aliases are surface topology
                // (`CAP_SYSINFO_GLOBAL`). Without the grant the domain is
                // skipped silently and no query is sent.
                SelectorDomain::Interface => {
                    if !self.holds(&transport, CapabilityId::SYSINFO_HW) {
                        return Ok(Vec::new());
                    }
                    let mut names = Vec::new();
                    for_each_net_interface(&transport, |record| {
                        names.push(if_name_string(&record.name));
                        Ok(WalkStep::Continue)
                    })
                    .map_err(walk_errno)?;
                    Ok(names)
                }
                SelectorDomain::Bond => {
                    if !self.holds(&transport, CapabilityId::SYSINFO_GLOBAL) {
                        return Ok(Vec::new());
                    }
                    // One record per member, each naming its bond, so the
                    // bond names are the distinct owners.
                    let mut names: Vec<String> = Vec::new();
                    for_each_net_bond_member(&transport, |record| {
                        let name = if_name_string(&record.bond);
                        if !name.is_empty() && !names.contains(&name) {
                            names.push(name);
                        }
                        Ok(WalkStep::Continue)
                    })
                    .map_err(walk_errno)?;
                    Ok(names)
                }
                SelectorDomain::IrqLine => {
                    if !self.holds(&transport, CapabilityId::SYSINFO_HW) {
                        return Ok(Vec::new());
                    }
                    let mut lines = Vec::new();
                    for_each_irq(&transport, |record| {
                        lines.push(record.line.to_string());
                        Ok(WalkStep::Continue)
                    })
                    .map_err(walk_errno)?;
                    Ok(lines)
                }
            }
        }
    }

    /// Map a paged-walk failure onto the seam's [`Errno`], keeping the
    /// service's own reason rather than flattening every cause into one code.
    ///
    /// The completion engine treats any error as "no candidates", so nothing
    /// depends on which code this is today — which is exactly why it should be
    /// the true one: a refusal reported as "not supported" would mislead the
    /// next reader of this seam.
    fn walk_errno(err: ListError) -> Errno {
        match err {
            ListError::Call(CallError::PermissionDenied) => Errno::PermissionDenied,
            ListError::Call(CallError::Service(errno)) | ListError::Sink(errno) => errno,
        }
    }

    /// A NUL-padded interface-name field as text, lossily decoded: a name the
    /// stack reports with a non-UTF-8 byte becomes a replacement character
    /// rather than dropping the interface — and the completion engine drops
    /// any name it could not spell back as a selector segment anyway.
    fn if_name_string(name: &[u8; tairix_abi::net_ipc::IF_NAME_LEN]) -> String {
        let len = name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(name.len());
        String::from_utf8_lossy(&name[..len]).into_owned()
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
                        return fail(&fds, pending_write, Errno::from_syscall(ret));
                    }
                    // A descriptor register is a small non-negative number.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    fds.push(ret as u32);
                }
                PlannedOpen::Resource { reference, flags } => {
                    let ret = tairix_rt::resource_open(reference.as_bytes(), *flags);
                    if ret < 0 {
                        return fail(&fds, pending_write, Errno::from_syscall(ret));
                    }
                    // A descriptor register is a small non-negative number.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    fds.push(ret as u32);
                }
                PlannedOpen::ValuePipe { reference } => {
                    let (read, write) = match tairix_rt::pipe_create() {
                        Ok(ends) => ends,
                        Err(ret) => return fail(&fds, pending_write, Errno::from_syscall(ret)),
                    };
                    // Registered before the fill so a refusal releases it
                    // through the one all-or-nothing path.
                    fds.push(read);
                    let filled = fill_value_pipe(write, reference);
                    // Closed either way: the whole value is already in the
                    // ring, so the child must see end-of-stream immediately.
                    let _ = tairix_rt::fs_close(write);
                    if let Err(err) = filled {
                        return fail(&fds, pending_write, err);
                    }
                }
                PlannedOpen::PipeRead => {
                    let (read, write) = match tairix_rt::pipe_create() {
                        Ok(ends) => ends,
                        Err(ret) => return fail(&fds, pending_write, Errno::from_syscall(ret)),
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
    fn kill_and_reap(pids: &[i64]) {
        for &pid in pids {
            let _ = tairix_rt::signal(pid, tairix_abi::Signal::Kill);
            let mut status = tairix_abi::WaitStatus::Exited(0);
            let _ = tairix_rt::wait(pid, &mut status, tairix_abi::WaitFlags::empty());
        }
    }

    /// Spawn one pipeline member with its descriptor wires, resolving the
    /// command word through the shared candidate policy (the fixed store
    /// prefix the session's `HOME` spells out, then `PATH`) and attempting
    /// each candidate in order. `spawn`'s `NotFound` is a definitive "no
    /// program is registered at this path, nothing ran", so moving to the
    /// next candidate is a deterministic first-match search, never a retry;
    /// any other refusal (a permission or capability denial, a malformed
    /// image, a rejected wire) is final and reported verbatim. The kernel
    /// authorises every attempt — a candidate spelling grants nothing.
    fn spawn_member(
        spec: &LaunchSpec<'_>,
        command: &ResolvedCommand,
        wires: &[PlannedWire; tairix_abi::STD_STREAM_COUNT],
        fds: &[u32],
    ) -> Result<i64, Errno> {
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
        // The search order reads both `HOME` (the user's own two stores)
        // and `PATH` from the exported environment the session inherited,
        // so the shell runs exactly the bundles its completion offered.
        let exported = |name: &str| {
            spec.env
                .iter()
                .find(|(seen, _)| *seen == name)
                .map(|(_, value)| *value)
        };
        let search_env = tairix_cmdres::CommandEnv {
            home: exported("HOME"),
            path_var: exported("PATH"),
        };
        for candidate in tairix_cmdres::resolution_candidates(word, search_env) {
            let ret =
                tairix_rt::spawn_attached(candidate.as_bytes(), &attach, &arg_bytes, &env_bytes);
            if ret >= 0 {
                return Ok(ret);
            }
            let err = Errno::from_syscall(ret);
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
                    let err = Errno::from_syscall(ret);
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
                            report_pump_error("read failed", Errno::from_syscall(ret));
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
                                report_pump_error("read failed", Errno::from_syscall(ret));
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
                Err(ret) => return Err(Errno::from_syscall(ret)),
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

    /// Read the value-backed `reference` through the System Information API
    /// and leave its rendered bytes in the pipe `write` end.
    ///
    /// Deliberately the *shell's* read, not the child's: reading here is what
    /// lets every stdin-consuming tool take `< info:mem/physical` without
    /// requesting `CAP_SYSINFO_*` for itself. It adds no authority — the same
    /// [`IpcTransport`] every client uses, so `sysinfod` gates the query on
    /// this shell's attested set.
    ///
    /// The whole value is written before any spawn, which cannot block
    /// because [`MAX_VALUE_LEN`](tairix_procinfo::MAX_VALUE_LEN) is far below
    /// one pipe's ring.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the refusal maps to, after reporting the reason on
    /// standard error. Every error path aborts the launch before the child
    /// exists, so a denied read is never a blank one.
    fn fill_value_pipe(write: u32, reference: &str) -> Result<(), Errno> {
        // The planner rendered this from a parsed reference, so it should
        // always re-parse; a malformed one is refused, never guessed at.
        let parsed = tairix_resref::parse(reference).map_err(|_| {
            write_stderr_line(&alloc::format!(
                "shell: redirection: {reference}: not a resource reference"
            ));
            // The kernel resolver's own errno for a malformed reference, so
            // a spelling refusal reads the same wherever it is caught.
            Errno::OutOfRange
        })?;
        let now = tairix_rt::wall_time().map_or(Time64::UNIX_EPOCH, |reading| reading.time());
        // The launch failure reported afterwards carries only the errno, so
        // the shared wording — which names the capability a denial wanted —
        // is stated here against the reference itself.
        let value = tairix_procinfo::read_value(&parsed, now, &IpcTransport).map_err(|err| {
            write_stderr_line(&alloc::format!("shell: redirection: {reference}: {err}"));
            err.to_errno()
        })?;
        write_all_at(write, 0, value.as_bytes()).inspect_err(|&err| {
            report_pump_error("value write failed", err);
        })
    }

    /// Launches and reaps external commands through the `spawn` and `wait`
    /// syscalls (`plans/SPAWN.md` SP3 / SP6 / SP10), resolving each command
    /// word to a bundle `Run` path through the shared candidate policy
    /// (`plans/APPS.md` §8: the fixed store prefix first, then `PATH`).
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
        members: RefCell<Vec<(u64, Vec<i64>)>>,
    }

    impl RtProcessHost {
        fn new() -> Self {
            Self {
                members: RefCell::new(Vec::new()),
            }
        }
    }

    impl ProcessHost for RtProcessHost {
        fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Pid, LaunchError> {
            // Lower the pipeline + redirections into a wiring plan first
            // (pure, fail-closed: an inexpressible redirection refuses the
            // launch before anything is opened), then execute it: open
            // every target, spawn every member with its attach block,
            // close the transferred ends, and run the byte pumps.
            // Both stages are redirection work: lowering refuses a
            // redirection the attach block cannot express, and the open
            // phase refuses a target that will not open.
            let plan = tairix_elsh::lower_wire_plan(spec).map_err(LaunchError::Redirection)?;
            let fds = open_planned(&plan.opens).map_err(LaunchError::Redirection)?;
            let mut pids: Vec<i64> = Vec::with_capacity(plan.members.len());
            for member in &plan.members {
                let command = &spec.commands[member.command];
                match spawn_member(spec, command, &member.wires, &fds) {
                    Ok(pid) => pids.push(pid),
                    Err(err) => {
                        // Unwind whole: kill and reap what already runs,
                        // then release every descriptor the plan opened.
                        kill_and_reap(&pids);
                        close_fds(fds.iter().copied());
                        return Err(LaunchError::Spawn(err));
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
            let leader = *pids
                .last()
                .ok_or(LaunchError::Spawn(Errno::NotImplemented))?;
            // Spawn returned the PID as a non-negative register, so
            // reinterpreting it preserves the value.
            let leader_pid = Pid::new(leader.cast_unsigned());
            let others: Vec<i64> = pids[..pids.len() - 1].to_vec();
            if !others.is_empty() {
                self.members
                    .borrow_mut()
                    .push((leader_pid.as_u64(), others));
            }
            Ok(leader_pid)
        }

        fn wait(&self, pid: Pid) -> Result<WaitOutcome, Errno> {
            // A pid is drawn inside the non-negative signed range, so
            // reinterpreting it for the signed `wait`/`console_foreground`
            // argument preserves the value.
            let signed_pid = pid.as_u64().cast_signed();
            // Mark the child as this console's foreground job before
            // blocking (the `tcsetpgrp` analogue): the kernel's cooked-mode
            // line discipline then delivers `^C`/`^Z` to the child while
            // the shell is parked in `wait`. A refusal (piped stdin, no
            // console backing, an unwired kernel) is not fatal — the wait
            // proceeds without interactive signal routing, exactly as a
            // non-interactive session should.
            let marked = tairix_rt::console_foreground(tairix_abi::STDIN, signed_pid) >= 0;
            let mut status = tairix_abi::WaitStatus::Exited(0);
            // `STOPPED` opts into stop reports, so a `^Z`-stopped foreground
            // job returns control to the shell instead of blocking forever.
            let ret = tairix_rt::wait(signed_pid, &mut status, tairix_abi::WaitFlags::STOPPED);
            if marked {
                // Reclaim the terminal: back at the prompt (or handling a
                // stop), bytes flow to the shell again.
                let _ = tairix_rt::console_foreground(tairix_abi::STDIN, 0);
            }
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
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
            let ret = tairix_rt::signal(pid.as_u64().cast_signed(), abi_signal);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
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
                return Err(Errno::from_syscall(ret));
            }
            // Report the resolved absolute directory the kernel settled on
            // (for the prompt and `cd`'s echo). A normalised absolute path
            // never exceeds `FS_PATH_MAX`, so this buffer always holds it.
            let mut buf = alloc::vec![0u8; tairix_abi::FS_PATH_MAX];
            let n = tairix_rt::fs_getcwd(&mut buf).map_err(Errno::from_syscall)?;
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
            tairix_rt::rlimit_get(kind).map_err(Errno::from_syscall)
        }

        fn set(&self, kind: LimitKind, value: ResourceLimit) -> Result<(), Errno> {
            let ret = tairix_rt::rlimit_set(kind, value);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
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
        /// ([`tairix_vt::line::LineEditor`]) over the shared standard-input
        /// reader, exactly as the REPL reads a command line. A zero-length
        /// read means the stream closed and fails closed, and a refusal
        /// surfaces its own reason; a line longer than `buf` is refused,
        /// never truncated.
        fn read_line_raw(buf: &mut [u8]) -> Result<usize, Errno> {
            let mut editor = tairix_vt::line::LineEditor::new();
            let mut len = 0;
            let mut byte = [0u8; 1];
            loop {
                match Stdin.read(&mut byte) {
                    Ok(0) => return Err(Errno::NotFound),
                    Ok(_) => {}
                    Err(err) => return Err(err.as_errno()),
                }
                match editor.push(buf, &mut len, byte[0]) {
                    // This reader never drives `resolve_escape`, so a bare
                    // `ESC` is only ever held, never surfaced as `Escape`;
                    // either way a pending line reads on.
                    tairix_vt::line::LineFeed::Pending | tairix_vt::line::LineFeed::Escape => {}
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
                return Err(Errno::from_syscall(toggled));
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
            let request = ElevateRequest::Run {
                username,
                password,
                program,
            };
            match tairix_rt::elevate(&request)? {
                ElevateReply::Completed { exit_code } => Ok(exit_code),
                ElevateReply::Refused(err) => Err(err),
                // The builtin only ever posts a `Run` request, so the
                // broker answers neither `Verified` (a `Verify` request's
                // reply) nor `Launched` (a `Launch` request's). Reachable
                // only through a protocol mismatch, so both fail closed
                // rather than being treated as success.
                ElevateReply::Verified | ElevateReply::Launched { .. } => Err(Errno::OutOfRange),
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
        tairix_elsh::run_repl(
            &mut shell,
            &console,
            &mut input,
            &RtDirLister,
            &RtResourceLister::new(),
        )
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

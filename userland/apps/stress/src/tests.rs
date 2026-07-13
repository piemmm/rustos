//! Host tests for the `stress` tool (`plans/STRESSTEST.md` §10):
//! the option grammar, the worker argv codec, the sizing policy, the five
//! load units over an in-memory scratch, the controller's teardown paths,
//! and the report shapes.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::Signal;

use crate::command::{parse, Command, RunSpec, Workers};
use crate::ctrl::{Action, Controller, Event};
use crate::error::StressError;
use crate::load::{
    cache_unit, cpu_unit, hdd_unit, io_unit, run_scratch_paths, scratch_file, vm_unit, Scratch,
    ScratchError, UnitOutcome,
};
use crate::report::{completion_line, dispatch_line, refusal_line, summary_record};
use crate::sizing::{size_targets, Discovered};
use crate::worker::{WorkerKind, WorkerSpec, REFUSED_EXIT};

// ---------------------------------------------------------------- parser

/// The spec of a plain `Run` parse, panicking (test-only) on any other
/// command.
fn run_spec(args: &[&str]) -> RunSpec {
    match parse(args) {
        Ok(Command::Run(spec)) => spec,
        other => panic!("expected a run, got {other:?}"),
    }
}

#[test]
fn parses_the_gnu_worker_counts_and_sizes() {
    let spec = run_spec(&[
        "--cpu",
        "2",
        "--io",
        "1",
        "--vm",
        "3",
        "--vm-bytes",
        "64M",
        "--hdd",
        "1",
        "--hdd-bytes",
        "128k",
    ]);
    assert_eq!(
        spec.workers,
        Workers {
            cpu: 2,
            vm: 3,
            io: 1,
            hdd: 1,
            cache: 0
        }
    );
    assert_eq!(spec.vm_bytes, Some(64 << 20));
    assert_eq!(spec.hdd_bytes, Some(128 << 10));
}

#[test]
fn parses_equals_forms_all_timeout_and_overcommit() {
    let spec = run_spec(&["--all=2", "--timeout=5m", "--overcommit=150%"]);
    assert_eq!(spec.workers.total(), 10);
    assert_eq!(spec.timeout_secs, Some(300));
    assert_eq!(spec.overcommit, Some(150));
}

#[test]
fn timeout_suffixes_follow_gnu_stress() {
    assert_eq!(
        run_spec(&["--cpu", "1", "--timeout", "90"]).timeout_secs,
        Some(90)
    );
    assert_eq!(
        run_spec(&["--cpu", "1", "--timeout", "2s"]).timeout_secs,
        Some(2)
    );
    assert_eq!(
        run_spec(&["--cpu", "1", "--timeout", "1h"]).timeout_secs,
        Some(3600)
    );
}

#[test]
fn monitor_background_contradiction_is_a_usage_error() {
    assert_eq!(
        parse(&["--cpu", "1", "--monitor", "--background"]),
        Err(StressError::Usage)
    );
}

#[test]
fn a_run_with_no_workers_is_a_usage_error() {
    assert_eq!(parse(&[]), Err(StressError::Usage));
    assert_eq!(parse(&["--quiet"]), Err(StressError::Usage));
    assert_eq!(parse(&["--cpu", "0"]), Err(StressError::Usage));
}

#[test]
fn malformed_values_fail_closed() {
    for bad in [
        &["--cpu", "x"][..],
        &["--cpu"][..],
        &["--vm-bytes", "0"][..],
        &["--vm-bytes", "12q"][..],
        &["--timeout", "0"][..],
        &["--overcommit", "0"][..],
        &["--temp-path", ""][..],
        &["--frobnicate", "1"][..],
    ] {
        assert_eq!(parse(bad), Err(StressError::Usage), "input {bad:?}");
    }
}

#[test]
fn background_implies_quiet_and_help_version_win() {
    let spec = run_spec(&["--cpu", "1", "--background"]);
    assert!(spec.quiet && spec.background);
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
    assert_eq!(parse(&["-h"]), Ok(Command::Help));
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--version"]), Ok(Command::Version));
}

// ---------------------------------------------------------- worker codec

#[test]
fn worker_argv_round_trips_for_every_kind() {
    for (kind, bytes, scratch) in [
        (WorkerKind::Cpu, 0, ""),
        (WorkerKind::Vm, 4096, ""),
        (WorkerKind::Io, 1 << 20, "/Users/x/Library/stress"),
        (WorkerKind::Hdd, 8 << 20, "/tmp-like"),
        (WorkerKind::Cache, 1 << 16, "/scratch"),
    ] {
        let spec = WorkerSpec {
            kind,
            bytes,
            index: 3,
            scratch: scratch.to_string(),
        };
        let argv = spec.encode_argv();
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        assert_eq!(parse(&words[1..]), Ok(Command::Worker(spec)), "{kind:?}");
    }
}

#[test]
fn worker_decode_fails_closed_on_every_malformed_shape() {
    for bad in [
        &["cpu", "0", "0"][..],              // too few tokens
        &["cpu", "0", "0", "", "extra"][..], // too many
        &["gpu", "0", "0", ""][..],          // unknown kind
        &["cpu", "1", "0", ""][..],          // cpu with a byte target
        &["vm", "0", "0", ""][..],           // vm without one
        &["vm", "4096", "0", "/s"][..],      // vm with a scratch
        &["io", "4096", "0", ""][..],        // io without a scratch
        &["io", "40x6", "0", "/s"][..],      // malformed bytes
        &["io", "4096", "-1", "/s"][..],     // malformed index
    ] {
        assert_eq!(
            parse(&[&["--worker"][..], bad].concat()),
            Err(StressError::Usage),
            "{bad:?}"
        );
    }
}

// ---------------------------------------------------------------- sizing

#[test]
fn sizing_scales_from_discovered_resources() {
    let spec = run_spec(&["--vm", "2", "--hdd", "2", "--io", "1", "--cache", "1"]);
    let discovered = Discovered {
        ram_bytes: Some(1 << 30),
        scratch_free_bytes: Some(1 << 30),
    };
    let targets = size_targets(&spec, &discovered);
    // Half the resource split over two workers: a quarter each.
    assert_eq!(targets.vm_bytes, 1 << 28);
    assert_eq!(targets.hdd_bytes, 1 << 28);
    // The cache budget is bounded regardless of free space.
    assert!(targets.cache_bytes <= 8 << 20);
}

#[test]
fn sizing_overcommit_pushes_past_the_resource() {
    let spec = run_spec(&["--vm", "1", "--overcommit", "200"]);
    let discovered = Discovered {
        ram_bytes: Some(1 << 30),
        scratch_free_bytes: None,
    };
    assert_eq!(size_targets(&spec, &discovered).vm_bytes, 2 << 30);
}

#[test]
fn sizing_explicit_bytes_win_and_fallbacks_apply() {
    let spec = run_spec(&["--vm", "1", "--vm-bytes", "1M", "--hdd", "1"]);
    let none = Discovered::default();
    let targets = size_targets(&spec, &none);
    assert_eq!(targets.vm_bytes, 1 << 20); // explicit wins
    assert_eq!(targets.hdd_bytes, 16 << 20); // documented fallback
}

#[test]
fn sizing_survives_huge_resources_without_wrap() {
    let spec = run_spec(&["--hdd", "1", "--overcommit", "400"]);
    let discovered = Discovered {
        ram_bytes: None,
        scratch_free_bytes: Some(u64::MAX),
    };
    // 400% of u64::MAX saturates instead of wrapping.
    assert_eq!(size_targets(&spec, &discovered).hdd_bytes, u64::MAX);
}

// ------------------------------------------------------------ load units

/// An in-memory scratch filesystem, optionally refusing after a byte
/// budget (the ENOSPC shape) so refusal handling is provable.
#[derive(Default)]
struct MemScratch {
    files: alloc::collections::BTreeMap<String, Vec<u8>>,
    /// Total bytes writable before every further write refuses.
    write_budget: Option<u64>,
    written: u64,
    syncs: u32,
}

impl Scratch for MemScratch {
    fn create(&mut self, path: &str) -> Result<(), ScratchError> {
        self.files.insert(path.to_string(), Vec::new());
        Ok(())
    }

    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), ScratchError> {
        if let Some(budget) = self.write_budget {
            if self.written + data.len() as u64 > budget {
                return Err(ScratchError::Refused("no space left on the scratch volume"));
            }
        }
        self.written += data.len() as u64;
        let file = self
            .files
            .get_mut(path)
            .ok_or(ScratchError::Failed("missing file"))?;
        let offset = usize::try_from(offset).map_err(|_| ScratchError::Failed("offset"))?;
        if file.len() < offset + data.len() {
            file.resize(offset + data.len(), 0);
        }
        file[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(), ScratchError> {
        let file = self
            .files
            .get(path)
            .ok_or(ScratchError::Failed("missing file"))?;
        let offset = usize::try_from(offset).map_err(|_| ScratchError::Failed("offset"))?;
        let end = offset + buf.len();
        if file.len() < end {
            return Err(ScratchError::Failed("short read"));
        }
        buf.copy_from_slice(&file[offset..end]);
        Ok(())
    }

    fn sync(&mut self, _path: &str) -> Result<(), ScratchError> {
        self.syncs += 1;
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<(), ScratchError> {
        self.files
            .remove(path)
            .map(|_| ())
            .ok_or(ScratchError::Failed("missing file"))
    }
}

#[test]
fn cpu_unit_is_bounded_and_deterministic() {
    assert_eq!(cpu_unit(7, 1000), cpu_unit(7, 1000));
    assert_ne!(cpu_unit(7, 1000), cpu_unit(8, 1000));
}

#[test]
fn vm_unit_touches_its_target_and_reports_done() {
    let mut buf = Vec::new();
    assert_eq!(vm_unit(&mut buf, 64 << 10, 0), UnitOutcome::Done);
    assert_eq!(buf.len(), 64 << 10);
    // A later pass rotates the touch pattern in place.
    assert_eq!(vm_unit(&mut buf, 64 << 10, 1), UnitOutcome::Done);
}

#[test]
fn io_unit_writes_syncs_and_verifies() {
    let mut fs = MemScratch::default();
    let path = scratch_file("/s", WorkerKind::Io, 0);
    assert_eq!(io_unit(&mut fs, &path, 64 << 10), UnitOutcome::Done);
    assert!(fs.syncs >= 2, "frequent syncs are the io load");
    assert!(fs.files.contains_key(&path), "the io file persists");
}

#[test]
fn hdd_unit_writes_verifies_and_deletes() {
    let mut fs = MemScratch::default();
    let path = scratch_file("/s", WorkerKind::Hdd, 1);
    assert_eq!(hdd_unit(&mut fs, &path, 256 << 10), UnitOutcome::Done);
    assert!(
        !fs.files.contains_key(&path),
        "the hdd unit is self-cleaning"
    );
}

#[test]
fn hdd_unit_detects_corruption() {
    /// A scratch whose reads return stale zeroes.
    struct LyingScratch(MemScratch);
    impl Scratch for LyingScratch {
        fn create(&mut self, path: &str) -> Result<(), ScratchError> {
            self.0.create(path)
        }
        fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), ScratchError> {
            self.0.write(path, offset, data)
        }
        fn read(&mut self, _path: &str, _offset: u64, buf: &mut [u8]) -> Result<(), ScratchError> {
            buf.fill(0);
            Ok(())
        }
        fn sync(&mut self, path: &str) -> Result<(), ScratchError> {
            self.0.sync(path)
        }
        fn remove(&mut self, path: &str) -> Result<(), ScratchError> {
            self.0.remove(path)
        }
    }
    let mut fs = LyingScratch(MemScratch::default());
    let path = scratch_file("/s", WorkerKind::Hdd, 0);
    assert_eq!(
        hdd_unit(&mut fs, &path, 128 << 10),
        UnitOutcome::Failed("hdd verify mismatch")
    );
}

#[test]
fn a_full_volume_is_a_typed_refusal_not_a_failure() {
    let mut fs = MemScratch {
        write_budget: Some(8 << 10),
        ..MemScratch::default()
    };
    let path = scratch_file("/s", WorkerKind::Hdd, 0);
    assert_eq!(
        hdd_unit(&mut fs, &path, 1 << 20),
        UnitOutcome::Refused("no space left on the scratch volume")
    );
}

#[test]
fn cache_unit_builds_then_rereads_its_tree() {
    let mut fs = MemScratch::default();
    assert_eq!(cache_unit(&mut fs, "/s", 2, 64 << 10), UnitOutcome::Done);
    let built = fs.files.len();
    assert!(built > 1, "the tree spreads over several files");
    // A second unit re-reads without rebuilding.
    assert_eq!(cache_unit(&mut fs, "/s", 2, 64 << 10), UnitOutcome::Done);
    assert_eq!(fs.files.len(), built);
}

#[test]
fn scratch_paths_cover_every_disk_worker_and_nothing_else() {
    let workers = Workers {
        cpu: 4,
        vm: 4,
        io: 2,
        hdd: 1,
        cache: 1,
    };
    let paths = run_scratch_paths(&workers, "/s");
    assert!(paths.contains(&scratch_file("/s", WorkerKind::Io, 0)));
    assert!(paths.contains(&scratch_file("/s", WorkerKind::Io, 1)));
    assert!(paths.contains(&scratch_file("/s", WorkerKind::Hdd, 0)));
    // Every path is namespaced under the run's own prefix.
    assert!(paths.iter().all(|p| p.starts_with("/s/stress-")));
    // cpu/vm workers contribute nothing.
    assert!(!paths.iter().any(|p| p.contains("cpu") || p.contains("vm")));
}

// ------------------------------------------------------------ controller

/// Drain a machine to done by reaping every live child with `code`.
fn reap_all(machine: &mut Controller, pids: &[i32], code: i32) {
    for &pid in pids {
        let _ = machine.on_event(Event::ChildExited { pid, code });
    }
}

#[test]
fn interrupt_terminates_workers_and_exits_130() {
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    machine.add_worker(11, WorkerKind::Vm);
    let actions = machine.on_event(Event::Signalled(Signal::Interrupt));
    assert!(actions.contains(&Action::Signal {
        pid: 10,
        signal: Signal::Terminate
    }));
    assert!(actions.contains(&Action::Signal {
        pid: 11,
        signal: Signal::Terminate
    }));
    assert!(actions.contains(&Action::ArmGrace));
    // The terminated workers reap with the Terminate status — counted
    // clean (the requested outcome), and the run exits 130.
    reap_all(&mut machine, &[10, 11], 143);
    assert!(machine.is_done());
    assert_eq!(machine.tally().clean, 2);
    assert_eq!(machine.exit_code(), 130);
}

#[test]
fn timeout_tears_down_and_exits_0() {
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    let actions = machine.on_event(Event::TimeoutElapsed);
    assert_eq!(
        actions,
        alloc::vec![
            Action::Signal {
                pid: 10,
                signal: Signal::Terminate
            },
            Action::ArmGrace
        ]
    );
    reap_all(&mut machine, &[10], 143);
    assert!(machine.is_done());
    assert_eq!(machine.exit_code(), 0);
}

#[test]
fn grace_expiry_escalates_to_kill() {
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Io);
    let _ = machine.on_event(Event::TimeoutElapsed);
    let actions = machine.on_event(Event::GraceElapsed);
    assert_eq!(
        actions,
        alloc::vec![Action::Signal {
            pid: 10,
            signal: Signal::Kill
        }]
    );
    // Killed during teardown is still the requested outcome.
    reap_all(&mut machine, &[10], 137);
    assert_eq!(machine.tally().clean, 1);
    assert_eq!(machine.exit_code(), 0);
}

#[test]
fn an_externally_killed_worker_fails_the_run() {
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    machine.add_worker(11, WorkerKind::Cpu);
    // Killed while the run was still going: a failure, and the run
    // continues with the surviving worker.
    let actions = machine.on_event(Event::ChildExited { pid: 10, code: 137 });
    assert!(actions.is_empty());
    assert!(!machine.is_done());
    assert_eq!(machine.tally().failed, 1);
    let _ = machine.on_event(Event::TimeoutElapsed);
    reap_all(&mut machine, &[11], 143);
    assert_eq!(machine.exit_code(), 1);
}

#[test]
fn refusals_are_expected_outcomes_and_end_the_run_when_all_workers_stop() {
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Vm);
    machine.add_worker(11, WorkerKind::Vm);
    let _ = machine.on_event(Event::ChildExited {
        pid: 10,
        code: REFUSED_EXIT,
    });
    assert!(!machine.is_done());
    // The last worker stopping on its own ends the run.
    let _ = machine.on_event(Event::ChildExited {
        pid: 11,
        code: REFUSED_EXIT,
    });
    assert!(machine.is_done());
    assert_eq!(machine.tally().refused, 2);
    assert_eq!(machine.exit_code(), 0);
}

#[test]
fn monitor_quit_ends_the_run_but_a_completed_run_leaves_the_monitor_up() {
    // Monitor quit → workers torn down, run reported when reaped.
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    machine.add_monitor(99);
    let actions = machine.on_event(Event::ChildExited { pid: 99, code: 0 });
    assert!(machine.monitor_quit());
    assert_eq!(
        actions,
        alloc::vec![
            Action::Signal {
                pid: 10,
                signal: Signal::Terminate
            },
            Action::ArmGrace
        ]
    );
    reap_all(&mut machine, &[10], 143);
    assert!(machine.is_done());

    // Completed run (timeout) → the monitor is NOT torn down; the run is
    // done only when the user quits it.
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    machine.add_monitor(99);
    let actions = machine.on_event(Event::TimeoutElapsed);
    assert!(!actions.contains(&Action::Signal {
        pid: 99,
        signal: Signal::Terminate
    }));
    reap_all(&mut machine, &[10], 143);
    assert!(!machine.is_done(), "the monitor is still up");
    let _ = machine.on_event(Event::ChildExited { pid: 99, code: 0 });
    assert!(machine.is_done());

    // A signal end tears the monitor down too.
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    machine.add_monitor(99);
    let actions = machine.on_event(Event::Signalled(Signal::Terminate));
    assert!(actions.contains(&Action::Signal {
        pid: 99,
        signal: Signal::Terminate
    }));
    reap_all(&mut machine, &[10, 99], 143);
    assert!(machine.is_done());
    assert_eq!(machine.exit_code(), 143);
}

#[test]
fn a_second_signal_never_regresses_the_kill_phase() {
    let mut machine = Controller::new();
    machine.add_worker(10, WorkerKind::Cpu);
    let _ = machine.on_event(Event::Signalled(Signal::Interrupt));
    let _ = machine.on_event(Event::GraceElapsed);
    // A late Terminate does not re-open the drain window (no fresh
    // worker Terminate) and the first signal keeps the exit status.
    let actions = machine.on_event(Event::Signalled(Signal::Terminate));
    assert!(!actions.contains(&Action::Signal {
        pid: 10,
        signal: Signal::Terminate
    }));
    reap_all(&mut machine, &[10], 137);
    assert_eq!(machine.exit_code(), 130);
}

// ---------------------------------------------------------------- report

#[test]
fn report_lines_follow_the_gnu_stress_shape() {
    let workers = Workers {
        cpu: 2,
        vm: 1,
        io: 0,
        hdd: 0,
        cache: 3,
    };
    assert_eq!(
        dispatch_line(42, &workers),
        "stress: info: [42] dispatching hogs: 2 cpu, 0 io, 1 vm, 0 hdd, 3 cache\n"
    );
    assert_eq!(
        completion_line(42, 0, 5),
        "stress: info: [42] successful run completed in 5s\n"
    );
    assert_eq!(
        completion_line(42, 1, 9),
        "stress: info: [42] failed run completed in 9s\n"
    );
    assert_eq!(refusal_line(42, 0), None);
    assert!(refusal_line(42, 2)
        .is_some_and(|line| line.contains("2 worker(s) refused by resource limits")));
}

#[test]
fn the_summary_record_is_a_bounded_jsonl_summary() {
    let tally = crate::ctrl::Tally {
        clean: 3,
        refused: 1,
        failed: 0,
    };
    let mut buf = [0u8; 512];
    let len = summary_record(&tally, 0, 7, &mut buf);
    assert!(len > 0);
    let text = core::str::from_utf8(&buf[..len]).expect("jsonl is utf-8");
    assert!(text.contains("\"kind\":\"summary\""));
    assert!(text.contains("stress.run_summary"));
    assert!(text.contains("\"elapsed_secs\":7"));
    assert!(text.ends_with('\n'));
}

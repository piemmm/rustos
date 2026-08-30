use tairix_log::{Event, EventId, Field, FieldValue, Level};

use super::{Stage, SupervisionWitness, EXIT_SYSCALL, SESSION_COMM, SUPERVISOR_COMM, WAIT_SYSCALL};

/// The login bundle PID 1 launches as the session. The kernel attests a
/// process's name from this path, so it is what [`SESSION_COMM`] must equal.
const SESSION_BUNDLE_PATH: &[u8] = b"/System/Services/login.app/Run";

fn syscall_event<'a>(fields: &'a [Field<'a>]) -> Event<'a> {
    Event {
        level: Level::Debug,
        id: super::SYSCALL_INVOKED,
        message: "syscall dispatched",
        fields,
    }
}

fn spawn_event() -> Event<'static> {
    Event {
        level: Level::Info,
        id: super::PROCESS_SPAWNED,
        message: "process spawned",
        fields: &[],
    }
}

/// Feed one audited syscall through the witness.
fn feed_syscall(witness: &SupervisionWitness, comm: &str, syscall: &str) -> bool {
    let fields = [
        Field {
            key: "comm",
            value: FieldValue::Str(comm),
        },
        Field {
            key: "sc",
            value: FieldValue::Str(syscall),
        },
    ];
    witness.observe(&syscall_event(&fields))
}

/// Drive the whole cycle: the session exits, having been waited on, and a
/// replacement image is built.
fn drive_full_cycle(witness: &SupervisionWitness) -> bool {
    feed_syscall(witness, SUPERVISOR_COMM, WAIT_SYSCALL);
    feed_syscall(witness, SESSION_COMM, EXIT_SYSCALL);
    witness.observe(&spawn_event())
}

/// The name the kernel attests for the session is the `.app` stem, not the
/// generic `Run` entry point. Pin the constant to that rule so a change to
/// the naming logic fails here rather than silently stalling a QEMU run.
#[test]
fn the_session_name_is_what_the_kernel_attests_for_the_session_bundle() {
    let attested = tairix_kernel_sec::ProcName::from_path(SESSION_BUNDLE_PATH);
    assert_eq!(attested.as_str(), SESSION_COMM);
}

#[test]
fn the_full_supervision_cycle_completes_the_witness() {
    let witness = SupervisionWitness::new();
    assert_eq!(witness.stage(), Stage::AwaitSessionExit);
    assert!(drive_full_cycle(&witness));
    assert_eq!(witness.stage(), Stage::Complete);
}

/// The defect this witness exists to prevent. Adding a boot service used to
/// shift the counting thresholds so PASS fired at the session's *first*
/// spawn. This is the real trace that did it, and it must not complete:
/// nothing here is the session exiting and being replaced.
#[test]
fn a_longer_boot_service_list_never_completes_the_witness() {
    let witness = SupervisionWitness::new();
    for _ in 0..7 {
        feed_syscall(&witness, SUPERVISOR_COMM, "spawn");
        witness.observe(&spawn_event());
    }
    for (comm, syscall) in [
        ("sysinfod", "call_create"),
        ("netstack", "call_create"),
        ("netstack", "call_create"),
        ("devmgr", "ipc_call"),
        ("devmgr", "fs_open"),
        ("seatmgr", "call_create"),
        ("confd", "call_create"),
        ("timed", "port_bind"),
        ("timed", "fs_open"),
        ("timed", EXIT_SYSCALL),
        (SUPERVISOR_COMM, WAIT_SYSCALL),
    ] {
        assert!(
            !feed_syscall(&witness, comm, syscall),
            "{comm}/{syscall} must not complete the witness"
        );
    }
    assert_eq!(witness.stage(), Stage::AwaitSessionExit);
}

/// A service exiting is not the session exiting, however many do it.
#[test]
fn another_process_exiting_never_advances_the_witness() {
    let witness = SupervisionWitness::new();
    feed_syscall(&witness, SUPERVISOR_COMM, WAIT_SYSCALL);
    for comm in ["timed", "devmgr", "confd"] {
        feed_syscall(&witness, comm, EXIT_SYSCALL);
        assert!(!witness.observe(&spawn_event()));
    }
    assert_eq!(witness.stage(), Stage::AwaitSessionExit);
}

/// Images built before the session exits are the boot services, not a
/// replacement session.
#[test]
fn a_spawn_before_the_session_exits_never_completes_the_witness() {
    let witness = SupervisionWitness::new();
    feed_syscall(&witness, SUPERVISOR_COMM, WAIT_SYSCALL);
    for _ in 0..20 {
        assert!(!witness.observe(&spawn_event()));
    }
    assert_eq!(witness.stage(), Stage::AwaitSessionExit);
}

/// A supervisor that never reaps has not supervised, so a relaunch alone
/// must not pass.
#[test]
fn a_relaunch_without_a_reaping_supervisor_never_completes_the_witness() {
    let witness = SupervisionWitness::new();
    feed_syscall(&witness, SESSION_COMM, EXIT_SYSCALL);
    assert_eq!(witness.stage(), Stage::AwaitRelaunch);
    assert!(!witness.observe(&spawn_event()));
    assert_eq!(witness.stage(), Stage::AwaitRelaunch);

    // The reap arriving later still completes it on the next image.
    feed_syscall(&witness, SUPERVISOR_COMM, WAIT_SYSCALL);
    assert!(witness.observe(&spawn_event()));
}

/// The session exiting is not by itself the cycle: the replacement has to be
/// built.
#[test]
fn a_session_exit_without_a_relaunch_never_completes_the_witness() {
    let witness = SupervisionWitness::new();
    feed_syscall(&witness, SUPERVISOR_COMM, WAIT_SYSCALL);
    assert!(!feed_syscall(&witness, SESSION_COMM, EXIT_SYSCALL));
    assert_eq!(witness.stage(), Stage::AwaitRelaunch);
}

/// A sink keeps calling `observe` after PASS; the answer must not flap.
#[test]
fn the_witness_stays_complete_once_the_cycle_is_observed() {
    let witness = SupervisionWitness::new();
    assert!(drive_full_cycle(&witness));
    for (comm, syscall) in [("timed", EXIT_SYSCALL), (SUPERVISOR_COMM, "spawn")] {
        assert!(feed_syscall(&witness, comm, syscall));
    }
    assert!(witness.observe(&spawn_event()));
    assert_eq!(witness.stage(), Stage::Complete);
}

/// Records that are neither of the two the witness reads leave it alone, and
/// a malformed record (missing or wrongly-typed fields) is ignored rather
/// than mistaken for a match.
#[test]
fn unrelated_or_malformed_records_leave_the_witness_untouched() {
    let witness = SupervisionWitness::new();
    feed_syscall(&witness, SUPERVISOR_COMM, WAIT_SYSCALL);

    let other = Event {
        level: Level::Info,
        id: EventId(1),
        message: "unrelated",
        fields: &[Field {
            key: "comm",
            value: FieldValue::Str(SESSION_COMM),
        }],
    };
    assert!(!witness.observe(&other));

    let untyped = [
        Field {
            key: "comm",
            value: FieldValue::UnsignedInt(0),
        },
        Field {
            key: "sc",
            value: FieldValue::Str(EXIT_SYSCALL),
        },
    ];
    assert!(!witness.observe(&syscall_event(&untyped)));

    let no_fields: [Field<'_>; 0] = [];
    assert!(!witness.observe(&syscall_event(&no_fields)));
    assert_eq!(witness.stage(), Stage::AwaitSessionExit);
}

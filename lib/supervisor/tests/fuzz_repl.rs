//! Deterministic fuzz harness for the `lib/supervisor` REPL line/command
//! parser (the Supervisor's untrusted-console-input decoder).
//!
//! The pre-boot Supervisor reads bytes from the physical console before any
//! user is authenticated, so those bytes are untrusted operator/attacker
//! input. The parser path — the line editor
//! ([`tairix_supervisor::run_supervisor`]'s reader) plus the tokeniser and
//! dispatcher — is driven by this harness whose single invariant is:
//!
//! * feeding any byte stream to the REPL never panics and always terminates
//!   (every command handler returns, and the reader stops at end of input) —
//!   the engine renders output or reports an unknown command, never a crash
//!   (fail closed, `AGENTS.md` §2.9 / §19.6).
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG
//! ([`tairix_fuzzseed`]) draws pseudo-random byte scripts, splices real
//! command words together with hostile arguments, and mutates them. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged
//! seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the
//! loop to a wall-clock budget.

use tairix_supervisor::{
    run_supervisor, MountOutcome, Report, SupInput, SupervisorEvent, SupervisorHost, TestOutcome,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 200_000;

/// Largest arbitrary byte script fed straight to the REPL.
const MAX_NOISE: usize = 2048;

/// Real command words the harness splices into scripts so the mutated input
/// exercises every dispatch arm, not only the unknown-command path.
const WORDS: &[&[u8]] = &[
    b"help",
    b"?",
    b"continue",
    b"boot",
    b"mount",
    b"reboot",
    b"poweroff",
    b"halt",
    b"version",
    b"mem",
    b"map",
    b"cpu",
    b"hw",
    b"lsdev",
    b"disk",
    b"partitions",
    b"arxfs",
    b"ls",
    b"log",
    b"panic-log",
    b"last",
    b"uptime",
    b"date",
    b"memtest",
    b"test",
    b"echo",
    b"clear",
    b"disk0",
    b"3",
    b"--",
    b"/System",
];

/// A [`Report`] that counts bytes and never allocates unboundedly — the
/// harness cares only that rendering does not panic, not what it renders.
#[derive(Default)]
struct CountReport {
    bytes: usize,
}

impl Report for CountReport {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.wrapping_add(bytes.len());
    }
}

/// A [`SupInput`] replaying a fixed script then reporting end of input, so the
/// REPL always terminates.
struct ScriptInput<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl SupInput for ScriptInput<'_> {
    fn read_byte(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.pos).copied();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }
}

/// A [`SupervisorHost`] that renders a short line for every query and returns
/// canned control outcomes. `mount` varies its result by the typed
/// passphrase's length so the fuzzer reaches every `mount` branch; `reboot`
/// and `poweroff` return (as an unsupported platform would), so the REPL
/// stays and the run terminates.
struct FuzzHost;

impl SupervisorHost for FuzzHost {
    fn version(&mut self, out: &mut dyn Report) {
        out.line("version");
    }
    fn memory(&mut self, out: &mut dyn Report) {
        out.line("mem");
    }
    fn memory_map(&mut self, out: &mut dyn Report) {
        out.line("mem map");
    }
    fn cpu(&mut self, out: &mut dyn Report) {
        out.line("cpu");
    }
    fn hardware(&mut self, out: &mut dyn Report) {
        out.line("hw");
    }
    fn disks(&mut self, out: &mut dyn Report) {
        out.line("disk");
    }
    fn partitions(&mut self, device: &str, out: &mut dyn Report) {
        out.write_str("part ");
        out.line(device);
    }
    fn arxfs_status(&mut self, out: &mut dyn Report) {
        out.line("arxfs");
    }
    fn list(&mut self, path: Option<&str>, out: &mut dyn Report) {
        out.line(path.unwrap_or("/System"));
    }
    fn log_tail(&mut self, _count: Option<usize>, out: &mut dyn Report) {
        out.line("log");
    }
    fn panic_log(&mut self, out: &mut dyn Report) {
        out.line("panic-log");
    }
    fn uptime(&mut self, out: &mut dyn Report) {
        out.line("uptime");
    }
    fn date(&mut self, out: &mut dyn Report) {
        out.line("date");
    }
    fn memtest(
        &mut self,
        _passes: u32,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        let _ = abort();
        out.line("memtest");
        TestOutcome::Passed
    }
    fn scan_disk(
        &mut self,
        _device: &str,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        let _ = abort();
        out.line("scan");
        TestOutcome::Passed
    }
    fn mount(&mut self, passphrase: &[u8], out: &mut dyn Report) -> MountOutcome {
        out.line("mount");
        // Reach every mount branch across iterations without ever echoing the
        // secret: the outcome is a pure function of the passphrase length.
        match passphrase.len() % 3 {
            0 => MountOutcome::Mounted,
            1 => MountOutcome::WrongPassphrase,
            _ => MountOutcome::Failed,
        }
    }
    fn reboot(&mut self) {}
    fn poweroff(&mut self) {}
    fn audit(&mut self, _event: SupervisorEvent) {}
}

/// Drive the REPL with `script`; it must never panic and must return.
fn repl_never_panics(script: &[u8]) {
    let mut out = CountReport::default();
    let mut input = ScriptInput {
        bytes: script,
        pos: 0,
    };
    let mut host = FuzzHost;
    let _ = run_supervisor(&mut out, &mut input, &mut host);
    // Silence the byte counter without dropping the "no panic" coverage.
    core::hint::black_box(out.bytes);
}

#[test]
fn repl_never_panics_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let seed = tairix_fuzzseed::start(
        "repl_never_panics_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut rng = tairix_fuzzseed::Lcg::new(seed);

    let mut iteration: u64 = 0;
    loop {
        // 1. A script of random real words and separators (spaces, tabs,
        //    newlines, carriage returns), so the tokeniser and dispatcher see
        //    plausible-but-hostile command lines.
        let word_count = rng.below(24);
        let mut script: Vec<u8> = Vec::new();
        for _ in 0..word_count {
            let word = WORDS[rng.below(WORDS.len())];
            script.extend_from_slice(word);
            match rng.below(6) {
                0 => script.push(b'\n'),
                1 => script.push(b'\r'),
                2 => script.push(b'\t'),
                3 => script.push(b'\x1b'),
                _ => script.push(b' '),
            }
        }
        // Flip a few bytes to corrupt words and separators alike.
        let flips = rng.below(6);
        for _ in 0..flips {
            if script.is_empty() {
                break;
            }
            let pos = rng.below(script.len());
            script[pos] ^= u8::try_from(rng.next_u64() & 0xff).unwrap_or(0);
        }
        repl_never_panics(&script);

        // 2. Pure noise straight into the reader/dispatcher.
        let nlen = rng.below(MAX_NOISE);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        repl_never_panics(&noise);

        // 3. A single over-long line with no terminator, to exercise the
        //    too-long drain path and the fixed line buffer's bound.
        let long_len = rng.below(2 * tairix_supervisor::MAX_LINE_LEN);
        let long: Vec<u8> = vec![b'a'; long_len];
        repl_never_panics(&long);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

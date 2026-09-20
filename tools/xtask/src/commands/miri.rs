//! `cargo xtask miri` — run the workspace's `unsafe` cores under an
//! undefined-behaviour oracle.
//!
//! A test suite proves what a program computes; it cannot prove that a raw
//! pointer stayed in bounds, that a slot was initialised before it was read,
//! or that two `&mut` never aliased. Miri interprets the program and checks
//! exactly those, so it is the oracle the hand-written containers need and
//! the ordinary test matrix cannot be.
//!
//! The stage is deliberately narrow. Miri interprets every operation, so
//! pointing it at the whole workspace would cost hours and tell us nothing
//! about the crates that carry no `unsafe` at all. [`TARGETS`] therefore names
//! the crates whose safety rests on a hand-written `unsafe` core, and each of
//! those crates scales its own sweeps down under `cfg(miri)` — the wide input
//! search belongs to the ordinary and budgeted runs; this one is looking for
//! undefined behaviour, which one pass over each code path already exposes.
//!
//! Adding a crate here means adding a [`Target`], never teaching `ci` about it
//! directly.
//!
//! The stage builds for the **host**, so a port's `target_os = "none"` code is
//! never interpreted, and what the `tairix-arch-*` crates are enrolled for is
//! the portable `unsafe` core — the page-table walks, the initial-frame writes
//! — which every host interprets alike. A port that reaches a hardware
//! instruction from a *host* build instead aborts that crate's whole run with
//! "unsupported operation", and only on the machine whose architecture it
//! names: a developer on one arch and a runner on another do not agree.

use std::ffi::OsString;
use std::process::{Command, Stdio};

use crate::commands::parallel::{self, Job};
use crate::commands::seed;
use crate::{
    await_within, effective_timeout, spawn_in_own_group, Context, DEFAULT_COMMAND_TIMEOUT,
};

/// Which of a crate's test targets the oracle interprets.
///
/// Isolation stays on for the whole stage (see [`MIRIFLAGS`]), so a test
/// that opens a file or reads the clock is not *failed* by the
/// interpreter — it is refused as an unsupported operation, taking the
/// whole crate's run down with it. A crate with such a test is scoped to
/// its own `--lib`, which is where its `unsafe` core lives anyway, rather
/// than weakening isolation for every other crate here.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Every test target the crate builds.
    AllTargets,
    /// The crate's `--lib` tests only, for the reason carried here.
    LibOnly(&'static str),
    /// The crate's `--lib` tests bar the modules `skip` names, for the
    /// reason carried here.
    ///
    /// Budget only: a skipped module carries no `unsafe` and passes when it
    /// is run, so what the interpreter would spend on it buys nothing the
    /// rest of the crate does not already prove. Skipping one that *reports*
    /// undefined behaviour would be dodging a finding, which the charter
    /// forbids.
    LibExcept {
        /// libtest `--skip` patterns.
        skip: &'static [&'static str],
        /// Why each skipped module costs the interpreter more than it tells
        /// it.
        reason: &'static str,
    },
}

impl Scope {
    /// Whether the run is confined to the crate's `--lib` target.
    const fn is_lib_only(self) -> bool {
        matches!(self, Self::LibOnly(_) | Self::LibExcept { .. })
    }

    /// The libtest `--skip` patterns this scope excludes.
    const fn skip_patterns(self) -> &'static [&'static str] {
        match self {
            Self::AllTargets | Self::LibOnly(_) => &[],
            Self::LibExcept { skip, .. } => skip,
        }
    }
}

/// How many processes one target's interpreted run is spread across.
///
/// Miri runs one interpreted thread at a time and reports a single CPU to the
/// program, so libtest takes a crate's tests one after another however many
/// cores the host has. A crate with hundreds of them is therefore one long
/// single-core job, and it alone sets the stage's makespan while the rest of
/// the machine idles — and a budget that fits it on a fast developer machine
/// does not fit it on a slower runner.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Spread {
    /// One process for the whole target.
    OneProcess,
    /// Enumerate the target's tests and deal them across one process per host
    /// core, for the reason carried here.
    ///
    /// The partition comes from the test binary's own `--list`, so a test
    /// added later cannot fall outside every shard and go uninterpreted while
    /// the stage still reports success.
    PerCore(&'static str),
}

/// One crate the oracle is pointed at.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Workspace package (`cargo miri test -p`).
    pub package: &'static str,
    /// Why this crate's safety needs an oracle.
    pub description: &'static str,
    /// Cargo features to enable, for a crate whose `unsafe` is behind one.
    /// Empty means the default build.
    pub features: &'static [&'static str],
    /// Which test targets to interpret.
    pub scope: Scope,
    /// How many processes the run is spread across.
    pub spread: Spread,
}

/// The crates whose soundness rests on a hand-written `unsafe` core.
pub const TARGETS: &[Target] = &[
    Target {
        package: "tairix-collections",
        description: "the open-addressed hash table's control array and iterators",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-inline",
        description: "the allocation-free tier's inline slot arrays, and the volatile scrub a secret ring leaves behind",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-hash",
        description: "the one-shot key-publication cell the containers are keyed through",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-sync",
        description: "the MCS queue's intrusive node chain, the set-once cell's MaybeUninit, \
                      and every guard's aliasing claim",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-parallel",
        description: "the index-to-element erasure every parallel pass runs through: the raw \
                      element pointer `for_each` hands its jobs, the `Send`/`Sync` claims that \
                      carry it into a job closure, and the `&mut` each job reconstructs from \
                      it. Every runner in the tree — the compositor's bands, the raster \
                      passes, the client's frame — reaches undefined behaviour through this \
                      one block if the pieces are not disjoint, and the shared `Threaded` \
                      runner makes that claim under real threads where the interpreter's \
                      data-race detector can read it",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-kalloc",
        description: "the kernel heap's in-band boundary tags: the physical back-link a \
                      coalesce dereferences, the block a split carves off, the descriptor an \
                      object's page is found through, and a returned region's header",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-sync",
        description: "the same, plus the lock-diagnostics observer seam, whose function \
                      pointers and site records live only under that feature",
        features: &["lock-diagnostics"],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-kernel-mem",
        description: "the slab tier's guarded storage, the remap window's slot arithmetic, the \
                      DMA pool's direct-map slices, the bounded-pointer helpers, and the direct \
                      physical map's provenance root",
        features: &[],
        scope: Scope::LibExcept {
            skip: &["dma::tests::a_full_span_window_serves_a_multi_device_enclosure_lazily"],
            reason: "that one test reserves a full gigabyte of window and streams thirteen \
                     32-page device regions through it, zeroed on carve and volatile-cleared on \
                     release; interpreted, the per-byte aliasing bookkeeping over that volume \
                     costs four hours. It passes when run, and what it proves beyond the rest of \
                     the module is slot *capacity* — the `unsafe` it reaches is the same \
                     direct-map slice the other twenty-five dma tests reach. Its own integration \
                     targets are excluded with it: the loom model does not build under the \
                     interpreter and the fuzz harnesses are budgeted elsewhere",
        },
        spread: Spread::PerCore(
            "465 tests, and interpreted they cost twenty minutes end to end — half the \
             pipeline's whole wall clock in one single-core process, against a budget only \
             twice that. The runner overran it. Dealt across the host's cores the work is \
             unchanged and the makespan falls to the longest single test",
        ),
    },
    Target {
        package: "tairix-abi",
        description: "the two shared-memory rings' headers: the atomic counters carved out of \
                      a mapped region by `align_to_mut`, the MMIO and port-I/O accessors, and \
                      the DMA descriptor views. The rings publish through those counters, so \
                      deriving them from a read-only view made every publication a write the \
                      borrow never granted — which only an interpreter can see",
        features: &[],
        scope: Scope::LibOnly(
            "its `*_ring_spsc` integration tests deliberately alias two `&mut` views over one \
             leaked region, which is how two processes map one `shm` object and is a situation \
             outside the aliasing model entirely; the shipped code never aliases within an \
             address space. The loom model does not build under the interpreter, and the fuzz \
             harnesses read the clock for their budget, which isolation refuses",
        ),
        spread: Spread::PerCore(
            "over fifteen hundred tests, and interpreted they cost five minutes end to end in \
             one single-core process. Dealt across the host's cores the work is unchanged and \
             the makespan falls to the longest single test",
        ),
    },
    Target {
        package: "tairix-arch-api",
        description: "the HAL's shared unsafe floor: the frame-pointer unwinder's walk over a \
                      hostile stack, the page-table reclaim walk, and the per-CPU and quiesce \
                      table publications",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-arch-aarch64",
        description: "the page-table walk's recovery of each level through its frame source, \
                      and the initial-frame write into a task's kernel stack",
        features: &[],
        scope: Scope::LibOnly(
            "its `real_dtb_probe` integration test reads the downloaded Pi 4 firmware blob \
             from disk, which the interpreter refuses under isolation before the test's own \
             absent-file skip can run",
        ),
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-arch-riscv64",
        description: "the same walk and initial-frame write for Sv39, plus the direct physical \
                      map's gigapage leaves",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
    Target {
        package: "tairix-arch-x86_64",
        description: "the same walk and initial-frame write for 4-level paging, whose pool and \
                      reclaim verticals run over the real page-table allocator",
        features: &[],
        scope: Scope::AllTargets,
        spread: Spread::OneProcess,
    },
];

/// Miri's own flags.
///
/// Stacked Borrows is the stricter aliasing model of the two Miri ships and is
/// the one an intrusive, pointer-based container is most likely to violate, so
/// the default stands. Isolation stays on — a container touches no clock, no
/// filesystem, and no network, and a stage that needed to would be telling us
/// something — with the one harness seed forwarded so a reported failure
/// replays exactly.
const MIRIFLAGS: &str = "-Zmiri-strict-provenance";

/// Parsed `miri` arguments.
pub struct Options {
    /// Restrict the run to one package.
    package: Option<String>,
    /// Base seed for the harnesses, so a reported failure replays.
    seed: Option<u64>,
    /// List the targets and exit.
    list: bool,
}

/// Parse `--package <name>`, `--seed <n>`, and `--list`.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut opts = Options {
        package: None,
        seed: None,
        list: false,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--list") => opts.list = true,
            Some("--package" | "-p") => {
                let value = rest
                    .next()
                    .and_then(|v| v.to_str().map(str::to_string))
                    .ok_or_else(|| "miri: --package needs a name".to_string())?;
                opts.package = Some(value);
            }
            Some("--seed") => {
                let value = rest
                    .next()
                    .and_then(|v| v.to_str())
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| "miri: --seed needs a u64".to_string())?;
                opts.seed = Some(value);
            }
            _ => {
                return Err(format!(
                    "miri: unexpected argument {}; usage: cargo xtask miri \
                     [--package <name>] [--seed <n>] [--list]",
                    arg.display()
                ))
            }
        }
    }
    Ok(opts)
}

/// The targets a run covers, honouring `--package`.
fn selected(opts: &Options) -> Result<Vec<&'static Target>, String> {
    let Some(name) = opts.package.as_deref() else {
        return Ok(TARGETS.iter().collect());
    };
    // Every matching entry, not the first: a crate whose `unsafe` is split
    // across features has one target per build, and running only one of them
    // would leave the rest uninterpreted while still reporting success.
    let picked: Vec<&'static Target> = TARGETS.iter().filter(|t| t.package == name).collect();
    if picked.is_empty() {
        return Err(format!(
            "miri: unknown package `{name}`; known: {}",
            TARGETS
                .iter()
                .map(|t| t.package)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(picked)
}

/// Run the oracle over every selected crate, failing closed.
pub fn run(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let opts = parse(args)?;
    if opts.list {
        for target in TARGETS {
            println!("{:<24} {}", target.package, target.description);
        }
        return Ok(());
    }
    if !crate::commands::cargo_subcommand_available(ctx, "miri") {
        return Err(
            "miri is not installed; run `rustup component add miri` (it is pinned in \
             rust-toolchain.toml, so `rustup toolchain install` also brings it)"
                .to_string(),
        );
    }

    let targets = selected(&opts)?;
    // Each package is an independent host process, so the set runs
    // concurrently under the shared bounded runner rather than paying the sum
    // of the interpreter's costs.
    //
    // Sharded targets are emitted first. Every job weighs one unit, so the
    // runner admits them in the order given, and these carry the longest runs:
    // started while the runner is still empty their tail lands inside the
    // stage rather than after everything else has drained.
    let mut jobs: Vec<Job> = Vec::new();
    let mut deferred: Vec<Job> = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let seed = seed::job_seed(opts.seed, index);
        match target.spread {
            Spread::PerCore(_) => match shard_jobs(ctx, target, seed) {
                Ok(shards) => jobs.extend(shards),
                // Carried into the runner as a failing job rather than
                // returned from here, so a crate whose listing cannot be read
                // does not hide every other crate's result.
                Err(why) => {
                    jobs.push(Job::closure(scope_label(target), 1, move || Err(why)));
                }
            },
            Spread::OneProcess => deferred.push(whole_job(ctx, target, seed)),
        }
    }
    jobs.append(&mut deferred);
    let concurrency = parallel::default_concurrency(jobs.len());
    parallel::run(jobs, concurrency)
}

/// The `cargo miri test` invocation for `target`, without the libtest
/// arguments that decide *which* of its tests run.
fn base_command(ctx: &Context, target: &Target, seed: u64) -> Command {
    let mut cmd: Command = ctx.cargo();
    cmd.args(["miri", "test", "-p", target.package, "--locked"]);
    if target.scope.is_lib_only() {
        cmd.arg("--lib");
    }
    if !target.features.is_empty() {
        cmd.args(["--features", &target.features.join(",")]);
    }
    cmd.env(seed::FUZZ_SEED_ENV, seed.to_string());
    // Miri hides the host environment from the interpreted program, so the
    // seed is forwarded explicitly; without it the harness falls back to the
    // wall clock, which isolation correctly refuses.
    cmd.env(
        "MIRIFLAGS",
        format!("{MIRIFLAGS} -Zmiri-env-forward={}", seed::FUZZ_SEED_ENV),
    );
    cmd
}

/// How a label names the target's scope.
fn scope_label(target: &Target) -> String {
    let scope = match target.scope {
        Scope::AllTargets => "",
        Scope::LibOnly(_) => " --lib",
        Scope::LibExcept { .. } => " --lib (part)",
    };
    if target.features.is_empty() {
        format!("miri {}{scope}", target.package)
    } else {
        format!(
            "miri {}{scope} +{}",
            target.package,
            target.features.join(",")
        )
    }
}

/// One package's whole interpreted test run, in a single process.
fn whole_job(ctx: &Context, target: &Target, seed: u64) -> Job {
    let mut cmd = base_command(ctx, target, seed);
    let skip = target.scope.skip_patterns();
    if !skip.is_empty() {
        cmd.arg("--");
        for pattern in skip {
            cmd.args(["--skip", pattern]);
        }
    }
    Job::new(format!("{} (seed {seed})", scope_label(target)), cmd)
}

/// One job per shard of `target`'s tests, dealt across the host's cores.
///
/// Every shard of a target carries the *same* seed: the shard count comes
/// from the host, so a per-shard seed would make a reported failure replay
/// only on a machine with the same core count.
fn shard_jobs(ctx: &Context, target: &Target, seed: u64) -> Result<Vec<Job>, String> {
    let names = enumerate(ctx, target, seed)?;
    let found = names.len();
    let shards = deal(names, parallel::host_parallelism());
    eprintln!(
        "xtask: [{}] {found} tests dealt across {} processes",
        scope_label(target),
        shards.len()
    );
    let total = shards.len();
    Ok(shards
        .into_iter()
        .enumerate()
        .map(|(index, names)| {
            let mut cmd = base_command(ctx, target, seed);
            cmd.arg("--").arg("--exact").args(&names);
            Job::new(
                format!(
                    "{} shard {}/{total} (seed {seed})",
                    scope_label(target),
                    index + 1
                ),
                cmd,
            )
        })
        .collect())
}

/// Every test the target's binary reports, with the scope's exclusions
/// already applied by libtest itself.
///
/// This pass is also what builds the target, so the shards behind it are
/// cargo no-ops that go straight to interpreting.
fn enumerate(ctx: &Context, target: &Target, seed: u64) -> Result<Vec<String>, String> {
    let mut cmd = base_command(ctx, target, seed);
    cmd.arg("--").arg("--list");
    for pattern in target.scope.skip_patterns() {
        cmd.args(["--skip", pattern]);
    }
    let label = format!("{} --list", scope_label(target));
    let listing = capture(&label, cmd)?;
    parse_listing(&listing).map_err(|why| format!("{label}: {why}"))
}

/// The test names in a libtest `--list` listing, checked against the count
/// libtest declares at the end of it.
///
/// The cross-check is what makes the partition trustworthy: without it a
/// change to the listing format would shrink the set of names silently, and
/// the stage would interpret a subset and still report success.
fn parse_listing(listing: &str) -> Result<Vec<String>, String> {
    let names: Vec<String> = listing
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_string)
        .collect();
    let declared = declared_count(listing).ok_or_else(|| {
        "the listing declares no test count, so the names read out of it \
         cannot be checked"
            .to_string()
    })?;
    if declared != names.len() {
        return Err(format!(
            "the listing declares {declared} tests but {} names were read \
             out of it",
            names.len()
        ));
    }
    if names.is_empty() {
        return Err("no tests to interpret; a sharded target with nothing to \
                    run would report success having interpreted nothing"
            .to_string());
    }
    Ok(names)
}

/// The count from libtest's closing `N tests, M benchmarks` line.
fn declared_count(listing: &str) -> Option<usize> {
    listing.lines().rev().find_map(|line| {
        let (count, rest) = line.split_once(' ')?;
        (rest.starts_with("test") && rest.contains("benchmark"))
            .then(|| count.parse().ok())
            .flatten()
    })
}

/// Deal `names` round-robin into at most `ways` shards.
///
/// Round-robin rather than contiguous blocks: the listing is sorted by test
/// path, so consecutive names are the ones most likely to cost alike, and
/// dealing them spreads an expensive module across every shard instead of
/// stacking it into one.
fn deal(names: Vec<String>, ways: usize) -> Vec<Vec<String>> {
    if names.is_empty() {
        return Vec::new();
    }
    let ways = ways.clamp(1, names.len());
    let mut shards = vec![Vec::new(); ways];
    for (position, name) in names.into_iter().enumerate() {
        shards[position % ways].push(name);
    }
    shards
}

/// Run `cmd` to completion under the standard command budget and return its
/// stdout, failing closed on a non-zero status.
fn capture(label: &str, mut cmd: Command) -> Result<String, String> {
    let budget = effective_timeout(DEFAULT_COMMAND_TIMEOUT)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_in_own_group(&mut cmd)
        .map_err(|err| format!("{label} could not be spawned: {err}"))?;
    let pid = child.id();
    let output = await_within(label, pid, budget, move || child.wait_with_output())?;
    if !output.status.success() {
        return Err(format!(
            "{label} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("{label} produced output that is not UTF-8: {err}"))
}

#[cfg(test)]
mod tests {
    use super::{deal, parse, parse_listing, selected, Scope, Spread, TARGETS};
    use std::ffi::OsString;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_selects_every_target() {
        let opts = parse(&[]).expect("no arguments");
        assert_eq!(selected(&opts).expect("all").len(), TARGETS.len());
    }

    #[test]
    fn a_package_filter_selects_exactly_one_target() {
        let opts = parse(&args(&["--package", "tairix-collections"])).expect("filter");
        let chosen = selected(&opts).expect("one");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].package, "tairix-collections");
    }

    /// A crate whose `unsafe` is split across features has one target per
    /// build, and a filter that returned only the first would run one and
    /// report success for both.
    #[test]
    fn a_package_filter_selects_every_feature_build_of_that_package() {
        let opts = parse(&args(&["--package", "tairix-sync"])).expect("filter");
        let chosen = selected(&opts).expect("both builds");
        assert_eq!(
            chosen.len(),
            TARGETS
                .iter()
                .filter(|t| t.package == "tairix-sync")
                .count()
        );
        assert!(chosen.iter().any(|t| t.features.is_empty()));
        assert!(chosen
            .iter()
            .any(|t| t.features.contains(&"lock-diagnostics")));
    }

    #[test]
    fn an_unknown_package_is_refused_rather_than_silently_skipped() {
        let opts = parse(&args(&["--package", "nope"])).expect("filter");
        assert!(selected(&opts).is_err());
    }

    #[test]
    fn a_malformed_argument_is_refused() {
        assert!(parse(&args(&["--seed"])).is_err());
        assert!(parse(&args(&["--seed", "not-a-number"])).is_err());
        assert!(parse(&args(&["--what"])).is_err());
    }

    /// Every target must name a real workspace package, and no *build* twice.
    ///
    /// A package may appear more than once — one entry per feature set, where
    /// its `unsafe` is split across features — so the identity a duplicate
    /// would waste the interpreter on is the pair, not the name alone.
    /// A narrowed scope must say why, so a future reader can tell a
    /// considered exclusion from one added to make a run go green.
    #[test]
    fn a_narrowed_scope_carries_its_reason() {
        for target in TARGETS {
            let reason = match target.scope {
                Scope::AllTargets => None,
                Scope::LibOnly(reason) | Scope::LibExcept { reason, .. } => Some(reason),
            };
            if let Some(reason) = reason {
                assert!(
                    !reason.trim().is_empty(),
                    "{} is narrowed with no reason",
                    target.package
                );
            }
            if let Scope::LibExcept { skip, .. } = target.scope {
                assert!(
                    !skip.is_empty(),
                    "{} excludes nothing, so it is not a narrowed scope",
                    target.package
                );
                for pattern in skip {
                    assert!(
                        !pattern.trim().is_empty(),
                        "{} carries an empty skip pattern",
                        target.package
                    );
                }
            }
        }
    }

    /// A sharded target must say why, for the same reason a narrowed scope
    /// must: a reader has to be able to tell a measured decision from one
    /// taken to make a run go green.
    #[test]
    fn a_sharded_target_carries_its_reason() {
        for target in TARGETS {
            if let Spread::PerCore(reason) = target.spread {
                assert!(
                    !reason.trim().is_empty(),
                    "{} is sharded with no reason",
                    target.package
                );
            }
        }
    }

    /// Sharding deals the names one test binary reported, so a target whose
    /// scope builds several would have `--exact` filters applied to each of
    /// them and could run a name more than once.
    #[test]
    fn a_sharded_target_is_confined_to_one_test_binary() {
        for target in TARGETS {
            if matches!(target.spread, Spread::PerCore(_)) {
                assert!(
                    target.scope.is_lib_only(),
                    "{} is sharded but its scope builds more than one test binary",
                    target.package
                );
            }
        }
    }

    #[test]
    fn a_listing_yields_its_names() {
        let listing = "dma::tests::a: test\ndma::tests::b: test\n\n2 tests, 0 benchmarks\n";
        assert_eq!(
            parse_listing(listing).expect("two names"),
            ["dma::tests::a", "dma::tests::b"]
        );
    }

    #[test]
    fn a_singular_listing_yields_its_one_name() {
        let listing = "m::only: test\n\n1 test, 0 benchmarks\n";
        assert_eq!(parse_listing(listing).expect("one name"), ["m::only"]);
    }

    /// The whole point of the cross-check: a listing whose names no longer
    /// parse must fail the stage, never shard a subset of them.
    #[test]
    fn a_listing_that_disagrees_with_its_own_count_is_refused() {
        let drifted = "dma::tests::a -> test\ndma::tests::b -> test\n\n2 tests, 0 benchmarks\n";
        let err = parse_listing(drifted).expect_err("names that no longer parse");
        assert!(err.contains("declares 2 tests"), "{err}");

        let countless = "dma::tests::a: test\n";
        assert!(parse_listing(countless)
            .expect_err("no declared count")
            .contains("no test count"));

        let empty = "\n0 tests, 0 benchmarks\n";
        assert!(parse_listing(empty)
            .expect_err("nothing to interpret")
            .contains("no tests to interpret"));
    }

    /// The partition must lose nothing: a name dropped here is a test the
    /// stage never interprets while still reporting success.
    #[test]
    fn dealing_covers_every_name_exactly_once() {
        let names: Vec<String> = (0..37).map(|n| format!("m::t{n}")).collect();
        for ways in [1usize, 2, 5, 8, 37, 64] {
            let shards = deal(names.clone(), ways);
            assert!(shards.len() <= names.len());
            assert!(shards.iter().all(|shard| !shard.is_empty()));
            let mut dealt: Vec<String> = shards.into_iter().flatten().collect();
            dealt.sort();
            let mut expected = names.clone();
            expected.sort();
            assert_eq!(dealt, expected, "ways = {ways}");
        }
    }

    /// Round-robin keeps the shards within one test of each other, so no
    /// shard is handed the tail of a long listing on its own.
    #[test]
    fn dealing_balances_the_shards() {
        let names: Vec<String> = (0..37).map(|n| format!("m::t{n}")).collect();
        let shards = deal(names, 8);
        assert_eq!(shards.len(), 8);
        let longest = shards.iter().map(Vec::len).max().unwrap_or(0);
        let shortest = shards.iter().map(Vec::len).min().unwrap_or(0);
        assert!(longest - shortest <= 1, "{shortest}..{longest}");
    }

    /// A single name still yields a runnable shard whatever the host reports,
    /// and an empty listing yields no shard to run rather than a shard with
    /// no filter, which libtest would read as "run everything".
    #[test]
    fn dealing_is_total_at_the_edges() {
        let shards = deal(vec!["m::only".to_string()], 16);
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].len(), 1);
        assert!(deal(Vec::new(), 16).is_empty());
    }

    #[test]
    fn the_registry_is_distinct() {
        for (index, target) in TARGETS.iter().enumerate() {
            assert!(target.package.starts_with("tairix-"), "{}", target.package);
            assert!(!target.description.is_empty());
            for other in &TARGETS[index + 1..] {
                assert_ne!(
                    (target.package, target.features),
                    (other.package, other.features),
                    "{} is registered twice with the same features",
                    target.package
                );
            }
        }
    }
}

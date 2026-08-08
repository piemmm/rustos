# CODEVERIFY.md — Find and fix bad code across the workspace

This is a staged plan to systematically hunt for code-quality and charter
violations across every `.rs` file in the workspace and fix each one to a
senior-engineer standard. It is **binding under `AGENTS.md`**; read `AGENTS.md`
and `PLAN.md` first. Every rule in both applies here without exception.

The work runs as a repeating loop across many fresh AI contexts. A single
context does a bounded amount of work: it finds (or picks up) **one** issue,
understands it fully, fixes it properly with tests, runs the whole-project gate
green, and records the remaining backlog in `.junie/next-ai-codereview.md` so
the next context can continue. The loop ends when no violations remain.

## 0. Scope and decisions (binding for this plan)

- **One issue per context, done completely.** A context fixes a single issue
  (or one tight cluster of the *same* defect) end to end — find, understand,
  fix, test, green gate, document — rather than half-fixing many. This keeps
  each diff reviewable (§14) and fits a single AI context window.
- **Quality bar is §2.6 / §23.** A fix is acceptable only if the agent
  understood the code it changed, its callers, and its callees, and would
  defend every line in review without embarrassment. A fix that compiles and
  passes tests but is not understood is **not** acceptable — escalate (§15.7)
  instead of guessing.
- **Correctness over volume.** Never weaken a test, silence a lint, or paper
  over a defect to "make it pass" (§2.1, §2.5, §15.3, §15.9). A misread issue
  is escalated, not bluffed.
- **No behaviour change without its test.** Every fix that changes behaviour
  lands with a regression test that fails before and passes after (§7, §2.18,
  §23.4); a fuzzer/proptest find also enters the corpus (§19.6).
- **The backlog lives in `.junie/next-ai-codereview.md`.** That file is both
  the re-runnable prompt for the next context and the running issue queue. It
  is the only handoff artefact; this plan (`CODEVERIFY.md`) is the stable
  methodology and does not accrue per-issue history (§13).
- **This is not a comment refactor.** Comment "waffle" reduction is owned by
  `plans/WAFFLE.md`; do not conflate the two. CODEVERIFY targets *defects and
  bad code*, not comment style.
- **The gate lints bare-metal code, so the lint-catchable classes need no
  manual hunt.** `cargo xtask clippy` runs `-D warnings` for the host *and*
  once per Tier-1 target, so a `freestanding` body is linted rather than only
  compiled (`docs/src/contributing.md`). Scan for what a lint cannot see —
  wrong invariants, ambient authority, duplication, dead code — not for what
  the gate already rejects.

## 1. What counts as a violation (the hunt checklist)

Applied while reading code. Any one of these is an issue to record and fix.
This list is the charter restated as a review checklist — it is not exhaustive;
"generally bad code a senior engineer would reject" (§2.6) is in scope even if
it is not enumerated here.

### Safety / panics (§2.9, §2.1)

- `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `assert!`/`debug_assert!`
  used as control flow, or arithmetic that can panic (indexing, `+`/`-`/`*`
  overflow, slicing, fallible `from`/`into`) on a **production** path — anything
  not a test and not a documented `// SAFETY-INVARIANT:` boot-time invariant.
- `.unwrap()` / `.expect()` outside `#[cfg(test)]` / `tests/`.
- Allocation or `Result` failure handled by panic rather than as a value (§4).

### Hacks and shortcuts (§2.1, §2.17, §15.10)

- `// TODO`, `// FIXME`, `// HACK`, `// works for now`, sleep/retry-until-it-works
  loops, busy-spins, commented-out code (also §2.14).
- `#[allow(...)]` without a justification comment tying it to a real invariant.
- A limit bumped (buffer/stack/quota enlarged) in place of a structural fix
  (§2.17).

### Security (§5.4, §2.7, §23.1)

- An entry point (syscall / IPC method / driver entry / parser / FS op) that
  checks capability *after* touching state, trusts a caller-supplied identity,
  skips validating any input field, fails *open*, or grants authority it was
  not delegated (ambient authority, §4).
- `unsafe` without an accurate `// SAFETY:` block, or whose invariant does not
  actually hold for all inputs, or that has no test/model exercising it (§2.10).
- A secret/key/capability token not zeroed on free, or reaching swap/logs/fd 3
  (§4, §19.4, §20).
- A parser of untrusted input not behind the §19.5 sandbox / without a fuzz
  harness (§19.6).

### Duplication, dead code, bloat, creep (§2.2, §2.3, §2.4, §2.14)

- Copy-pasted logic that should be a `lib/*` crate or shared function (but
  **not** the sibling-implementation carve-out, §2.2).
- Dead code: unused items, `_old`/`legacy_`/`unused_` names,
  `#[allow(dead_code)]`, orphaned files/fixtures/doc pages.
- Speculative public surface added "for later" with no present caller (§2.4),
  one-line "convenience" wrappers (§2.3, §15.5).

### Correctness, concurrency, layering, scaling (§23.2, §17.4, §24)

- Data races / torn reads / "works on one core" assumptions; lock-ordering that
  can deadlock; shared state not behind `lib/sync` (§4).
- Error paths that leak (memory, locks, capabilities, handles), or skip cleanup.
- `cfg(target_arch …)` / `cfg(target_pointer_width …)` outside the §17.2
  allow-list; a non-GUI crate depending on `userland/gui/*` (§17.3); any §17.4
  layering reversal.
- Absolute time stored as 32-bit / `usize` / `time_t` in an ABI or persisted
  format (§21).
- A hand-picked `const` capacity ceiling that a larger machine outgrows or a
  smaller one wastes, where it should be derived/grown (§24.1) — *not* a
  fixed security/format bound (§24.4).

### Performance (§2.16, §23.2)

- Gratuitous waste on a hot path (syscall/IPC dispatch, capability check,
  scheduler, context switch, allocator, compositor, FS/network data paths):
  needless allocation/copy in a loop, a lock held too long, O(n²) where O(n)
  is as clear. Back any regression claim with measurement (§2.16); never reach
  for `unsafe` for speculative speed.

### Tests and docs (§7, §2.8, §13)

- A public item without rustdoc; a `docs/src/` page out of sync with code;
  a stale symbol reference.
- A weakened/`#[ignore]`d/deleted test, a flaky test, or a fixed bug with no
  regression test. A flaky test is a defect regardless of how it is perceived:
  a failure excused as "machine load", CPU contention, an oversubscribed host,
  a slow runner, or "it passes when run on its own", or "resolved" by re-running
  it in isolation, is the exact get-out the charter forbids (§7) — it is
  diagnosed to root cause and fixed structurally, with a regression test.

## 2. Method for one issue (binding per context)

1. **Pick the issue.** If `.junie/next-ai-codereview.md` lists an unfixed,
   ready issue, take the top one. Otherwise scan an unfinished area (§3) with
   `search_project` for the patterns in §1 and stop at the **first** genuine
   violation, then record it.
2. **Understand it fully before touching anything.** Read the offending code,
   *every* caller of it, and *everything it calls*, until the intended
   behaviour and invariants are clear. If understanding is not reachable from
   the repository, **stop and ask** (§15.7) — do not guess (§2.1, §15.7).
   Confirm it is a real defect, not an intended carve-out (e.g. the §2.2
   sibling implementations, a justified `#[allow]`, a test-only `unwrap`).
3. **Design the proper fix.** Prefer making illegal states unrepresentable
   (§2.11) over adding a runtime guard; prefer a structural control over a
   limit bump (§2.17); never introduce a compat shim or dead code (§2.13,
   §2.14). If the clean fix is genuinely larger than one context can do well,
   record it as escalated (§15.7) and pick a smaller issue instead — do **not**
   land a partial or low-quality fix.
4. **Implement** the minimal correct change, updating every caller in the same
   change (§2.13). Update rustdoc and the relevant `docs/src/` page in the same
   change (§2.8, §13).
5. **Add the regression test** that fails before and passes after (§7, §2.18).
   For a fuzzer/proptest find, add the input to the corpus (§19.6).
6. **Run the whole-project gate green** (§4 below) and quote the output.
7. **Self-review under §23** adversarially; state the verdict.
8. **Update `.junie/next-ai-codereview.md`**: mark the fixed issue done (delete
   its entry — git is the history, §13), and append any *new* violations
   noticed in passing as backlog entries (§2.18: notice it → record it, never
   stay silent). Leave the file ready for the next context.

## 3. Work breakdown (scan areas / stages)

The hunt is staged by workspace area so coverage is trackable and a context
knows where to look next. Areas are scanned roughly highest-risk first; within
an area, fix issues one per context until the area is clean, then move on. An
area is `done` only when a full §1 scan of it surfaces no remaining violation.

Status keys: `planned` / `in progress` / `done` / `blocked`.

### Stage V1 — `kernel/*` and `lib/caps`, `lib/crypto`, `lib/sync` — Status: planned

The capability-critical and `unsafe`-dense core (allocator, paging, arch ports,
context switch, scheduler, syscall/IPC dispatch, `lib/caps`, `lib/crypto`,
`lib/sync`). Highest blast radius; §23.1 security review and §2.10 `unsafe`
review apply most strictly here. Coverage floor ≥ 95% for the §7-listed crates.

### Stage V2 — remaining `lib/*` — Status: planned

All other shared crates. Watch for duplication that belongs behind one crate
(§2.2), missing rustdoc (§6), and untrusted-input parsers without sandboxing /
fuzzing (`lib/vt`, `lib/fdt`, `lib/svg`, `lib/font`, `lib/compress`).

### Stage V3 — `drivers/*` — Status: planned

All driver crates. Watch for ambient authority (§4/§18.5), entry points that
validate input incompletely (§5.4), and `cfg(target_arch …)` that should live
behind the Arch HAL (§17.2).

### Stage V4 — `userland/*` — Status: planned

All userland crates (`system`, `session`, `shell`, `gui`, `apps`, `net`). The
known seed issue (the shell parser `unreachable!`) lives here; see
`.junie/next-ai-codereview.md`.

### Stage V5 — `tools/*` and `tests/*` — Status: planned

`tools/xtask`, `tools/qemu`, `tools/cc`, `tools/mkimage`, `tools/ci`;
`tests/fuzzseed` and `tests/integration/*`. Host-only build glue: still held to
§2.6, but the §2.9 production-path panic rule is read against these being
build/test tooling, not shipped kernel/userland paths.

## 4. Definition of done (per issue and overall)

An issue fix is done only when, run from the repository root over the **entire**
workspace (§7, §2.15, §15.6, never a `-p` subset):

- `cargo fmt --all` (and `cargo fmt --all --check`),
- the full `cargo xtask ci` pipeline (clippy `-D warnings`, the test matrix,
  `docs-check`, `deny`, cfg/deps/abi checks, the per-PR fuzz/proptest gates,
  model-check, spec-review, crypto constant-time),
- `cargo xtask fuzz --secs 5`,
- anything else `.github/workflows/ci.yml` exercises (e.g. `tools/ci/soak.sh`,
  which on a developer machine (that's us) runs for a **maximum of 20 seconds**
  — `tools/ci/soak.sh both --secs 20` — never the unbounded 24 h soak, which is
  the CI/soak host's job),

all pass; the §23 self-review confirms the fix is correct, understood, tested,
and leaves no dead code or compat shim; and `.junie/next-ai-codereview.md` has
been updated. The actual gate output is quoted in the completion report.

The overall task is done when V1–V5 are all `done` — a clean §1 scan of the
whole workspace surfaces no violation — and `.junie/next-ai-codereview.md`
records an empty backlog.


---

## Open sweep: the raw-syscall-result → `Errno` conversion

`lib/rt::errno_from_raw` is now the one public, tested conversion from a raw
`i64` syscall result to an `Errno`, and the four sites that motivated it (the
private copy in `lib/rt`, and the ones in `drivers/storage/volmgr`,
`drivers/storage/raid_member`, `drivers/storage/raid`) use it.

Roughly nineteen further private re-implementations remain, and they do **not**
agree: three different fallbacks are in use for an unrecognised value
(`NotImplemented`, `NotFound`, and `DeviceFault`), and none guards `i64::MIN`,
whose negation overflows. A caller therefore gets a different error class for
the same kernel refusal depending on which crate it happens to be in — the
divergence duplication invites. Known sites: `drivers/display/framebuffer`,
`drivers/network/virtio_net_driver`, `lib/blkclient`, `lib/display`,
`lib/font`, `lib/sandbox`, `tests/integration/blkio_fault_program`,
`userland/apps/{files,terminal,viewer,widgets}`, `userland/net/netstack`,
`userland/session/login`, `userland/shell/elsh`, and
`userland/system/{devmgr,init,journald,seatmgr,sysinfod}`.

The sweep is its own change: it spans a dozen crates, and each site's current
fallback must be read before it is replaced, because adopting the shared
`NotImplemented` where a caller today branches on `NotFound` would change that
caller's behaviour rather than merely deduplicate it.

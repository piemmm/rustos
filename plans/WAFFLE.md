# WAFFLE.md — Reduce comment waffle across all `.rs` files

This is a staged plan to bring every `.rs` file in the workspace to the
Linux-kernel-style commenting discipline the charter already mandates. It is
**binding under `AGENTS.md`**; read `AGENTS.md` and `PLAN.md` first. Every rule
in both applies here without exception.

The work is a pure comment refactor: it changes *comments only*, never
behaviour, never public/private signatures, never code. Done correctly it is a
§2.11 conformance improvement that raises signal-to-noise for both human and AI
readers — it must never delete information a reader (or a build check) relies
on.

## 0. Scope and decisions (binding for this plan)

- **The charter already requires this.** `AGENTS.md` §2.11: comments are for
  *why* (rationale, invariants, references, `// SAFETY:`), **never** for
  restating *what* the code already says; if a comment is the only thing making
  a line understandable, the **code** is rewritten (better name, smaller
  function, illegal-states-unrepresentable types), the comment is not kept.
  §2.3 (no bloat) and §13 ("no aimless waffle") express the same value.
- **A file's existing prose is not a precedent.** §2.11 requires terse
  comments — the required *why* and no more — and binds regardless of how much
  prose a file already carries. Waffle found here is the backlog this plan
  clears, never a style to match: no pass, and no unrelated change, may add to
  it (§15.17).
- **Comments only.** This plan touches comment text. It does **not** rename
  items, reshape functions, change control flow, or alter any runtime
  behaviour. A "the comment is the only thing making this line clear" case
  (§2.11) that *would* require a code rewrite is **out of scope here**: record
  it and raise it (§15.7) as a separate refactor — do not smuggle a behavioural
  change into a waffle-reduction diff.
- **rustdoc is not waffle and is never stripped.** `///` / `//!` documentation
  on public items is part of the code (§2.8, §13) and is mandatory; removing it
  fails `cargo xtask docs-check` (`RUSTDOCFLAGS="-D warnings"`) and the stale-
  symbol check. rustdoc is only ever *improved for accuracy/brevity*, never
  deleted.
- **`docs/src/` and `plans/*.md` are out of scope.** This plan is `.rs`
  comments. (The plan-file waffle rules in §13, and `docs/` prose, are handled
  elsewhere.)
- **Fail safe.** When unsure whether a comment carries load-bearing *why*,
  **keep it**. Over-trimming that removes rationale is the defect this plan
  must avoid; the bar is "delete only what is provably redundant".
- **No stubs, no half-done areas** (§15.1): each stage ends green on the
  whole-project validation gate (§7) before the next begins.

## 1. Classification (binding: what is deleted vs. kept)

Applied per comment. The test: *does this comment tell the reader something the
code itself does not?* If no → delete. If yes → keep.

### Delete (waffle)

- Comments that restate the code: `// increment i`, `// set x to 5`,
  `// loop over entries`, `// return the result`.
- Narration of obvious control flow or obvious structure.
- Commented-out code — forbidden by §2.14 (delete, never comment out).
- `// TODO`, `// FIXME later`, `// works for now`, `// HACK` and similar —
  forbidden by §2.1 / §15.10. Do **not** simply delete the marker and leave the
  defect: if it flags real missing work, fix it in scope or raise it (§15.7,
  §2.18); a waffle pass must not bury a known defect.
- Redundant section-banner / decorative comments that add no information.
- Essay-length prose: multi-paragraph exposition, tutorials, narration of the
  change that produced the code, and body comments restating the rustdoc above
  them. Keep the load-bearing *why* in a line or two; delete the rest.
- **Charter section citations** — `AGENTS.md §5.4`, `§2.9`, `sec.5.4`,
  "Section 5.4", or a bare trailing `(§5.4)`. `AGENTS.md` §2.11 / §15.17 forbid
  them outright: the rule lives in the charter, so citing its number restates
  *what* the rule is and never *why* the code does what it does. Keep the
  reason in plain prose and drop the number ("fail closed", "zeroed on drop");
  where the charter itself is the subject, name it in prose ("the charter
  forbids this duplication"). Two things are **not** comments and stay: the
  provenance a *generator* stamps onto a generated artefact (the `include/`
  C-header banners), and a runtime diagnostic that tells a developer which rule
  they broke (a CI-check error message, an assertion message) — that is program
  output.

### Keep — always (these are *why*, not *what*)

- **`// SAFETY:` blocks** above every `unsafe` — mandatory (§2.10). Never strip;
  only correct if inaccurate.
- **`// SAFETY-INVARIANT:`** markers on boot-time invariants (§2.9).
- **`// SPEC-DRAFT:`** markers (§19.7) — removing one falsely promotes a draft;
  `cargo xtask spec-review` guards this.
- Rationale: *why* this algorithm/ordering/lock discipline/memory layout; the
  reasoning behind a security (§2.7) or performance (§2.16) choice; non-obvious
  invariants.
- References the reader cannot derive from the code: an external spec, erratum,
  hardware manual, algorithm paper, RFC, or ticket, and pointers to another
  file in this tree (`plans/PI.md`, a `docs/.../*-spec.md`, a sibling module).
  A charter section number is **not** such a reference and is deleted (see
  above).
- All **rustdoc** (`///`, `//!`) on public items (§2.8).

## 2. Method (per file / per crate)

1. Read each comment against §1. Delete pure waffle; keep every *why* and all
   rustdoc and `// SAFETY:` / `// SAFETY-INVARIANT:` / `// SPEC-DRAFT:` markers.
2. If a comment is load-bearing only because the *code* is unclear, **do not**
   rewrite the code in this pass — note it for a separate §15.7 refactor.
3. If a `// TODO`/`// FIXME` flags real work, fix it in scope or escalate
   (§2.18) — never silently drop it.
4. Keep diffs comment-only and reviewable; one crate (or a small set) per
   logical change (§14), so a reviewer can confirm no code or rustdoc moved.
5. Self-review under §23 — especially §23.3 (nothing live deleted) and §23.4
   (docs current, gate green).

## 3. Work breakdown (stages)

Each stage is one reviewable chunk: a workspace area whose crates are trimmed
together, then the **whole-project** gate (§7) is run green before the next
stage starts (never a `-p` subset, §15.6). The `docs-check` step is the real
guard that no rustdoc or stale `docs/src/` symbol reference was lost.

Status keys: `planned` / `in progress` / `done` / `blocked`.

### Stage W1 — `lib/*` — Status: planned

All shared crates in `lib/` (e.g. `abi`, `abi-sys`, `abi-trap`,
`caps`, `collections`, `compress`, `crt0`, `crypto`, `curses`, `cursor`,
`fdt`, `font`, `geometry`, `icon`, `input`, `kalloc`, `log`, `procinfo`, `raster`,
`rng`, `rt`, `svg`, `sync`, `termcap`, `theme`, `util`, `virtio`, `vt`).
These carry the densest rustdoc surface (public ABI/library items, §6) and the
`// SAFETY:` cores (`kalloc`, `sync`, `raster`, `crypto`) — strict §1 keep
rules apply.

### Stage W2 — `kernel/*` — Status: planned

All kernel crates (`core`, `mem`, `sched/*`, `ipc`, `irq`, `sec`, `syscall`,
`virtio`, `arch/api`, `arch/<target>`, `tairix-kernel`). Heaviest `// SAFETY:`
and `// SAFETY-INVARIANT:` density (allocator, paging, arch ports, context
switch); these are kept verbatim unless inaccurate. The capability-critical
crates (`kernel/sec`, `kernel/mem`, `kernel/ipc`) also carry `// SPEC-DRAFT:`
markers (§19.7) — keep.

### Stage W3 — `drivers/*` — Status: planned

All driver crates across `display`, `filesystem`, `input`, `bus`, `storage`,
`network`. Keep the rationale comments that cite hardware errata / device-spec
behaviour (§8 `README.md`-adjacent *why*).

### Stage W4 — `userland/*` — Status: planned

All userland crates across `system`, `session`, `shell`, `gui`, `apps`, `net`.

### Stage W5 — `tools/*` and `tests/*` — Status: planned

`tools/xtask`, `tools/qemu`, `tools/cc`; `tests/fuzzseed` and the
`tests/integration/*` crates. Test-helper comments that explain *why* a fixture
is shaped a certain way are *why* and are kept.

## 4. Definition of done (per stage and overall)

A stage is done only when, run from the repository root over the **entire**
workspace (§7, §2.15, §15.6):

- `cargo fmt --all` (and `cargo fmt --all --check`),
- the full `cargo xtask ci` pipeline (clippy `-D warnings`, the test matrix,
  `docs-check`, `deny`, cfg/deps/abi checks, the per-PR fuzz/proptest gates,
  model-check, spec-review),
- `cargo xtask fuzz --secs 5`,

all pass, and the §23 self-review confirms the diff is comment-only with no
rustdoc, `// SAFETY:`, `// SAFETY-INVARIANT:`, or `// SPEC-DRAFT:` content lost
and no dead code left behind (§2.14). `docs-check` passing is the proof that no
public-item documentation was removed by mistake.

The overall task is done when W1–W5 are all `done` and the whole-project gate is
green.

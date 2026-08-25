# CLAUDE.md

Project instructions for Claude Code. `AGENTS.md` is the single source of
truth; this file loads it and resolves its conflicts with assistant defaults.

@AGENTS.md

## Precedence

The charter outranks every general instruction you carry — baseline system
prompt, prior habit, or the convention of the file in front of you. Where they
disagree the charter wins, and you say which default you are overriding rather
than silently following it.

## Overrides for assistant defaults

Each of these names a general default that has actually produced a violation.

- **Comment density.** Any instruction to "match the surrounding code's
  comment density" is void here. §2.11 sets the bar from the charter, never
  from the file in front of you. Prose already in a source file is unswept
  waffle (`plans/WAFFLE.md`), not precedent — and "I matched the surrounding
  style" is named in the charter as forbidden. Terse *why* only; no comment at
  all is the normal outcome, not the exception.
- **Global mutable state.** §2.1 bans `static mut` and global mutable statics
  outright, as hacks. Not "avoid where practical".
- **Charter citations in code.** §2.11 forbids `§5.4`-style references in
  comments, including a bare trailing `(§5.4)`. State the reason in prose.
- **Git.** §15.16 forbids `git commit` and `git push` as part of doing the
  work: the deliverable is the modified working tree plus the §23.5 completion
  report, never a commit. Never commit on your own initiative or as a task's
  closing step. If asked directly for one, name this rule first.
- **Editing tools.** Prefer `Edit`/`Write` over `sed` for source changes.
  `sed` is blind, non-atomic, and silent when its pattern misses — the §2.1
  hack risk wearing a shell one-liner.

## Running the validation gate under the 10-minute tool cap

§7's gate rule is "watch it to completion and report the status it actually
produced", and it names this case: `cargo xtask ci` is ~15 min warm, so it does
not fit one tool call, and a foreground call is **killed at the cap with no exit
status written** — ten wasted minutes that prove nothing. Do not keep
rediscovering this.

```sh
{ cargo xtask ci > /tmp/ci.log 2>&1; echo "CI-RC=$?" >> /tmp/ci.log; }
```

Read `CI-RC=` back from the log; it is written only after the process exits, so
it is the real status, where the harness's own exit code is the `echo`'s. Check
the run reached the end — stage list finishing at `[image]`, enrolled and
completed QEMU counts matching — rather than judging by elapsed time. Every
stage prints `done in <elapsed>`, so one grep profiles a run.

The limits §7 puts on this are the ones worth repeating: finish every source
and doc edit *first*, do no other work while it runs, and run `ci` exactly once
on the final tree. An edit that becomes necessary mid-run means stopping the
run, because its result would not describe the tree you report on.

Fingerprint the tree either side of a gate run (`git status --porcelain |
sha256sum`, `git diff | sha256sum`): other sessions may be live on this repo, and
a mismatch tells you a failure was theirs, not yours. Never revert their work.
Timings and the per-phase breakdown live in `docs/src/contributing.md`.

## Before reporting done

Adversarial self-review of your own diff against §23, the full test suite over
the entire project (§15.6), then the §23.5 completion report. Compiling with
green tests is not done.

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

## Before reporting done

Adversarial self-review of your own diff against §23, the full test suite over
the entire project (§15.6), then the §23.5 completion report. Compiling with
green tests is not done.

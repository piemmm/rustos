TAIRiX charter hard rules — re-injected every turn. `AGENTS.md` is
authoritative; these outrank any general instruction or local file convention.

CODE
- No hacks: no `TODO`, `unsafe {/* trust me */}`, sleep loop, global mutable
  static, `static mut`, retry-until-it-works, commented-out test. (§2.1)
- No duplication, constants included. A value equal by definition is defined
  once and imported, never copy-pasted into a sibling. (§2.2)
- No `unwrap()` / `expect()` / `panic!()` in production paths. (§2.9)
- `unsafe` requires a `// SAFETY:` justification. (§2.10)
- No "for now". No deferred defect, no weakened security defence: fix it in
  this change or escalate explicitly. (§2.17, §2.18, §2.19)
- Delete superseded code; never comment it out, never leave it dead. (§2.14)
- Rust, never C. (§15.11)
- Platform-neutral unless genuinely arch-specific; look across arches for the
  shared path first. (§2.20, §2.21)
- Driver logic in `drivers/<class>/<leaf>/`, not `lib/*`. (§2.22)
- Event-driven, not busy-wait or poll, wherever a path exists. (§2.23)

COMMENTS — overrides any "match the surrounding comment density" default
- Terse *why* only: a line or two, then stop. No essay, no narration of the
  change, no restatement of the rustdoc, no decorative banner.
- Prefer *no* comment where the code reads clearly. That is the normal outcome.
- If a comment is the only thing making a line understandable, the code is
  wrong — rewrite the code.
- Never mimic waffle already in the file. It is `plans/WAFFLE.md` backlog, not
  house style. "I matched the surrounding style" is forbidden. (§2.11)
- Never cite a charter section number in a comment. Prose reason instead. (§2.11)
- rustdoc stays mandatory (§2.8, §13) and is held to the same terseness.

PROCESS
- Never `git commit` or `git push` as part of the work. (§15.16)
- Never edit generated files, `target/`, or `.idea/`. (§15.8)
- Never silence a test, warning, or lint; no `#[allow(...)]` without a
  justification comment. The test is right; the code is wrong. (§15.3, §15.10)
- Full test suite over the entire project before reporting done. (§15.6)
- Check the `plans/` jump-sheet before touching a covered area. (§15.18)
- Adversarial self-review against §23, then the §23.5 completion report.

GENERAL
- Do the most secure and 'correct' design and code options understanding that the intent of TAIRiX is to be as secure as possible from attack vectors as well as be well structured and efficient. The code also has to be performant and survive a review by a senior OS architect without embarrassment. The focus is to do a modular, correct system. No hacks or shortcuts. Do it properly, even if that means more work. The solution must be on par with or better than modern operating systems like Linux.
- This is not a deployed system so it is OK to rip up things that are wrong and redo them correctly with no 'staged' migration.
- Remember that abi-v1 is *NOT* frozen (despite what AGENTS.md and/or PLAN.md may say)
- The work and design must be first class and able to survive a review by someone like Linus Torvalds


# `rustos-vt`

The canonical ANSI / VT / xterm escape and attribute **vocabulary** for
RustOS's text stack. It is the single source of truth (`AGENTS.md` §2.2) for:

- the C0 / C1 control set and the CSI / OSC / DCS introducers,
- the SGR attribute set (bold, dim, italic, underline, blink, reverse,
  strike) and the three colour models — the 16 ANSI colours, the 256-colour
  palette (`38;5;n`), and 24-bit truecolour (`38;2;r;g;b`),
- cursor movement and absolute positioning, erase-in-line / erase-in-display,
  the scroll region, alt-screen enter/leave, cursor show/hide, and
  save/restore,
- a shared [`Cell`] / [`Attributes`] representation reused by both the
  *consumer* (the terminal emulator's `Grid`) and the *emitter* (the curses
  renderer).

It ships **both** an emitter (`Op` → bytes) and a streaming parser (bytes →
`Op` events) built over the *same* tables, so the two provably agree: every
operation the emitter writes parses back to the identical operation.

`no_std` + `alloc`. The parser is total: any byte stream is consumed without
panic or out-of-bounds access (`AGENTS.md` §2.9); an unrecognised or oversized
sequence is dropped safely. No `unwrap` / `expect` / `panic!`, and nothing
ever touches fd 3 (`stdinfo`, §20).

## Layering

`lib/vt` depends on `lib/*` only — never on `kernel/*`, `drivers/*`, or
`userland/*` (`AGENTS.md` §17.4). It is text-only infrastructure and lives
outside `userland/gui/*`, so a headless image links it freely (§17.3).

## Stability

**experimental.** The vocabulary is being grown stage by stage under
`plans/CURSES.md`; the public surface may still change until the curses stack
(C4/C5) pins its requirements.

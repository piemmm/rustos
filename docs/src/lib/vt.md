# `rustos-vt`

The canonical ANSI / VT / xterm escape and attribute **vocabulary** for
RustOS's text stack, and the first stage of the `plans/CURSES.md` build plan.
Every real terminal, multiplexer, and remote Linux host speaks this
vocabulary, so RustOS speaks it too — from **one** definition (`AGENTS.md`
§2.2). The terminal emulator (the *consumer*) and the future curses renderer
(the *emitter*) share this crate rather than each carrying their own
escape-sequence tables.

Stability tier: **experimental** (the surface grows stage by stage under
`plans/CURSES.md`).

## What it defines

| Module | Contents |
|--------|----------|
| `control` | The C0/C1 control bytes, the CSI/OSC/DCS introducers, the final bytes, and the DEC private-mode numbers, as typed constants — plus the read-line-discipline erase vocabulary (`is_line_erase`, recognising Backspace `BS` or Delete `DEL`, and the `ERASE_ECHO` `BS SP BS` rub-out) the kernel console echo and a reader's line buffer share so they never disagree on which byte erases (`AGENTS.md` §2.2). |
| `color` | `BasicColor` (the 16 ANSI colours, palette index `0..=15`) and `Color` (the default, the 16 basic colours, the 256-colour palette, and 24-bit truecolour). |
| `attr` | `Sgr` (one Select Graphic Rendition operation), the single `write_params`/`decode_params` SGR table both the emitter and parser use, and `Attributes` (the folded rendition state of a cell). |
| `cell` | `Cell` — a glyph plus its `Attributes`, the shared screen-cell representation. |
| `line` | The read line discipline's **buffer** half: `LineEditor` / `LineFeed`, with the `EraseSeq` recogniser both halves share — CR or LF completes the line, an erase (the single-byte Backspace/Delete controls, or the Delete key's `CSI 3 ~` sequence held across split reads) rubs out the last kept byte (zeroed on removal), and a line that outgrows the caller's buffer fails closed `TooLong`. The one editor every console reader runs (the boot passphrase prompt, login's prompt reads, the shell REPL), matching the kernel console's echo half byte for byte. |
| `secret` | The secret-entry activity indicator: `SecretIndicator` / `SecretInput` — the `[input active...]` marker every echo-suppressed (password) prompt shows after the first typed character, its dots cycling `.` → `..` → `...` on a one-second cadence. The animation is bounded: it runs for at least three seconds (`SECRET_ANIMATE_NS`) after the most recent keystroke and then freezes, and a later keystroke restarts it. On Enter the marker is replaced in place with `[input complete]`; erasing back to empty (or aborting) removes an in-progress marker, while a completed marker is left on screen. A pure, clock-free state machine emitting plain text plus backspace/space rub-outs; the kernel console hosts and renders it with one-shot deadlines (tickless — nothing is armed until the first typed character, and the animation stops arming wake-ups once it freezes). |
| `op` | `Op` — the operation vocabulary (print, C0 controls, cursor movement/positioning, erase, scroll region, alt-screen, cursor visibility, save/restore, SGR, window title) and `EraseMode`. |
| `emit` | `encode` / `encode_into` / `encode_all` — render an `Op` to bytes. |
| `parse` | `Parser` — the streaming byte → `Op` state machine. |

## Emitter and parser agree by construction

Each `Op` and `Sgr` has exactly one canonical byte encoding, so parsing the
emitter's output reproduces the original operation. This is the §2.2 "one
vocabulary" guarantee made testable:

```rust
use rustos_vt::{encode, BasicColor, Color, Op, Parser, Sgr};

fn main() {
    let op = Op::Sgr(Sgr::Foreground(Color::Basic(BasicColor::Green)));
    let bytes = encode(&op);

    let mut parser = Parser::new();
    let mut seen = Vec::new();
    parser.feed(&bytes, |parsed| seen.push(parsed));
    assert_eq!(seen, vec![op]);
}
```

A single SGR sequence can carry several attributes, which the parser unfolds
into one `Op::Sgr` per attribute, in order — so `CSI 1;31;4m` decodes to bold,
then red foreground, then underline.

## Fail-closed parsing of untrusted input

A terminal consumes bytes it did not produce — local shell output and, in the
remote stages of `plans/CURSES.md`, a foreign host's output (`AGENTS.md`
§19.5). The `Parser` is therefore total:

- numeric parameters saturate at `PARAM_MAX`, so a long digit run cannot
  overflow;
- the parameter and string buffers are bounded (`MAX_PARAMS`, `MAX_STRING`);
- UTF-8 is decoded with overlong and stray-continuation rejection;
- an unrecognised, oversized, or malformed sequence is consumed and dropped
  rather than corrupting screen state — never a panic (`AGENTS.md` §2.9).

There is no `unwrap` / `expect` / `panic!` anywhere in the crate, and nothing
writes to fd 3 (`stdinfo` is reserved, §20).

## Layering and testing

`lib/vt` depends on `lib/*` only — never on `kernel/*`, `drivers/*`, or
`userland/*` (`AGENTS.md` §17.4) — and is text-only infrastructure outside
`userland/gui/*`, so a headless image links it freely (§17.3).

Tests (`AGENTS.md` §7) live next to the code (`src/tests.rs`: round-trip
identity and fail-closed robustness) plus two integration harnesses: a
`proptest` (`tests/proptest_bytes.rs`) for the no-panic / chunk-invariant /
emit-parse-identity properties, and the §19.6 deterministic fuzz harness
(`tests/fuzz_vt.rs`, registered as `fuzz_vt` in `cargo xtask fuzz`).

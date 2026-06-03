# `rustos-termcap`

The first-party, **compiled-in** terminal capability database for RustOS's
text stack, and the third stage of the `plans/CURSES.md` build plan. It answers
one question — "given this `TERM`, what can the terminal do?" — without the
terminfo / termcap files a POSIX system reads from `/usr/share` or `/etc`.
RustOS has no such paths (`AGENTS.md` §16.1), so the database is compiled in: a
closed, versioned `TermType` set (`AGENTS.md` §2.4) and a `const`
`Capabilities` record per terminal.

Stability tier: **experimental** (the surface grows stage by stage under
`plans/CURSES.md`).

## The recognised terminals

`TermType` is the frozen set of `TERM` values RustOS recognises:

| `TermType` | `TERM` | Colour | Notes |
|------------|--------|--------|-------|
| `Xterm` | `xterm` | 16 ANSI | baseline xterm feature set |
| `XtermColor` | `xterm-color` | 16 ANSI | colour-advertising xterm |
| `Xterm16Color` | `xterm-16color` | 16 ANSI | full 16-colour palette |
| `Xterm256Color` | `xterm-256color` | 256 | mouse + bracketed paste |
| `Alacritty` | `alacritty` | truecolour | full-motion mouse |
| `XtermKitty` | `xterm-kitty` | truecolour | full-motion mouse |
| `Dumb` | `dumb` | none | the fail-closed baseline |
| `Vt100` | `vt100` | none | minimal cursor-addressable |
| `Vt220` | `vt220` | none | VT100 + editing/function keys |

## What a record describes

`Capabilities` records, for one terminal: the colour depth (`ColorDepth` — one
of none, the 16 ANSI colours, the 256-colour palette, or 24-bit truecolour),
whether it can address the cursor, erase, set a scroll region, switch to an
alternate screen, show and hide the cursor, and set its title; its
mouse-reporting support (`MouseSupport`); whether it supports bracketed paste;
and the keys it sends (`KeyInput`, including the arrow keys as `ArrowKeys`).

## One vocabulary

Every escape sequence a record references is a `rustos_vt::Op` — the one shared
vocabulary (`AGENTS.md` §2.2). The crate defines no second escape-sequence
table: output capabilities are the `Op`s the terminal accepts, the renderable
colours are `rustos_vt::Color` models, and an arrow key is the `Op` its bytes
parse back to through `rustos_vt::Parser`. `Capabilities::referenced_ops`
returns that exact set, and the `no_record_emits_a_sequence_absent_from_vt`
test round-trips each one through `lib/vt`, proving the database invents
nothing.

Mouse reporting, bracketed paste, and the function / editing / keypad keys are
recorded as capability *facts*, not byte sequences. Their enabling and report
sequences are not yet in `lib/vt`'s vocabulary; they are added there — not
duplicated here — when the curses input decoder (`plans/CURSES.md` §C4) needs
to emit and parse them (`AGENTS.md` §2.2).

## Fail closed

`from_term` parses an untrusted `TERM` value:

```rust
use rustos_termcap::{from_term, ColorDepth, TermType};

fn main() {
    assert_eq!(from_term("xterm-256color"), TermType::Xterm256Color);

    // An unknown or empty value degrades to the safe baseline (§2.9, §5.4),
    // never a richer terminal and never a file read (§16.1).
    assert_eq!(from_term("no-such-terminal"), TermType::Dumb);
    assert_eq!(from_term(""), TermType::Dumb);

    let caps = TermType::Xterm256Color.capabilities();
    assert_eq!(caps.color, ColorDepth::Indexed256);
    assert!(caps.alt_screen);
}
```

## Layering and testing

`lib/termcap` depends on `rustos-vt` and `lib/*` only — never on `kernel/*`,
`drivers/*`, or `userland/*` (`AGENTS.md` §17.4) — and is text-only
infrastructure outside `userland/gui/*`, so a headless image links it freely
(§17.3). It is `no_std` + `alloc`, never panics (§2.9), and never touches fd 3
(`stdinfo`, §20).

Tests (`AGENTS.md` §7) live next to the code (`src/tests.rs`): one capability
test per `TermType`, an "unknown / empty `TERM` falls back safely" test, a
`term_name` ↔ `from_term` round-trip, the `ColorDepth::supports` depth checks,
and the `no_record_emits_a_sequence_absent_from_vt` guarantee.

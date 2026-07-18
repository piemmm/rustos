# `tairix-termcap`

The first-party, **compiled-in** terminal capability database for TAIRiX's
text stack, and the third stage of the `plans/CURSES.md` build plan. It maps a
`TERM` value to a capability record describing what that terminal can do.

There is no terminfo / termcap file: TAIRiX has no `/etc`, `/usr`, or `/proc`
(`AGENTS.md` §16.1), so the database is a closed, versioned set of
`TermType`s and a `const` `Capabilities` record per terminal — adding a
terminal is data plus a capability test, not new control flow.

## What it defines

- `TermType` — the recognised `TERM` values: `xterm`, `xterm-color`,
  `xterm-16color`, `xterm-256color`, `alacritty`, `xterm-kitty`, `dumb`,
  `vt100`, `vt220`.
- `Capabilities` — per terminal: colour depth (`ColorDepth`), cursor
  addressing, erase, scroll region, alt-screen, cursor visibility, title,
  mouse reporting (`MouseSupport`), bracketed paste, and the keys it sends
  (`KeyInput` / `ArrowKeys`).
- `from_term(&str) -> TermType` — parses an untrusted `TERM`, **failing
  closed** to `dumb` on an unknown or empty value (`AGENTS.md` §2.9, §5.4)
  and never reading a file derived from `TERM`.
- `ColorChoice` / `resolve_color(choice, attested, term)` — the one shared
  `--color[=WHEN]` decision every colour-capable command app uses (`AGENTS.md`
  §2.2): `never` is plain, `auto` colours only an attested colour terminal,
  and `always` colours even an unattested console at an `Ansi16` floor. It is
  pure (the caller supplies the attestation and `TERM`); a `None` result means
  emit no escape sequences.

## One vocabulary

Every escape sequence a record references is a `tairix_vt::Op` — the one
shared vocabulary (`AGENTS.md` §2.2). The crate defines no second
escape-sequence table: output capabilities are the `Op`s the terminal
accepts, colours are `tairix_vt::Color` models, and arrow-key input is the
`Op` the terminal's bytes parse back to. `Capabilities::referenced_ops`
returns that exact set, and a test round-trips every one of them through
`lib/vt` to prove the database invents nothing.

Mouse-reporting, bracketed-paste, and the function / editing / keypad keys are
recorded as capability *facts* (not byte sequences): their enabling and report
sequences enter `lib/vt`'s vocabulary when the curses input decoder
(`plans/CURSES.md` §C4) needs to emit and parse them, never duplicated here.

## Layering

`lib/termcap` depends on `tairix-vt` and `lib/*` only — never on `kernel/*`,
`drivers/*`, or `userland/*` (`AGENTS.md` §17.4) — and is text-only
infrastructure outside `userland/gui/*`, so a headless image links it freely
(§17.3). `no_std` + `alloc`, never panics (§2.9), and nothing touches fd 3
(`stdinfo`, §20).

## Stability

**experimental.** The capability surface grows stage by stage under
`plans/CURSES.md`; it may still change until the curses stack (C4/C5) pins its
requirements.

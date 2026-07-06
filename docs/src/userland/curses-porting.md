# Building a curses TUI against `lib/curses`

This page is a porting guide: how to write a RustOS terminal application on
top of the OS curses library (`lib/curses`), using the `top` process viewer
(`userland/apps/top`) as the worked example. It assumes the library reference
in [`rustos-curses`](../lib/curses.md).

## The shape of a curses app

A curses program never writes escape sequences by hand. It keeps an in-memory
model, draws that model into one or more `Window`s, and asks a `Screen` to
make the terminal match. RustOS splits a well-behaved TUI into three parts,
all of which `top` follows:

1. **An I/O-free model.** All view state — what is selected, where the list is
   scrolled, which mode is active — and the rules that move it live in a plain
   type with no terminal I/O. `top`'s `Model` is exhaustively unit-tested
   without a terminal because it is just data and transitions.
2. **A pure `render`.** A function that, given the model and a `Screen`, draws
   the current frame. It reads the model and writes the screen; it makes no
   decisions. `top`'s `render` builds a full-screen `Window`, fills the title,
   the column header, the visible process rows, and the footer, then composites
   the optional help overlay on top.
3. **A thin loop.** `run` wires the model, the `Screen`, and any data source
   together: draw, wait for one event, dispatch it to the model, repeat.

Keeping the model and `render` free of I/O is what makes the whole app
host-testable over in-memory channels (`AGENTS.md` §7).

## The two seams

`lib/curses` injects its I/O through the `Tty` trait — somewhere to write
rendered bytes and somewhere to read input bytes. A real app wires a
capability-checked console; a test wires an in-memory queue. `top` adds a
second seam for its data, the `lib/procinfo` `Transport`, so the same code
runs against `sysinfod` in production and against a fixture in tests.

Construct the driver with the terminal type the session advertises and the
screen size:

```rust,ignore
use rustos_curses::{InputMode, Screen, Size};
use rustos_termcap::TermType;

let mut screen = Screen::new(tty, TermType::Xterm256Color, Size::new(rows, cols));
screen.set_cursor_visible(false);
```

## Drawing

Draw into a `Window`, then refresh:

```rust,ignore
use rustos_curses::{Pos, Window};

let mut win = Window::new(Pos::ORIGIN, screen.size());
win.add_str("hello");
screen.wnoutrefresh(&win);   // composite onto the virtual screen
screen.doupdate()?;          // diff against the physical screen, flush
```

`doupdate` runs the minimal-diff renderer: it emits only the cells that
changed, degrades every colour to what the terminal can show, and never emits
a sequence the terminal would misinterpret.

A few patterns `top` uses:

- **Colour pairs.** `Screen::alloc_pair(fg, bg)` returns the pair id for the
  requested colours — reusing an identical existing pair, or defining the next
  free id — so you do not track ids by hand and per-redraw requests never fill
  the table; apply a colour through
  `Window::set_colors` or by setting the `rustos_vt::Attributes` foreground and
  background. `top` allocates a white-on-blue header pair on colour terminals
  and falls back to reverse video on monochrome ones — the renderer's colour
  downgrade does the rest.
- **Wide text.** Measure with `rustos_curses::str_width` and clip with
  `truncate_to_width` so a double-width (CJK / fullwidth / emoji) glyph is
  never split across the right edge. `Window::add_char` stores a wide glyph as
  a lead cell plus a continuation cell automatically.
- **Overlays.** Stack windows by draw order: `wnoutrefresh` the base window,
  then `wnoutrefresh` the overlay, then `doupdate` once. `top`'s help box is
  just a second, smaller `Window` composited after the main one — no separate
  panel type is needed.

## Input

Read input through the driver. `getch` returns the next event, waiting
according to the input mode:

```rust,ignore
screen.set_input_mode(InputMode::Blocking);      // wait for a key (default)
// or NonBlocking — return None immediately when nothing is pending
// or Timeout(d) — wait up to d, then give up (e.g. to refresh on a tick)

match screen.getch()? {
    Some(event) => { /* dispatch to the model */ }
    None => { /* nothing within the wait, or the channel closed */ }
}
```

`Event` is the typed key/mouse/paste vocabulary: `Char(_)`, the arrows,
`PageUp`/`PageDown`, `Home`/`End`, the function/editing keys, `Mouse(_)`, and
`Paste(_)`. `top` maps these to selection moves, the scope toggle, the help
overlay, and quit. Decoding untrusted terminal bytes never panics: an
unrecognised sequence simply yields no event.

## Fail closed and capabilities

A TUI is still subject to the capability model. `top` reads processes through
the `sysinfo-v1` API; the system-wide view requires `CAP_SYSINFO_GLOBAL`, and
the service — not the app — enforces it. A denied query comes back as an
error the app surfaces honestly (`TopError::PermissionDenied`) rather than a
partial or fabricated listing (`AGENTS.md` §5.4) — and a refusal of an
*optional* action degrades gracefully rather than killing the session:
`top`'s `a` key falls back to the caller's own processes and states the
reason on its status line (`Model::refresh_recovering`), while a genuinely
fatal failure ends the session with its reason printed to `stderr`, never a
silent exit. Nothing in a curses app
writes to fd 3 (`stdinfo` is reserved, §20), and production paths carry no
`unwrap`/`expect`/`panic!` (`AGENTS.md` §2.9).

## Linking

`lib/curses` is part of the OS: it is the curated `/System/Libraries/`
Terminal/TUI shared-library class, so an app — OS-bundled or third-party —
**dynamically links** it rather than compiling it in (`AGENTS.md` §16.4). A
third-party app brings any *additional* libraries it needs in its own bundle
(statically, or dynamically from its bundle `Libraries/`), but links the OS
curses library dynamically like every other OS library. In the source
workspace this is expressed as an ordinary cargo path dependency; the
dynamic-vs-static distinction is the OS-image runtime model, not a cargo
setting.

## Layering

A curses app links only `lib/*` crates — `lib/curses`, `lib/termcap`,
`lib/vt`, and whatever data crates it needs — never a `kernel/*` or
`drivers/*` crate (`AGENTS.md` §17.4). It is text-only and works in a headless
image (§17.3).

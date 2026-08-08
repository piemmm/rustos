# tairix-greeter-service — the graphical login screen

`greeter.app`: the screen TAIRiX puts up to ask who is at the machine. It ships
as a `kind = "service"` bundle planted at `/System/Services/greeter.app/Run`
and is spawned by the `login` authority as the dedicated `greeter` service
account (uid 16).

**Stability tier: experimental.**

## What it is

**The greeter draws and types; the authority decides.**

It owns the seat, paints the shared authentication surface (`lib/greeter`), and
relays what was typed to the session authority over `session-v1`. It holds no
credential store, cannot read the user database, cannot start a process, and
binds no privileged endpoint. Everything it knows about the machine's accounts
is what the authority chose to publish — a display name, a login name, and
whether that account already has a live session. Everything it learns about a
secret is one of three answers, and only a verified one finishes the screen.

Compromising it therefore yields a screen, not an account.

The one shared surface it paints is `lib/greeter`'s, the same one the desktop's
screen lock uses; there is no second login surface here. The one pixel path it
scans out through is `lib/display`'s (`ChannelOrder`, `scanout_len`,
`sub_screen_damage`); only the loop that walks one painted surface into one
frame at the mode's stride is this crate's own, because a compositor blending
many windows is genuinely different work. The pointer it draws is
`lib/cursor`'s artwork placed by `lib/cursor`'s `PlacedCursor` — the same type
the compositor uses, which is why the placement lives in `lib/*`: a
`userland/session/*` crate may not depend on `userland/gui/*`.

## Capabilities

The manifest (`AppInfo.toml`) requests exactly six, and the granted set is that
request intersected with the `greeter` service account's ceiling
(`lib/users`, `grants::GREETER_CEILING`) — deliberately the same six:

| Capability | Why |
|---|---|
| `CAP_DISPLAY` | hold the seat's exclusive revocable lease and configure the display service |
| `CAP_INPUT_READ` | drain the owned seat's keyboard and pointer channels |
| `CAP_SHM` | create the double-buffered frame region and grant it to the display service |
| `CAP_FS_ACCESS` | read the shipped wallpaper master (read-only) |
| `CAP_CONSOLE_WRITE` | state an abnormal exit's reason on `stderr` |
| `CAP_LOG_EMIT` | its own audit records |

It stops there on purpose. No `CAP_USERS_READ`: it never sees a credential
store. No `CAP_PROC_SPAWN` or `CAP_SPAWN_AS_USER`: it cannot start the session
it is authenticating for, so it can never choose *which* program runs as the
authenticated user — the authority starts that on its own loop once the greeter
exits `0`. No `CAP_IPC_BIND_PRIVILEGED`: it serves nothing and is only ever a
*client* of `SESSION_ENDPOINT` and `DISPLAY_ENDPOINT`.

## Degradation

A login screen that refuses to appear locks a user out of their own machine, so
every absence short of "there is no screen" is presented rather than fatal:

| Absent | What happens |
|---|---|
| No account list | the chooser stands with its typed-name tile alone |
| The authority unreachable | the surface says so and keeps asking — it does **not** exit, so a transient fault cannot spend the authority's restart budget |
| No wallpaper, or an undecodable one | the flat desktop colour |
| A pointer that will not rasterise | no cursor is drawn; the pointer still moves and hit-tests, and the keyboard alone logs in |
| No trusted clock, or no host name | that line of chrome is empty. Never invented |
| A zero-extent or unqueryable display mode | **fatal**: the reason is stated on `stderr`, the exit is non-zero, and the authority falls back to a text login |
| The seat lease taken away | **fatal**: the seat reports a lost lease ready forever, so re-parking would spin a core. The reason is stated and the exit is non-zero |

Every abnormal exit writes one concise reason to `stderr` before exiting; the
reason never names an account or anything about a secret.

## How it parks

An idle login screen must consume no CPU. There is one wait set holding the
seat's input, and the timeout is the *next* thing that actually needs a
repaint — the next clock-minute boundary, or the next one-second tick of a
lockout while one is counting down, whichever is nearer. When neither applies
the wait has no timeout at all, so an untouched screen arms no timer. There is
no poll loop and no yield.

## The pointer

The seat reports relative motion, so `screen` keeps the running position,
holds it inside the frame, and hands the surface the absolute position it
hit-tests. One seat report expands to at most two surface events — a button is
a move *and* a transition — and they present as one frame.

The built-in arrow is rasterised **once** at start-up for the active scale and
blended over each painted frame before it is scanned out, so it sits on top of
everything the surface drew. A move presents the union of the cursor's old and
new rectangles clipped to the screen — never the whole screen, and never a
cursor left where it no longer is — and a move the screen edge swallows
presents nothing.

## Untrusted input

The shipped wallpaper is attacker-shaped data like any other image, so it is
decoded by re-entering this same binary as a capability-empty sandbox worker
(`lib/sandbox`), under a fixed byte bound, never in the address space that owns
the seat. The worker role is checked before anything else in `main`. A
malformed or oversize image is the flat desktop colour, not a crash.

## Module map

* `events` — the stable audit event ids (`19000` range).
* `accounts` — the `SessionTransport` seam and the bounded paging walk that
  turns the authority's account pages into chooser tiles.
* `verify` — the `session-v1` client behind the surface's `Verifier`. The
  buffer that carries the secret is a `Wiped` field, sized once so encoding
  cannot reallocate and strand a copy, and erased on every path out.
* `chrome` — the clock, date, and host name text.
* `cursor` — the pointer position the seat's relative motion accumulates into,
  held inside the screen for every screen shape, and the built-in arrow
  resolved for a scale.
* `frame` — the surface-to-scan-out composition.
* `wait` — the lockout countdown and the park deadline.
* `screen` — `LoginScreen`, the whole flow over those seams.

`src/run.rs` is the freestanding `Run` program: seat, frames, accounts, first
paint, park. It is an inert stub on the host, so host tooling never links the
userland runtime.

## Why the freestanding build enables extra crate features

Several `lib/*` crates the screen draws and queries through are seam-injected
by default and only reach the running system when their runtime feature is on.
The host build wants the seams (tests inject mocks); the real `Run` program
wants the syscalls, so the `program` feature turns each on under
`cfg(target_os = "none")`:

| Crate | Feature | Without it |
|---|---|---|
| `tairix-font` | `rt` | no font-service transport is installed, **every glyph request fails closed, and the screen draws no text at all** |
| `tairix-procinfo` | `program` | no System Information transport, so the host name on the backdrop is always blank |
| `tairix-sandbox` | `program` | no worker launcher, so the wallpaper can never be decoded |

`tairix-display` is deliberately **not** given its `rt` feature: that gates the
display *service*'s shared-memory mapper, and the greeter is a client — it maps
its own frame ring through `tairix-rt` directly.

## Tests

Everything about *what the screen does* is host-testable behind the injected
seams, and `tests/session_v1.rs` additionally wires the transport seam straight
to the authority's own `handle_session_request` — a **test-only** edge, so the
two halves of one protocol are proven against each other rather than each
against its own mock.

`src/run.rs` is compiled out of every host build, so nothing on the host can
check it. Building and linting it for a real target is what does:

```
cargo test -p tairix-greeter-service
cargo clippy -p tairix-greeter-service --all-targets --no-deps -- -D warnings

for t in aarch64-unknown-none riscv64gc-unknown-none-elf x86_64-unknown-none; do
  cargo clippy -p tairix-greeter-service --bin tairix-greeter-service-run \
    --target "$t" -Z build-std=core,alloc,compiler_builtins --no-deps -- -D warnings
done
```

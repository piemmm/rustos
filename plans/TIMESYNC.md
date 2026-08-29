# TIMESYNC.md — Time synchronisation: the NTP client, RTC drivers, and the clock's provenance

Staged build plan for how a TAIRiX machine learns what time it is. **Binding
under `AGENTS.md`** (read it first, especially §2, §4, §5, §17, §19, §21,
§24, §26). It consumes the seams `plans/NETWORK.md`, `plans/DNS.md`, and
`plans/NEW-SERVICEMANAGER.md` establish and never contradicts them; where they
touch, those plans' decisions stand. `abi-v1` is not frozen, so the ABI and
config additions here are ordinary pre-release changes (§2.13).

The motivating hardware fact: a Raspberry Pi 3/4 has **no RTC**, and the Pi 5's
is optional and battery-backed. The motivating software fact: nothing in this
workspace calls `wall_time_set` outside tests, so every boot starts at
`WallTimeState::Unset` — the desktop clock renders its unset label, the Date &
Time app deliberately shows empty fields, and audit-log hash chains, ARXFS
`Time64` metadata, and certificate validity all rest on a clock a human has to
set by hand.

## 0. Scope and decisions (binding)

- **Two independent sources, one arbiter.** An RTC (when the machine has one)
  gives a time at boot with no network; NTP corrects and maintains it. The
  arbitration between them is the *provenance ladder* the wall clock already
  models (`WallTimeState`): `Unset` → `Firmware` (an RTC) → `Trusted` (a
  validated network sync) / `Adjusted` (a slew). A source never overwrites a
  reading of strictly better provenance, so an RTC read cannot clobber a
  network sync.
- **TAIRiX is an NTP *client*, not a server.** A stub SNTP/NTPv4 client
  (RFC 5905 §14) querying configured servers in unicast mode. An NTP server,
  broadcast/multicast/manycast modes, symmetric peering, Autokey, NTS, and a
  full clock-filter/selection/clustering algorithm are explicitly **not** in
  this plan (§2.3/§2.4). One server's validated sample disciplines the clock;
  a full peer-selection algorithm is a later increment if measurement shows it
  is needed.
- **Two crates, following the shape `dns` already set (§2.2).** The wire
  protocol lives with its siblings and the client that drives it is its own
  crate, exactly as `lib/net::dns` and `lib/resolver` divide:
  - **`lib/net::ntp`** — the pure, `no_std`, allocation-free RFC 5905 codec,
    every response-validation rule, the sample computation, the Kiss-o'-Death
    decode, and the poll/retransmit/backoff/server-rotation state machine.
    It sits beside `dhcp` and `dns` because it *is* a network wire protocol,
    and it reuses that crate's existing monotonic-deadline arithmetic
    (`timeutil`) rather than carrying a second copy.
  - **`lib/timesync`** — the userland client: the sync-decision policy of §2
    (which is clock policy, not NTP), driving the engine over an injected
    transport seam, with the freestanding socket glue behind a `program`
    feature so the host tests never pull the runtime (the `lib/resolver` /
    `lib/procinfo` split).

  Both are driven by injected monotonic time, an injected wall-clock reading,
  and **caller-supplied CSPRNG values** — neither generates randomness itself
  (the `lib/net::dhcp` / `tcp::conn` `iss` / `dns` precedent). Unit tests, the
  fuzz harness, and the live service all exercise the *same* engine.
- **Every packet on the wire is hostile (§26.4), and NTP is unauthenticated.**
  The codec is total over a fixed 48-byte header, bounded, fuzzed (§19.6), and
  fails closed: a malformed or implausible response is dropped whole and
  nothing partial is surfaced. Off-path spoofing is bounded by the RFC 5905
  §8 on-wire check — the response's origin timestamp must equal the exact
  CSPRNG 64-bit nonce the request carried in its transmit field — plus the
  plausibility window of §2. A mismatch is discarded, never accepted.
- **The decode never runs in the same address space as `CAP_TIME_SET`**
  (§19.5). See §4.
- **Event-driven and tickless (§2.23, §17.1).** The engine folds one
  `next_deadline()`; the service arms a single one-shot wait and calls
  `poll(now)` when it lapses and `on_response(now, bytes)` when a datagram
  arrives. There is no polling loop and no periodic tick.
- **64-bit time throughout (§21).** Every instant is `Time64`, every span
  `Duration64`. The NTP 1900-epoch timestamp wraps in 2036, so the NTP **era**
  is handled explicitly rather than assumed zero; tests cover pre-1970,
  post-2038, and the era boundary.
- **Automatic time-setting is enabled by default and the user can turn it
  off.** That switch is service enrolment, owned by the service manager, and
  is reachable from both a command line and the desktop (§6).

## 1. Target architecture (binding)

```
lib/net::ntp            pure protocol: codec, validation, politeness, retry
   ^                    (beside dhcp and dns; no sockets, no clock, no RNG)
   |
lib/timesync            the client: the sync-decision policy over an injected
   ^                    transport; freestanding glue behind `program`
   |
userland/system/timed   the service: reactor, sockets, DNS, wall_time_set
   |         \
   |          `-- lib/sandbox::timesync   the decode, in a capability-less worker
   |
lib/sysconfig           time.* keys in /System/Settings/Configuration/system.conf
lib/resolver            server names -> addresses

lib/abi/src/driver/rtc.rs   the RTC driver class + the shared BCD civil codec
   ^
drivers/rtc/<leaf>/         one crate per chip, matched by discovery
   ^
lib/i2c + drivers/bus/i2c/bcm2835/   the transfer path the I2C chips are reached through
```

- `timed` is a system service under its own service account, `requires`
  `ReadyCondition::NetworkUp`, `activation: permanent`,
  `restart: on-failure` — SUM1 unit metadata in its signed `AppInfo`, so the
  kernel derives its grant as `manifest ∩ account-ceiling` at load
  (`plans/NEW-SERVICEMANAGER.md` SVC-A). It holds `CAP_TIME_SET` and a UDP
  socket, and no filesystem authority beyond its own state file.
- An RTC driver is an ordinary `drivers/` module: matched by its
  hardware-tree `compatible` key through `lib/devmatch`, autoloaded by
  `devmgr` under `CAP_DRV_LOAD`, granted only the resources its matched node
  requested (§18.3). It sets the clock as `Firmware` and holds no network
  authority.
- The reactor shape is `userland/net/netstack/src/run.rs`'s, unchanged:
  `waitset_create` / `waitset_ctl(Port)` / `waitset_wait(set, timeout_ns)`
  with `timeout_ns` folded from the engine's `next_deadline()`.

## 2. The sync-decision policy

The question "should I sync *now*?" is a pure function of the wall clock's
current reading, the monotonic uptime, and a persisted last-seen record. It is
the engine's core and is host-tested as a matrix.

A reading is **implausible** when it falls outside the plausibility window:
before `tairix_abi::time::RELEASE_EPOCH` (a fixed, documented release-epoch
constant — no TAIRiX build can legitimately be running before the release it
was built from) or more than `PLAUSIBLE_FUTURE_YEARS` (100) ahead of it. The
window is a **fixed validation bound** (§24.4), never a capacity, and never
widened to make a case pass.

Decision order, first match wins:

| Condition | Action |
|---|---|
| Clock `Unset` | Sync as soon as `NetworkUp` is satisfied |
| Reading implausible (pre-`RELEASE_EPOCH`, or ≥ 100 years hence) | Sync immediately |
| Reading minus the persisted last-seen instant exceeds `STALE_BOOT_GAP` (5 days) | Sync immediately |
| Otherwise (clock set and plausible) | **No boot sync.** Refresh once `REFRESH_UPTIME` of uptime has elapsed, then on that cadence |

`REFRESH_UPTIME` defaults to one day and is configurable (§3); the point of
gating it on *uptime* rather than wall time is that a machine which reboots
often must not re-query on every boot — an RTC's reading is trusted until
there is a reason to doubt it, which is exactly the row above it.

The persisted record is `/System/Settings/Time/state`: a small fail-closed
document holding the last successful sync instant and the last-seen instant,
rewritten on each successful sync. `/System/Settings` is one of the two
writable paths under `/System` (§16.2) and is already mounted
`nosuid,nodev,noexec`. A missing or corrupt record resolves to "no last-seen
instant" — which makes the stale-boot row simply not fire, never a guess
(§5.4).

**Discipline.** The kernel wall clock is set-and-project — it has no frequency
or gradual-slew primitive — so a correction is always applied as a single
`wall_time_set`. A genuine frequency-correcting slew (an `adjtimex` analogue,
which would need a kernel primitive that does not exist) is out of scope here.

**Provenance describes the source, never the size of the change.** A validated
sample always records `WallTimeState::Trusted`, because the new reading comes
wholly from the network time source — whether it established an `Unset` clock,
replaced a `Firmware` one, or refreshed an earlier sync. `Adjusted` is
deliberately *not* used by `timed`: the ABI defines it as a previously-set time
corrected after the fact, so that the offset is no longer its original
source's, which describes a manual step (the Date & Time application) rather
than a source replacing its own value.

The service still classifies a correction larger than `STEP_THRESHOLD` as a
*step* rather than a refinement, but that is an **audit** distinction, not a
provenance one: a large step can move certificate validity and reorder how a
reader interprets the log, so it is worth its own event. Nothing else in the
service touches the clock.

## 3. Politeness — not overloading NTP servers

Encoded as engine policy, never as a sleep loop (§2.1) and never as
retry-until-it-works. The reference is RFC 8633 (NTP BCP).

- **A hard minimum poll interval floor.** Steady state is a multi-minute
  cadence; the floor is a fixed bound the configuration cannot lower.
- **One request in flight per server**, and the configured list is *rotated*
  rather than one server being hammered.
- **Bounded exponential backoff with CSPRNG jitter** on timeout or refusal,
  clamped to a cap. Jitter is load-bearing, not decoration: a fleet of Pis
  restored from the same image and rebooting together must not stampede one
  server.
- **A randomised initial delay** before the very first query, for the same
  reason. The `Unset`-clock case still resolves quickly — the delay is small
  and bounded — because a machine with no clock is unusable until it has one.
- **Kiss-o'-Death is obeyed** (RFC 5905 §7.4). A stratum-0 response's 4-octet
  reference id is decoded as a kiss code: `RATE` widens this server's poll
  interval, `DENY` and `RSTR` retire the server for the boot. Ignoring a KoD
  is exactly the abuse this section exists to prevent.
- Configuration lives in `lib/sysconfig`'s `system.conf` as new `time.*` keys
  (the server list and the refresh cadence), parsed by the existing
  fail-closed engine — an unknown key is refused as it already is for `net.*`.
  Server names resolve through `lib/resolver`.

## 4. Security: the authority split

`timed` holds `CAP_TIME_SET`. Setting the machine clock arbitrarily is a real
attack — it can invalidate certificate lifetimes, reorder audit reasoning, and
move capability expiry — so the process holding that capability must not parse
an attacker-controlled packet.

The NTP decode therefore runs inside a **`lib/sandbox` worker holding nothing
but its pipe**, as a new `timesync` module beside the existing `decode`,
`helpdoc`, and `imagerender` consumers of that seam:

1. `timed` receives the datagram and copies a bounded 48 bytes into a request
   frame. Copying a fixed-length buffer is not parsing.
2. The worker decodes and validates, and replies with a candidate sample.
3. `timed` **re-validates the reply** — the nonce echo and the plausibility
   window — before applying anything. The worker is hostile the moment it has
   touched a byte; that is the seam's existing stated discipline.

A worker crash is a typed error to the caller, the worker is reaped and
replaced, and the event is logged with a stable id — all of which
`host::ParserSandbox` already does. Host tests drive the entire parent path
through `sandbox::loopback`, so the containment is covered without processes.

Every sync applied, sample rejected, and server retired is audited through
`lib/log` with a stable event id (§19.4).

## 5. RTC support

The RTC class ABI is `lib/abi/src/driver/rtc.rs`: read and set an instant as
`Time64`, a declared precision, and honest `battery_backed` /
`oscillator_stopped` flags. It also carries the **BCD civil-time codec** every
chip's register block shares, so that encoding has one definition rather than
one per chip (§2.2); chip-specific register offsets, century bits, and quirks
stay in each chip's own driver (§2.22). `HwDeviceClass::Rtc` is appended to the
hardware-tree class set, never renumbering an existing discriminant.

A driver that cannot vouch for its reading — oscillator stopped, clock
integrity flag set, firmware refusal — reports the time as unavailable rather
than returning a fabricated one, and the node is left with the clock `Unset`
for `timed` to fix. An unbound RTC node is logged, never an error (§18.4).

Three tiers, in the order they land:

- **QEMU-emulable, one per hardware target** — the reference implementations,
  each with a QEMU vertical proving a `Firmware`-provenance clock before any
  network exists: `drivers/rtc/pl031/` (`arm,pl031`, the `aarch64 -M virt`
  RTC the tree's DTB path already discovers), `drivers/rtc/mc146818/` (the
  x86_64 CMOS RTC over a port-I/O resource, UIP-safe, honouring register B's
  binary/BCD and 12/24-hour bits, its node synthesised on the
  legacy-fallback discovery path §18.2 sanctions), and
  `drivers/rtc/goldfish/` (`google,goldfish-rtc`, the riscv64 `virt` RTC's
  64-bit nanosecond pair).
- **The Pi 5's built-in RTC** — `drivers/rtc/rpi/`, `raspberrypi,rpi-rtc`.
  The RTC is inside the PMIC and is *not* memory-mapped, so the VideoCore
  firmware mailbox is the only route: `lib/vcmailbox` grows the RTC property
  tags (get `0x0003_0087`, set `0x0003_8087`) with the `RTC_TIME` register
  selector, and the driver reaches them through `drivers/bus/mailbox`.
  Host-tested against the existing `vcmailbox::mock::MockFirmware`. Some Pi 5
  firmware revisions fail every mailbox property request, so the driver fails
  closed to "RTC unavailable" and never spins or hangs on the mailbox.
- **The I²C HAT chips Pi 3/4 use** — reached through a new bus path:
  `lib/i2c` holds the bus-agnostic transfer vocabulary (addressing, transfer
  direction, register sequences, error taxonomy) exactly as `lib/usb` and
  `lib/virtio` hold protocol without device logic, and
  `drivers/bus/i2c/bcm2835/` drives the Broadcom Serial Controller the whole
  Pi family exposes. The BSC driver is interrupt-driven (`irq_bind` /
  `irq_wait`, never a spin, §2.23) with a bounded fail-closed timeout on the
  hardware handshake, and it serves one grant-restricted IPC transfer
  endpoint per enumerated child — the pattern the USB host controllers
  already use. The chips: `drivers/rtc/ds3231/` (`maxim,ds3231`; its
  0x00–0x06 register block is the DS1307-compatible one, so `maxim,ds1307`
  binds here too), `drivers/rtc/pcf8523/` (`nxp,pcf8523`, the chip Raspberry
  Pi's own `i2c-rtc` overlay documentation names), and
  `drivers/rtc/pcf85063a/` (`nxp,pcf85063a`).

QEMU does not model the BSC, so the I²C tier is host-unit-tested against a
mock bus with no QEMU vertical possible. Each affected driver's `README.md`
states that plainly; §8's emulable-hardware clause is satisfied by the first
tier.

## 6. Enable and disable

Automatic time-setting is on by default: `timed` ships enrolled in
`/System/Settings/Services/enabled`. Turning it off is *unenrolment*, which is
the service manager's job, so both surfaces drive the one control path rather
than inventing a second switch (§2.2).

- **Command line.** `lib/abi/src/service_control.rs` gains `Enable`,
  `Disable`, and `Status` beside the existing `Start`/`Stop`, and
  `userland/shell/servicectl` is the tool — the `systemctl` analogue this
  plan's dependency (`plans/NEW-SERVICEMANAGER.md` SVC-8) specifies and
  leaves unlanded. Enable and disable write through the existing pure
  `registry::enrol` / `unenrol` record transforms, so the enroller's ceiling
  check cannot be bypassed and enrolment can never widen authority.
- **Desktop.** The taskbar clock's menu gains a *Set Date & Time
  Automatically* toggle, derived from `clock_menu::ROWS` like every other row
  so a row cannot exist without a command behind it. The session holds no
  authority to change service enrolment, exactly as it holds none to set the
  clock, so the row reuses the re-authentication broker path the existing
  *Set Date & Time…* row already uses; a console with no broker renders the
  row non-actionable with the reason stated rather than offering a command
  that could only fail. The Date & Time settings pane exposes the same
  toggle over the same control path.

Landing `servicectl` requires the control **transport** —
`plans/NEW-SERVICEMANAGER.md` SVC-8's remaining item: a per-manager wait-set
reactor in PID 1 serving `SERVICE_CONTROL_ENDPOINT` alongside child reaping
and arming the real one-shot linger/grace/restart/watchdog timers the engine
already computes deadlines for, plus `CAP_SERVICE_CONTROL` added *with* its
enforcement point and its first holder in the same change (§5.2). That is a
genuine prerequisite of this plan's third deliverable and is built properly,
not stubbed (§2.19).

## 7. Stages

Each stage leaves the whole-project §7 gate green before it is reported done.

### TS-1 — `lib/net::ntp` and `lib/timesync`: the engine and the policy — DONE
- `tairix_abi::time::{RELEASE_EPOCH_SECS, PLAUSIBLE_FUTURE_SECS,
  is_plausible_wall_time}` — the plausibility window, defined once, re-exported
  at the crate root, and mirrored into the generated C header beside the
  sibling time constants (`TAIRIX_RELEASE_EPOCH_SECS` /
  `TAIRIX_PLAUSIBLE_FUTURE_SECS`, emitted by the generator, not hand-edited).
- `lib/net::ntp` — the RFC 5905 §7.3 codec (`Header::decode` total over any
  buffer ≥ 48 bytes, a longer tail ignored so an extension-field or MAC-bearing
  reply is not refused), `NtpTimestamp` held as the wire's two 32-bit fields
  with era placement anchored on `RELEASE_EPOCH_SECS` (never on the local
  clock, which may be the wrong thing being corrected), the §0 validation
  rules, the sample computation (round trip from the *monotonic* legs minus the
  server's own processing, instant = transmit + ½ round trip), the §3
  politeness policy, and the `NtpClient` transaction machine with one folded
  `next_deadline()`. Shaped like `dhcp::DhcpClient` (non-blocking, poll /
  on_datagram) rather than `dns::resolve` (blocking one-shot), because a time
  client is a long-lived engine in a reactor; each entry point yields at most
  one action, so nothing is allocated.
- `lib/timesync` — the §2 decision matrix (`decide`), the `SyncRecord` whose
  `last_seen` never moves backwards, and the `TimeSync` client that turns a
  validated sample into a `ClockUpdate`. Provenance is always `Trusted`; the
  step-vs-refinement classification is audit-only, and an establishment carries
  no `correction` rather than a fabricated magnitude.
- Politeness invariants a future change must not break: **`MIN_POLL` bounds the
  steady state, not every packet** — a failed transaction retries sooner under
  the bounded exponential backoff, which is what RFC 8633 §3.2 and RFC 4330 §10
  permit for start-up and recovery, and that retry always rotates to the next
  server. Claiming a server is what advances the rotation cursor, so a failure
  path must not advance it again or a two-server client returns to the one that
  just failed. A Kiss-o'-Death hold is folded into the scheduled instant, so a
  `RATE` is waited out in a single sleep rather than a wake that finds every
  server still held off. Jitter scales by multiply-shift (a modulo leaves the
  top of the band unreachable and skews low) over a span clamped to
  `MAX_JITTER_SPAN_NANOS`, because overflow checks are on in every profile and
  an unclamped fixed-point multiply would panic rather than misbehave.
- 65 host tests (39 engine, 26 policy) over the decision matrix, the
  backoff/jitter/floor policy, the KoD codes, the anti-spoof gate under a
  wrong-nonce flood, and the pre-1970 / post-2038 / era-1 boundaries. The
  `fuzz_net_ntp` harness asserts the five never-panic / never-implausible /
  nonce-gate invariants and is registered in `cargo xtask fuzz`.
- Deferred to TS-2 by design (not a gap): the persisted record's on-disk
  encoding, the `time.*` configuration keys, and the sandboxed decode all
  belong with the service that owns the I/O.

### TS-2 — `timed`: the service
`lib/sandbox::timesync` (the capability-less decode worker, §4);
`userland/system/timed` with the tickless reactor; the `time.*` `system.conf`
keys; the `/System/Settings/Time/state` record; the service account,
manifest, ceiling, SUM1 unit metadata, and default enrolment; the audit event
ids. QEMU vertical: an unset-clock guest with a fixture responder reaches
`Trusted`, and a companion case proves a wrong-nonce response is rejected and
audited.

### TS-3 — RTC class + the QEMU-emulable RTCs
`lib/abi/src/driver/rtc.rs`, `HwDeviceClass::Rtc`, and the `pl031` /
`mc146818` / `goldfish` drivers with the `devmgr` autoload path setting the
clock as `Firmware`, plus the provenance-ladder rule that an RTC read never
overwrites a `Trusted` reading. A QEMU vertical per hardware target.

### TS-4 — Pi RTC support
The `lib/vcmailbox` RTC tags and `drivers/rtc/rpi/`; then `lib/i2c`,
`drivers/bus/i2c/bcm2835/`, and the `ds3231` / `pcf8523` / `pcf85063a` chip
drivers.

### TS-5 — Service-control transport + `servicectl`
The `Enable`/`Disable`/`Status` ops, the PID 1 control reactor and its
one-shot timers, `CAP_SERVICE_CONTROL` with its enforcement point and holder,
the enrolment store-write seam, and the `servicectl` tool with its bundle
`Help/` tree and coreutils-shaped options and exit codes (§16.7). QEMU
vertical: a live `servicectl disable timed` survives a reboot.

### TS-6 — The desktop toggle
The taskbar clock menu row and the Date & Time settings pane entry over the
one control path, through the existing elevation broker. QEMU desktop
vertical: toggle off, confirm `timed` is disabled.

## 8. Cross-references

- `plans/NETWORK.md` — the socket ABI and the `NetworkUp` readiness condition
  `timed` requires.
- `plans/DNS.md` — the stub resolver that turns configured server names into
  addresses, and the engine-shape precedent this plan follows.
- `plans/NEW-SERVICEMANAGER.md` — enrolment, SUM1 unit metadata, the control
  endpoint, and SVC-8's transport that TS-5 lands.
- `plans/TIMEZONES.md` — civil time zones and local rendering. This plan sets
  UTC; that one renders it.
- `plans/PI.md` — Pi bring-up, the VideoCore mailbox, and the BCM2711/2712
  platform surface the Pi RTC tiers sit on.
- `plans/SYSLOG.md` — the hash-chained audit log whose timestamps depend on
  the clock this plan establishes.
- `plans/NEW-DESKTOP-SETTINGS.md` — the Date & Time pane the TS-6 toggle
  joins.
- `plans/NEW-TASKBAR.md` — the clock and its menu.
- `kernel/core/src/wallclock.rs`, `lib/abi/src/time.rs` — the wall clock, its
  provenance ladder, and `Time64`.
- RFC 5905 (NTPv4), RFC 8633 (NTP BCP), RFC 4330 (SNTP).

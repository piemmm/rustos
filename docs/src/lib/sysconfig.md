# `tairix-sysconfig` — the boot-time system-configuration store engine

`lib/sysconfig` is the single definition of TAIRiX's administrator-settable
boot-time configuration store: the document at
`/System/Settings/Configuration/system.conf`
(`tairix_sysconfig::CONFIG_PATH`). It owns the line grammar, the **closed**
key registry, each key's typed value set, the bounded fail-closed parser,
and the canonical render. The `configure` command app
(`userland/apps/configure`) writes the store through this engine and every
boot-time consumer reads it through the same engine, so producer and
consumer can never diverge.

## The store

One small text document: `key value` settings, one per line; `#` comments
and blank lines are ignored; every key appears at most once. The document
is bounded (`MAX_CONFIG_LEN`, 4 KiB) and the parser refuses — never guesses
at — anything it does not fully understand: an unknown key, a value outside
its key's set, a duplicate, an oversized document (`ConfigError`). An
**absent** store is not an error: it means every key is at its documented
default (`SystemConfig::default`), which is how a fresh installation and a
pre-unlock boot behave.

The store lives inside `/System/Settings` on the encrypted root volume, so
it can only be read after the operator's `ARXFS passphrase:`
unlocks the root — a boot-time consumer therefore parses it post-unlock by
construction, and runs on defaults before that. Write authority is the
existing `/System/Settings` per-inode policy under the caller's
kernel-attested identity: no new capability exists for it, and an ordinary
account can read the settings but not change them.

## The registry

| Key               | Values               | Consumer                                                        |
|-------------------|----------------------|-----------------------------------------------------------------|
| `os.loginType`    | `text` \| `graphical` | `login`: the boot-default session type (`graphical`, the default, starts the desktop directly, degrading to the text prompt on a machine that cannot run one; `text` keeps the prompt) |
| `cache.all`       | `on` \| `off`         | the kernel cache-admission control: the master caching switch / ceiling |
| `cache.filesystem`| `auto` \| `off`       | the kernel filesystem cache (`kernel/core::fs::CachedFs`) |
| `cache.block`     | `auto` \| `off`       | the kernel whole-disk block cache (`kernel/tairix-kernel::block_cache`) |
| `cache.transform` | `auto` \| `off`       | the ARXFS decompressed-cluster cache (`kernel/tairix-kernel::transform_cache`) |
| `cache.semantic`  | `auto` \| `off`       | the application-launch cache (`kernel/core::launch_cache`) |
| `net.ipv4.enabled`| `true` \| `false`     | the network stack (`netstack`): the stack-wide IPv4 address-family switch |
| `net.ipv6.enabled`| `true` \| `false`     | the network stack (`netstack`): the stack-wide IPv6 address-family switch |
| `net.ipv6.privacy`| `true` \| `false`     | the network stack (`netstack`): RFC 8981 temporary (privacy) IPv6 addresses |
| `net.tcp.syncookies` | `auto` \| `always` | the network stack (`netstack`): the TCP SYN-flood defence policy |
| `net.tcp.keepalive` | `true` \| `false`   | the network stack (`netstack`): RFC 9293 §3.8.4 TCP keepalive probing on idle connections |
| `net.tcp.ecn`     | `true` \| `false`     | the network stack (`netstack`): RFC 3168 Explicit Congestion Notification negotiation |

Adding a key is adding a `Key` variant (plus its `SystemConfig` field and
match arms) **and** its consumer in the same change — the compiler then
forces every reader to state what the new key means for it. There is no
free-form key namespace and no second store.

The `net.*` family is the stack-wide network configuration; its consumer is
the user-space network stack (`netstack`). Because `netstack` is the
network-parsing sandbox and holds **no** filesystem capability, it cannot
read the store itself: the **device manager** (`devmgr`, which already
drives the stack's admin endpoint) reads these keys off the read-only
`/System` volume over the pre-unlock store endpoint and delivers them once
over the capability-gated
(`CAP_NET_ADMIN`) `ApplyNetworkSettings` admin op — audited, and
fail-soft-retried until the stack accepts them (`plans/NETWORK.md` N9b-2).
Until then the stack holds these same registry defaults. Per-interface
network settings (addresses, MTU, bonding) live in the separate
`/System/Settings/Network/network.conf` store (`lib/netconfig`), never
here.

## The caching switches

Caching is a first-class, classed subsystem (`plans/SMARTRAM.md`), so its
switches live in a dedicated `cache.*` domain rather than scattered under
`fs`/`net`/… A master `cache.all` sits above the per-class switches as a
**ceiling**: `SystemConfig::effective_cache(class)` is `off` whenever
`cache.all` is `off`, otherwise the class's own value — one canonical,
fail-closed interpretation of the two persisted keys, never an ambiguous
contradiction.

The per-class values are `auto` (the pressure governor manages the class —
today's behaviour, and the absent-store default) and `off` (a hard bypass:
the cache admits and holds nothing). There is deliberately **no** per-class
`on` — a class cannot be forced to ignore memory pressure without breaking
the SMARTRAM reserve invariants. Only classes whose cache exists today have
a key; a shelved or future cache gains its key in the change that lands it.

The switch is safe because every SMARTRAM cache is a reclaimable
accelerator that is never the source of truth: disabling any or all of them
is degrade-gracefully (slower, still correct), never a behavioural change.
The kernel applies these switches once at unlock (`kernel/core::syscfg`
reads the store off the just-unlocked root and calls
`CacheControl::apply`), into the one process-global control every cache
consults at admission — so `off` takes effect on each cache's next
operation, dropping (and zeroing any decrypted plaintext) what it held.

## The network switches

The `net.*` keys are the stack-wide network posture, typed as
`NetToggle` (`true` / `false`) for the two address-family switches, the
privacy switch, the TCP keepalive switch, and the TCP ECN switch, and
`SynCookies` (`auto` / `always`) for the SYN-flood defence. Their defaults
reproduce today's behaviour: both families enabled, IPv6 privacy addresses
off, the `auto` SYN-cookie policy (bounded half-open queue, stateless
cookies on overflow), TCP keepalive off (RFC 1122 §4.2.3.6 — an idle
connection is never probed unless the operator opts in), and TCP ECN off
(RFC 3168 — connections are Not-ECT unless the operator opts in). There is
deliberately **no** `off` for `net.tcp.syncookies` — an undefended or
unbounded connection queue is a security regression the charter forbids,
not a configuration. A disabled address family binds no addresses, answers
no packets, and refuses a socket in that family with a typed error (fail
closed), never a silent drop.

## API shape

- `SystemConfig::parse(&str) -> Result<SystemConfig, ConfigError>` — the
  bounded, fail-closed parse.
- `SystemConfig::render() -> String` — the canonical document (header
  comment plus every registry key, so render→parse round-trips exactly and
  the file a user opens always shows the whole registry).
- `SystemConfig::get/set(Key, …)` — the typed per-key access `configure`
  lists and edits through.
- `Key::{ALL, name, from_name, values}` — the closed registry, for
  listings and stated-choice diagnostics.
- `SystemConfig::effective_cache(CacheClass) -> CacheMode` — the master
  `cache.all` ceiling folded over a class's own switch, the one
  interpretation the kernel cache-admission control applies.
- `CacheClass::{ALL, key}` / `CacheMode` / `CacheSwitch` — the closed
  cache-switch vocabulary the kernel reuses (no duplicate enum).
- `NetToggle` (`true`/`false`, `is_enabled`) / `SynCookies`
  (`auto`/`always`) — the closed `net.*` value vocabulary the network
  stack reuses (no duplicate enum).

The crate is `no_std` + `alloc`, performs no I/O, holds no authority, and
is host-unit-tested in `src/lib.rs`. Stability tier: experimental
(`lib/sysconfig/README.md`).

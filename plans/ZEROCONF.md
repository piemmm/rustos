# ZEROCONF.md — Link-local service discovery (mDNS / DNS-SD)

Staged build plan for TAIRiX's zero-configuration service discovery: RFC 6762
multicast DNS and RFC 6763 DNS-based service discovery. **Binding under
`AGENTS.md`** (read it first, especially §2, §5, §19, §24, §26); it consumes
the seams `plans/NETWORK.md` and `plans/DNS.md` establish and never
contradicts them — where they touch, NETWORK.md's decisions stand. `abi-v1`
is not frozen: the ABI, manifest, and config additions here are ordinary
pre-release changes (§2.13).

`plans/DNS.md` §7 lists "no mDNS / LLMNR / NetBIOS name resolution" as a
non-goal of the unicast stub resolver. This is the plan that adds mDNS —
and keeps LLMNR and NetBIOS permanently out (§1 below).

## Ledger

| id | What it is | Status |
|---|---|---|
| Z1 | `lib/net::mdns` pure engine: `SRV`/`TXT`/mDNS-`NSEC` record types, responder/querier state machine, per-interface cache, fuzz harness | done |
| Z2 | `lib/net::dnssd` DNS-SD vocabulary: the instance/type/domain triple, TXT key-value grammar, service-type grammar | done |
| Z3 | `discoveryd` split process: a capability-empty sandboxed decoder, a front owning the sockets and every authority, the `lib/sandbox` supervised session | done |
| Z4 | Browse + resolve, scoped by grant; `.local` routing in `lib/resolver`; `lib/discovery` client | done |
| Z5 | Publication: the three-gate authority check (attested / granted / owned), manifest `publishes` section, grant store | planned |
| Z6 | Per-interface `discovery.mode` posture, per-network identity, service-manager advertise/goodbye lifecycle | planned |
| Z7 | Attested records over TOFU host keys — **decision pending**, may be dropped | planned |
| Z8 | The catalogue query surface: storage-class filter, per-source fairness, endpoint pinning, and the discovered-share mount into `Storage:` | planned |
| Z9 | The `files.app` Network view over the existing renderer; gated on the §6.6 display-safety policy | planned |

## 0. Scope and decisions (binding)

- **One pure engine, host-testable: `lib/net::mdns`.** Wire parsing, the
  probe/announce/conflict state machine, known-answer suppression, and the
  cache live in the pure, `no_std`, `#![forbid(unsafe_code)]`,
  allocation-bounded `lib/net` crate, driven by injected monotonic time and
  caller-supplied CSPRNG values (the `dhcp` / `dns` precedent). The engine
  never touches a socket and never generates randomness itself.
- **mDNS is DNS on the wire, so it reuses the DNS codec.** `Name`,
  `RecordType`, and the header pack/unpack in `lib/net::dns` are the single
  definition (§2.2); a second codec is forbidden. `RecordType` gains `Srv`,
  `Txt`, and mDNS `Nsec` **in the increment that consumes them** (Z1), never
  ahead of it (§2.4). mDNS `NSEC` (RFC 6762 §6.1, a negative response
  asserting which types an owner name does *not* have) is a different record
  from DNSSEC's `NSEC`; only the mDNS reading is implemented.
- **Every packet is hostile, and the attacker sets the arrival rate
  (§26.4).** An mDNS packet is unauthenticated, unsolicited, and
  attacker-controlled. The codec is total, bounded (§24.4), fuzzed (§19.6),
  and fails closed: a malformed message is dropped whole and nothing partial
  is cached. Cost per packet is bounded and allocation-free on the receive
  path, because an attacker choosing our CPU cost is a denial-of-service
  channel, not merely a performance question.
- **Event-driven, tickless (§2.23).** The engine exposes a folded one-shot
  `next_deadline()` covering probe (3 × 250 ms), announce (2 × 1 s,
  doubling), response jitter (20–120 ms), and cache refresh (80/85/90/95 % of
  TTL). The caller arms one timer. There is no polling loop.
- **Authority is proven, never asserted.** A process may publish a service
  only when all three of the Z5 gates hold. This is the design's reason to
  exist; §3 states it in full.
- **Discovery is optional and absent-safe.** A build or a target without it
  — `wasm32` has no multicast — resolves `.local` to nothing and publishes
  nothing, exactly as an interface with discovery off does. Absence is never
  an error (the §18.4 unbound-node discipline).

## 1. Non-goals (permanent, not deferred)

- **No LLMNR. No NetBIOS name service. Ever.** They are the substrate of the
  Responder/Inveigh credential-theft family and carry no capability the
  charter wants. Refusing them costs nothing and closes the attack class by
  construction. This is not a staging decision to revisit.
- **No unicast DNS for `.local`, in either direction.** A `.local` query is
  never sent to a configured recursive server (a real privacy leak on
  nsswitch-ordered systems), and the unicast resolver never answers for
  `.local`. Cost, stated honestly: an enterprise using `.local` as an Active
  Directory domain is unsupported. That is the correct trade — the practice
  has been advised against since 2009 — but it is a product decision, not a
  technical inevitability.
- **No off-link responses.** RFC 6762 §5.5 and §11 are enforced strictly:
  a query whose source is not on-link for the receiving interface is not
  answered. mDNS reflection is a real ~10× DDoS amplifier.
- **No recursive or authoritative server, no DNSSEC, no DNS-SD over unicast
  (RFC 6763 §11) in this plan.** Each is a later increment or its own plan
  (§2.3).

## 2. The failure this plan is designed around

Shipped zeroconf implementations share one structural flaw: **advertisement
authority is self-asserted.** Any local process can register any service type
on any port — `DNSServiceRegister("_ssh._tcp", 22)` succeeds for any caller on
macOS, and Avahi's only control is the all-or-nothing
`disable-user-service-publishing`. A malicious application therefore appears
in every peer's browser as the file server, the printer, or the login host,
which is a credential-harvesting position obtained with no exploit.

TAIRiX is able to close this because it has three things no mainstream system
has together: signed bundle manifests (§16.5), a kernel-attested
`AppIdentity { bundle_id, publisher }` on every IPC caller
(`lib/abi/src/origin.rs`, filled in `kernel/sec/captable.rs`), and an
origin-keyed socket table that knows which `ProcId` owns which port
(`userland/net/netstack/src/socket.rs`).

Secondary weaknesses this plan also closes: unauthenticated cache poisoning
across interfaces, whole-LAN inventory reconnaissance available to any
process, identity leakage onto untrusted networks, an unauthenticated remote
parser running inside a privileged long-lived daemon, and stale
advertisements outliving the service that published them.

## 3. Publication authority — the three gates (binding)

To publish `<instance>._<type>._tcp.local` on port *P* over interface *I*, all
three must hold. They answer three different questions and none substitutes
for another.

1. **Attested — who is calling?** `discoveryd` reads the caller's `Origin` and
   requires an `AppIdentity`; a caller with none may not publish. The identity
   is taken from the kernel, never from the wire (the `confd` discipline).
   Because `AppIdentity` carries `publisher`, a different developer shipping
   the same `bundle_id` is a *different* principal and inherits nothing.
2. **Granted — may it claim this type?** The bundle's signed `AppInfo`
   declares a `publishes` set of service types. **A manifest declaration is a
   request, not an authorisation** — a bundle signs its own manifest, so a
   signature proves authorship only. The effective right is the §5.2
   intersection: *manifest request ∩ grant recorded for that `AppIdentity`*.
   The default grant set is **empty**.
   - A **reserved** type (`_ssh._tcp`, `_smb._tcp`, `_ipp._tcp`,
     `_afpovertcp._tcp`, `_nfs._tcp`, `_http._tcp`, …) — the set worth
     impersonating — requires an explicit administrative grant and is never
     auto-granted at install.
   - A non-reserved type may be auto-granted at install from the manifest
     request, because publishing `_myapp._tcp` claims no established role.
   - The reserved set is **data**, carried beside the content-type registry
     (`plans/ICONS.md` precedent), never a `match` arm in code.
3. **Owned — is that endpoint actually theirs?** `netstack` confirms the
   calling `ProcId` holds a listening socket on port *P* bound reachably on
   *I*. This is the gate that stops one process advertising another's
   endpoint. It is **not** an authority gate: ports ≥ 1024 need no capability,
   so owning a port says "a peer connecting here reaches you", nothing more.
   Ownership is re-checked on the interface-change edge, and the advert is
   withdrawn the instant the socket closes.

Every grant, refusal, publish, and withdrawal is a §19.4 hash-chained audit
event with a stable id. Refusals state their reason to the caller (§2.24)
without leaking whether a *different* principal holds the type.

## 4. Containment — the decoder holds no authority

Per §19.5 the mDNS decoder is untrusted-input parsing and must be sandboxed.
The `lib/sandbox` request/reply worker (`host::request`) is **the wrong shape
here**: it is a synchronous round trip returning an owned `Vec` per call, sized
for the `timesync` case of one NTP reply per hour. mDNS on a populated segment
is hundreds of packets per second at a rate the attacker chooses, so a
per-packet round trip is both a §2.16 defect and a self-inflicted
denial-of-service channel.

So `discoveryd` is two processes, and the one that parses holds nothing:

- **The decoder** is the service's own binary in the kernel's sandbox spawn
  mode: capability-empty, confined to one pipe pair, with no IPC, no clock, no
  randomness, no filesystem, and no spawn. It runs one `lib/net::mdns` engine
  per interface. Everything it needs arrives on its pipe: each relayed
  datagram carries the instant it was relayed, a tick carries time when the
  decoder's reported deadline comes, and the keys its caches are indexed under
  and its CSPRNG is seeded from arrive once, first, drawn by the front for
  that decoder alone. It answers only with the one instant its engines next
  need time.
- **The front** owns the UDP 5353 sockets and the group memberships, and every
  authority the service will exercise: grants, attested identities, the
  netstack ownership check, the service-manager lifecycle, and the client API.
  It never parses a wire byte. It relays a datagram only when the stack found
  its sender on-link and the sender is within its relay budget; the payload
  crosses unread.

The socket belongs to the front because the sandbox spawn mode forbids IPC,
and a socket is IPC: a decoder that owned one would hold `CAP_NET` and a path
to the stack — a network-capable process parsing hostile input, the posture
§19.5 exists to rule out. Relaying costs one pipe write per datagram and no
round trip.

The streaming shape is `lib/sandbox::supervise::SupervisedSession`: the duplex
session seam with the one-shot seam's containment (typed error, reap, log,
replace), paced so a crafted packet cannot buy a process spawn — replacement
waits a delay doubling from 100 ms to 30 s, forgotten once a replacement stays
up 30 s — and counted in generations, so the owner knows to discard what the
dead worker told it and re-key its replacement. It lives in `lib/sandbox`, not
beside `discoveryd`, so the second consumer finds it (§2.2).

## 5. Consumption — scoped, not ambient

- **Browse is scoped by grant.** An application that declares and is granted
  `_ipp._tcp` sees printers and nothing else. Whole-LAN enumeration is
  reconnaissance; on other systems it is free to any process.
- **Unrestricted browse is a separate authority.** `CAP_NET_ADMIN` is the
  wrong instrument — it reconfigures interfaces, and a file manager's Network
  view must not hold that. This is a genuine §5.2 item-3 split (a demonstrated
  need to grant one without the other), so a narrow `CAP_NET_DISCOVER_ALL`
  lands **with its enforcement point in Z4** and never before (§5.2 item 2).
- **Caches are per-interface and never merged.** A record learned on hostile
  Wi-Fi can never answer a query scoped to the wired LAN. Interface scope is
  part of every cache key and every answer.
- **Resolution returns an address, not a capability.** A one-shot connect
  capability to a "verified" endpoint was considered and **rejected**: plain
  mDNS authenticates nothing, so a verified address is no safer than a
  resolved one, and the existing `ServiceManifest.connect_capability` is a
  gate *to* a service, not a handle to an endpoint — reusing the name would
  invent ABI for no security gain (§2.3). It becomes meaningful only atop Z7,
  and is reconsidered only if Z7 lands.
- **`.local` routing has one policy point**, in `lib/resolver` beside the
  existing unicast ordering, so no consumer re-derives it (§2.2, the §16.8
  one-policy discipline). Link-local reverse resolution (`169.254/16`,
  `fe80::/10` → mDNS `PTR`, RFC 6762 §11) routes there too.

## 6. Navigating the network — a catalogue, never a path

### 6.1 The rejected design

Every mainstream system projects discovery into the filesystem: macOS
`/Network`, GNOME's gvfs under the user's home, Explorer's UNC roots. TAIRiX
does not, and the reason is not taste.

A directory is a set `readdir` enumerates. A discovery result is none of what
that implies:

- **Not a set.** mDNS returns whoever answered inside a time window. There is
  no complete listing, so a `readdir` claiming to have returned one lies.
- **Not bounded in latency.** `stat` on such an entry waits on a stranger's
  host.
- **Not ours.** The names are authored by unauthenticated peers on the segment.

The consequences are not hypothetical: a synthetic network directory is why
`find /` hangs, why backup and sync tools wander onto the LAN, and why a file
manager freezes when a host goes dark mid-enumeration. `drives.md` §25 already
rejects "`/mnt` / `/media` under another name" and a new default-view entry
without a charter amendment; a `/Network` root would be both.

**The invariant: no path in TAIRiX ever resolves by asking the network who
exists.** `ls /` touches no interface. A discovered service that has not been
mounted has no path at all, so nothing — no glob, no recursive delete, no
scrub, no backup — can reach it by accident.

### 6.2 The split

- **Browsing is a query.** `lib/discovery` answers a typed, bounded,
  cancellable catalogue query. It is not a filesystem and has no path grammar.
- **Mounting is what creates a path.** Attaching a discovered share publishes
  an ordinary alias projected into `Storage:`, exactly as removable media is —
  which `drives.md` §10 already names as the home for network shares. No new
  resolver, no new root, no amendment.

The journey is *browse a catalogue → choose → mount → work in a normal path*.
The transition from untrusted to addressable is one explicit, audited,
capability-gated act, and nothing before it sits in anyone's namespace.

### 6.3 Mount policy for a discovered share

A share found by multicast from an unauthenticated peer is the least trusted
volume the system can hold, so it is mounted **more** strictly than a local
disk:

- `nosuid,nodev,noexec` with **no relaxation path**: `CAP_FS_MOUNT_RELAX` does
  not apply. Letting a stranger on the segment place executable content in the
  user's namespace is not a setting.
- `CAP_FS_MOUNT` as any mount, with the endpoint and its resolved address in
  the audit record.
- The foreign filesystem/protocol client parses hostile data, so it is
  sandboxed (§19.5) and declares its lossy semantics through the filesystem
  capability API (`drives.md` §20).
- The mount uses the endpoint **as displayed at the moment of choice**. If
  re-resolution disagrees, it fails closed and re-presents rather than
  silently mounting a different host.

### 6.4 Credentials pin to an endpoint, not to a name

The three gates bind *local* publication and say nothing about a hostile peer,
so a fake `_smb._tcp` called "Backup" is the residual attack — and it is a
credential-phishing one.

- Nothing is auto-mounted, and discovery alone never raises a credential
  prompt. A prompt follows a user's explicit choice, never an arrival.
- A stored credential binds to a **pinned endpoint identity**, never to a
  display name. A different host claiming a pinned name presents as new and
  unknown, and a credential is never offered to an endpoint whose pin does not
  match (fail closed).
- For TAIRiX peers Z7 would make that pin cryptographic. For foreign SMB/NFS
  the pin is all there is, and this plan claims no more.

### 6.5 The file manager's Network view

`files.app` **reaches** discovery; it does not reimplement it — the same
relationship it has with the mount table today, and consistent with its
refusal to grow subsystems owned elsewhere.

- **One renderer.** The view supplies entries to the existing
  `ListView`/`GridView` through a second source beside `DirectorySource`.
  `Browser`'s location becomes a sum type — a path, or the catalogue — so
  there is no second renderer and no fake path.
- **Storage-class types only.** A printer is not a file manager's business.
  This makes `files.app` the worked example of §5's grant-scoped browse: it
  declares the storage types and does **not** hold `CAP_NET_DISCOVER_ALL`.
- **Activation mounts.** Choosing an entry performs §6.3 and navigates to the
  resulting `/Storage/<name>`, an ordinary path from that instant.
- **§28, absolutely.** Discovery is I/O and never runs on the event loop: the
  view requests through the existing reader desk and collects. Records arrive
  continuously, so arrivals coalesce and repaint once per present rather than
  once per record, scoped to what changed.
- **Browsing runs only while the view is open.** A multicast query announces
  what you are looking for to the whole segment; browsing continuously in the
  background, as other systems do, leaks that permanently and wakes the
  machine for nothing.
- **Liveness is shown honestly.** A peer that vanishes without a goodbye
  lingers until its TTL; the view marks it unavailable rather than presenting
  a dead host as reachable, the discipline the places rail already applies.
- **Per-source display fairness.** One peer cannot crowd the list: a
  per-source display quota with a visible "N more from this host" — the
  `omission` record kind on `stdinfo` for the command-line equivalent — never
  a silent truncation.
- **A discovered entry is visually distinct from a mounted volume**, and its
  icon derives from the *service type* in the curated registry, never from
  anything the peer supplies. A stranger must not be able to draw itself as
  the user's own disk.
- **The trusted picker gets no Network view.** It composes the renderer with
  no rail today because a machine-wide device list is outside the authority it
  is given; inducing a user to mount a network share while an app waits on a
  file is further outside it still.

### 6.6 Escalation — untrusted display text is a live gap, wider than this plan

Designing §6.5 surfaced a defect that already exists and is **not** zeroconf's
to fix. Recorded here rather than scoped around:

Two sanitisers guard untrusted text and they disagree.
`lib/controls::window::sanitize_label` replaces control characters with a space
and caps length at 512; `lib/browse::vfs::shown_name` replaces them with U+FFFD
and caps nothing. Both filter on `char::is_control`, which is the `Cc` category
alone. Confirmed against this toolchain: U+202E RIGHT-TO-LEFT OVERRIDE and
U+200B ZERO WIDTH SPACE are `Cf`, U+0301 is `Mn`, and Cyrillic U+0430 is `Ll` —
**all four pass both filters**, and the console atlas covers Cyrillic and Greek,
so a homograph name renders indistinguishably from its Latin twin today.

This reaches any untrusted name drawn on any surface: a FAT32 volume's
filenames reach it now, with no network involved. The fix is one shared
display-safety policy in `lib/controls` — the single definition the two
divergent helpers should already have been — not a `files.app` or
discovery-side filter. It is a **prerequisite for Z9**, not part of it, and is
raised for a decision on where it is scheduled.

## 7. Posture and privacy

- **Per-interface `discovery.mode`** — a new `lib/netconfig` `IfaceKey`:
  `off` / `browse` / `publish`. **An unclassified network defaults to
  `browse`**: consume the segment, announce nothing. Fail-closed (§5.4)
  applied to a protocol whose default posture the field got backwards.
- **Per-network host identity.** The published label is derived with
  `lib/hash`'s keyed SipHash from (per-install secret, interface, network
  selector), so one LAN sees a stable name while two LANs cannot correlate
  one host. The real hostname is published only where the user set `publish`.
  - **This is privacy hygiene, not a security boundary, and the plan says so.**
    The network selector (gateway MAC / SSID / DHCP server id) is
    attacker-controlled in both directions: forgeable to force correlation,
    changeable to force churn.
  - **An advert MUST carry the RFC 8981 temporary address, never the stable
    one** (N10, already shipped). A randomised hostname published over a
    stable `AAAA` is theatre, and shipping it would be worse than shipping
    nothing, because it looks like a defence.
- **Conflict resolution is bounded.** RFC 6762 §9 renaming is weaponisable: a
  peer that keeps claiming our name walks us to `host-47.local`. After a
  bounded rename budget the interface fails closed to *not published*, raising
  a §NOTICE and an audit event, rather than silently degrading.

## 8. Resource bounds (§24.4 — fixed, not capacities)

The cache is fed by hostile peers, so its bounds are **security bounds, fixed
and fail-closed**, never §24.1 growable capacities:

- A fixed per-interface record ceiling, and a fixed per-source-host ceiling
  within it, so one flooding peer evicts **its own** entries rather than
  everyone else's.
- Response rate limiting per RFC 6762 §6 (one response per record per second)
  plus a per-peer, per-interface budget.
- Under §26.7 (1 GiB RAM, several active segments) total resident discovery
  state is bounded by the per-interface ceilings times the interface count —
  a figure independent of segment population and of attacker behaviour.
- The front's relay is bounded before a byte reaches the decoder: at most 32
  senders tracked, each held to a burst of 64 datagrams refilled at 32 a
  second, beneath one budget of 2048 at 1024 a second that all of them share.
  A sender past its own budget is refused before the shared one is charged,
  so one flooding peer cannot starve the rest. What the decoder has not yet
  taken is at most 64 KiB queued, after which the front stops draining its
  delivery port and the stack's mailbox (64 deep) drops; each wake drains at
  most one mailbox's worth. Ticks are never closer together than 10 ms,
  whatever deadline a decoder reports.

## 9. Interop obligations

Strictness must not break correct peers:

- **Legacy unicast queries** (RFC 6762 §5.5 — source port ≠ 5353) are
  *handled per spec*, not blanket-refused: answered unicast, with the
  shortened TTL the RFC requires. Refusing them breaks real clients.
- **The QU (unicast-response) bit** is honoured within the on-link rule.
- Anything Z7 adds is strictly additive: a standard DNS-SD client that
  ignores unknown records must interoperate unchanged.

## 10. Layout

```
lib/net/src/mdns.rs         pure engine: state machine, cache, suppression
lib/net/src/dnssd.rs        instance/type/domain triple, TXT grammar
lib/sandbox/src/supervise.rs  the supervised streaming session
lib/discovery/              userland client (mirrors lib/resolver)
lib/abi/src/discovery_ipc.rs     the discovery-v1 session ABI
lib/abi/src/discovery_policy.rs  the grant store codec
userland/net/discoveryd/    front + decoder
userland/apps/dns-sd/       the command app
```

`lib/net/src/rxfilter.rs` needs **no change**: it gates group destinations on
membership alone, so joining `224.0.0.251` / `ff02::fb` admits the traffic
(its module docs name mDNS as noise precisely because nothing joins today).

## 11. Capabilities

Z1–Z3 and Z5 add **no** capability: publication authority is the §3 gate
composition, which is stronger than a capability would be and adds no
vocabulary (§5.2 minimalism). Z4 adds exactly one,
`CAP_NET_DISCOVER_ALL`, with its holder and enforcement point in the same
increment. Posture changes (§7) are `CAP_NET_ADMIN`, which already covers
interface reconfiguration.

## 12. Stages

### Z1 — the pure `lib/net::mdns` engine — **done**

`lib/net/src/mdns.rs` and its `codec` / `cache` / engine submodules carry the
whole of it: the message codec over `lib/net::dns`'s own `Name` and
`RecordType`, the bounded per-interface `RecordCache`, and the `MdnsEngine`
responder/querier. Pure — no service, no ABI, no netstack change.

What it now guarantees, and the decisions a later increment must not
re-derive:

- **`dns::RecordType` is the wire vocabulary; `dns::LookupType` is the
  subset a stub lookup can ask for.** Widening `RecordType` with `SRV` /
  `TXT` / `NSEC` left the stub resolver's `Answer` with no shape for three
  of its variants, so the resolver takes the refinement type instead. A
  consumer that wants "the types `host -t` accepts" reads `LookupType::ALL`
  rather than keeping a list.
- **`dns::Name` preserves case and compares without it** (RFC 4343). It
  used to fold on decode, which would have destroyed the display spelling
  of every DNS-SD instance name before Z2 could ever read it. `PartialEq`,
  `Hash`, and `Ord` fold; the octets do not. `Name::from_labels` builds a
  name from raw label octets (an instance name is free-form UTF-8, which
  `Name::encode`'s host-name rules refuse) and `Name::labels` reads them
  back — the pair Z2's instance/type/domain split needs.
- **The cache index is keyed and the bounds are fixed.** Records chain
  under a keyed hash of (owner name, type), so a peer cannot choose a name
  set that collapses into one chain. `MAX_RECORDS` (512) and
  `MAX_RECORDS_PER_SOURCE` (32) are §24.4 bounds; a full slot table is
  about half a mebibyte per interface, grown on demand, and independent of
  segment population. A source at its own ceiling evicts its own oldest
  record; only a globally full table evicts by recency.
- **A derived `NSEC` is announced and answered with, never probed.** A
  unique publication gains an `NSEC` asserting exactly the types published
  at the name, but it is excluded from the probe's authority section and
  from the RFC 6762 §8.2 comparison: a record a peer's probe cannot carry
  would decide every tiebreak in our favour.
- **Defending our own name is never charged to a budget.** Unicast replies
  are budgeted per peer and per interface, but a probe for a name we own is
  answered at once and multicast, outside both budgets and outside the
  one-per-second rule. Otherwise draining the budget would be a lever for
  taking a name.
- **Renaming is bounded and then fails closed** to *not published*, with a
  `ConflictBudgetExhausted` event — the §7 posture, implemented.
- **Whether a sender is on-link is the stack's verdict, not the engine's.**
  `on_message` takes a `Sender { addr, port, on_link }`, and `on_link` is
  what `netstack` stamped on the delivery from its live routes
  (`Stack::is_on_link`: link-local, or covered by a route with no gateway).
  The engine keeps no copy of any interface's prefixes to go stale; a sender
  that is not on-link is neither answered nor cached.
- **`rate::TokenBucket` is the one token bucket in `lib/net`, and
  `rate::PeerBudgets` the one per-peer budget table** — a bucket per recent
  peer beneath one they all share, the peer's charged first so a peer at its
  own limit cannot drain the shared one. The engine's unicast replies and
  `discoveryd`'s relay admission both take it; a second protocol needing a
  rate limit reaches for these, never a private copy and never across a layer
  into `icmp`.

### Z2 — `lib/net::dnssd` vocabulary — **done**

`lib/net/src/dnssd.rs` carries the RFC 6763 §4.1 naming grammar and the §6
`TXT` key/value rules: `ServiceInstance` / `ServiceType` / `InstanceName` /
`Transport`, the name they spell and are split from, and
`TxtAttributes` / `TxtBuilder` over `mdns::TxtRecord`. Pure — no state, no
allocation, no wire codec of its own. Host-tested and folded into the Z1
fuzz harness.

What it now guarantees, and the decisions a later increment must not
re-derive:

- **One type/domain parser and one assembler.** The instance form calls the
  service-type parser on the suffix after the instance label rather than
  repeating the split, and both `to_name` paths share one assembler. The
  split and the assembly work over the name's own wire octets through
  `dns::Name`'s reader, so a deep domain costs no arbitrary label ceiling —
  sound because a label length never carries the two high bits a compression
  pointer is spelled with. `mdns::MAX_RENAME_LABELS` stays private to the
  renamer; nothing here inherits a label count.
- **Case is preserved and compared without it**, matching `dns::Name` (RFC
  4343). Two instance names differing only in ASCII case are one DNS name
  and so conflict, which is what Z5's uniqueness check needs; the owner's
  spelling survives for Z9 to draw. `Hash` is deliberately absent — there is
  no caller, and a derived one beside the manual `PartialEq` is a
  compile-time error rather than a silent trap, so the first increment that
  needs a map key writes the consistent one.
- **Structure is validated; drawability is not.** The §6.6 display-safety
  policy is one shared definition above this crate, so `dnssd` implements
  the RFC's length / character / encoding MUSTs and stops. It adds no
  homograph, bidi, or `Cf`/`Mn` filter, and applies no NFC normalisation
  (RFC 5198) because the Unicode tables that needs do not belong in a
  `no_std` wire crate. `InstanceName` renders nothing and offers no
  `as_str`, so no unsanitised display path looks like the obvious one.
- **Reading a `TXT` record is linear, and `get` is first-wins.** A peer
  authors the record and chooses the arrival rate, so the iterator yields
  duplicates rather than folding them — deduplicating inside the walk would
  be quadratic on the receive path. RFC 6763 §6.5's "take the first" is
  `get`'s job. The builder's duplicate check *is* quadratic and is allowed
  to be: publication happens once, off any hot path.
- **`TxtStrings` gained a `pub(crate)` slice constructor** so the builder
  reads back its own partial rdata through the one string walker rather
  than growing a private copy.
- **Subtypes (RFC 6763 §7.1 `_sub`) are deliberately out.** A subtype is
  selective *enumeration*, not part of the instance-name abstraction, and
  browse does not need it; an increment that does adds it in place. RFC 6763 §9's service-enumeration
  name needs nothing: it is instance-shaped and the ordinary triple reads
  it.

### Z3 — `discoveryd`: the split process — **done**

`userland/net/discoveryd/` is the front and the decoder (§4) in one bundle,
`/System/Services/discoveryd.app`. `lib/sandbox/src/supervise.rs` is the
streaming seam it runs the decoder under.

What it now guarantees, and the decisions a later increment must not
re-derive:

- **The decoder never holds a socket** — the §4 inversion, because the
  sandbox spawn mode forbids IPC. The front relays datagrams over the
  session, and reads back only fixed, bounds-checked fields.
- **The channel is fixed-shape and both readers fail closed** (`wire`). Every
  frame is one layout behind a tag; either side refuses a frame whose tag,
  length, or any field an honest peer would not send. A configuration comes
  first and exactly once, and anything else ends the decoder.
- **Admission before relay, in a fixed order.** The stack's on-link verdict
  first, then the sender's own budget, then the shared one (§8), all before a
  byte is encoded.
- **Back-pressure costs nothing per datagram.** A datagram the decoder's
  queue has no room for is held, and the front stops draining its delivery
  port until the queue drains, leaving the stack's bounded mailbox to drop.
- **Time is paced by the front, not the decoder.** A tick goes only when the
  decoder's reported deadline has come, never before the decoder has reported
  since the last tick, never within 10 ms of the last, and a tick the queue
  has no room for waits on the queue's room, not a timer — so a decoder
  reporting any deadline at all cannot make the front spin.
- **Containment forgets everything.** A decoder that crashes, breaks the
  framing, ends its stream, or sends a frame no decoder sends is reaped and
  logged (`lib/sandbox`'s `6000`, with the reason). Everything learned from
  it is dropped, and its replacement starts after the paced delay under keys
  drawn afresh. A random source that cannot key a decoder stops the service
  rather than run one under predictable keys.
- **Events are `25_000..26_000`:** service started (with which families
  joined), decoder started (with its generation), service unavailable (with
  its reason).
- **Proven by test:** host tests over in-process workers, including the real
  decoder end to end and doomed, lying, and back-pressured ones; the
  `fuzz_discoveryd` harness (both codecs canonical, the decoder total over
  hostile datagrams, the front whole against a decoder saying anything); and
  the `sandbox_qemu_aarch64` supervised leg, where a stream worker dying
  through its panic path mid-conversation is reaped and logged, and its
  replacement starts only after the paced delay on a real one-shot wait and
  serves on a fresh pipe pair.

### Z4 — browse, resolve, `.local` routing — **done**

A program browses, resolves, and looks up link-local services through
`lib/discovery` and the `discovery-v1` session ABI (`lib/abi::discovery_ipc`)
on `discoveryd`'s reserved endpoint; `lib/resolver` routes every `.local` name
and link-local address there; `dns-sd` is the command app. `discoveryd` runs
under its own account (uid 20, `lib/users::provision`) with
`DISCOVERYD_CEILING` equal to its manifest, and PID 1 starts it at boot.

Decisions (binding):

- **Browse authority is declared ∩ granted.** A bundle's signed `AppInfo`
  carries a `browses` table of service types (the `mime_count` table
  precedent); the grant is recorded for the kernel-attested
  `AppIdentity { bundle_id, publisher }` in a fail-closed, line-oriented store
  at `/System/Security/Policy/Discovery` (`lib/abi::discovery_policy`). That
  path is on the read-only `/System` volume, which no projection shadows, and
  the image builder is its only writer: for every bundle it plants whose
  verified manifest declares `browses`, it records exactly that set. Every
  other identity, and every caller with no `AppIdentity`, holds nothing. The
  service reads the store once at start and takes it whole or not at all. The
  administrator's layer in `/System/Settings` and its grant/revoke surface land
  with Z5, whose reserved publish types are their first writer; the store grows
  `publish` lines there, never a second store.
- **`CAP_NET_DISCOVER_ALL` is administrative.** It is in `ADMINISTRATIVE_SET`
  beside `CAP_SYSINFO_GLOBAL`, because enumerating a whole segment is the same
  reconnaissance class as listing every process. Its live holder is the
  `dns-sd` command app, and its enforcement point is the front's request
  admission: it unlocks every browse and resolve, and is the only thing that
  unlocks the RFC 6763 §9 type enumeration. A caller without it gets exactly
  its granted types and a refusal (`25_004`) naming what it lacked, never
  whether another principal holds the type. `files.app` never holds it.
- **UDP 5353 and the groups are reserved to the service account.** The stack
  refuses, audited, a bind of 5353 in either transport and a connect or send
  to `224.0.0.251:5353` or `[ff02::fb]:5353` from any principal but the
  `discoveryd` account, keyed on the attested uid, so no capability is added.
  Unicast to one peer's 5353 stays open: it names a host, not the segment.
- **Enrolled at boot, restart on failure, only while the stack is up.** The
  service listens from boot — caching what the segment volunteers costs no
  transmission — and transmits only the questions live clients are asking. It
  requires `network-up`, which `netstack` provides once its endpoints are
  bound, so it never races the stack's start, and because its sockets live in
  the stack, a stack relaunch withdraws the condition, stops it, and brings it
  back against the new stack instead of leaving it deaf.
- **A dead client's questions end with it.** The kernel's `peer_watch` exit
  feed (one wait-set source, drained like signal intake, its registry owned by
  the kernel state) is what both `discoveryd` and `netstack` release a dead
  principal's state on; `netstack` reclaims a dead owner's sockets, groups, and
  ports, so a restarted `discoveryd` can rebind 5353. SVC-9's connection sink
  is the next consumer.
- **A socket learns where its membership is live.** The stack holds each group
  once per logical interface however many sockets join it, carries every
  membership onto an interface as it is added or composed into a bond, and
  tells each member socket every interface where its family can speak — the
  link up and a source address of the family held — and every edge, a flap as
  down then up. A socket is told from a remembered view, so an edge a full
  port had no room for is told later rather than lost. The port space is per
  family, so the service holds 5353 in both.

What it now guarantees:

- **Questions are shared and replayed.** Each distinct question is asked once
  on every up link however many requests share it; a request joining one
  already asked is brought up to date by a replay from the decoder's cache.
- **The front holds the decoder to its word.** It keeps keyed fingerprints of
  what each question holds per link: an answer is added once, renewed or
  retired only while held, never past `MAX_RECORDS` for one question on one
  link. Stale answers (across a stop, a flush, or an edge in flight) are
  dropped; impossible ones condemn the decoder.
- **Nothing crosses a link edge.** The decoder acknowledges every link edge
  (`Linked`); an answer about a link is believed, and a datagram built for it
  sent, only once every edge told has been acknowledged.
- **Transmit is bounded and link-pinned.** `SocketRequest::Send` names its
  egress interface. The front sends to the group within a per-interface budget
  (a burst of 32 at 8 per second), or directly to a peer it relayed from on
  that interface in the last second, once per datagram relayed and at most four
  owed at once.
- **Clients are bounded (§8).** 8 sessions per account, 16 requests per
  session, 64 KiB of queued answers per session (then one `Lost` notice), 256
  sessions in all; one doorbell outstanding per session, authenticated by the
  service's uid.
- **Every answer names its interface; caches never merge.**
- **`.local` never leaves the link, and absence is not an error.** A build
  without discovery answers a link name with nothing.
- **Proven by test:** host tests for the ABI codecs, the grant store, the
  front, the decoder, the question table, the sessions, the planner, the
  client, the resolver routing, and `dns-sd`; `fuzz_decode` and
  `fuzz_discoveryd` over the new frames; `fuzz_net_sockabi` over link edges,
  flaky ports, and reclaim; and the live `tairix-test-discovery-qemu-aarch64`
  vertical, where `dns-sd` browses, resolves, and looks up a host-side
  responder's service through the whole path and the responder requires that
  the guest asked the wire for every record. x86_64 and riscv64 build the
  same service and command into their boot floor and start it in every
  production boot; their live client vertical rides the net-tool shell world
  those targets do not yet have, as `ping` and `telnet` do.

### Z5 — publication and the three gates

`AppInfo` `publishes` section (the `mime_count` declarations precedent); the
grant store and the reserved-type data set; the netstack ownership query; the
audit events. The increment this plan exists for.

The gates bind only what reaches the wire through `discoveryd`, which is why
Z4 reserves UDP 5353 to the service account: no other principal can speak
multicast DNS around them.

### Z6 — posture, privacy, lifecycle

`discovery.mode` in `lib/netconfig`, with the group memberships joined and
left per interface as its posture allows (a socket's join covers every
interface today); per-network identity bound to the RFC
8981 temporary address; service-manager advertise-on-ready and
**goodbye-on-stop** (deterministic withdrawal, not TTL decay); discovery-driven
on-demand activation over the existing SVC-4 path.

### Z7 — attested records — decision pending

Sign announcements with the host key and TOFU-pin as `plans/SSH.md` does, so
on-link impersonation becomes detectable — the one attack the §3 gates cannot
reach, because they bind *local* publication and say nothing about a hostile
peer on the segment.

**Recorded reservation:** this is a protocol extension only TAIRiX speaks, it
needs a pairing story, and it risks being the §2.3 bloat the charter warns
about. Z1–Z6 deliver the security result without inventing protocol. Z7 is
built only on an explicit decision to build it, and is dropped cleanly
otherwise — nothing in Z1–Z6 depends on it.

### Z8 — the catalogue and the mount path

The `lib/discovery` catalogue query (incremental, cancellable, bounded,
per-source-fair); the storage-class service-type set as data beside the
reserved set; endpoint identity pinning and the §6.4 credential rule; the
discovered-share mount through the storage resolver with §6.3's forced flags
and no relaxation. Host-tested, plus a live QEMU vertical that mounts a
discovered share and proves `/` never touched an interface to find it.

### Z9 — the `files.app` Network view

`Browser`'s location sum type and the catalogue source over the existing
renderer; reader-desk integration with coalesced arrivals; per-source display
quota; service-type icons; liveness marking; browse only while the view is
open; no view in the trusted picker.

**Prerequisite:** the §6.6 shared display-safety policy. Z9 does not start
until that lands, because drawing attacker-authored names on a trusted
surface is the whole point of the view.

## 13. Cross-references

`plans/NETWORK.md` (socket ABI, multicast membership, rx pre-filter, the
two-process vertical shape) · `plans/DNS.md` (the `Name`/codec this reuses) ·
`plans/NEW-SERVICEMANAGER.md` (SVC-4 activation, readiness and stop edges) ·
`plans/APPS.md` (`AppInfo` manifest sections) ·
`plans/NEW-FILEMANAGER.md` (the renderer, the places rail, the reader desk) ·
`docs/src/filesystem/drives.md` (§10 `Storage:`, §20 foreign filesystems, §25
rejected designs) · `plans/NEW-NAMESPACE.md` (volume attachment vs projection) ·
`plans/CAPABILITY_USE.md` (grants, ceilings, admin) · `plans/SSH.md` (host
keys and TOFU, for Z7) · `plans/SYSLOG.md` (the audit events).

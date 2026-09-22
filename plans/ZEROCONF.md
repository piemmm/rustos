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
| Z2 | `lib/net::dnssd` DNS-SD vocabulary: the instance/type/domain triple, TXT key-value grammar, service-type grammar | planned |
| Z3 | `discoveryd` split-process skeleton: unprivileged decoder owning the socket, privileged front owning authority | planned |
| Z4 | Browse + resolve, scoped by grant; `.local` routing in `lib/resolver`; `lib/discovery` client | planned |
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

So the containment is inverted, and `discoveryd` is two processes:

- **The decoder** owns the UDP 5353 socket and the group memberships, and
  holds *nothing else* — no filesystem capability, no spawn, no manifest
  access, no grant state. It parses, validates, applies the cache and
  rate-limit bounds, and streams fixed-shape, already-validated records to the
  front over one channel. A crash is contained and it is replaced, the
  `lib/sandbox::host` discipline (typed error, reap, replace, log) applied to
  a long-lived streaming worker rather than a request/reply one.
- **The front** owns authority: grants, attested identities, the netstack
  ownership check, the service-manager lifecycle, and the client API. It never
  parses a wire byte.

**This extends the `lib/sandbox` seam** — a worker that owns a socket and
streams, where today every worker is request/reply over a channel alone. That
extension is part of Z3 and is built in `lib/sandbox`, not privately beside
`discoveryd`, so the second consumer finds it (§2.2).

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
lib/net/src/mdns.rs        pure engine: state machine, cache, suppression
lib/net/src/dnssd.rs       instance/type/domain triple, TXT grammar
lib/discovery/             userland client (mirrors lib/resolver)
userland/net/discoveryd/   front + decoder
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
- **`rate::TokenBucket` is the one token bucket in `lib/net`.** A second
  protocol needing a rate limit reaches for that, never a private copy and
  never across a layer into `icmp`.

Still Z3's, not done here: nothing binds a socket, joins a group, or holds
an identity. The engine is handed the interface's on-link prefixes and
refuses everything else; who supplies them is the service's question.

### Z2 — `lib/net::dnssd` vocabulary

The instance/type/domain triple with the RFC 6763 §4.1 grammar and §6 TXT
key-value rules, bounded and total. Host-tested, folded into the Z1 harness.

### Z3 — `discoveryd`: the split process

The `lib/sandbox` streaming-worker extension (§4); the decoder owning the
socket and memberships and holding nothing else; the front holding authority;
crash containment and replacement proven by test.

### Z4 — browse, resolve, `.local` routing

`lib/discovery` client; grant-scoped browse; `CAP_NET_DISCOVER_ALL` with its
enforcement point; `.local` and link-local reverse routing in `lib/resolver`;
per-interface answer scoping. A live two-process QEMU vertical, the N4e-β
precedent.

### Z5 — publication and the three gates

`AppInfo` `publishes` section (the `mime_count` declarations precedent); the
grant store and the reserved-type data set; the netstack ownership query; the
audit events. The increment this plan exists for.

### Z6 — posture, privacy, lifecycle

`discovery.mode` in `lib/netconfig`; per-network identity bound to the RFC
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

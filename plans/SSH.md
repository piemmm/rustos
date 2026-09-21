# SSH — the secure shell, client and server, first class

Binding under `AGENTS.md`. TAIRiX has **no remote access at all**: every
network consumer in the tree today is a client (`ping`, `host`, `telnet`), and
nothing has ever called `listen`/`accept` in anger. This plan owns the whole of
SSH — the protocol engine, the server, the client, the key tooling, the agent,
file transfer, and forwarding — and the privilege architecture that makes
exposing a shell to a hostile network defensible.

Read first (§15.18): `AGENTS.md` (all of it, especially §2, §4, §5, §16, §19,
§20, §24, §26, §27); `plans/NETWORK.md` (the socket ABI, and the N6b-2 listener
with its SYN-flood defence that `sshd` is the first real consumer of — where
they touch, NETWORK.md's decisions stand); `plans/PTY.md`
(`pty_create`/`pty_set_size` and the shared line discipline the session channel
hosts a shell over); `plans/SPAWN.md` (`SpawnAttach`, `target_uid`, the pipe
wiring); `plans/USERS.md` (the service-account and grant registries);
`plans/CAPABILITY_USE.md` (manifest request ∩ user grant); `plans/APPS.md`
(bundle layout, command apps, `Help/`); `plans/SYSLOG.md` (audit events);
`plans/TELNET.md` (the pure-engine and two-thread-client precedents this scales
up); `docs/src/security/sandbox.md` (the `SPAWN_FLAG_SANDBOX` brand and its
closed syscall allow-list).

`abi-v1` is **not** frozen. The `lib/abi` additions here (`SystemConfigFile`
gains a variant; `LimitKind` is discussed and deliberately not extended) are
ordinary pre-release changes (§2.13).

## Ledger

| # | Item | Status |
|---|---|---|
| S0a | `lib/crypto` extension: the algorithm set §4 admits, each with its §2.12 justification, exact pin, `deny.toml`/`supply-chain.toml`/SBOM entry, and §19.1 constant-time test | done |
| S0b | The `netstack` socket-quota defect: a derived total, a per-principal share of it, and a `net.*` administrative override — the fail-closed refusal unchanged, over an indexed socket table | done |
| S0c | `lib/sandbox::session` — the duplex, long-lived worker seam beside the one-shot `host`/`worker` pair | planned |
| S0d | `lib/compress` gains the DEFLATE **compressor** (RFC 1951) and the zlib envelope encoder (RFC 1950); the decoders already exist | planned |
| S1 | `lib/ssh` wire codec (RFC 4251 §5), version exchange, the binary packet protocol with every cipher/MAC framing, strict KEX, rekey thresholds | planned |
| S2 | KEXINIT negotiation, the exchange hash, key derivation (RFC 4253 §7), the KEX methods of §4, RFC 8308 `ext-info`/`server-sig-algs` | planned |
| S3 | Keys: blobs, the OpenSSH v1 private-key format with bcrypt-pbkdf, armour, fingerprints and randart, the `authorized_keys` and `known_hosts` grammars, OpenSSH certificates | planned |
| S4 | Userauth (none/password/publickey/keyboard-interactive/hostbased/hostbound) and the RFC 4254 connection protocol | planned |
| S5 | `sshd`: monitor, sandboxed worker, the monitor protocol, the provisioned `Settings/SSH` records and their ACL grants, host-key minting, pty + shell spawn, throttling, `lib/sshconfig`, audit, the first live QEMU vertical | planned |
| S6 | The `ssh` client and `ssh-keygen` | planned |
| S7 | Forwarding: `-L`, `-R`, `-D`, and the server-side policy with the §1.5 user-ceiling check | planned |
| S8 | `lib/sftp`, the `sftp-server` subsystem, the `sftp` client, and `scp` | planned |
| S9 | `lib/ssh::agent`, `ssh-agent`, `ssh-add`, agent forwarding, `publickey-hostbound-v00@openssh.com` | planned |
| S10 | `zlib@openssh.com` delayed compression over S0d | planned |
| S11 | Client multiplexing, `ProxyJump`, `ProxyCommand` | planned |
| S12 | `ssh-keyscan`, `ssh-copy-id`, `sshd -t`/`-T`, `ssh -Q` | planned |
| S13 | `tools/sshinterop` and the interop verticals, in both directions, against a pinned real OpenSSH | planned |
| S14 | FIDO/U2F `sk-*` key types | blocked — needs a CTAP2-over-USB-HID path that does not exist (§8) |
| S15 | `rsa-sha2-256` / `rsa-sha2-512` host and user keys | blocked — the only pure-Rust RSA carries an unpatched advisory (§8) |

---

## 0. The three things that make this not a port of OpenSSH

OpenSSH's privilege separation gives its pre-authentication child a socket, a
filesystem, and the full system-call surface, and lets that child ask the
monitor to sign things. TAIRiX can do better, because the primitives for better
already exist:

1. **The protocol engine holds no capability at all** (§1.1). It is a
   `SPAWN_FLAG_SANDBOX` process: the kernel brands it capability-empty and
   refuses every syscall but nine, none of which can open a file, a socket, an
   endpoint, a process, or a clock. It is a state machine over two pipes.
2. **There is no signing oracle** (§1.2). The monitor chooses the server's
   ephemeral share, computes the shared secret, *recomputes the exchange hash
   itself*, and signs only its own reconstruction. A compromised engine cannot
   obtain a host-key signature over anything that does not already contain that
   connection's fresh, monitor-chosen ephemeral.
3. **Nothing holds "read every user's files"** (§1.4). `sshd` reaches a user's
   `authorized_keys` through a per-inode ACL grant naming its own uid —
   search-only on the home, read-only on that one file — and parses it in the
   capability-empty sandbox. Homes stay `0700`, `sshd` cannot even *list* the
   SSH directory, and it can never read a private key. OpenSSH-as-root is
   exactly the authority TAIRiX does not have and does not need.

Everything below follows from those three, plus the ordinary charter rules.

---

## 1. Architecture (binding)

### 1.1 Privilege separation: a capability-empty engine, a monitor that never parses

Every connection is served by two processes.

**The worker** runs the SSH protocol engine and is spawned with
`SpawnAttach::sandbox`. `kernel/sec` brands it `as_sandboxed()` — user grant,
manifest request, and effective set are all discarded and cannot be
re-derived — and `kernel/syscall`'s `sandbox_allows` admits only `yield`,
`exit`, `stream_read`, `stream_write`, `fs_read`, `fs_write`, `fs_close`,
`mem_map`, `mem_unmap`. `fs_open` is *not* on that list, and a sandbox block
carries only the four standard descriptors, so the worker's entire world is the
pipe pair it was handed. It cannot open a file, reach `netstack`, call an
endpoint, spawn, or read a clock.

**The monitor** is `sshd` itself. It owns the listening socket, every accepted
socket, every pty master, every spawned shell, the host keys, and the
authentication state machine. It **parses nothing but its own typed framed
control protocol**.

Three consequences are design constraints, not afterthoughts:

- **The monitor relays session bytes** (socket ↔ worker ↔ pty master). The
  rejected alternative — a netstack socket-handoff primitive giving the worker
  its own socket — forfeits the sandbox brand entirely (a sandbox cannot issue
  the `ipc_call` a socket operation is) to save two `memcpy`s. At 1 Gbit/s with
  64 KiB frames that is roughly 250 MB/s of copy and about 2 000 extra pipe
  operations a second. The containment is worth more, and the arithmetic is
  stated here so a later increment does not re-derive it.
- **The worker has no RNG and no clock.** RFC 4253 §6 requires random padding,
  so the monitor issues the worker a per-connection reserve drawn from
  `random_get` (the one kernel CSPRNG, §22) and tops it up when the worker asks,
  event-driven, never polled. A reserve is never shared between workers and
  never reused across connections; a compromised worker knowing its own padding
  is harmless. Time-driven events — `LoginGraceTime` expiry, rekey-by-time,
  `ClientAlive` keepalive — arrive as frames the monitor sends. Byte- and
  packet-counted rekey thresholds (RFC 4344) the worker counts itself, because
  those are properties of the stream it is already framing.
- **Deadlock avoidance is explicit and structural.** Both directions of the
  monitor↔worker pipe pair are driven from the owning shard's wait-set, and the
  monitor's outbound queue to a worker is bounded by that connection's SSH
  channel window. A monitor that cannot write to a worker stops reading that
  connection's socket — TCP back-pressure — rather than blocking on the pipe.
  The monitor never blocks on a worker pipe while holding a lock another shard
  needs.

**The monitor is multi-threaded and sharded.** `lib/rt` threads with futex
`Mutex`/`Condvar`; each shard owns a wait-set over its connections' socket
delivery ports, worker pipes, pty masters, and child exits. One greedy session
cannot starve another (§26.2). Nothing polls (§2.23): every wake has a real
source. The listener thread accepts and hands each connection to a shard over a
bounded queue plus a wake; that handoff is the one place ordering matters, and
it carries a loom model (§7).

**Why `sshd` is not itself a parser sandbox.** `netstack` *is* the §19.5
sandbox for its own inputs, because it needs no authority beyond its device
channels. `sshd` cannot be: it must spawn shells as authenticated users, so it
holds `CAP_SPAWN_AS_USER`. A service that cannot be sandboxed pushes the
parsing *down* into one instead. That inversion is the whole shape of this
design.

### 1.2 No signing oracle — the monitor owns the exchange hash

For every KEX method (RFC 4253 §8, RFC 5656, RFC 8731, the ML-KEM hybrid) the
exchange hash is

```
H = hash(V_C || V_S || I_C || I_S || K_S || Q_C || Q_S || K)
```

and the server's host-key signature is over `H` alone. Hand that signing
operation to a pre-authentication process and it is an oracle: it can obtain a
host-key signature over a hash it influenced, which is the whole of host
authentication.

TAIRiX splits it so the oracle cannot exist:

- The **monitor** composes `I_S` from its own configured algorithm lists, holds
  `V_S` and `K_S`, generates the ephemeral secret and `Q_S`, computes
  `K` from `Q_C`, **recomputes `H` itself**, and signs only its own
  reconstruction. It then returns `K` and `H` to the worker, which derives the
  session keys (RFC 4253 §7).
- The worker supplies `V_C`, `I_C`, and `Q_C` as **length-bounded opaque byte
  strings that are only ever hashed**, plus a *typed* statement of which
  algorithms it negotiated.
- The monitor **validates the claimed negotiation against its own permitted
  set** and fails closed on anything outside it. So a compromised worker cannot
  cause a method the administrator forbade — SHA-1 KEX, `ssh-rsa` — to be used,
  and cannot cause the monitor to sign with a host key it does not hold.

The residual, stated rather than hidden: a compromised worker may select any
*permitted* algorithm rather than the client's most-preferred one, and may lie
about `V_C`/`I_C` — which makes `H` disagree with the client's and fails the
handshake. Neither is an escalation: every permitted algorithm is one the
administrator configured as acceptable, and failing a handshake is a denial of
service the worker could achieve by saying nothing. What it cannot do is obtain
a signature useful anywhere else, because every `H` the monitor will sign
contains that connection's fresh, monitor-chosen `Q_S`.

**Every secret is zeroed on drop, by the type that owns it.** The host private
key, the ephemeral secret, `K`, the derived session keys, and any passphrase or
password buffer live in zeroizing wrappers and are wiped before the memory
leaves scope — the userland heap does not scrub a process's own freed bytes
(§25), so the holder must. `K` and the session keys additionally never leave
the two processes that need them: the monitor computes `K` and hands it to the
one worker that derives from it, and no third party ever sees either.

The alternative considered and rejected was having the monitor parse the
client's KEXINIT name-lists itself and run the first-match rule. That decode is
small and would remove the "worker lies about negotiation" reasoning entirely —
but it puts attacker-controlled bytes back through the monitor's parser, giving
up the invariant that makes the whole design easy to audit. The invariant is
worth more than the marginal gain; the policy check above is what actually
bounds the outcome.

### 1.3 Re-encode and compare — how the monitor consumes a parse it did not perform

The monitor must act on structures it refuses to parse: an offered public key,
a signature, a certificate, an `authorized_keys` line. The rule, binding
everywhere in this plan:

> The monitor accepts a parse **only** as typed fields, and trusts those fields
> only after **re-encoding** them itself and finding the result byte-identical
> to the original bytes. It verifies signatures over its **own** encoding,
> never over bytes a parser handed it.

Applied:

- **Public-key auth** (RFC 4252 §7). The monitor reconstructs
  `string session_id || SSH_MSG_USERAUTH_REQUEST || user || service ||
  "publickey" || TRUE || algorithm || keyblob` from the typed fields it was
  given, re-encodes the key blob from typed key material, requires it to equal
  the offered blob byte-for-byte, and verifies the client's signature against
  its own reconstruction. It never verifies "the signed bytes" a worker
  supplies. Hostbased (RFC 4252 §9) and
  `publickey-hostbound-v00@openssh.com` follow identically.
- **Authorisation** compares the re-encoded offered key against the re-encoded
  key from `authorized_keys`, both canonical, so no blob comparison depends on
  an attacker's encoding choices.
- **Certificates** (`*-cert-v01@openssh.com`) are parsed in the sandbox, which
  returns typed fields; the monitor re-encodes the signed body, requires the
  bytes to match, and verifies the CA signature over its own encoding before
  reading a single principal, validity bound, critical option, or extension
  from the typed view.

### 1.4 Reading a user's `authorized_keys` without reaching into their home

`sshd` runs as its own unprivileged account (§1.6), and a home is `0700`
(`tairix_users::policy::HOME_MODE`). There is deliberately no "read every
user's files" authority in TAIRiX to reach for — which is exactly what
OpenSSH-as-root is — so `sshd` must be granted the two files it needs and
nothing else.

The mechanism already exists and is already used. `kernel/core/src/fs/perm.rs`
decides every path component in a fixed order — capability gate, then an ACL
allow/deny, then the mode triad — ARXFS persists a per-inode ACL on disk, and
`tairix_users::policy` authors those records when a home is provisioned.
`appdata_transit_security` is the worked precedent: a home and its `Settings`
/`Library` parents carry a **search-only** grant to the app-data service's uid,
because a walk to a gated root needs search on every directory it descends and
those are owned by the user, not the service. Search alone cannot list a
directory and cannot open a child whose own record refuses it.

`Settings/SSH` is provisioned as part of the fixed home shape with the same
machinery, and the records are:

| Inode | Owner / mode | ACL grant to the `sshd` uid |
|---|---|---|
| `<home>`, `<home>/Settings` | user, `0700` | search |
| `<home>/Settings/SSH` | user, `0700` | search |
| `…/SSH/authorized_keys` | user, `0600` | read |
| `…/SSH/known_hosts` | user, `0600` | read |
| `…/SSH/id_*` (private keys) | user, `0600` | **none** |

The grant names the `sshd` **uid**, so no other principal gains anything. The
directory grant is search and not read, so `sshd` cannot *list* `SSH/` — it
cannot even learn the names of the private key files, let alone open one. This
is strictly stronger than OpenSSH, where root reads every one of them.

`appdata_transit_security` becomes multi-subject rather than being copied
(§2.2): one transit record carrying a search grant per service uid that needs
one, with `SEARCH_ONLY` and the read triad each defined once.
`MAX_ACL_ENTRIES` is 8, so two service subjects is not near the bound.

The monitor reads those two files itself — reading bytes is not parsing. It
applies the `StrictModes` equivalent to the `fs_stat` result (a group- or
other-writable key file or directory is refused, fail closed), and then hands
the bytes to the existing one-shot `lib/sandbox::host::ParserSandbox` under a
new `authkeys` service — exactly the `helpdoc` / `imagerender` / `timesync`
precedent — which returns typed entries the monitor consumes under §1.3.
`authorized_keys` is **user-writable**, so it is untrusted input and the
monitor never parses it, whatever its provenance.

Two shapes were rejected. **A group-readable key store** (`0640 <user>:ssh`)
requires every user to be able to set that group unprivileged, which then lets
every user read every other user's `authorized_keys` — a real leak of key
material, `from=` restrictions, and account existence. **A reader helper
spawned as the claimed user** (`CAP_SPAWN_AS_USER` with a `CAP_FS_ACCESS`-only
manifest) also works and needs no ACL, but it lets an *unauthenticated* remote
client cause a process to run under a named user's credential, and it is a
second bundle and a spawn per authentication for something a static grant
already expresses. The ACL is both smaller and tighter.

**The residual, and it is real.** There is no `fs_set_acl`: `fs_set_mode` and
`fs_set_owner` exist, but the ACL an inode carries can only be authored by the
provisioning policy at creation. So the grant survives for as long as the
provisioned inode does. `ssh-copy-id` therefore **appends** to the provisioned
`authorized_keys` rather than replacing it — which preserves the inode and its
grant, and is what `ssh-copy-id` does anyway — and `ssh-keygen` writes private
keys, which need no grant. For the same reason `ssh` and `ssh-keygen -R`
update `known_hosts` **in place** rather than by write-to-temp-and-rename: a
rename replaces the inode and silently drops its grant. A user who deletes and
recreates `authorized_keys` by hand loses the grant; `sshd` then refuses that
user's public-key authentication and says exactly why (§2.24), rather than
failing mysteriously.
Fixing it properly means `fs_set_acl` and a `getfacl`/`setfacl` surface, which
is a syscall, ABI, VFS, ARXFS and tooling change well beyond this plan — it is
surfaced in §8.

Nothing else in `sshd` ever reads a path under `/Users`.

### 1.5 No new syscall, no new capability

The monitor composes existing authority: `CAP_NET`,
`CAP_NET_BIND_PRIVILEGED`, `CAP_PROC_SPAWN`, `CAP_SPAWN_AS_USER`,
`CAP_SANDBOX_SPAWN`, `CAP_USERS_READ`, `CAP_FS_ACCESS`, `CAP_IPC_ENDPOINT`,
`CAP_LOG_EMIT`. No `CAP_SSH_*` exists, and §5.2's minimalism test is passed by
not reaching for one: every authority SSH needs already guards the class of
resource it needs. `abi-check` and `c-header` see no syscall change.

A privileged **remote** forward (`-R` onto a well-known port) is the one place
the monitor's authority exceeds the requester's. It is admitted only if the
**authenticated user's** own ceiling carries `CAP_NET_BIND_PRIVILEGED`; the
monitor then binds with its own authority but never above the user's. The check
is on the authenticated ceiling, not on anything the connection asserted.

The monitor holds the auth state machine, so "start a session as user X" is
honoured only when the monitor itself observed a successful authentication for
X on that connection. A worker cannot ask for a session it did not earn.

Passwords arrive from the network and reach the monitor to be verified against
`lib/users`. The monitor treats a password as an **opaque bounded byte string**
handed to `UserDb::authenticate` — never a parsed structure — and relies on that
function's existing equal-cost property (an unknown user, a no-login account,
and a wrong password all pay one PBKDF2) so the network cannot enumerate
accounts by timing. The users database itself is system-owned and is not
untrusted input in the §19.5 sense, exactly as it is not for `login`.

### 1.6 Paths, configuration, accounts, host keys

| Thing | Location |
|---|---|
| Server config | `/System/Settings/SSH/sshd.conf` — system-owned `0644`; it holds no secret, and the per-inode policy on `/System/Settings` decides who may write, exactly as for `network.conf` |
| Host keys | `/System/Settings/SSH/HostKeys/` — the directory owned by the `sshd` account, mode `0700`, so the monitor can mint into it and nothing else can read a private half; private keys `0600`, public `0644` |
| System-wide known hosts | `/System/Settings/SSH/ssh_known_hosts` — system-owned `0644` |
| Per-user | `/Users/<n>/Settings/SSH/{config,known_hosts,authorized_keys,id_*}` |
| Server bundle | `/System/Services/sshd.app` (source `userland/system/sshd`) |
| Subsystem | `/System/Commands/sftp-server.app` |

`SystemConfigFile` (`lib/abi/src/driver_store.rs`) — the closed set that stops a
writer and a reader naming different files — gains an `Ssh` variant so
`configure` and `sshd` share one definition of the path. It has no pre-unlock
reader: `sshd` starts after the encrypted root is unlocked.

**Configuration grammar mirrors `lib/netconfig` exactly**, in a dedicated
`lib/sshconfig`: `<scope>.<key> value` lines over `lib/util::conf`'s shared
comment grammar, a closed key registry per document, typed value sets, bounded
allocation-bounded fail-closed parse (`MAX_CONFIG_LEN`, per-document element
caps), and one canonical render. One engine serves the writer (`configure
ssh.*`) and every reader, so they cannot diverge. A document `sshd` cannot fully
parse leaves the running configuration untouched and refuses the start; the
write path refuses the edit outright. The OpenSSH keyword → TAIRiX key mapping
is a table in `Help/` and `docs/src/userland/ssh.md` — documentation, never a
second parser.

**Accounts.** `lib/users/src/provision.rs` gains `sshd` (uid 19, the next free
after `timed` at 18) and `grants.rs` gains its ceiling: the capabilities of
§1.5. That uid is also the subject of the §1.4 ACL grants, so it is one
definition shared by the account, the ceiling, and the home-provisioning
policy. The worker needs no account: a sandbox has no credential of
consequence.

**Enrolment.** `sshd.app` ships installed but **not enrolled**; an
administrator enables it through the existing `init` enrolment registry
(`enrol`/`unenrol`, `servicectl`). Remote access is never on by default.

**Host keys are minted by `sshd`, on the machine, at first start — never by the
installer and never baked into an image.** An image-baked host key is identical
on every machine flashed from that image, which is the embedded-router and
Debian-2008 class of defect; minting on the machine is the only way each
installation gets a distinct identity.

- On start the monitor enumerates its configured `HostKey` set. Any **absent**
  key it mints then, in-process through the same `lib/ssh::key` code
  `ssh-keygen -A` uses (one implementation, §2.2), writing it into the
  `sshd`-owned `HostKeys/` directory above. Minting is the *only* write `sshd`
  ever makes outside its own logs.
- A key file that is **present but unreadable, malformed, or not the type its
  name claims** is *never* silently replaced: the monitor refuses to start and
  says which file and why (§2.24). Silently regenerating a host key is the event
  that makes every client's `known_hosts` cry wolf and trains users to accept a
  man in the middle.
- Minting requires the kernel CSPRNG; if `random_get` cannot supply entropy the
  monitor fails closed rather than minting a weak key.
- **`ssh-keygen -A` is deliberately not offered**, and says so rather than
  failing obscurely (§2.24). It exists upstream because OpenSSH's `sshd` does
  not mint, so a distribution runs it from a start script; here `sshd` mints,
  and the store is `0700` under the `sshd` account, so *nothing else can write
  into it* — which is the property that keeps a host private key unreadable
  even to an administrator, unlike OpenSSH where root reads every one. Adding a
  host-key type is therefore an edit to `HostKey` in `sshd.conf` plus a
  restart, and the tool's refusal names that path. `ssh-keygen` still generates
  *user* keys with the full switch set. The divergence is documented in the
  tool's `Help/` and `docs/src/userland/ssh.md` (§16.7).

### 1.7 Audit

`lib/ssh::events` claims the `24_000..25_000` `EventId` range. Verified against
every `*_RANGE_START` in the tree: the highest assigned is `timed` at `23_000`,
and `22_000..23_000` stays **retired** (a deleted subsystem held it; a shipped
range is never re-used). `docs/src/security/network.md`'s registry table is the
format precedent, and `docs/src/security/ssh.md` carries this one.

Every authentication grant and refusal, host-key decision, host-key mint,
forwarding grant and refusal, rate-limit drop, `StrictModes` refusal, session
open and close, and worker crash carries a stable id. A refusal never records
the secret that was refused (§23.1).

### 1.8 Scale and fairness

The floor this is designed against is §26.2 and §26.7, not a developer's
laptop: many simultaneous users, each with greedy processes, on a machine that
may have 1 GiB of RAM.

- **Per-connection cost is bounded before it is incurred.** `MaxStartups` with
  random early drop bounds unauthenticated connections; `LoginGraceTime` bounds
  how long one may live; `MaxAuthTries` bounds the work each may demand;
  `MaxSessions` bounds session channels per connection, and the forwarding
  policy of S7 bounds the rest. Each is a §24.4 fixed fail-closed bound, not a
  capacity to grow.
- **Aggregate cost scales with the machine**, through the derived socket policy
  of S0b and the `LimitKind` bounds the `sshd` process inherits.
- **Per-source penalties** (OpenSSH 9.8's `PerSourcePenalties`) make a
  misbehaving source pay before a well-behaved one does.
- Nothing in the serving path spins. A shard parks on its wait-set; a worker
  blocks on its pipe; a forwarded connection parks on its socket's delivery
  port.

---

## 2. Defects fixed on the way (§2.18)

**S0b — the `netstack` socket quota (done).** `MAX_SOCKETS_PER_PRINCIPAL`
and `MAX_SOCKETS_TOTAL` are gone. The total is derived from the machine's
usable physical RAM — an eighth of it at the configured worst case of one
socket's TCP send and receive buffers — and the per-principal figure is a
sixteenth share of that total, so a full table always has room for sixteen
principals. A 1 GiB machine derives exactly the 1024 and 64 the constants
named, which is the evidence the fraction and the share are not another
guess. `net.sockets.max` (`auto` or a count) overrides the derivation
through the existing `system.conf` store under the existing
`CAP_NET_ADMIN`; no new capability. The fail-closed `LimitExceeded`
refusal is untouched.

The stack reads neither the machine nor `system.conf` — it is the
network-parsing sandbox — so both deliverers (`devmgr` at boot,
`configure` on a live edit) resolve the document against the ungated
System Information API RAM total and hand the stack one effective figure.
`SystemConfig::network_settings` therefore takes that total as an
argument: one mapping, so a live edit and the next boot cannot differ.
`netstack` deliberately does **not** query the total itself, because
`sysinfod` already calls `netstack` for its socket listing and the reverse
edge would close an IPC cycle.

**The prerequisite this turned out to need.** The table was a `Vec` found
only by linear scan — per received packet (the established four-tuple,
then the listener), on all eleven owned-handle lookups, up to 128×O(n) per
ephemeral bind, and O(n) per id allocation. Letting a derived capacity
grow that ~170× would have made packet-receive cost follow the table and
let one principal's sockets slow every other principal's traffic, which is
a denial of service rather than merely slow — so the bound could not
honestly be derived until the table was indexed. It now carries four keyed
indices (handle→position, four-tuple→handle, port→holders and demux
target, principal→live count), keyed with the process's SipHash key
because a peer chooses the address and port half of a connection key and
an unkeyed hash would be collision-floodable. Every index row is derived
from an entry's own state by one `index_entry`/`unindex_entry` pair, and a
`#[cfg(test)]` invariant check runs after every served request, inbound
segment, and timer pass; each of the five index-maintenance steps was
verified to fail the suite when removed.

Two defects fell out of that work and are fixed with it: `bind` on an
already-bound socket silently moved its port, stranding the old one and
cutting a connected socket's inbound segments adrift from the four-tuple
they are demultiplexed by (now refused); and five copies of the lazy
"assign an ephemeral port if unbound" block are one `ensure_local_port`.

A per-principal `LimitKind::Sockets` would be the better shape in the
abstract, since it is what `ulimit` means, but the socket table lives in a
*user-space* service and the kernel cannot today attest a caller's
effective limit to a user-space resource owner. Inventing that channel for
one consumer is speculative interface (§2.4). It becomes the right answer
the moment a second user-space service owns a per-principal resource; the
condition is recorded here so the decision is revisited on evidence rather
than re-argued.

**S0c — `lib/sandbox` has no long-lived worker.** `host::ParserSandbox::request`
is one-shot request→reply and `worker::serve` answers one frame at a time
against a total `Service`. An SSH connection is a duplex session either side may
originate on. A `session` module lands beside `host`/`worker` (§27 — complete
the primitive, do not ship the caller's slice), reusing `proto`'s framing and
`host`'s crash containment, and is reusable by any future long-lived protocol
service.

**S0d — `lib/compress` has no DEFLATE compressor.** `inflate` (RFC 1951) and
`zlib` (RFC 1950) already exist and are deliberately **decode-only**, because
nothing in the tree produced a DEFLATE stream. `zlib@openssh.com` does, so the
encode direction lands with the same total, `unsafe`-free, bounded discipline —
and the crate's module documentation, which currently states that no compressor
exists and why, is corrected rather than left to mislead (§2.14).

---

## 3. Crate and file layout

```
lib/crypto/                 # extended — the §2.12 audited-crate carve-out only
lib/ssh/                    # the pure engine: wire, ident, packet, kex, key,
                            #   cert, auth, connect, client, server, monitor,
                            #   agent, events
lib/sftp/                   # the SFTP protocol (v3 + the OpenSSH extensions)
lib/sshconfig/              # the sshd.conf / client-config store engine
lib/sandbox/src/session.rs  # the duplex long-lived worker seam
lib/compress/               # + the RFC 1951/1950 encode direction
userland/system/sshd/       # the monitor; its own binary re-invoked as worker
userland/apps/{ssh,ssh-keygen,ssh-add,ssh-agent,ssh-keyscan,ssh-copy-id,
               sftp,scp,sftp-server}/
tools/sshinterop/           # version-pinned, checksummed OpenSSH wrapper
                            #   (tools/cc is the precedent) for S13
```

`lib/ssh` and `lib/sftp` are `no_std` + `alloc`, `#![forbid(unsafe_code)]`, and
**have no I/O**: every state machine is driven by typed events and emits typed
actions, so the fuzzed code, the property-tested code, and the shipped code are
the same code (the `plans/TELNET.md` and `lib/net` precedent, scaled up).

Each `lib/*` crate and each `tools/` directory gains its `AGENTS.md` §3 map row
**in the increment that creates it**, matching the standing convention (`lib/audio`
is in the map because it landed; `lib/sound` is not because it has not). This
plan adds no §3 row.

---

## 4. The algorithm set

Broad interoperability is the stated scope: a stock OpenSSH client must connect
to `sshd`, and `ssh` must connect to a stock OpenSSH server, without either side
being configured specially.

**Host keys and signatures:** `ssh-ed25519`, `ecdsa-sha2-nistp{256,384,521}`,
and the `*-cert-v01@openssh.com` certificate form of each. `rsa-sha2-256`
and `rsa-sha2-512` are **absent**, not deferred: see S15 and §8.

**Key exchange:** `curve25519-sha256` (and the `@libssh.org` alias),
`mlkem768x25519-sha256`, `ecdh-sha2-nistp{256,384,521}`,
`diffie-hellman-group14-sha256`, `diffie-hellman-group16-sha512`,
`diffie-hellman-group18-sha512`, and `diffie-hellman-group-exchange-sha256`
(RFC 4419). The group-14/16/18 names each pair with exactly one hash, as
RFC 8268 assigns them; there is no cross-product.

**Ciphers:** `chacha20-poly1305@openssh.com`, `aes{128,256}-gcm@openssh.com`,
`aes{128,192,256}-ctr`.

**MACs:** `hmac-sha2-256-etm@openssh.com`, `hmac-sha2-512-etm@openssh.com`, and
their non-ETM forms for interop; the AEAD ciphers carry their own.

**Refused, not accepted-and-weak:** `ssh-rsa` and `ssh-dss` (SHA-1 and DSA;
OpenSSH disabled the former by default in 8.8 and removed the latter in 10),
`diffie-hellman-group1-sha1`, `hmac-md5`, `hmac-sha1`, CBC ciphers, `arcfour`,
`none` cipher, and `3des-cbc`. Each is absent from the negotiation lists
entirely, so a peer offering only those gets a clean "no matching algorithm"
rather than a weak session.

**One deliberate SHA-1 use, and only one.** OpenSSH's hashed `known_hosts` form
(`|1|<salt>|<HMAC-SHA1(salt, host)>`) is a fixed foreign format; reading a
user's existing file requires it. It is an *obfuscation index* over a hostname,
never a signature and never an authentication decision — the host's identity is
compared over the full key blob, and the hash only selects which line to compare
against. TAIRiX defaults `HashKnownHosts` **off** (upstream OpenSSH's default),
so SHA-1 appears on the read path and an explicit opt-in write path only. This
is documented in `docs/src/security/ssh.md` rather than left for a reader to
discover.

`lib/crypto` now carries the whole of the above, and is the only crate in the
workspace that names a cryptographic dependency — production path, test, or
build script alike. S0a moved the existing pins and added the rest as **one
generation** of the RustCrypto and dalek-cryptography stacks, so the tree holds
a single copy of `digest`, `cipher`, and the curve arithmetic rather than two
of each. What it added: Ed25519 signing and key derivation from a seed;
SHA-384/512 and HMAC-SHA-512; a 64-bit-nonce ChaCha20 keystream with an
explicit start counter and a standalone Poly1305 (`chacha20-poly1305@openssh.com`
uses two keys and a sequence-number nonce, so the packaged AEAD cannot serve
it); AES-CTR and AES-GCM; ECDH and ECDSA over P-256/384/521; finite-field DH
over the RFC 3526 groups; ML-KEM-768; bcrypt-pbkdf; and HMAC-SHA1 for the one
use above. Each arrived with its §2.12 justification, an exact `=x.y.z` pin, a
`deny.toml` licence check, a `supply-chain.toml` source pin, an SBOM entry (the
generator reads `Cargo.lock`, so it follows automatically), and the §19.1
constant-time test under `-C opt-level=3`.

Two constraints shaped it and are recorded so a later increment does not
re-derive them:

- **Nothing in `lib/crypto` draws randomness**, so every construction is
  reached through its deterministic form: Ed25519 and ECDSA derive their
  nonces from the key and message (RFC 8032, RFC 6979), and ML-KEM's key
  generation and encapsulation take the caller's bytes as FIPS 203 defines
  them. This is what forced the pin generation: on the older RustCrypto
  generation `p521` implements no `DigestAlgorithm`, so its only signing path
  was a randomised nonce through `OsRng` — unavailable bare-metal, and a
  reused or biased `k` is immediate private-key recovery. Moving the whole
  stack to one newer generation makes `ecdsa-sha2-nistp521` deterministic like
  its siblings, and the RFC 6979 §A.2.7 vector in the unit tests is what
  proves it.
- **Group exchange (RFC 4419) has no primality test available.** `ffdh` offers
  the three RFC 3526 groups and refuses an arbitrary caller-supplied modulus:
  validating a server-offered group as a safe prime needs Miller-Rabin, and
  the only pure-Rust option sits on a different `crypto-bigint` major version.
  S2 must therefore decide the client's group-exchange posture — accept only
  groups it can vouch for, or carry the primality test — rather than assuming
  an arbitrary group is checkable. It is not reached against a stock OpenSSH
  peer, whose proposal always contains curve25519 and the NIST curves first.

---

## 5. Staged increments

Each row of the ledger is one gated landing. What each contains:

**S0a** — the `lib/crypto` extension of §4.

**S0b** — the socket-quota defect of §2, with tests covering the derived policy
on small and large discovered RAM, the per-principal share, the `CAP_NET_ADMIN`
override, and the unchanged fail-closed refusal at the bound.

**S0c** — `lib/sandbox::session`: the duplex seam, its crash containment, and a
`loopback` fake so consumers' host tests run the full parent path.

**S0d** — the DEFLATE and zlib encode direction, fuzzed round-trip against the
existing decoders and against fixtures a foreign encoder produced.

**S1** — `lib/ssh` wire codec (RFC 4251 §5 `string`/`mpint`/`name-list`/
`boolean`), version exchange with its banner rules, the binary packet protocol
with every cipher and MAC framing including ETM and the AEAD length handling,
**strict KEX** (the CVE-2023-48795 prefix-truncation fix: sequence numbers reset
at `NEWKEYS` and no unexpected message is tolerated during KEX), and the RFC
4344 rekey thresholds. Fuzz harnesses from this increment, not later.

**S2** — KEXINIT, the first-match rule, the exchange hash, key derivation
(RFC 4253 §7 including the extend-to-length rule), the KEX method families of
§4, and RFC 8308 `ext-info-c`/`ext-info-s` with `server-sig-algs`.

**S3** — keys end to end: public-key and signature blobs; the OpenSSH v1 private
key format including bcrypt-pbkdf passphrase encryption; base64 and PEM armour;
SHA-256 fingerprints and randomart; the `authorized_keys` grammar (options,
`restrict`, `from=`, `command=`, `expiry-time=`, `permitopen=`,
`permitlisten=`); the `known_hosts` grammar (plain and hashed hosts, patterns,
`@revoked`, `@cert-authority`); and OpenSSH certificates — parse, validate,
and mint.

Certificate validity (`valid_after`/`valid_before`) and the `expiry-time=`
option are absolute times, so they cross into TAIRiX as `Time64` and never as
a bare integer of unstated width (§21). The foreign format's field is `u64`
seconds since the epoch: the conversion is **checked**, and a value the
`Time64` seconds field cannot represent is refused rather than saturated or
wrapped — a certificate whose expiry wrapped to the past would fail safe, but
one that wrapped to the future would not. Tests cover a pre-1970
`valid_after`, a post-2038 `valid_before`, and `u64::MAX` (OpenSSH's spelling
of "no expiry").

**S4** — userauth (`none`, `password` with `SSH_MSG_USERAUTH_PASSWD_CHANGEREQ`,
`publickey` with its two-phase probe, `keyboard-interactive` RFC 4256,
`hostbased`, and `publickey-hostbound-v00@openssh.com`) and the RFC 4254
connection protocol: channels, windows, adjust, `session`/`shell`/`exec`/
`subsystem`, `pty-req`, `env`, `window-change`, `signal`, `exit-status`,
`exit-signal`, global requests, and the OpenSSH extensions `ping@openssh.com`,
`no-more-sessions@openssh.com`, `hostkeys-00@openssh.com`.

**S5** — `sshd`. The monitor and its sharded wait-sets; the sandboxed worker;
the monitor protocol with the §1.2 split and the §1.3 rule; the §1.4
`Settings/SSH` provisioning records, the multi-subject transit grant, and the
`authkeys` sandbox service; host-key minting per §1.6; pty creation and the
shell spawn as the authenticated user; `LoginGraceTime`, `MaxStartups` with
random early drop, `MaxAuthTries`, `MaxSessions`, per-source penalties;
`lib/sshconfig` and the `SystemConfigFile::Ssh` variant; the `sshd` account and
ceiling; the audit events; and the **first live QEMU vertical** — a guest client
authenticating to guest `sshd` over loopback, running a command, and getting its
exit status.

**S6** — the `ssh` client (config file with `host.*` and `match.*` scopes,
host-key checking with `StrictHostKeyChecking` and `UpdateHostKeys`, the escape
sequences `~.`/`~C`/`~#`/`~?`/`~~`, `-t`/`-T`/`-N`/`-o`/`-i`/`-p`/`-l`/`-v`) and
`ssh-keygen` (the user-key switch set: `-t`, `-b`, `-C`, `-f`, `-N`, `-p`,
`-l`, `-y`, `-e`, `-i`, `-s`, `-L`, `-R`, `-H`, `-F`, `-Y sign`/`-Y verify`;
`-A` is refused per §1.6). The client's `Run` binary takes the two-thread shape
`plans/TELNET.md` §2 establishes and explains: a console-backed standard input
cannot join a wait-set, so one thread blocks in `Stdin::read` and forwards to a
port the main thread waits on alongside the socket. Neither thread spins.

**S7** — forwarding: `direct-tcpip` (`-L`), `tcpip-forward`/`forwarded-tcpip`
(`-R`), `-D` SOCKS4a and SOCKS5, and the server policy
(`AllowTcpForwarding`, `GatewayPorts`, `PermitOpen`, `PermitListen`) with the
§1.5 authenticated-ceiling check on a privileged remote bind.

**S8** — `lib/sftp` (v3 plus the OpenSSH extensions: `posix-rename@`,
`statvfs@`, `hardlink@`, `fsync@`, `limits@`, `expand-path@`,
`copy-data@`), the `sftp-server` subsystem bundle — spawned as the user with no
special authority, so its reach *is* the user's reach — the interactive `sftp`
client, and `scp` (SFTP-backed, OpenSSH 9's behaviour, not the legacy rcp
protocol).

**S9** — `lib/ssh::agent`, the `ssh-agent` service backed by an IPC endpoint
(`SSH_AUTH_SOCK` carries an endpoint id, since TAIRiX has no Unix sockets),
`ssh-add`, agent forwarding, and `publickey-hostbound-v00@openssh.com` binding
an agent signature to the session's host key.

**S10** — `zlib@openssh.com` delayed compression over S0d. Default off. The
pre-auth `zlib` method is refused outright: compressing attacker-chosen
plaintext before authentication is a CRIME-class oracle and a pre-auth
decompressor is attack surface for nothing.

**S11** — client multiplexing (`ControlMaster`, `ControlPath`,
`ControlPersist`, `-O check|stop|exit|forward`) over an IPC endpoint,
`ProxyJump`, and `ProxyCommand`. `ProxyCommand` runs under the invoking user's
own authority from the user's own configuration; the client refuses to honour
one from a configuration file the invoking user does not own (the `StrictModes`
analogue), which is the actual defence — unlike `telnet`'s `!` escape, which was
refused because it would have given a program parsing hostile network input the
authority to spawn a shell.

**S12** — `ssh-keyscan`, `ssh-copy-id`, `ssh -Q`, and `sshd -t`/`-T`. The
configuration test validates the document and reports the `HostKey` entries it
names; whether each key *exists* is not something an administrator's `sshd -t`
can see, because the store is `0700` under the `sshd` account (§1.6) — that
answer is `sshd`'s own at start, and it is on the audit log.

**S13** — `tools/sshinterop` and the interop verticals (§7).

**S14** — FIDO/U2F. **Blocked** (§8).

---

## 6. Explicit non-goals

Each is stated and absent, never stubbed.

- **`ssh-rsa`, `ssh-dss`, `diffie-hellman-group1-sha1`, CBC ciphers,
  `hmac-md5`/`hmac-sha1` MACs** — refused, per §4.
- **`rsa-sha2-256` / `rsa-sha2-512`** — absent, not refused on merit: there is
  no charter-legal implementation. See §8, S15.
- **`sntrup761x25519-sha512@openssh.com`** — the only available implementations
  are bindings to C reference code, which §1 forbids. `mlkem768x25519-sha256`
  is pure Rust, standardised, and OpenSSH's own current default; it is the
  post-quantum answer here.
- **X11 forwarding** — TAIRiX has no X11.
- **GSSAPI / Kerberos** — TAIRiX has no Kerberos.
- **PKCS#11** — that surface is a dynamically loaded C library, which the
  bundle-local, curated `/System/Libraries` model (§16.4) and §1 both refuse.
- **`streamlocal-*` Unix-domain-socket forwarding** — TAIRiX has no Unix
  sockets. The agent's endpoint-id channel (S9) covers the one case that
  mattered.
- **Pre-authentication `zlib` compression** — §5, S10.
- **Remote unlock of the encrypted root** (`dropbear`-in-initramfs). The
  network stack, the users database, and `/System/Settings` are all behind the
  root unlock, so `sshd` cannot run before it. Reaching that would mean a second,
  far more exposed pre-unlock service; it is not smuggled in here.

---

## 7. Tests, oracles, docs, gate

**Per increment**: host unit tests beside the code; a `cargo xtask fuzz` target
in `tools/xtask/src/commands/fuzz.rs`'s `TARGETS` for *every* decoder of
untrusted input (the wire codec, the packet protocol, KEXINIT, every key and
certificate grammar, `authorized_keys`, `known_hosts`, userauth, the connection
protocol, SFTP, the agent protocol, the monitor protocol) per §19.6; §19.7
proptest models for the authentication state machine (the property: no sequence
of worker-originated frames yields a session for a user the monitor did not
observe authenticate); `docs/src/userland/ssh.md` and `docs/src/security/ssh.md`
plus their `docs/src/SUMMARY.md` entries, in the same change (§13); and the full
§2.15 gate. The `README.md` feature-matrix row lands with **S5**, the first
increment that adds a runnable feature a matrix could mark per architecture —
S0a–S4 add none.

**Oracles (§19.11).** `lib/ssh`, `lib/sftp`, and `lib/sshconfig` are
`#![forbid(unsafe_code)]`, so miri has nothing new to interpret and no enrolment
changes — stated rather than left silent. Loom **does** apply: S5's
listener→shard handoff and the monitor↔worker flow-control queue are the one
place correctness depends on an ordering pairing, and a lost wake-up there is a
hung connection that only shows up under load. S5 carries that model in
`tools/xtask/src/commands/loom.rs`'s `TARGETS`, and — per the `tairix-abi` PCM
ring precedent — it is verified to *fail* with the ordering downgraded before it
is accepted.

**Verification of the finished thing, end to end (S13).** `tools/sshinterop` is
a version-pinned, checksummed wrapper around a real OpenSSH build
(`tools/cc` is the precedent for pinning and auditing an external toolchain),
driving the guest under QEMU in **both** directions — host OpenSSH `ssh` → guest
`sshd`, and guest `ssh` → host OpenSSH `sshd`. Each of these is a serial gate
whose verdict names the first step that failed:

a password login; an Ed25519 publickey login; an RSA publickey login; a
certificate login; an `exec` session with its exit status; an interactive pty
session including a window-change; a `-L` forward; a `-R` forward; an
`sftp put` and `get` round trip; an `scp` in both directions; and a forced
rekey mid-session.

Interop against the real implementation is what makes the protocol claims in §4
true rather than asserted. A test suite that only talks to itself would agree
with its own bugs.

---

## 8. Surfaced, not fixed here (§2.18, §15.7)

**A per-inode ACL can be authored, but never changed: there is no
`fs_set_acl`.** The §5.3 model is otherwise complete — the VFS enforces
capability gate, then ACL, then mode (`kernel/core/src/fs/perm.rs`), ARXFS
persists a per-inode ACL, and `tairix_users::policy` authors one at home
provisioning. What is missing is the userland write path: `fs_set_mode` and
`fs_set_owner` exist and their ACL counterpart does not, so no program can add,
remove, or inspect an entry, and no `getfacl`/`setfacl` exists to show a user
what their own files grant.

Three consequences, only the first of which touches SSH: an ACL grant lives and
dies with the inode the provisioner created (§1.4's residual); an account
provisioned before a new grant is introduced never receives it, with no repair
path short of recreating the home; and a user cannot see, let alone audit, the
non-mode authority over their own files — which for a *security* mechanism is
the sharper problem.

This predates SSH. §1.4 is designed to need no write path, so nothing in this
plan is blocked on it, and it is far too large to fold in — it is a syscall,
an ABI addition, a VFS path, ARXFS persistence, and a tool. It is recorded as
`plans/OPEN-DEFECTS.md` **D141**.

**S15 — RSA host and user keys (`rsa-sha2-256`, `rsa-sha2-512`).** The only
pure-Rust RSA is the `rsa` crate, which carries RUSTSEC-2023-0071 (the Marvin
timing attack) with `patched = []` — unfixed on 0.9.10 and on the 0.10 release
candidates as of 2026-09-12 — and whose own advisory text says to avoid it
"in settings where attackers can observe timing, for example over the
network". That is exactly SSH. §19.3 blocks an advisory-affected dependency
and §2.12 forbids hand-rolling the alternative, so the algorithm is **absent**
rather than shipped weak or stubbed. Verify-only would not help: `cargo deny`
flags the crate, not the call.

What this costs, stated rather than hidden: nothing for host keys against a
stock OpenSSH peer, which has offered Ed25519 by default since 7.0 and always
proposes it. What it does cost is a user whose only key is `~/.ssh/id_rsa`,
and the rare server with an RSA-only host key — for those, `ssh-keygen -t
ed25519` on the other side is the answer, and `sshd` says so when it refuses
(§2.24).

**This is queued for re-checking, not forgotten.** At every stage boundary
this plan advances, and whenever `lib/crypto`'s pins are audited, confirm
whether RUSTSEC-2023-0071 has gained a `patched` version; if it has, S15
unblocks and is an ordinary increment. It is also tracked as
`plans/OPEN-DEFECTS.md` **D143** so it is visible from the tree's single
open-item ledger rather than only from this plan.

**S14 — FIDO/U2F `sk-ssh-ed25519@openssh.com` and `sk-ecdsa-*`.** These need a
CTAP2-over-USB-HID path to a security key. `lib/hid` is boot-protocol decode
plus a report-descriptor parser for the keyboard and mouse class drivers, and
`plans/USB.md` has no generic HID-raw device channel a userland program can
reach. The dependency is named rather than the feature silently omitted; S14
lands when that path does.

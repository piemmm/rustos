# Network security posture

The network stack lives in its own user-space process, `netstack`, with
no kernel privilege: it holds only its NIC device channel and the two
reserved IPC endpoints it serves. All frame input is attacker-controlled
(§26.4), so every decoder in the pure `lib/net` engine is total, bounded
(§24.4), fuzzed (§19.6), and fails closed. This page records the threat
model, the defences, and the audit event-id registry for network events.

## Threat model ↔ defence

| Threat | Defence |
|---|---|
| Hostile frame parsing (malformed Ethernet/IP/ICMP/TCP/UDP, truncation, option abuse) | `lib/net` decoders are total and bounded; `netstack` is the §19.5 minimum-capability parser sandbox — it holds no filesystem or spawn authority. Every decoder has a fuzz harness (`fuzz_net_*`). |
| Fragment/reassembly resource exhaustion | Reassembly sets are capacity-bounded; the oldest incomplete reassembly is evicted (fail closed), and the eviction is counted (`stats:net/stack/reassembly-evicted`). |
| SYN flood (half-open exhaustion) | The listener keeps a bounded half-open backlog; on overflow it answers with stateless RFC 4987 SYN cookies (a keyed MAC over the 4-tuple), holding no per-connection state. |
| ICMP error storms / amplification | ICMP/ICMPv6 error emission is rate-limited; suppressed errors are counted (`stats:net/stack/icmp-suppressed`) so the throttling is visible. |
| Unprivileged origination of raw/spoofed traffic | Sockets are capabilities, not ambient authority: a socket is a kernel-brokered IPC channel obtained through the versioned socket ABI and gated per operation — outbound transport under `CAP_NET`, binding a privileged (well-known) port under `CAP_NET_BIND_PRIVILEGED`, raw access under `CAP_NET_RAW`. |
| Forged caller identity | `netstack` derives every caller's identity from the kernel-attested `Origin` (`call_peer_origin`), never from a claimed field; the capability is checked before any state is touched, and the receiver does not re-check. |
| Cross-principal socket / peer disclosure | The system-wide socket listing (`NET_SOCKETS`), the per-interface counter/rate queries, and the bond-topology listing (`NET_BOND_MEMBERS`) name other principals' sockets, peers, and the link-aggregation layout, so they require `CAP_SYSINFO_GLOBAL` and are audited; there is no `/proc/net` and no unprivileged path to another principal's sockets. |
| Silent failure hiding an attack in progress | Every refusal is a typed error and an audited event (below); the socket listing fails loud rather than returning an empty table (§24), and the defence counters surface a DoS in progress. |
| A bond member's link silently failing (path loss going unnoticed) | Bond failover is link-state-driven and audited (`BOND_FAILOVER`); a dead member becomes ineligible immediately, the transmit path re-targets a healthy member and re-announces the bond's presence (gratuitous ARP / unsolicited NA), and per-member health/eligibility is observable. A member holds no addresses and refuses direct address assignment (the bond owns them). |

## Capabilities

The network capabilities are deliberately coarse and few (§5.2):

- `CAP_NET` — originate transport-layer (TCP/UDP) traffic.
- `CAP_NET_BIND_PRIVILEGED` — bind a listening port at or below the
  privileged-port bound (the Unix `CAP_NET_BIND_SERVICE` model).
- `CAP_NET_RAW` — raw packet access (also the restricted-sender grant a
  NIC driver's device channel carries).
- `CAP_NET_ADMIN` — the interface admin surface (address/route add, NIC
  channel bind).
- `CAP_SYSINFO_GLOBAL` — the system-wide, cross-principal introspection
  queries (`NET_SOCKETS`, per-interface counters and rates, and the bond
  member/health listing `NET_BOND_MEMBERS`), audited.

## Audit event-id registry (§19.4)

`netstack` reserves the stable `tairix_log::EventId` range **`16_000` …
`16_999`** (`NETSTACK_RANGE_START` inclusive, `NETSTACK_RANGE_END`
exclusive), so an audit consumer filters network events by subsystem in
one range test. The assigned identifiers:

| Id | Name | Level | Meaning |
|---|---|---|---|
| `16_001` | `ADMIN_APPLIED` | Info | An admin mutation (address or route add) was applied. |
| `16_002` | `REQUEST_DENIED` | Warn | An admin/broker request was refused for want of its required capability. |
| `16_003` | `REQUEST_MALFORMED` | Warn | An admin/broker request frame failed to decode. |
| `16_004` | `ADMIN_REFUSED` | Warn | An admin mutation named an unmanaged interface or the engine refused it. |
| `16_005` | `SOCKET_MALFORMED` | Warn | A socket-service request frame failed to decode. |
| `16_006` | `SOCKET_DENIED` | Warn | A socket request was denied (missing `CAP_NET` or privileged-port grant). |
| `16_007` | `SOCKET_OPENED` | Info | A socket was opened for a principal. |
| `16_008` | `SOCKET_REFUSED` | Warn | A socket operation was refused after the capability check (quota, address in use, no route). |
| `16_009` | `DRIVER_BOUND` | Info | A NIC driver's device channel was bound to a managed interface. |
| `16_010` | `DRIVER_BIND_DENIED` | Warn | A `BindDriver` request was denied for want of `CAP_NET_ADMIN`. |
| `16_011` | `DRIVER_BIND_FAILED` | Warn | A `BindDriver` passed the gate but could not be carried out (interface stays unbound). |
| `16_012` | `INBOUND_ECHO_SERVED` | Info | An inbound ICMP/ICMPv6 echo to a local interface was answered. |
| `16_013` | `SOCKET_LISTENING` | Info | A stream socket entered the passive LISTEN state. |
| `16_014` | `SOCKET_ACCEPTED` | Info | A passive connection was accepted onto a child stream socket. |
| `16_015` | `NETWORK_SETTINGS_APPLIED` | Info | The stack-wide `net.*` policy (family enable, SYN-cookie mode) was applied over the `ApplyNetworkSettings` admin op. |
| `16_016` | `INTERFACE_CONFIG_APPLIED` | Info | A per-interface `network.conf` configuration (static addressing, MTU, family enable) was applied over the `NetInterfaceConfigMsg` admin message. |
| `16_017` | `BOND_CONFIG_APPLIED` | Info | A bond (link-aggregation) interface was composed or reconfigured over the `NetBondConfigMsg` admin message (members, mode, primary, monitor interval). |
| `16_018` | `BOND_CONFIG_REFUSED` | Warn | A bond-configuration request passed the capability check but was refused (a member not present yet, an alias clash, or validation) — the bond is left untouched (fail closed). |
| `16_019` | `BOND_FAILOVER` | Info | A bond's transmit path changed member (failover or deliberate failback); the bond re-announced its presence so peers relearn the path — a dead member is a visible, audited fact. |

The stack-wide `net.*` policy (`net.ipv4.enabled`, `net.ipv6.enabled`,
`net.tcp.syncookies`) is read from `system.conf` and delivered to
`netstack` by the FS-capable device manager, which records the delivery
in its own `devmgr` range: `13_012` `NETWORK_SETTINGS_DELIVERED` (Info,
the policy was read and accepted) and `13_013`
`NETWORK_SETTINGS_DELIVERY_FAILED` (Warn, the stack refused it or was
unreachable — retried, fail-soft). `netstack` holds no filesystem
capability, so this split (devmgr reads, netstack enforces) keeps the
parser sandbox filesystem-free (`plans/NETWORK.md` §0, N9b-2).

The socket-listing, counter/rate, and bond-member queries are
additionally audited by the `sysinfod` broker under their `sysinfo-v1`
query names (`net_sockets`, `net_interface_stats`, `net_interface_rates`,
`net_bond_members`), so a privileged read of another principal's network
state always leaves a record.

## Out of scope

The stack does not defend against the operational classes §19.9 names
(physical attacks, a compromised holder of an administrative capability,
compiler bugs). The capability model bounds blast radius; it cannot
prevent abuse by a legitimate holder of `CAP_NET_ADMIN` or
`CAP_SYSINFO_GLOBAL`.

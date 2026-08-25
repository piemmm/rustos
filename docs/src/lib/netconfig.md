# `tairix-netconfig` — the network interface-configuration store engine

`lib/netconfig` is the single definition of TAIRiX's administrator-settable
per-interface network configuration store: the document at
`/System/Settings/Network/network.conf` (`tairix_netconfig::CONFIG_PATH`).
It owns the per-interface line grammar, the **closed** key registry, each
key's typed value set, the bounded fail-closed parser, and the canonical
render. The image builder and the installer write the store through this
engine; the one reader — the device manager, which maps it into the
per-interface admin messages `netstack` applies — reads it through the same
engine, so producer and consumer can never diverge.

`CONFIG_PATH` is the document's `/System` **view** path. The bytes live on
the read-only `/System` volume at the volume-relative path the ABI's closed
`SystemConfigFile` set names, because the device manager reads it over the
pre-unlock store endpoint (see `docs/src/userland/netstack.md`); at runtime
the view path resolves to the writable sub-mount on the encrypted root,
which no bootstrap client can reach.

## The store

One text document: `<interface>.<key> value` settings, one per line; `#`
comments and blank lines are ignored; every `(interface, key)` pair appears
at most once. The document is bounded (`MAX_CONFIG_LEN`, 16 KiB;
`MAX_INTERFACES`; `MAX_BOND_MEMBERS`) and the parser refuses — never
guesses at — anything it does not fully understand, returning a
`ParseError` that carries the offending 1-based line where one is
meaningful. An **absent** store is not an error: it means "no managed
interfaces beyond loopback" (`NetworkConfig::default`), which is how a
fresh installation and a pre-unlock boot behave.

The store lives inside `/System/Settings` on the encrypted root volume, so
it can only be read after the operator's `ARXFS passphrase:` unlocks the
root — `netstack` therefore parses it post-unlock by construction. Write
authority is the existing `/System/Settings` per-inode policy under the
caller's kernel-attested identity: no new capability exists for the file
itself; applying a changed store to the live stack is the `CAP_NET_ADMIN`
admin reload.

## Interfaces are aliases bound by identity

An interface is named by a stable, admin-chosen **alias** (`wan`, `lan0`),
never a discovery-order name, and bound to hardware by identity —
`<iface>.match.mac` (the NIC's MAC) or `<iface>.match.node` (the NIC's
stable location on the bus, written as the hex register-window base of its
hardware-tree node) — so a NIC keeps its configuration across reboots and
reprobes regardless of enumeration order. An interface carries at most one
of the two selectors; the two are mutually exclusive.

## The registry

Every key is `<iface>.<suffix>` where the suffix is drawn from the closed
`IfaceKey` set:

| Suffix                  | Values                                   | Meaning |
|-------------------------|------------------------------------------|---------|
| `kind`                  | `ethernet` \| `bond` \| `loopback`       | the link kind (default `ethernet`) |
| `match.mac`             | `aa:bb:cc:dd:ee:ff`                       | bind the alias to this NIC by MAC |
| `match.node`            | hex register base, e.g. `0xa003e00`      | bind the alias to the NIC at this bus location |
| `ipv4.method`           | `static` \| `dhcp` \| `disabled`          | IPv4 addressing (default `disabled`) |
| `ipv4.address`          | `a.b.c.d/prefix`                         | the static IPv4 address (`static` only) |
| `ipv4.gateway`          | `a.b.c.d`                                | the IPv4 default gateway (`static` only) |
| `ipv6.method`           | `slaac` \| `static` \| `dhcp` \| `disabled` | IPv6 addressing (default `slaac`) |
| `ipv6.address`          | `addr/prefix`                            | the static IPv6 address (`static` only) |
| `ipv6.gateway`          | `addr`                                   | the IPv6 default gateway (`static` only) |
| `dns.servers`           | `9.9.9.9,2001:db8::1[,…]`                | recursive DNS servers for this interface |
| `mtu`                   | `1280..=65535`                           | the interface MTU |
| `bond.members`          | `eth0,eth1[,…]`                          | the member NIC aliases (bond only) |
| `bond.mode`             | `active-backup` \| `balance`             | the bond transmit policy |
| `bond.monitor-interval` | `100..=60000` (ms)                       | the member-health probe interval |
| `bond.primary`          | a member alias                           | the preferred active member |

Adding a key is adding an `IfaceKey` variant (plus its `InterfaceConfig`
field and the parse/render arms) in the same change — the compiler then
forces every reader to state what the new key means. There is no free-form
key namespace and no second store. Values are case-sensitive with exactly
one canonical spelling, so a render→parse round trip is exact.

## Bonding

A **bond** is a virtual interface `netstack` composes over two or more
member NICs (`plans/NETWORK.md` §6.3). The engine enforces the structural
invariants a per-line parse cannot, whole-document, fail-closed
(`ConfigError::InconsistentInterface`):

- a `bond.*` key may appear only on an interface of `kind bond`;
- a bond has at least `MIN_BOND_MEMBERS` (2) members, and any `bond.primary`
  must be one of them;
- every member must be a **declared** interface of `kind ethernet`, must
  carry no addressing of its own (a member's addresses belong to the bond),
  and must be enrolled in at most one bond.

Addressing is also checked for internal consistency: a `static` method
requires its `address`, and an `address`/`gateway` requires the `static`
method. A `dhcp` interface (DHCPv4 on `ipv4`, RFC 8415 stateful DHCPv6 on
`ipv6`) carries no static `address`/`gateway` — the lease supplies the
address — so setting either alongside `dhcp` is inconsistent and refused.

## DNS servers

`<iface>.dns.servers` names the recursive DNS servers to use on an
interface, as a comma-separated list of IPv4 and/or IPv6 addresses
(at most `MAX_DNS_SERVERS`, the same bound as the host-wide active
resolver set). Each entry must be a genuine unicast host address —
the unspecified address, any multicast group, and the IPv4 limited
broadcast are refused; loopback is allowed for a local resolver.
The device manager delivers the list to `netstack`, where it joins
the interface's DHCP-learned servers (option 6 / option 23) in the
active resolver set, ranking first as the operator's explicit choice.
DNS servers are addressing the bond owns, so — like an address — a
bond member may not carry a `dns.servers` key.

## API shape

- `NetworkConfig::parse(&str) -> Result<NetworkConfig, ParseError>` — the
  bounded, fail-closed, line-numbered parse.
- `NetworkConfig::render() -> String` — the canonical document (header
  comment plus one line per **set** key, in declaration + registry order,
  so render→parse round-trips exactly and shows exactly the live config).
- `NetworkConfig::{interfaces, interface}` — the parsed interface set.
- `InterfaceConfig` — one interface's `Option`-per-key state plus the
  effective-value accessors
  (`kind`/`ipv4_method`/`ipv6_method`/`members`/`dns_servers`).
- `IfaceKey::{ALL, name, from_name}` — the closed registry.
- `IfaceKind` / `Ipv4Method` / `Ipv6Method` / `BondMode` / `MacAddr` /
  `Ipv4Cidr` / `Ipv6Cidr` — the typed value vocabulary.

The crate is `no_std` + `alloc`, performs no I/O, holds no authority, is
host-unit-tested in `src/lib.rs`, and is fuzzed by `tests/fuzz_netconfig.rs`
(registered with `cargo xtask fuzz`). Stability tier: experimental
(`lib/netconfig/README.md`).

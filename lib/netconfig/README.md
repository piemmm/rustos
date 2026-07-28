# tairix-netconfig

Stability tier: **experimental**.

The network interface-configuration store engine: the one definition of the
`/System/Settings/Network/network.conf` document — its per-interface
`<iface>.<key>` line grammar, the closed key registry (`kind`, `match.mac` /
`match.node`, `ipv4.*` / `ipv6.*`, `dns.servers`, `mtu`, and the `bond.*`
aggregation keys), each key's typed value set, the bounded fail-closed parser,
and the canonical render.

The `configure` command app and the installer write the store through this
engine; the one reader — the `netstack` service, at start and on a typed
`CAP_NET_ADMIN` reload — reads it through the same engine, so producer and
consumer can never diverge. Interface names are stable, admin-chosen aliases
bound to hardware by identity (`match.mac` / `match.node`), never
discovery-order names.

The store text is untrusted input: the parser is bounded
(`MAX_CONFIG_LEN` / `MAX_INTERFACES` / `MAX_BOND_MEMBERS`) and fails closed
(`ParseError`, with the offending line where one is meaningful) on anything it
does not fully understand — an unknown or malformed key, an out-of-set or
malformed value, a duplicate, an oversized document, or a semantically
inconsistent interface set (a `bond.*` key on a non-bond, a bond with fewer
than two members, a static method without its address, a member that is
undeclared, itself addressed, or enrolled in two bonds). A malformed file
never yields a half-configured stack; the consumer keeps its running
configuration untouched. The crate performs no I/O and holds no authority:
file access goes through the secured VFS under the caller's own kernel-attested
identity, and the per-inode policy on `/System/Settings` decides who may write.

`no_std` + `alloc`; host-unit-tested in `src/lib.rs` and fuzzed by
`tests/fuzz_netconfig.rs` (registered with `cargo xtask fuzz`).

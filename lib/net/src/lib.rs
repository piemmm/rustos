//! TAIRiX network protocol engine (`lib/net`).
//!
//! This crate is the single home of the wire protocols the user-space
//! network stack speaks (`plans/NETWORK.md`). It is deliberately pure: no
//! I/O, no syscalls, no endpoints, no capability checks — the engine
//! transforms caller-owned byte slices and explicit time values, so the
//! exact code the live `netstack` service runs is the code the unit
//! tests, property tests, and fuzz harnesses exercise.
//!
//! # Contents (increments N1–N3a of `plans/NETWORK.md`)
//!
//! - [`addr`] — the dual-stack address vocabulary: IPv4 and IPv6 as
//!   equals, IPv6 scope classification and zone handling for link-local
//!   addresses.
//! - [`checksum`] — the one Internet-checksum definition (RFC 1071),
//!   including the IPv4 and IPv6 pseudo-header variants the transport
//!   layers fold over.
//! - [`eth`] — Ethernet II framing.
//! - [`arp`] — ARP for IPv4 over Ethernet (RFC 826), the IPv4 provider
//!   of the neighbour-cache contract.
//! - [`ipv4`] — the IPv4 codec (RFC 791): options-tolerant parse,
//!   strict option-free emit, and fragmentation on emit.
//! - [`ipv6`] — the IPv6 codec (RFC 8200): the fixed header and the
//!   bounded extension-header chain walk with the RFC 8200
//!   unrecognised-header/option dispositions.
//! - [`icmp`] — ICMP and `ICMPv6` over one shared machinery (RFC 792,
//!   RFC 4443): echo, error messages, and token-bucket rate-limited
//!   error generation.
//! - [`nd`] — Neighbour Discovery wire codecs (RFC 4861): RS/RA/NS/NA/
//!   redirect with hop-limit-255 enforcement, driving [`neigh`].
//! - [`frag`] — dual-stack fragment reassembly with per-source and
//!   global budgets, oldest-first eviction, and overlap ⇒ drop.
//! - [`route`] — the generic longest-prefix-match table (one trie,
//!   v4/v6 instantiations), the RFC 4861 default-router list, RFC 6724
//!   source-address selection, and the RFC 8201 path-MTU cache.
//! - [`neigh`] — the provider-agnostic neighbour cache: one bounded
//!   RFC 4861 §7.3.2 state machine that ARP and Neighbour Discovery
//!   both drive (one table, two providers).
//! - [`iface`] — the per-interface address engine: static IPv4/IPv6
//!   assignment plus RFC 4862 SLAAC (DAD, router solicitation,
//!   lifetimes) over an injected interface identifier.
//! - [`stack`] — the dual-stack host engine composing all of the
//!   above: frames in, frames + typed events out, one folded one-shot
//!   timer deadline.
//!
//! - [`udp`] — the dual-stack UDP codec (RFC 768): one parse/emit core
//!   folding the family-appropriate pseudo-header checksum, with the
//!   IPv4-optional / IPv6-mandatory checksum discipline.
//! - [`igmp`] — the IGMPv2 codec (RFC 2236) and [`mld`] the MLDv2 codec
//!   (RFC 3810): the IPv4 and IPv6 multicast group-membership message
//!   framings.
//! - [`mcast`] — the family-generic host multicast-membership engine:
//!   one join/leave/query state machine (RFC 2236 §3 / RFC 3810 §6)
//!   driven by two protocol providers, exactly as [`neigh`] is one
//!   cache driven by ARP and Neighbour Discovery.
//!
//! - [`bond`] — the pure link-aggregation decision core (`plans/NETWORK.md`
//!   §6.3): a family-agnostic bond state machine over member NICs with
//!   `active-backup` and `balance` transmit policies, link-state-driven
//!   health with an anti-flap up-delay and deliberate `primary` failback,
//!   a tickless one-shot monitor deadline, and the transmit-path-change
//!   events that drive gratuitous ARP / unsolicited NA — exactly as
//!   [`neigh`] and [`mcast`] are pure cores driven by injected time.
//!
//! - [`rate`] — the pure, tickless windowed-throughput meter that turns
//!   an interface's byte/packet counters into the live `rx.pps`/`tx.bps`
//!   rates the observability surface (`stats:net/<iface>/…`, plan §5)
//!   reports, averaged over the window that actually elapsed.
//!
//! - [`tcp`] — the TCP segment codec (RFC 9293): the header, the
//!   control flags, the recognised options (MSS, window scale,
//!   timestamps, SACK-permitted, SACK), the mandatory pseudo-header
//!   checksum, and the modulo-2³² sequence-space arithmetic
//!   ([`tcp::SeqNumber`]) the connection layer's window comparisons use.
//!   [`tcp::conn`] is the pure, event-driven RFC 9293 connection state
//!   machine built on top of it: active/passive/simultaneous open,
//!   teardown, send/receive windows, RFC 7323 scaling + timestamps
//!   (PAWS), RFC 2018 SACK, RFC 6298 retransmission with Karn's
//!   algorithm, fast retransmit, zero-window probing, RFC 5961
//!   challenge ACKs, and the user timeout — driven by injected time and
//!   a caller-supplied CSPRNG initial sequence number.
//!   [`tcp::cc`] is the pluggable congestion-control policy the
//!   connection consults for its send window: a [`tcp::cc::CongestionControl`]
//!   trait (the scheduler-policy precedent) with RFC 9438 CUBIC (default)
//!   and RFC 6582 `NewReno` siblings, held to a shared conformance suite —
//!   pure integer fixed-point arithmetic, no floating point.
//!   [`tcp::listen`] is the demultiplexing server-side listener that sits
//!   above the connection: it keeps a bounded backlog of half-open
//!   handshakes, moves completed connections onto a bounded accept queue,
//!   and — when the backlog is full, the SYN-flood condition — falls back to
//!   stateless RFC 4987 **SYN cookies** over an injected keyed-MAC seam, so a
//!   flood of spoofed SYNs consumes no per-connection memory (at the
//!   documented cost of the connection's options).
//!
//! # Security
//!
//! Every decoder in this crate parses attacker-controlled bytes. Each one
//! is total (never panics, for any input), bounded (fixed validation
//! bounds, never attacker-sized allocation), and fail-closed (a malformed
//! input is rejected whole; nothing is partially applied). The neighbour
//! cache is bounded and never creates an entry from an unsolicited
//! confirmation, so a spoofing peer cannot fill or poison it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod addr;
pub mod arp;
pub mod bond;
pub mod checksum;
pub mod eth;
pub mod frag;
pub mod icmp;
pub mod iface;
pub mod igmp;
pub mod ipv4;
pub mod ipv6;
pub mod mcast;
pub mod mld;
pub mod nd;
pub mod neigh;
pub mod rate;
pub mod route;
pub mod stack;
pub mod tcp;
mod timeutil;
pub mod udp;

pub use addr::{IpAddr, Ipv4Addr, Ipv6Addr};
pub use checksum::internet_checksum;

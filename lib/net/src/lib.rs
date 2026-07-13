//! RustOS network protocol engine (`lib/net`).
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
//!
//! Later increments extend this crate in place with `igmp`/`mld` and
//! `tcp` (`plans/NETWORK.md` §2.1); none of that surface is speculated
//! here.
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
pub mod checksum;
pub mod eth;
pub mod frag;
pub mod icmp;
pub mod iface;
pub mod ipv4;
pub mod ipv6;
pub mod nd;
pub mod neigh;
pub mod route;
pub mod stack;
pub mod udp;

pub use addr::{IpAddr, Ipv4Addr, Ipv6Addr};
pub use checksum::internet_checksum;

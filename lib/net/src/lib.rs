//! RustOS network protocol engine (`lib/net`).
//!
//! This crate is the single home of the wire protocols the user-space
//! network stack speaks (`plans/NETWORK.md`). It is deliberately pure: no
//! I/O, no syscalls, no endpoints, no capability checks — the engine
//! transforms caller-owned byte slices and explicit time values, so the
//! exact code the live `netstack` service runs is the code the unit
//! tests, property tests, and fuzz harnesses exercise.
//!
//! # Contents (increment N1 of `plans/NETWORK.md`)
//!
//! - [`addr`] — the dual-stack address vocabulary: IPv4 and IPv6 as
//!   equals, IPv6 scope classification and zone handling for link-local
//!   addresses, and the multicast IP → multicast MAC mappings.
//! - [`checksum`] — the one Internet-checksum definition (RFC 1071),
//!   including the IPv4 and IPv6 pseudo-header variants the transport
//!   layers fold over.
//! - [`eth`] — Ethernet II framing.
//! - [`arp`] — ARP for IPv4 over Ethernet (RFC 826), the IPv4 provider
//!   of the neighbour-cache contract.
//! - [`ipv4`] — the IPv4 header codec (RFC 791).
//! - [`icmp`] — ICMP echo (RFC 792).
//! - [`neigh`] — the provider-agnostic neighbour cache: one bounded
//!   RFC 4861 §7.3.2 state machine that ARP drives today and Neighbour
//!   Discovery drives when IPv6 lands (one table, two providers).
//!
//! Later increments extend this crate in place with `ipv6`, `icmpv6`/`nd`,
//! `igmp`/`mld`, `udp`, `tcp`, `route`, and `frag` (`plans/NETWORK.md` §2.1);
//! none of that surface is speculated here.
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
pub mod icmp;
pub mod ipv4;
pub mod neigh;

pub use addr::{IpAddr, Ipv4Addr, Ipv6Addr};
pub use checksum::internet_checksum;

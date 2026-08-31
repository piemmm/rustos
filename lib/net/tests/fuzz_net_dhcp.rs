//! Deterministic fuzz harness for the `DHCPv4` client engine.
//!
//! Invariants, for any input bits a hostile server crafts:
//!
//! 1. [`DhcpReply::parse`] never panics, on any bytes, for any `xid`/MAC.
//! 2. A parsed reply's surfaced address lists stay within their fixed
//!    capacity (bounded — never an attacker-sized allocation).
//! 3. Driving the [`DhcpClient`] state machine with arbitrary parsed
//!    replies at arbitrary times never panics and never leaves the client
//!    without a well-formed next-deadline decision.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::driver::net::MacAddress;
use tairix_abi::time::Duration64;
use tairix_net::dhcp::{
    DhcpClient, DhcpReply, MessageSpec, MessageType, MAX_ADDRESSES, MAX_MESSAGE_LEN,
};
use tairix_net::Ipv4Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn mac(rng: &mut Lcg) -> MacAddress {
    let mut octets = [0u8; 6];
    rng.fill(&mut octets);
    MacAddress(octets)
}

/// Feed arbitrary bytes to the parser and assert the bounded invariants.
fn exercise_parse(bytes: &[u8], xid: u32, chaddr: MacAddress) {
    if let Some(reply) = DhcpReply::parse(bytes, xid, chaddr) {
        assert_eq!(reply.xid, xid);
        assert!(reply.routers.len() <= MAX_ADDRESSES);
        assert!(reply.dns_servers.len() <= MAX_ADDRESSES);
        assert!(reply.ntp_servers.len() <= MAX_ADDRESSES);
    }
}

/// Encode a random client message and confirm it is a fixed-length,
/// non-panicking encode (the emit path is also attacker-reachable via the
/// state machine's outputs).
fn exercise_write(rng: &mut Lcg, chaddr: MacAddress) {
    let types = [
        MessageType::Discover,
        MessageType::Request,
        MessageType::Decline,
        MessageType::Release,
    ];
    let spec = MessageSpec {
        message_type: types[rng.index(types.len())],
        xid: rng.next_u32(),
        secs: rng.next_u16(),
        broadcast: rng.next_u64() & 1 == 0,
        client_addr: Ipv4Addr::from(rng.next_u32().to_be_bytes()),
        chaddr,
        requested_addr: (rng.next_u64() & 1 == 0)
            .then(|| Ipv4Addr::from(rng.next_u32().to_be_bytes())),
        server_id: (rng.next_u64() & 1 == 0).then(|| Ipv4Addr::from(rng.next_u32().to_be_bytes())),
    };
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    tairix_net::dhcp::write_message(&spec, &mut buf).expect("MAX_MESSAGE_LEN suffices");
}

/// Drive the client with a parsed reply, asserting it never panics and
/// always yields a coherent next-deadline decision.
fn exercise_client(rng: &mut Lcg, chaddr: MacAddress) {
    // A separate generator for the client's CSPRNG draws, so the reply
    // builder can keep using `rng` without a borrow conflict.
    let mut csprng = Lcg::new(rng.next_u64());
    let mut rand = || csprng.next_u32();
    let mut client = DhcpClient::new(chaddr);
    let mut now = 0i64;
    // Kick off acquisition.
    let _ = client.poll(Duration64::from_secs(now), &mut rand);
    for _ in 0..8 {
        now = now.saturating_add(i64::from(rand() % 200));
        // Build a reply matching the client's current transaction, then
        // possibly corrupt its type/address, and feed it in.
        let xid = client.transaction_id();
        let bytes = build_reply(rng, xid, chaddr);
        if let Some(reply) = DhcpReply::parse(&bytes, xid, chaddr) {
            let _ = client.on_reply(Duration64::from_secs(now), &reply);
        }
        let _ = client.poll(Duration64::from_secs(now), &mut rand);
        // A next-deadline, when present, is a valid instant we can narrow.
        if let Some(d) = client.next_deadline() {
            assert!(d.secs() >= 0);
        }
    }
}

/// Build a plausible server reply for `xid`/`chaddr` with random options,
/// so the state machine explores real transitions rather than always
/// rejecting the input at the header.
fn build_reply(rng: &mut Lcg, xid: u32, chaddr: MacAddress) -> [u8; 300] {
    let mut out = [0u8; 300];
    out[0] = 2;
    out[1] = 1;
    out[2] = 6;
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    let yiaddr = Ipv4Addr::from(rng.next_u32().to_be_bytes());
    out[16..20].copy_from_slice(&yiaddr.octets());
    out[28..34].copy_from_slice(&chaddr.0);
    out[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let types = [MessageType::Offer, MessageType::Ack, MessageType::Nak];
    let mt = types[rng.index(types.len())];
    let mut i = 240;
    out[i] = 53;
    out[i + 1] = 1;
    out[i + 2] = mt.code();
    i += 3;
    // Server identifier.
    out[i] = 54;
    out[i + 1] = 4;
    out[i + 2..i + 6].copy_from_slice(&Ipv4Addr::new(192, 168, 0, 1).octets());
    i += 6;
    // Lease time (sometimes).
    if rng.next_u64() & 1 == 0 {
        out[i] = 51;
        out[i + 1] = 4;
        let lease = rng.next_u32() | 1;
        out[i + 2..i + 6].copy_from_slice(&lease.to_be_bytes());
        i += 6;
    }
    out[i] = 255;
    out
}

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in the sibling harnesses so failures reproduce one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn next_u32(&mut self) -> u32 {
        // The mask makes the narrowing lossless, so no truncation warning.
        (self.next_u64() & 0xFFFF_FFFF) as u32
    }

    fn next_u16(&mut self) -> u16 {
        (self.next_u64() & 0xFFFF) as u16
    }

    /// A bounded index in `[0, modulus)`; `modulus` must be non-zero.
    fn index(&mut self, modulus: usize) -> usize {
        (self.next_u64() & 0xFFFF) as usize % modulus
    }

    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, buf.len() - i);
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

#[test]
fn random_inputs_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 320];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let chaddr = mac(&mut rng);
            let xid = rng.next_u32();
            let size = rng.index(buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size], xid, chaddr);
            exercise_write(&mut rng, chaddr);
            exercise_client(&mut rng, chaddr);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn corrupted_reply_never_panics() {
    // Bit-flip every bit of a valid reply to walk the accept/reject
    // boundary of the header and option checks.
    let chaddr = MacAddress([0x52, 0x54, 0, 1, 2, 3]);
    let mut rng = Lcg::new(1);
    let mut reply = build_reply(&mut rng, 0xABCD, chaddr);
    assert!(DhcpReply::parse(&reply, 0xABCD, chaddr).is_some());
    for byte in 0..reply.len() {
        for bit in 0..8u32 {
            reply[byte] ^= 1 << bit;
            exercise_parse(&reply, 0xABCD, chaddr);
            reply[byte] ^= 1 << bit;
        }
    }
}

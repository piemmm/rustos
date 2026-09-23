//! Deterministic fuzz harness for the `DHCPv6` client engine (RFC 8415).
//!
//! Invariants, for any input bits a hostile server crafts:
//!
//! 1. [`Dhcp6Reply::parse`] never panics, on any bytes, for any transaction
//!    id / DUID.
//! 2. A parsed reply's surfaced address lists stay within their fixed
//!    capacity (bounded — never an attacker-sized allocation).
//! 3. Driving the [`Dhcp6Client`] state machine with arbitrary parsed
//!    replies at arbitrary times never panics and always yields a coherent
//!    next-deadline decision.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::driver::net::MacAddress;
use tairix_abi::time::Duration64;
use tairix_fuzzseed::Prng;
use tairix_net::dhcpv6::{
    opt, status, Dhcp6Client, Dhcp6Reply, Duid, MessageSpec, MessageType, MAX_ADDRESSES,
    MAX_MESSAGE_LEN,
};
use tairix_net::Ipv6Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn mac(rng: &mut Prng) -> MacAddress {
    let mut octets = [0u8; 6];
    rng.fill(&mut octets);
    MacAddress(octets)
}

/// Feed arbitrary bytes to the parser and assert the bounded invariants.
fn exercise_parse(bytes: &[u8], xid: u32, client: &Duid) {
    if let Some(reply) = Dhcp6Reply::parse(bytes, xid, client) {
        assert_eq!(reply.transaction_id, xid & 0x00FF_FFFF);
        assert!(reply.addresses.len() <= MAX_ADDRESSES);
        assert!(reply.dns_servers.len() <= MAX_ADDRESSES);
        assert!(reply.ntp_servers.len() <= MAX_ADDRESSES);
    }
}

/// Encode a random client message and confirm it is a bounded, non-panicking
/// encode (the emit path is also attacker-reachable via the state machine).
fn exercise_write(rng: &mut Prng, client: &Duid) {
    let types = [
        MessageType::Solicit,
        MessageType::Request,
        MessageType::Renew,
        MessageType::Rebind,
        MessageType::Release,
        MessageType::Decline,
    ];
    let spec = MessageSpec {
        message_type: *rng.pick(&types),
        transaction_id: rng.next_u32(),
        client_duid: *client,
        server_id: (rng.next_u64() & 1 == 0).then(|| Duid::ll_ethernet(mac(rng))),
        iaid: rng.next_u32(),
        elapsed_centis: rng.next_u16(),
        ia_addr: (rng.next_u64() & 1 == 0).then(|| addr(rng)),
        request_options: rng.next_u64() & 1 == 0,
    };
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    tairix_net::dhcpv6::write_message(&spec, &mut buf).expect("MAX_MESSAGE_LEN suffices");
}

/// Drive the client with a parsed reply, asserting it never panics and
/// always yields a coherent next-deadline decision.
fn exercise_client(rng: &mut Prng, mac: MacAddress) {
    let mut csprng = Prng::new(rng.next_u64());
    let mut rand = || csprng.next_u32();
    let mut client = Dhcp6Client::new(mac, rng.next_u32());
    let duid = Duid::ll_ethernet(mac);
    let mut now = 0i64;
    let _ = client.poll(Duration64::from_secs(now), &mut rand);
    for step in 0..8 {
        now = now.saturating_add(i64::from(rand() % 4000));
        let xid = client.transaction_id();
        let bytes = build_message(rng, xid, &duid);
        if let Some(reply) = Dhcp6Reply::parse(&bytes, xid, &duid) {
            let _ = client.on_reply(Duration64::from_secs(now), &reply, &mut rand);
        }
        let _ = client.poll(Duration64::from_secs(now), &mut rand);
        // Occasionally exercise the teardown paths too.
        if step == 3 {
            let _ = client.release(Duration64::from_secs(now), &mut rand);
        }
        if let Some(d) = client.next_deadline() {
            assert!(d.secs() >= 0);
        }
    }
}

fn addr(rng: &mut Prng) -> Ipv6Addr {
    let mut octets = [0u8; 16];
    rng.fill(&mut octets);
    Ipv6Addr::from(octets)
}

/// Build a plausible server reply for `xid`/`client` with random options,
/// so the state machine explores real transitions rather than always
/// rejecting the input at the header.
fn build_message(rng: &mut Prng, xid: u32, client: &Duid) -> Vec<u8> {
    let mut out = Vec::new();
    let types = [MessageType::Advertise, MessageType::Reply];
    out.push(rng.pick(&types).code());
    out.extend_from_slice(&xid.to_be_bytes()[1..4]);
    push_option(&mut out, opt::CLIENT_ID, client.as_slice());
    let server = Duid::ll_ethernet(MacAddress([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
    push_option(&mut out, opt::SERVER_ID, server.as_slice());

    // A random IA_NA, sometimes carrying an IA Address, sometimes a status.
    let mut ia = Vec::new();
    ia.extend_from_slice(&rng.next_u32().to_be_bytes()); // IAID
    ia.extend_from_slice(&(rng.next_u32() % 4000).to_be_bytes()); // T1
    ia.extend_from_slice(&(rng.next_u32() % 8000).to_be_bytes()); // T2
    if rng.next_u64() & 1 == 0 {
        let mut iaddr = Vec::new();
        iaddr.extend_from_slice(&addr(rng).octets());
        iaddr.extend_from_slice(&((rng.next_u32() % 8000) | 1).to_be_bytes()); // preferred
        iaddr.extend_from_slice(&((rng.next_u32() % 16000) | 1).to_be_bytes()); // valid
        push_option(&mut ia, opt::IA_ADDR, &iaddr);
    } else {
        let codes = [status::SUCCESS, status::NO_ADDRS_AVAIL, status::NO_BINDING];
        let sc = *rng.pick(&codes);
        push_option(&mut ia, opt::STATUS_CODE, &sc.to_be_bytes());
    }
    push_option(&mut out, opt::IA_NA, &ia);
    out
}

fn push_option(out: &mut Vec<u8>, code: u16, data: &[u8]) {
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
    out.extend_from_slice(data);
}

#[test]
fn random_inputs_never_panic() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 320];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let mac = mac(&mut rng);
            let duid = Duid::ll_ethernet(mac);
            let xid = rng.next_u32();
            let size = rng.below(buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size], xid, &duid);
            exercise_write(&mut rng, &duid);
            exercise_client(&mut rng, mac);
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
    let mac = MacAddress([0x52, 0x54, 0, 1, 2, 3]);
    let duid = Duid::ll_ethernet(mac);
    let mut rng = Prng::new(1);
    let mut reply = build_message(&mut rng, 0x00AB_CDEF, &duid);
    assert!(Dhcp6Reply::parse(&reply, 0x00AB_CDEF, &duid).is_some());
    for byte in 0..reply.len() {
        for bit in 0..8u32 {
            reply[byte] ^= 1 << bit;
            exercise_parse(&reply, 0x00AB_CDEF, &duid);
            reply[byte] ^= 1 << bit;
        }
    }
}

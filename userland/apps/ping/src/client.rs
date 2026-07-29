//! The `ping` engine: send echo requests, collect replies, print each
//! result and the final statistics.
//!
//! Pure and host-testable: it drives the injected [`PingIo`] seam (clock,
//! echo socket, wait/park) and the [`Output`] seam, never a syscall.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::net::{Ipv4Addr, Ipv6Addr};

use tairix_abi::net_ipc::NetAddrFamily;
use tairix_help::{own_short_help, HelpSource};

use crate::command::{Command, Config};
use crate::error::PingError;
use crate::io::Output;
use crate::net::PingIo;

/// The one-line usage banner, printed on a usage error and as the fallback
/// when the bundled help document is unavailable.
pub const USAGE: &str = "usage: ping [-c count] [-i interval] [-s size] [-W timeout] \
                         [-w deadline] [-46nq] <address>";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "ping";

/// Nanoseconds in one millisecond.
const NANOS_PER_MS: u64 = 1_000_000;

/// The outcome of a completed ping run: enough to choose the exit code.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunSummary {
    /// Requests sent.
    pub transmitted: u32,
    /// Replies received and verified.
    pub received: u32,
}

impl RunSummary {
    /// Whether at least one reply came back (the exit-`0` condition).
    #[must_use]
    pub fn any_received(&self) -> bool {
        self.received > 0
    }
}

/// Round-trip-time accumulator, integer-only (`no_std`, no float/libm).
#[derive(Clone, Copy, Debug, Default)]
struct RttStats {
    count: u32,
    min_ns: u64,
    max_ns: u64,
    total_ns: u128,
}

impl RttStats {
    fn record(&mut self, rtt_ns: u64) {
        if self.count == 0 || rtt_ns < self.min_ns {
            self.min_ns = rtt_ns;
        }
        if rtt_ns > self.max_ns {
            self.max_ns = rtt_ns;
        }
        self.total_ns += u128::from(rtt_ns);
        self.count += 1;
    }
}

/// Run a parsed `ping` command against the injected seams.
///
/// # Errors
///
/// A [`PingError`] when the socket cannot be opened/connected, a send or
/// receive fails fatally, or a line cannot be written. A run in which every
/// request timed out is **not** an error — it completes and returns a
/// [`RunSummary`] with `received == 0` (the caller maps that to exit `1`).
pub fn run(
    command: Command,
    locale: Option<&str>,
    io: &mut dyn PingIo,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<RunSummary, PingError> {
    let _ = err;
    let config = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(PingError::Output)?;
            return Ok(RunSummary::default());
        }
        Command::Run(config) => config,
    };
    ping(&config, io, out)
}

/// The ping loop proper: send each request, await its reply within the
/// per-reply timeout, print the result, and print the closing statistics.
fn ping(config: &Config, io: &mut dyn PingIo, out: &dyn Output) -> Result<RunSummary, PingError> {
    let payload = build_payload(config.size);
    let start = io.now();
    let overall_deadline = config.deadline_ns.map(|d| start.saturating_add(d));

    out.write_all(header(config).as_bytes())
        .map_err(PingError::Output)?;

    let mut summary = RunSummary::default();
    let mut rtt = RttStats::default();
    let mut seq: u16 = 0;
    while config.count.is_none_or(|c| u32::from(seq) < c) {
        if let Some(deadline) = overall_deadline {
            if io.now() >= deadline {
                break;
            }
        }
        seq = seq.wrapping_add(1);
        let sent_at = io.now();
        summary.transmitted += 1;
        match io.send(seq, &payload) {
            Ok(()) => {
                await_reply(
                    config,
                    io,
                    out,
                    &payload,
                    seq,
                    sent_at,
                    &mut summary,
                    &mut rtt,
                )?;
            }
            // NetworkUnreachable while the link is still coming up is worth
            // reporting but not fatal to the whole run — the next request
            // may succeed once the interface binds.
            Err(errno) => {
                let line = format!("ping: sendto: {errno}\n");
                if !config.quiet {
                    out.write_all(line.as_bytes()).map_err(PingError::Output)?;
                }
            }
        }
        let last = config.count.is_some_and(|c| u32::from(seq) >= c);
        if !last {
            io.sleep_until(sent_at.saturating_add(config.interval_ns));
        }
    }

    out.write_all(statistics(config, summary, &rtt, io.now().saturating_sub(start)).as_bytes())
        .map_err(PingError::Output)?;
    Ok(summary)
}

/// Await the reply to `seq` within its timeout, discarding stray or
/// mismatched replies, and record the outcome.
#[allow(clippy::too_many_arguments)]
fn await_reply(
    config: &Config,
    io: &mut dyn PingIo,
    out: &dyn Output,
    payload: &[u8],
    seq: u16,
    sent_at: u64,
    summary: &mut RunSummary,
    rtt: &mut RttStats,
) -> Result<(), PingError> {
    let deadline = sent_at.saturating_add(config.timeout_ns);
    loop {
        match io.recv(deadline).map_err(PingError::Receive)? {
            Some(reply) if reply.seq == seq && reply_matches(config, payload, &reply) => {
                let rtt_ns = io.now().saturating_sub(sent_at);
                rtt.record(rtt_ns);
                summary.received += 1;
                if !config.quiet {
                    let line = reply_line(&reply, rtt_ns);
                    out.write_all(line.as_bytes()).map_err(PingError::Output)?;
                }
                return Ok(());
            }
            // A stray/old/corrupt reply: keep waiting until the deadline.
            Some(_) => {}
            None => {
                if !config.quiet {
                    let line = format!("Request timeout for icmp_seq {seq}\n");
                    out.write_all(line.as_bytes()).map_err(PingError::Output)?;
                }
                return Ok(());
            }
        }
    }
}

/// Whether a reply's source and payload match what we sent (defence in
/// depth: the stack already filters by identifier and connected peer).
fn reply_matches(config: &Config, payload: &[u8], reply: &crate::net::EchoReply) -> bool {
    reply.family == config.target.family
        && reply.addr == config.target.addr
        && reply.payload == payload
}

/// Build the deterministic echo payload: a repeating byte pattern so a
/// corrupt reply is detectable.
fn build_payload(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
        .collect()
}

/// The opening `PING …` header line.
fn header(config: &Config) -> String {
    let addr = render_addr(config.target.family, &config.target.addr);
    match config.target.family {
        NetAddrFamily::V4 => {
            // v4 total on the wire = payload + 8 (ICMP) + 20 (IPv4).
            let total = config.size + 28;
            format!(
                "PING {} ({}) {}({}) bytes of data.\n",
                config.target.display, addr, config.size, total
            )
        }
        NetAddrFamily::V6 => format!(
            "PING {} ({}) {} data bytes\n",
            config.target.display, addr, config.size
        ),
    }
}

/// One `<n> bytes from <addr>: icmp_seq=<seq> time=<ms> ms` line.
///
/// The IP TTL is not surfaced through the echo-socket abstraction, so —
/// unlike iputils — the line carries no `ttl=` field (a documented
/// TAIRiX-specific divergence).
fn reply_line(reply: &crate::net::EchoReply, rtt_ns: u64) -> String {
    let addr = render_addr(reply.family, &reply.addr);
    // "bytes from" counts the ICMP message: the 8-byte header plus data.
    let bytes = reply.payload.len() + 8;
    format!(
        "{} bytes from {}: icmp_seq={} time={} ms\n",
        bytes,
        addr,
        u32::from(reply.seq),
        format_ms(rtt_ns),
    )
}

/// The closing statistics block.
fn statistics(config: &Config, summary: RunSummary, rtt: &RttStats, elapsed_ns: u64) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "\n--- {} ping statistics ---", config.target.display);
    let loss = if summary.transmitted == 0 {
        0
    } else {
        (u64::from(summary.transmitted - summary.received) * 100) / u64::from(summary.transmitted)
    };
    let _ = writeln!(
        text,
        "{} packets transmitted, {} received, {}% packet loss, time {}ms",
        summary.transmitted,
        summary.received,
        loss,
        elapsed_ns / NANOS_PER_MS,
    );
    if rtt.count > 0 {
        // Integer average: total / count, both in nanoseconds.
        let avg_ns = u64::try_from(rtt.total_ns / u128::from(rtt.count)).unwrap_or(u64::MAX);
        let _ = writeln!(
            text,
            "rtt min/avg/max = {}/{}/{} ms",
            format_ms(rtt.min_ns),
            format_ms(avg_ns),
            format_ms(rtt.max_ns),
        );
    }
    text
}

/// Format a nanosecond duration as milliseconds with three decimals,
/// integer-only (`X.YYY`).
fn format_ms(ns: u64) -> String {
    let whole = ns / NANOS_PER_MS;
    // Microseconds within the millisecond → three fractional digits.
    let frac = (ns % NANOS_PER_MS) / 1_000;
    format!("{whole}.{frac:03}")
}

/// Render an address for display.
fn render_addr(family: NetAddrFamily, addr: &[u8; 16]) -> String {
    match family {
        NetAddrFamily::V4 => {
            let a = Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]);
            format!("{a}")
        }
        NetAddrFamily::V6 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(addr);
            format!("{}", Ipv6Addr::from(octets))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Target;
    use crate::net::EchoReply;
    use alloc::collections::VecDeque;
    use alloc::string::ToString;
    use alloc::vec;
    use core::cell::RefCell;
    use tairix_abi::Errno;

    /// An in-memory `PingIo`: it echoes every sent request unless its `seq`
    /// is in `drop`, advancing a synthetic monotonic clock by `rtt_step` on
    /// each delivered reply. `send_err` forces a fatal-ish send error for
    /// the modelled unreachable-link case.
    struct FakeIo {
        clock: u64,
        rtt_step: u64,
        family: NetAddrFamily,
        addr: [u8; 16],
        drop: Vec<u16>,
        send_err: Option<Errno>,
        replies: VecDeque<EchoReply>,
    }

    impl FakeIo {
        fn v4() -> Self {
            let mut addr = [0u8; 16];
            addr[..4].copy_from_slice(&[10, 0, 2, 2]);
            Self {
                clock: 1_000,
                rtt_step: 250_000, // 0.250 ms
                family: NetAddrFamily::V4,
                addr,
                drop: Vec::new(),
                send_err: None,
                replies: VecDeque::new(),
            }
        }
    }

    impl PingIo for FakeIo {
        fn now(&self) -> u64 {
            self.clock
        }

        fn send(&mut self, seq: u16, payload: &[u8]) -> Result<(), Errno> {
            if let Some(err) = self.send_err {
                return Err(err);
            }
            if !self.drop.contains(&seq) {
                self.replies.push_back(EchoReply {
                    seq,
                    family: self.family,
                    addr: self.addr,
                    payload: payload.to_vec(),
                });
            }
            Ok(())
        }

        fn recv(&mut self, deadline_ns: u64) -> Result<Option<EchoReply>, Errno> {
            if let Some(reply) = self.replies.pop_front() {
                self.clock += self.rtt_step;
                Ok(Some(reply))
            } else {
                // No reply queued: the request was dropped — the wait times
                // out at the deadline.
                self.clock = self.clock.max(deadline_ns);
                Ok(None)
            }
        }

        fn sleep_until(&mut self, deadline_ns: u64) {
            if deadline_ns > self.clock {
                self.clock = deadline_ns;
            }
        }
    }

    /// A buffer `Output` capturing everything written.
    struct BufOut(RefCell<Vec<u8>>);

    impl BufOut {
        fn new() -> Self {
            Self(RefCell::new(Vec::new()))
        }
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("utf8")
        }
    }

    impl Output for BufOut {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn config(count: Option<u32>, quiet: bool) -> Config {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[10, 0, 2, 2]);
        Config {
            target: Target {
                display: "10.0.2.2".to_string(),
                family: NetAddrFamily::V4,
                addr,
            },
            count,
            interval_ns: 0,
            timeout_ns: 1_000_000_000,
            deadline_ns: None,
            size: 4,
            quiet,
        }
    }

    #[test]
    fn all_replies_are_received_and_reported() {
        let mut io = FakeIo::v4();
        let out = BufOut::new();
        let summary = ping(&config(Some(3), false), &mut io, &out).expect("run");
        assert_eq!(summary.transmitted, 3);
        assert_eq!(summary.received, 3);
        assert!(summary.any_received());
        let text = out.text();
        assert!(text.starts_with("PING 10.0.2.2 (10.0.2.2) 4(32) bytes of data."));
        assert_eq!(text.matches("bytes from 10.0.2.2").count(), 3);
        assert!(text.contains("icmp_seq=1"));
        assert!(text.contains("time=0.250 ms"));
        assert!(text.contains("3 packets transmitted, 3 received, 0% packet loss"));
        assert!(text.contains("rtt min/avg/max = 0.250/0.250/0.250 ms"));
    }

    #[test]
    fn a_dropped_request_is_a_timeout_and_counts_as_loss() {
        let mut io = FakeIo::v4();
        io.drop = vec![2];
        let out = BufOut::new();
        let summary = ping(&config(Some(3), false), &mut io, &out).expect("run");
        assert_eq!(summary.transmitted, 3);
        assert_eq!(summary.received, 2);
        let text = out.text();
        assert!(text.contains("Request timeout for icmp_seq 2"));
        assert!(text.contains("3 packets transmitted, 2 received, 33% packet loss"));
    }

    #[test]
    fn quiet_suppresses_per_reply_lines_but_keeps_statistics() {
        let mut io = FakeIo::v4();
        let out = BufOut::new();
        ping(&config(Some(2), true), &mut io, &out).expect("run");
        let text = out.text();
        assert!(!text.contains("bytes from"));
        assert!(text.contains("2 packets transmitted, 2 received, 0% packet loss"));
    }

    #[test]
    fn a_send_error_is_reported_and_the_run_completes() {
        let mut io = FakeIo::v4();
        io.send_err = Some(Errno::NetworkUnreachable);
        let out = BufOut::new();
        let summary = ping(&config(Some(2), false), &mut io, &out).expect("run completes");
        assert_eq!(summary.transmitted, 2);
        assert_eq!(summary.received, 0);
        assert!(!summary.any_received());
        let text = out.text();
        assert!(text.contains("ping: sendto:"));
        assert!(text.contains("2 packets transmitted, 0 received, 100% packet loss"));
        assert!(!text.contains("rtt min/avg/max"));
    }

    #[test]
    fn a_reply_from_a_wrong_source_is_not_counted() {
        let mut io = FakeIo::v4();
        // Corrupt the queued reply's source after send by intercepting: use
        // a drop plus a manually-queued foreign reply.
        io.drop = vec![1];
        let mut foreign_addr = [0u8; 16];
        foreign_addr[..4].copy_from_slice(&[10, 0, 2, 99]);
        io.replies.push_back(EchoReply {
            seq: 1,
            family: NetAddrFamily::V4,
            addr: foreign_addr,
            payload: vec![0, 1, 2, 3],
        });
        let out = BufOut::new();
        let summary = ping(&config(Some(1), false), &mut io, &out).expect("run");
        // The foreign reply is discarded; the request times out.
        assert_eq!(summary.received, 0);
        assert!(out.text().contains("Request timeout for icmp_seq 1"));
    }

    #[test]
    fn format_ms_renders_three_decimals() {
        assert_eq!(format_ms(0), "0.000");
        assert_eq!(format_ms(1_000_000), "1.000");
        assert_eq!(format_ms(1_234_000), "1.234");
        assert_eq!(format_ms(250_000), "0.250");
    }
}

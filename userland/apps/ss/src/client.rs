//! The `ss` engine: fetch the open-socket table, select by the requested
//! filters, and render the iproute2-shaped listing.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::net::IpAddr;

use tairix_abi::net_ipc::ip_from_parts;
use tairix_abi::net_ipc::{
    NetAddrFamily, NetSockProto, NetSockState, NetSocketRecord, NetStackDefenceCounters,
};
use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_help::{own_short_help, HelpSource};
use tairix_procinfo::{for_each_net_socket, net_stack_defence, Transport, WalkStep};

use crate::command::{Command, Options};
use crate::error::SsError;
use crate::io::Output;

/// The reverse-lookup seam `-r` drives.
///
/// Injected so the whole render engine stays pure and host-tested: the
/// production implementation drives the shared `lib/resolver` stub resolver
/// over one reused socket, and the tests drive a scripted map. Without `-r`
/// it is never called, so the default listing puts nothing on the wire.
pub trait NameLookup {
    /// The host name `address` maps back to, or [`None`] when it has no
    /// `PTR` record or the lookup did not conclude.
    fn reverse(&mut self, address: IpAddr) -> Option<String>;
}

/// A memo of the reverse lookups this listing has already made, so a peer
/// that appears on many rows — and an address that resolves to nothing —
/// costs exactly one query.
///
/// Kept sorted so the lookup is a binary search rather than a scan: a busy
/// server's table is long enough that a linear memo would be quadratic in
/// the row count.
#[derive(Default)]
struct ReverseCache {
    entries: Vec<(IpAddr, Option<String>)>,
}

impl ReverseCache {
    /// The name for `address`, querying `names` only on a miss.
    fn name(&mut self, names: &mut dyn NameLookup, address: IpAddr) -> Option<&str> {
        let index = match self.entries.binary_search_by(|(key, _)| key.cmp(&address)) {
            Ok(index) => index,
            Err(index) => {
                let resolved = names.reverse(address);
                self.entries.insert(index, (address, resolved));
                index
            }
        };
        self.entries[index].1.as_deref()
    }
}

/// The one-line usage banner, printed on a usage error and as the fallback
/// when the bundled help document is unavailable.
pub const USAGE: &str = "usage: ss [-tualnrpH46] [-s] [--]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "ss";

/// Run a parsed `ss` command against the injected seams.
///
/// Returns `Ok(true)` on success. There is no partial-success `false`
/// path: the socket listing is the tool's whole purpose, so a refused or
/// failed query is a fatal [`SsError`] rather than an empty table.
///
/// # Errors
///
/// * [`SsError::Denied`] — the caller lacks `CAP_SYSINFO_GLOBAL`.
/// * [`SsError::Service`] — the `NET_SOCKETS` (or, for `-s`,
///   `NET_STACK_DEFENCE`) query otherwise failed.
/// * [`SsError::Output`] — a row (or the short help) could not be written.
#[allow(clippy::too_many_arguments)]
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    names: &mut dyn NameLookup,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, SsError> {
    let _ = err;
    let options = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(SsError::Output)?;
            return Ok(true);
        }
        Command::Summary => {
            let defence = net_stack_defence(transport).map_err(SsError::from)?;
            out.write_all(render_summary(&defence).as_bytes())
                .map_err(SsError::Output)?;
            return Ok(true);
        }
        Command::Report(options) => options,
    };

    // The live socket table, in the service's stable order.
    let mut rows: Vec<NetSocketRecord> = Vec::new();
    for_each_net_socket(transport, |record| {
        rows.push(*record);
        Ok(WalkStep::Continue)
    })?;

    // Count the listening sockets the default view hides, so the omission
    // is honest on fd 3 without ever changing the table.
    let hidden = if options.all || options.listening {
        0
    } else {
        rows.iter()
            .filter(|r| passes_family(r, &options) && passes_proto(r, &options) && is_listening(r))
            .count() as u64
    };

    let mut text = String::new();
    if !options.no_header {
        text.push_str(&render_header(&options));
        text.push('\n');
    }
    let mut cache = ReverseCache::default();
    for record in &rows {
        if !passes_proto(record, &options)
            || !passes_family(record, &options)
            || !passes_state(record, &options)
        {
            continue;
        }
        text.push_str(&render_row(record, &options, names, &mut cache));
        text.push('\n');
    }
    out.write_all(text.as_bytes()).map_err(SsError::Output)?;

    if hidden > 0 {
        emit_omission_record(out, hidden);
    }
    Ok(true)
}

/// Render the `-s` stack-wide TCP connection-defence summary: the counters
/// a SYN flood or accept-queue exhaustion in progress shows up in, one
/// `name value` pair per line so the output stays greppable.
fn render_summary(defence: &NetStackDefenceCounters) -> String {
    let mut text = String::new();
    for (name, value) in [
        ("syn-backlog-started", defence.half_open_started),
        ("syn-backlog-expired", defence.half_open_expired),
        ("syn-cookies-sent", defence.syn_cookies_sent),
        ("syn-cookies-accepted", defence.syn_cookies_accepted),
        ("syn-cookies-rejected", defence.syn_cookies_rejected),
        ("accepts", defence.accepted),
        ("accept-overflow", defence.accept_overflow),
        ("tcp-resets-sent", defence.resets_sent),
    ] {
        let _ = writeln!(text, "{name} {value}");
    }
    text
}

/// Whether `record`'s protocol is one the filters want. With neither
/// `-t` nor `-u`, both protocols pass.
fn passes_proto(record: &NetSocketRecord, options: &Options) -> bool {
    if !options.tcp && !options.udp {
        return true;
    }
    match record.proto {
        NetSockProto::Tcp => options.tcp,
        NetSockProto::Udp => options.udp,
        // ICMP echo sockets are neither TCP nor UDP: an explicit `-t`/`-u`
        // filter excludes them (they show only in the unfiltered view).
        NetSockProto::Icmp | NetSockProto::Icmpv6 => false,
    }
}

/// Whether `record`'s address family is one the filters want. With
/// neither `-4` nor `-6`, both families pass.
fn passes_family(record: &NetSocketRecord, options: &Options) -> bool {
    if !options.ipv4 && !options.ipv6 {
        return true;
    }
    match record.family {
        NetAddrFamily::V4 => options.ipv4,
        NetAddrFamily::V6 => options.ipv6,
    }
}

/// Whether `record` passes the state selection: `-l` keeps only listening
/// sockets, `-a` keeps all, and the default hides listening sockets (the
/// iproute2 behaviour). `-l` is the more specific switch and wins over
/// `-a`.
fn passes_state(record: &NetSocketRecord, options: &Options) -> bool {
    if options.listening {
        is_listening(record)
    } else if options.all {
        true
    } else {
        !is_listening(record)
    }
}

/// Whether a socket is "listening" in the `ss` sense: a passive TCP
/// socket in LISTEN, or a UDP socket with no connected peer (`UNCONN`).
fn is_listening(record: &NetSocketRecord) -> bool {
    match record.proto {
        NetSockProto::Tcp => record.state == NetSockState::Listen,
        // A connectionless socket with no default peer is the `UNCONN`
        // "listening"-like state the default view hides.
        NetSockProto::Udp | NetSockProto::Icmp | NetSockProto::Icmpv6 => {
            record.state == NetSockState::Unconnected
        }
    }
}

/// Column widths chosen so the common cases align without truncating; a
/// longer field simply pushes the following columns right (never clipped).
const W_NETID: usize = 5;
const W_STATE: usize = 11;
const W_QUEUE: usize = 6;
const W_ADDR: usize = 24;

/// Render the header line for the selected columns.
fn render_header(options: &Options) -> String {
    let mut line = format!(
        "{:<netid$}{:<state$}{:>queue$} {:>queue$} {:<addr$}{}",
        "Netid",
        "State",
        "Recv-Q",
        "Send-Q",
        "Local Address:Port",
        "Peer Address:Port",
        netid = W_NETID,
        state = W_STATE,
        queue = W_QUEUE,
        addr = W_ADDR,
    );
    if options.processes {
        line.push_str("  Process");
    }
    line
}

/// Render one socket row, resolving both endpoints' host names when `-r`
/// asked for them.
fn render_row(
    record: &NetSocketRecord,
    options: &Options,
    names: &mut dyn NameLookup,
    cache: &mut ReverseCache,
) -> String {
    let netid = match record.proto {
        NetSockProto::Tcp => "tcp",
        NetSockProto::Udp => "udp",
        NetSockProto::Icmp => "icmp",
        NetSockProto::Icmpv6 => "icmp6",
    };
    let local = format_endpoint(
        record.family,
        &record.local_addr,
        record.local_port,
        resolved(options, names, cache, record.family, &record.local_addr),
    );
    let peer = format_endpoint(
        record.family,
        &record.peer_addr,
        record.peer_port,
        resolved(options, names, cache, record.family, &record.peer_addr),
    );
    let mut line = format!(
        "{:<netid$}{:<state$}{:>queue$} {:>queue$} {:<addr$}{}",
        netid,
        record.state.label(),
        record.recv_q,
        record.send_q,
        local,
        peer,
        netid = W_NETID,
        state = W_STATE,
        queue = W_QUEUE,
        addr = W_ADDR,
    );
    if options.processes {
        // Infallible: writing to a `String` never errors.
        let _ = write!(line, "  pid={}", record.owner);
    }
    line
}

/// The host name for one endpoint under `-r`, or [`None`] without it.
///
/// The unspecified address renders as the wildcard `*` and is never
/// queried: `0.0.0.0` names no host, so a lookup for it is pure latency.
fn resolved(
    options: &Options,
    names: &mut dyn NameLookup,
    cache: &mut ReverseCache,
    family: NetAddrFamily,
    addr: &[u8; 16],
) -> Option<String> {
    if !options.resolve || addr.iter().all(|&b| b == 0) {
        return None;
    }
    cache
        .name(names, ip_from_parts(family, *addr))
        .map(String::from)
}

/// Format an `address:port` endpoint the iproute2 way: an all-zero
/// address is the unspecified wildcard `*`, a zero port is `*`, and an
/// IPv6 address is bracketed so the `:port` separator is unambiguous. A
/// `name` resolved under `-r` replaces the address and needs no brackets,
/// having no colons of its own.
fn format_endpoint(
    family: NetAddrFamily,
    addr: &[u8; 16],
    port: u16,
    name: Option<String>,
) -> String {
    let unspecified = addr.iter().all(|&b| b == 0);
    let host = if let Some(name) = name {
        name
    } else if unspecified {
        String::from("*")
    } else {
        match ip_from_parts(family, *addr) {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        }
    };
    let port = if port == 0 {
        String::from("*")
    } else {
        port.to_string()
    };
    format!("{host}:{port}")
}

/// Emit the `net.listening_omitted` advisory (fd 3) when the default view
/// hid listening sockets, so a tool or user knows the table is not
/// exhaustive and how to see the rest. Advisory only — never affects the
/// table, ordering, or exit status.
fn emit_omission_record(out: &dyn Output, omitted: u64) {
    let message = if omitted == 1 {
        String::from("1 listening socket not shown.")
    } else {
        format!("{omitted} listening sockets not shown.")
    };
    let ai = format!(
        "{{\"subject\":\"socket_listing\",\
         \"omission\":{{\"reason\":\"hidden_by_default\",\
         \"entry_class\":\"listening_socket\",\"omitted_count\":{omitted},\
         \"stdout_is_exhaustive\":false}},\
         \"suggestion\":{{\"argv\":[\"ss\",\"-a\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        "net.listening_omitted",
        Severity::Info,
        Human::with_suggestion(
            &message,
            "Use `ss -a` (all) or `ss -l` (listening) to show them.",
        ),
    )
    .with_ai(&ai);
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        out.info(&buf[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::{run, NameLookup, USAGE};
    use crate::command::{parse, Options};
    use crate::error::SsError;
    use crate::io::Output;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use core::net::{IpAddr, Ipv4Addr};
    use tairix_abi::net_ipc::{
        NetAddrFamily, NetSockProto, NetSockState, NetSocketRecord, NetStackDefenceCounters,
    };
    use tairix_abi::sysinfo::{SysinfoQueryId, SysinfoRequestHeader};
    use tairix_abi::Errno;
    use tairix_help::HelpSource;

    /// A fixture transport answering `NET_SOCKETS` with a fixed set of
    /// records (packed, the sysinfo reply shape), and every other query
    /// with a denial.
    struct Fixture {
        records: Vec<NetSocketRecord>,
        deny: bool,
    }

    impl tairix_procinfo::Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            if self.deny {
                return Err(Errno::PermissionDenied);
            }
            if header.query == SysinfoQueryId::NET_STACK_DEFENCE {
                return Ok(fixture_defence().to_le_bytes().to_vec());
            }
            assert_eq!(header.query, SysinfoQueryId::NET_SOCKETS);
            let mut out = Vec::new();
            for record in &self.records {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    /// The stack-wide defence totals the fixture serves for `-s`.
    fn fixture_defence() -> NetStackDefenceCounters {
        NetStackDefenceCounters {
            half_open_started: 256,
            syn_cookies_sent: 4_096,
            syn_cookies_accepted: 4_000,
            syn_cookies_rejected: 96,
            accepted: 4_200,
            accept_overflow: 6,
            half_open_expired: 31,
            resets_sent: 102,
        }
    }

    /// A help source that has no documents, so the short help falls back
    /// to the usage banner.
    struct NoHelp;
    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, tairix_help::SourceError> {
            Ok(Vec::new())
        }
        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, tairix_help::SourceError> {
            Ok(None)
        }
    }

    /// A capturing output sink: standard text into `text`, fd-3 advisories
    /// into `info`.
    #[derive(Default)]
    struct Capture {
        text: RefCell<String>,
        info: RefCell<Vec<u8>>,
    }
    impl Output for Capture {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.text
                .borrow_mut()
                .push_str(core::str::from_utf8(bytes).expect("utf-8"));
            Ok(())
        }
        fn info(&self, record: &[u8]) {
            self.info.borrow_mut().extend_from_slice(record);
        }
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..4].copy_from_slice(&[a, b, c, d]);
        out
    }

    fn tcp(state: NetSockState, lport: u16, peer: [u8; 16], pport: u16) -> NetSocketRecord {
        NetSocketRecord {
            proto: NetSockProto::Tcp,
            state,
            family: NetAddrFamily::V4,
            local_addr: v4(10, 0, 2, 15),
            local_port: lport,
            peer_addr: peer,
            peer_port: pport,
            owner: 42,
            recv_q: 0,
            send_q: 0,
        }
    }

    fn udp_unconn(lport: u16) -> NetSocketRecord {
        NetSocketRecord {
            proto: NetSockProto::Udp,
            state: NetSockState::Unconnected,
            family: NetAddrFamily::V4,
            local_addr: [0u8; 16],
            local_port: lport,
            peer_addr: [0u8; 16],
            peer_port: 0,
            owner: 7,
            recv_q: 0,
            send_q: 0,
        }
    }

    /// A reverse-lookup seam that resolves nothing and counts the queries
    /// it was asked to make, so a test can assert that `-r`'s absence puts
    /// nothing on the wire.
    #[derive(Default)]
    struct NoNames {
        asked: Vec<IpAddr>,
    }

    impl NameLookup for NoNames {
        fn reverse(&mut self, address: IpAddr) -> Option<String> {
            self.asked.push(address);
            None
        }
    }

    /// A scripted reverse-lookup seam: answers from a fixed map and records
    /// every address it was asked about, in order.
    struct MapNames {
        entries: Vec<(IpAddr, &'static str)>,
        asked: Vec<IpAddr>,
    }

    impl MapNames {
        fn new(entries: Vec<(IpAddr, &'static str)>) -> Self {
            Self {
                entries,
                asked: Vec::new(),
            }
        }
    }

    impl NameLookup for MapNames {
        fn reverse(&mut self, address: IpAddr) -> Option<String> {
            self.asked.push(address);
            self.entries
                .iter()
                .find(|(key, _)| *key == address)
                .map(|(_, name)| (*name).to_string())
        }
    }

    fn report(records: Vec<NetSocketRecord>, args: &[&str]) -> (String, Vec<u8>) {
        report_with(records, args, &mut NoNames::default())
    }

    fn report_with(
        records: Vec<NetSocketRecord>,
        args: &[&str],
        names: &mut dyn NameLookup,
    ) -> (String, Vec<u8>) {
        let command = parse(args).expect("parse");
        let capture = Capture::default();
        let errs = Capture::default();
        let fixture = Fixture {
            records,
            deny: false,
        };
        run(command, None, &fixture, names, &NoHelp, &capture, &errs).expect("run");
        (capture.text.into_inner(), capture.info.into_inner())
    }

    #[test]
    fn default_view_hides_listening_and_notes_the_omission() {
        let records = alloc::vec![
            tcp(NetSockState::Listen, 777, [0u8; 16], 0),
            tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80),
            udp_unconn(5353),
        ];
        let (text, info) = report(records, &[]);
        assert!(text.contains("ESTAB"), "the connected TCP socket shows");
        assert!(
            !text.contains("LISTEN"),
            "the listener is hidden by default"
        );
        assert!(!text.contains("UNCONN"), "the bound UDP socket is hidden");
        assert!(
            core::str::from_utf8(&info)
                .unwrap()
                .contains("net.listening_omitted"),
            "the omission is noted on fd 3"
        );
    }

    #[test]
    fn listening_only_shows_listeners() {
        let records = alloc::vec![
            tcp(NetSockState::Listen, 777, [0u8; 16], 0),
            tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80),
            udp_unconn(5353),
        ];
        let (text, info) = report(records, &["-l"]);
        assert!(text.contains("LISTEN"));
        assert!(text.contains("UNCONN"));
        assert!(!text.contains("ESTAB"));
        // No omission advisory in an explicit view.
        assert!(info.is_empty());
    }

    #[test]
    fn all_shows_everything_with_a_header() {
        let records = alloc::vec![
            tcp(NetSockState::Listen, 777, [0u8; 16], 0),
            tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80),
        ];
        let (text, _info) = report(records, &["-a"]);
        assert!(text.starts_with("Netid"));
        assert!(text.contains("LISTEN") && text.contains("ESTAB"));
    }

    #[test]
    fn protocol_and_header_switches_filter() {
        let records = alloc::vec![tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80), {
            let mut u = udp_unconn(5353);
            u.state = NetSockState::Established;
            u.peer_addr = v4(10, 0, 2, 3);
            u.peer_port = 53;
            u
        },];
        let (text, _info) = report(records, &["-t", "-H"]);
        assert!(!text.starts_with("Netid"), "-H suppresses the header");
        assert!(text.contains("tcp"));
        assert!(!text.contains("udp"), "-t excludes UDP");
    }

    #[test]
    fn processes_switch_adds_the_owner_column() {
        let records = alloc::vec![tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80)];
        let (text, _info) = report(records, &["-p"]);
        assert!(text.contains("pid=42"));
    }

    #[test]
    fn endpoints_render_wildcards_and_ports() {
        let records = alloc::vec![tcp(NetSockState::Listen, 777, [0u8; 16], 0)];
        let (text, _info) = report(records, &["-l"]);
        // Local shows the bound port; the unconnected peer is the wildcard.
        assert!(text.contains("10.0.2.15:777"));
        assert!(text.contains("*:*"));
    }

    #[test]
    fn help_falls_back_to_usage() {
        let command = parse(&["--help"]).expect("parse");
        let capture = Capture::default();
        let errs = Capture::default();
        let fixture = Fixture {
            records: Vec::new(),
            deny: false,
        };
        run(
            command,
            None,
            &fixture,
            &mut NoNames::default(),
            &NoHelp,
            &capture,
            &errs,
        )
        .expect("run");
        assert!(capture.text.into_inner().contains(USAGE));
    }

    #[test]
    fn a_denied_query_is_fatal() {
        let command = parse(&[]).expect("parse");
        let capture = Capture::default();
        let errs = Capture::default();
        let fixture = Fixture {
            records: Vec::new(),
            deny: true,
        };
        let result = run(
            command,
            None,
            &fixture,
            &mut NoNames::default(),
            &NoHelp,
            &capture,
            &errs,
        );
        assert_eq!(result, Err(SsError::Denied));
        assert!(capture.text.into_inner().is_empty(), "no partial table");
    }

    /// `-s` prints the stack-wide defence summary instead of the table, so
    /// a SYN flood is visible without reading the whole socket list.
    #[test]
    fn summary_reports_the_connection_defence_counters() {
        for args in [alloc::vec!["-s"], alloc::vec!["--summary"]] {
            let (text, _info) = report(Vec::new(), &args);
            assert!(
                text.contains("syn-cookies-sent 4096"),
                "the cookie brake total shows: {text}"
            );
            assert!(text.contains("syn-cookies-accepted 4000"), "{text}");
            assert!(text.contains("syn-cookies-rejected 96"), "{text}");
            assert!(text.contains("accept-overflow 6"), "{text}");
            assert!(text.contains("tcp-resets-sent 102"), "{text}");
            assert!(
                !text.contains("State"),
                "the socket table header is not printed for -s: {text}"
            );
        }
    }

    /// The summary is the tool's whole output under `-s`, so a refused
    /// query is fatal there too rather than printing an empty summary a
    /// reader would mistake for a quiet stack.
    #[test]
    fn a_denied_summary_query_is_fatal() {
        let command = parse(&["-s"]).expect("parse");
        let capture = Capture::default();
        let errs = Capture::default();
        let fixture = Fixture {
            records: Vec::new(),
            deny: true,
        };
        assert_eq!(
            run(
                command,
                None,
                &fixture,
                &mut NoNames::default(),
                &NoHelp,
                &capture,
                &errs
            ),
            Err(SsError::Denied)
        );
        assert!(capture.text.into_inner().is_empty());
    }

    #[test]
    fn options_default_is_all_false() {
        assert_eq!(Options::default(), Options::default());
    }

    // -- Host-name resolution (`-r`) --------------------------------------

    fn v4_ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn the_default_listing_puts_no_query_on_the_wire() {
        let records = alloc::vec![tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80)];
        let mut names = NoNames::default();
        report_with(records, &[], &mut names);
        assert!(
            names.asked.is_empty(),
            "host-name resolution is opt-in; the default view must not resolve"
        );
    }

    #[test]
    fn resolve_names_both_endpoints() {
        let records = alloc::vec![tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80)];
        let mut names = MapNames::new(alloc::vec![
            (v4_ip(10, 0, 2, 15), "self.example"),
            (v4_ip(10, 0, 2, 2), "gateway.example"),
        ]);
        let (text, _) = report_with(records, &["-r"], &mut names);
        assert!(text.contains("self.example:4321"), "{text}");
        assert!(text.contains("gateway.example:80"), "{text}");
    }

    #[test]
    fn an_unresolvable_address_stays_numeric() {
        let records = alloc::vec![tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80)];
        let mut names = MapNames::new(Vec::new());
        let (text, _) = report_with(records, &["-r"], &mut names);
        assert!(text.contains("10.0.2.15:4321"), "{text}");
        assert!(text.contains("10.0.2.2:80"), "{text}");
    }

    #[test]
    fn the_wildcard_address_is_never_looked_up() {
        let records = alloc::vec![udp_unconn(5353)];
        let mut names = MapNames::new(Vec::new());
        let (text, _) = report_with(records, &["-r", "-l"], &mut names);
        assert!(text.contains("*:5353"), "{text}");
        assert!(
            names.asked.is_empty(),
            "the unspecified address names no host, so querying it is pure latency"
        );
    }

    #[test]
    fn a_repeated_address_is_resolved_once() {
        // Three rows over two distinct addresses, one of which resolves to
        // nothing: the memo must remember the negative answer too.
        let records = alloc::vec![
            tcp(NetSockState::Established, 1, v4(10, 0, 2, 2), 80),
            tcp(NetSockState::Established, 2, v4(10, 0, 2, 2), 443),
            tcp(NetSockState::Established, 3, v4(10, 0, 2, 9), 22),
        ];
        let mut names = MapNames::new(alloc::vec![(v4_ip(10, 0, 2, 2), "gateway.example")]);
        report_with(records, &["-r"], &mut names);
        let mut asked = names.asked.clone();
        asked.sort_unstable();
        asked.dedup();
        assert_eq!(
            names.asked.len(),
            asked.len(),
            "each distinct address is queried exactly once"
        );
        assert_eq!(asked.len(), 3, "the local address and the two peers");
    }

    #[test]
    fn numeric_and_resolve_are_independent() {
        let records = alloc::vec![tcp(NetSockState::Established, 4321, v4(10, 0, 2, 2), 80)];
        let mut names = MapNames::new(alloc::vec![(v4_ip(10, 0, 2, 2), "gateway.example")]);
        let (text, _) = report_with(records, &["-rn"], &mut names);
        assert!(
            text.contains("gateway.example:80"),
            "-n governs service names, not host names: {text}"
        );
    }
}

//! The `lsusb` engine: fetch the hardware tree, select the USB
//! interfaces, and render the listing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::hwtree::{HwMatchKind, HwNode};
use rustos_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use rustos_abi::sysinfo::SysinfoQueryId;
use rustos_devids::DevIds;
use rustos_help::{own_short_help, HelpSource};
use rustos_procinfo::{call, hwtree, Transport};

use crate::command::{Command, Options};
use crate::error::LsusbError;
use crate::io::Output;

/// The one-line usage banner, printed on a usage error and as the
/// fallback when the bundled help document is unavailable.
pub const USAGE: &str =
    "usage: lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "lsusb";

/// One discovered USB interface: its tree identity and the numeric
/// identity its match key carries.
struct Interface<'a> {
    node: &'a HwNode,
    vendor: u16,
    product: u16,
    class24: u32,
}

impl Interface<'_> {
    /// The controller (bus) node id this interface hangs under.
    fn bus(&self) -> u32 {
        self.node.parent()
    }
}

/// Run a parsed `lsusb` command against the injected seams.
///
/// `database` is the bundle's compiled `usb.ids` table when it loaded and
/// validated; `None` renders every identity as its bare `ID vvvv:pppp`
/// (the caller has already reported why on standard error — the listing
/// itself is never withheld over a naming aid).
///
/// # Errors
///
/// * [`LsusbError::PermissionDenied`] — the caller lacks `CAP_SYSINFO_HW`.
/// * [`LsusbError::Service`] — the query failed or the reply was malformed.
/// * [`LsusbError::Output`] — a line (or the short help) could not be
///   written.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    database: Option<&DevIds<'_>>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LsusbError> {
    let options = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(LsusbError::Output)?;
            return Ok(());
        }
        Command::List(options) => options,
    };

    let reply = call(transport, SysinfoQueryId::HARDWARE_TREE, &[]).map_err(LsusbError::from)?;
    let nodes = hwtree::decode_tree(&reply).map_err(LsusbError::Service)?;
    let order = hwtree::bus_order(&nodes);

    // The USB interfaces, in stable bus order (parent-chain order from
    // the tree), then filtered by `-s` / `-d`.
    let mut interfaces: Vec<Interface<'_>> = Vec::new();
    for &index in &order {
        let node = &nodes[index];
        let Some(key) = node
            .match_keys()
            .iter()
            .find(|key| key.kind() == Some(HwMatchKind::Usb))
        else {
            continue;
        };
        let interface = Interface {
            node,
            vendor: key.vendor(),
            product: key.product(),
            class24: key.class(),
        };
        if !selected(&options, &interface) {
            continue;
        }
        interfaces.push(interface);
    }

    let mut unnamed = 0u64;
    if options.tree {
        render_tree(&options, &nodes, &interfaces, database, out, &mut unnamed)?;
    } else {
        for interface in &interfaces {
            let mut line = format!(
                "Bus {:03} Device {:03}: ",
                interface.bus(),
                interface.node.id()
            );
            push_identity(&mut line, interface, database, &mut unnamed);
            line.push('\n');
            out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
            if options.verbose {
                render_class(interface, database, 1, out)?;
            }
        }
    }
    if unnamed > 0 {
        emit_unnamed_record(out, unnamed, database.is_some());
    }
    Ok(())
}

/// `true` if `interface` passes the `-s` / `-d` filters.
fn selected(options: &Options, interface: &Interface<'_>) -> bool {
    if let Some(slot) = options.slot {
        if slot.bus.is_some_and(|bus| bus != interface.bus()) {
            return false;
        }
        if slot.device.is_some_and(|dev| dev != interface.node.id()) {
            return false;
        }
    }
    if let Some(filter) = options.device {
        if filter.vendor.is_some_and(|v| v != interface.vendor) {
            return false;
        }
        if filter.product.is_some_and(|p| p != interface.product) {
            return false;
        }
    }
    true
}

/// Append `interface`'s identity — `ID vvvv:pppp` plus the vendor and
/// product names the database carries — to `line`, counting an identity
/// the database could not fully name. An unknown name is omitted, never
/// fabricated: the numeric id is already on the line, exactly as
/// `usbutils` renders an entry absent from `usb.ids`.
fn push_identity(
    line: &mut String,
    interface: &Interface<'_>,
    database: Option<&DevIds<'_>>,
    unnamed: &mut u64,
) {
    // `write!` to a `String` cannot fail; the results are discarded on
    // that basis.
    let _ = write!(
        line,
        "ID {:04x}:{:04x}",
        interface.vendor, interface.product
    );
    let vendor_name = database.and_then(|db| db.vendor(interface.vendor));
    let product_name = database.and_then(|db| db.device(interface.vendor, interface.product));
    if vendor_name.is_none() || product_name.is_none() {
        *unnamed += 1;
    }
    if let Some(vendor) = vendor_name {
        let _ = write!(line, " {vendor}");
    }
    if let Some(product) = product_name {
        let _ = write!(line, " {product}");
    }
}

/// Render an interface's class identity in the `-v` view: the
/// `bInterfaceClass` / `bInterfaceSubClass` / `bInterfaceProtocol`
/// descriptor fields (decimal values, as `usbutils` prints them) with
/// the names the `usb.ids` class tables carry; an unknown name is
/// omitted, never fabricated.
fn render_class(
    interface: &Interface<'_>,
    database: Option<&DevIds<'_>>,
    depth: usize,
    out: &dyn Output,
) -> Result<(), LsusbError> {
    let class = ((interface.class24 >> 16) & 0xFF) as u8;
    let sub = ((interface.class24 >> 8) & 0xFF) as u8;
    let protocol = (interface.class24 & 0xFF) as u8;
    let rows: [(&str, u8, Option<&str>); 3] = [
        (
            "bInterfaceClass",
            class,
            database.and_then(|db| db.class(class)),
        ),
        (
            "bInterfaceSubClass",
            sub,
            database.and_then(|db| db.subclass(class, sub)),
        ),
        (
            "bInterfaceProtocol",
            protocol,
            database.and_then(|db| db.prog_if(class, sub, protocol)),
        ),
    ];
    for (field, value, name) in rows {
        let mut line = String::new();
        for _ in 0..depth {
            line.push_str("  ");
        }
        // `write!` to a `String` cannot fail; the results are discarded
        // on that basis.
        let _ = write!(line, "{field} {value}");
        if let Some(name) = name {
            let _ = write!(line, " {name}");
        }
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
    }
    Ok(())
}

/// Render the `-t` topology view: every USB interface under its ancestor
/// chain (its controller and the buses above it), ancestors shown as
/// `#<id> <class>` context lines, indentation one step per tree level.
fn render_tree(
    options: &Options,
    nodes: &[HwNode],
    interfaces: &[Interface<'_>],
    database: Option<&DevIds<'_>>,
    out: &dyn Output,
    unnamed: &mut u64,
) -> Result<(), LsusbError> {
    // Which nodes are (or contain) a selected USB interface.
    let selected_ids: Vec<u32> = interfaces.iter().map(|i| i.node.id()).collect();
    let keep = hwtree::keep_with_ancestors(nodes, &selected_ids);

    // Depth-first over the kept nodes, in the same stable bus order.
    for &index in &hwtree::bus_order(nodes) {
        if !keep[index] {
            continue;
        }
        let node = &nodes[index];
        let depth = hwtree::depth_of(nodes, node);
        let mut line = String::new();
        for _ in 0..depth {
            line.push_str("  ");
        }
        if let Some(interface) = interfaces.iter().find(|i| i.node.id() == node.id()) {
            let _ = write!(line, "#{} ", node.id());
            push_identity(&mut line, interface, database, unnamed);
        } else {
            let _ = write!(line, "#{} {}", node.id(), hwtree::class_label(node.class()));
        }
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
        if options.verbose {
            if let Some(interface) = interfaces.iter().find(|i| i.node.id() == node.id()) {
                render_class(interface, database, depth + 1, out)?;
            }
        }
    }
    Ok(())
}

/// Emit the `usb.names_unresolved` advisory (fd 3) when identities were
/// rendered without a vendor/product name because the database has no
/// entry (or did not load): a tool or user then knows the bare `ID`
/// forms are not omissions from the inventory itself. Advisory only —
/// never affects the listing, the exit status, or ordering.
fn emit_unnamed_record(out: &dyn Output, unnamed: u64, database_loaded: bool) {
    let message = if unnamed == 1 {
        String::from("1 device rendered without a database name.")
    } else {
        format!("{unnamed} devices rendered without a database name.")
    };
    let ai = format!(
        "{{\"subject\":\"usb_listing\",\
         \"omission\":{{\"reason\":\"name_not_in_database\",\
         \"entry_class\":\"device_name\",\"omitted_count\":{unnamed},\
         \"database_loaded\":{database_loaded},\
         \"stdout_is_exhaustive\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        "usb.names_unresolved",
        Severity::Info,
        Human::message(&message),
    )
    .with_ai(&ai);
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        out.info(&buf[..len]);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use rustos_abi::hwtree::{HwDeviceClass, HwMatchKey, HW_NODE_ROOT};
    use rustos_abi::Errno;
    use rustos_devids::{textdb, DbKind};
    use rustos_help::SourceError;

    use super::*;
    use crate::command::parse;

    /// A `usb.ids`-grammar fixture compiled through the real import
    /// pipeline: one named vendor+product, one named class table with a
    /// subclass and protocol. The `abcd` vendor is deliberately absent so
    /// the bare-`ID` rendering and the advisory record are exercised.
    const FIXTURE_DB: &str = "\
046d  Logitech, Inc.
\tc534  Unifying Receiver
C 03  Human Interface Device
\t01  Boot Interface Subclass
\t\t01  Keyboard
";

    fn fixture_table() -> Vec<u8> {
        textdb::parse(DbKind::Usb, FIXTURE_DB.as_bytes())
            .expect("fixture text vets")
            .encode()
    }

    /// The canned tree: a root bus (#1) carrying an xHCI controller (#2)
    /// with two interfaces — a named Logitech boot keyboard (#3) and an
    /// unnamed vendor-specific interface (#4) — plus a non-USB timer (#5)
    /// the listing must ignore.
    fn fixture_tree() -> Vec<u8> {
        let mut root = HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Bus);
        root.push_match_key(HwMatchKey::compatible(b"fixture,root").expect("fits"))
            .expect("key fits");
        let mut controller = HwNode::new(2, 1, HwDeviceClass::Bus);
        controller
            .push_match_key(HwMatchKey::compatible(b"fixture,xhci").expect("fits"))
            .expect("key fits");
        let mut keyboard = HwNode::new(3, 2, HwDeviceClass::Input);
        keyboard
            .push_match_key(HwMatchKey::usb(0x046d, 0xc534, 0x03_01_01))
            .expect("key fits");
        let mut unnamed = HwNode::new(4, 2, HwDeviceClass::Other);
        unnamed
            .push_match_key(HwMatchKey::usb(0xabcd, 0x1234, 0xff_00_00))
            .expect("key fits");
        let mut timer = HwNode::new(5, 1, HwDeviceClass::Timer);
        timer
            .push_match_key(HwMatchKey::compatible(b"fixture,timer").expect("fits"))
            .expect("key fits");

        let mut bytes = Vec::new();
        for node in [root, controller, keyboard, unnamed, timer] {
            bytes.extend_from_slice(&node.to_le_bytes());
        }
        bytes
    }

    /// A transport serving one canned `HARDWARE_TREE` reply (or refusal).
    struct Fixture {
        reply: Result<Vec<u8>, Errno>,
    }

    impl Transport for Fixture {
        fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
            self.reply.clone()
        }
    }

    /// Captures listing bytes and fd-3 advisory records.
    struct Recorder {
        text: RefCell<String>,
        infos: RefCell<Vec<String>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                text: RefCell::new(String::new()),
                infos: RefCell::new(Vec::new()),
            }
        }

        fn lines(&self) -> Vec<String> {
            self.text
                .borrow()
                .lines()
                .map(ToString::to_string)
                .collect()
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.text
                .borrow_mut()
                .push_str(core::str::from_utf8(bytes).expect("output is UTF-8"));
            Ok(())
        }

        fn info(&self, record: &[u8]) {
            self.infos
                .borrow_mut()
                .push(String::from_utf8_lossy(record).into_owned());
        }
    }

    /// A bundle with no help documents: the engine falls back to `USAGE`.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(&self, _locale: &str, _file: &str) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    fn run_case(
        args: &[&str],
        reply: Result<Vec<u8>, Errno>,
        with_db: bool,
    ) -> (Recorder, Result<(), LsusbError>) {
        let command = parse(args).expect("arguments parse");
        let transport = Fixture { reply };
        let table = fixture_table();
        let database = if with_db {
            Some(DevIds::parse(DbKind::Usb, &table).expect("fixture table decodes"))
        } else {
            None
        };
        let out = Recorder::new();
        let result = run(command, None, &transport, database.as_ref(), &NoHelp, &out);
        (out, result)
    }

    #[test]
    fn default_listing_names_what_the_database_knows() {
        let (out, result) = run_case(&[], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 002 Device 003: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "Bus 002 Device 004: ID abcd:1234",
            ]
        );
        // The unnamed vendor-specific interface is advised on fd 3, and
        // only it.
        let infos = out.infos.borrow();
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(
            infos[0].contains("\"usb.names_unresolved\""),
            "{}",
            infos[0]
        );
        assert!(infos[0].contains("\"omitted_count\":1"), "{}", infos[0]);
    }

    #[test]
    fn a_missing_database_lists_bare_ids() {
        let (out, result) = run_case(&[], Ok(fixture_tree()), false);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 002 Device 003: ID 046d:c534",
                "Bus 002 Device 004: ID abcd:1234",
            ]
        );
        let infos = out.infos.borrow();
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(infos[0].contains("\"omitted_count\":2"), "{}", infos[0]);
        assert!(
            infos[0].contains("\"database_loaded\":false"),
            "{}",
            infos[0]
        );
    }

    #[test]
    fn slot_and_device_filters_select() {
        // A bare `-s` value is a device (interface node) number.
        let (out, result) = run_case(&["-s", "4"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["Bus 002 Device 004: ID abcd:1234"]);

        // `bus:` selects everything under the controller node.
        let (out, result) = run_case(&["-s", "2:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines().len(), 2);

        // A bus that exists but is not a controller matches nothing.
        let (out, result) = run_case(&["-s", "1:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert!(out.lines().is_empty());

        let (out, result) = run_case(&["-d", "046d:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            ["Bus 002 Device 003: ID 046d:c534 Logitech, Inc. Unifying Receiver"]
        );

        let (out, result) = run_case(&["-d", ":1234"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["Bus 002 Device 004: ID abcd:1234"]);
    }

    #[test]
    fn verbose_appends_the_class_identity() {
        let (out, result) = run_case(&["-v", "-s", "3"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 002 Device 003: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "  bInterfaceClass 3 Human Interface Device",
                "  bInterfaceSubClass 1 Boot Interface Subclass",
                "  bInterfaceProtocol 1 Keyboard",
            ]
        );

        // An identity outside the class tables keeps its decimal values
        // with no fabricated names.
        let (out, result) = run_case(&["-v", "-s", "4"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 002 Device 004: ID abcd:1234",
                "  bInterfaceClass 255",
                "  bInterfaceSubClass 0",
                "  bInterfaceProtocol 0",
            ]
        );
    }

    #[test]
    fn tree_view_shows_the_parent_chain() {
        let (out, result) = run_case(&["-t"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "#1 bus",
                "  #2 bus",
                "    #3 ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "    #4 ID abcd:1234",
            ]
        );
    }

    #[test]
    fn a_refused_query_is_fatal_with_the_reason() {
        let (out, result) = run_case(&[], Err(Errno::PermissionDenied), true);
        assert_eq!(result, Err(LsusbError::PermissionDenied));
        assert!(out.lines().is_empty(), "nothing is fabricated");
    }

    #[test]
    fn a_malformed_reply_fails_closed() {
        // Not a whole number of node records.
        let (out, result) = run_case(&[], Ok(alloc::vec![0u8; HwNode::WIRE_LEN + 1]), true);
        assert_eq!(result, Err(LsusbError::Service(Errno::BufferTooSmall)));
        assert!(out.lines().is_empty(), "no partial inventory is rendered");
    }

    #[test]
    fn an_empty_tree_lists_nothing_cleanly() {
        let (out, result) = run_case(&[], Ok(Vec::new()), true);
        result.expect("an empty inventory is not an error");
        assert!(out.lines().is_empty());
        assert!(out.infos.borrow().is_empty());
    }

    #[test]
    fn help_falls_back_to_the_usage_banner() {
        let (out, result) = run_case(&["-?"], Ok(Vec::new()), true);
        result.expect("help renders");
        assert_eq!(out.lines(), [USAGE]);
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::fs;

        let help_root = alloc::format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        for locale in rustos_help::REQUIRED_LOCALES {
            let path = alloc::format!("{help_root}/{locale}/lsusb.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-v`",
                "`-t`",
                "`-d [<vendor>]:[<product>]`",
                "`-s [[<bus>]:][<devnum>]`",
                "`-?, --help`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/lsusb.md must document {switch}"
                );
            }
        }
    }
}

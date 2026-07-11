//! The `lsusb` engine: fetch the hardware tree, select the USB
//! interfaces, and render the listing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::hwtree::{HwMatchKind, HwNode};
use rustos_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use rustos_devids::DevIds;
use rustos_help::{own_short_help, HelpSource};
use rustos_procinfo::{hwtree, Transport};

use crate::command::{Command, Options};
use crate::error::LsusbError;
use crate::io::Output;

/// The one-line usage banner, printed on a usage error and as the
/// fallback when the bundled help document is unavailable.
pub const USAGE: &str =
    "usage: lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "lsusb";

/// One physical USB device: the interfaces grouped under it, its
/// controller, and the rendered bus/device numbers.
///
/// Interface nodes are the inventory's bind unit; a physical device is
/// their truthful grouping — the interfaces under one controller that
/// carry the same non-zero [`HwNode::address`] (the device address the
/// host controller reported). A node whose emitter reported no address
/// (`0`) cannot be grouped and stands as its own device. Bus and device
/// numbers are per-snapshot ordinals starting at 1, in stable bus order
/// — `usbutils`-shaped small numbers, not kernel node ids.
struct Device {
    /// Hardware-tree node id of the controller this device hangs under.
    controller: u32,
    /// The device address its interface nodes carry (`0` = unreported).
    address: u32,
    /// Rendered bus number (1-based ordinal of the controller).
    bus: u32,
    /// Rendered device number (1-based ordinal within the bus).
    number: u32,
    /// The device's vendor id (shared by all its interfaces).
    vendor: u16,
    /// The device's product id (shared by all its interfaces).
    product: u16,
    /// Each interface's 24-bit class triple
    /// `(bInterfaceClass << 16) | (bInterfaceSubClass << 8) | bInterfaceProtocol`,
    /// in stable bus order.
    interfaces: Vec<u32>,
}

/// Group the tree's USB interface nodes into physical devices, in stable
/// bus order, assigning the 1-based bus and per-bus device ordinals the
/// listing renders.
fn group_devices(nodes: &[HwNode], order: &[usize]) -> Vec<Device> {
    let mut devices: Vec<Device> = Vec::new();
    // Controller node ids in first-appearance order; a controller's bus
    // number is its position + 1.
    let mut controllers: Vec<u32> = Vec::new();
    for &index in order {
        let node = &nodes[index];
        let Some(key) = node
            .match_keys()
            .iter()
            .find(|key| key.kind() == Some(HwMatchKind::Usb))
        else {
            continue;
        };
        let controller = node.parent();
        if node.address() != 0 {
            if let Some(device) = devices
                .iter_mut()
                .find(|d| d.controller == controller && d.address == node.address())
            {
                device.interfaces.push(key.class());
                continue;
            }
        }
        let bus = if let Some(position) = controllers.iter().position(|&c| c == controller) {
            ordinal(position)
        } else {
            controllers.push(controller);
            ordinal(controllers.len() - 1)
        };
        let number = ordinal(devices.iter().filter(|d| d.bus == bus).count());
        devices.push(Device {
            controller,
            address: node.address(),
            bus,
            number,
            vendor: key.vendor(),
            product: key.product(),
            interfaces: alloc::vec![key.class()],
        });
    }
    devices
}

/// The 1-based ordinal for zero-based `position`. The tree's node count
/// bounds every ordinal, so the conversion cannot truncate in practice;
/// it saturates rather than wrapping if it ever would.
fn ordinal(position: usize) -> u32 {
    u32::try_from(position).map_or(u32::MAX, |p| p.saturating_add(1))
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

    let nodes = hwtree::fetch_tree(transport).map_err(LsusbError::from)?;
    let order = hwtree::bus_order(&nodes);
    let devices = group_devices(&nodes, &order);

    let mut unnamed = 0u64;
    if options.tree {
        render_tree(&options, &nodes, &devices, database, out, &mut unnamed)?;
    } else {
        for device in devices.iter().filter(|d| selected(&options, d)) {
            let mut line = format!("Bus {:03} Device {:03}: ", device.bus, device.number);
            push_identity(&mut line, device, database, &mut unnamed);
            line.push('\n');
            out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
            if options.verbose {
                for &class24 in &device.interfaces {
                    render_class(class24, database, 1, out)?;
                }
            }
        }
    }
    if unnamed > 0 {
        emit_unnamed_record(out, unnamed, database.is_some());
    }
    Ok(())
}

/// `true` if `device` passes the `-s` / `-d` filters. `-s` names the
/// rendered bus/device numbers; `-d` the vendor/product ids.
fn selected(options: &Options, device: &Device) -> bool {
    if let Some(slot) = options.slot {
        if slot.bus.is_some_and(|bus| bus != device.bus) {
            return false;
        }
        if slot.device.is_some_and(|dev| dev != device.number) {
            return false;
        }
    }
    if let Some(filter) = options.device {
        if filter.vendor.is_some_and(|v| v != device.vendor) {
            return false;
        }
        if filter.product.is_some_and(|p| p != device.product) {
            return false;
        }
    }
    true
}

/// Append `device`'s identity — `ID vvvv:pppp` plus the vendor and
/// product names the database carries — to `line`, counting an identity
/// the database could not fully name. An unknown name is omitted, never
/// fabricated: the numeric id is already on the line, exactly as
/// `usbutils` renders an entry absent from `usb.ids`.
fn push_identity(
    line: &mut String,
    device: &Device,
    database: Option<&DevIds<'_>>,
    unnamed: &mut u64,
) {
    // `write!` to a `String` cannot fail; the results are discarded on
    // that basis.
    let _ = write!(line, "ID {:04x}:{:04x}", device.vendor, device.product);
    let vendor_name = database.and_then(|db| db.vendor(device.vendor));
    let product_name = database.and_then(|db| db.device(device.vendor, device.product));
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

/// Render one interface's class identity in the `-v` view: the
/// `bInterfaceClass` / `bInterfaceSubClass` / `bInterfaceProtocol`
/// descriptor fields (decimal values, as `usbutils` prints them) with
/// the names the `usb.ids` class tables carry; an unknown name is
/// omitted, never fabricated. A composite device renders one triple per
/// interface, in bus order.
fn render_class(
    class24: u32,
    database: Option<&DevIds<'_>>,
    depth: usize,
    out: &dyn Output,
) -> Result<(), LsusbError> {
    let class = ((class24 >> 16) & 0xFF) as u8;
    let sub = ((class24 >> 8) & 0xFF) as u8;
    let protocol = (class24 & 0xFF) as u8;
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

/// Render the `-t` topology view: each bus (controller), the devices on
/// it, and each device's interfaces — the USB topology only, never the
/// non-USB ancestors above the controller.
///
/// The bus line names the controller by its first `compatible` match key
/// (the model identity discovery recorded) or, absent one, its class
/// label. An interface line is its class triple `Class=cc:ss:pp` (hex
/// bytes, `usbutils`' `Class=` shape) with the `usb.ids` class name when
/// the database carries it.
fn render_tree(
    options: &Options,
    nodes: &[HwNode],
    devices: &[Device],
    database: Option<&DevIds<'_>>,
    out: &dyn Output,
    unnamed: &mut u64,
) -> Result<(), LsusbError> {
    let mut last_bus = 0u32;
    for device in devices.iter().filter(|d| selected(options, d)) {
        if device.bus != last_bus {
            last_bus = device.bus;
            let label = controller_label(nodes, device.controller);
            let line = format!("Bus {:03}: {label}\n", device.bus);
            out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
        }
        let mut line = format!("  Device {:03}: ", device.number);
        push_identity(&mut line, device, database, unnamed);
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
        for &class24 in &device.interfaces {
            let class = ((class24 >> 16) & 0xFF) as u8;
            let sub = ((class24 >> 8) & 0xFF) as u8;
            let protocol = (class24 & 0xFF) as u8;
            let mut line = format!("    Class={class:02x}:{sub:02x}:{protocol:02x}");
            if let Some(name) = database.and_then(|db| db.class(class)) {
                // `write!` to a `String` cannot fail; the result is
                // discarded on that basis.
                let _ = write!(line, " {name}");
            }
            line.push('\n');
            out.write_all(line.as_bytes()).map_err(LsusbError::Output)?;
        }
    }
    Ok(())
}

/// The `-t` bus line's controller label: the controller node's first
/// `compatible` match key, or its class label when it carries none (or
/// the node is absent from the snapshot).
fn controller_label(nodes: &[HwNode], controller: u32) -> String {
    let node = nodes.iter().find(|node| node.id() == controller);
    if let Some(node) = node {
        if let Some(key) = node
            .match_keys()
            .iter()
            .find(|key| key.kind() == Some(HwMatchKind::Compatible))
        {
            if let Ok(compatible) = core::str::from_utf8(key.compatible_bytes()) {
                return String::from(compatible);
            }
        }
    }
    String::from(hwtree::class_label(node.and_then(HwNode::class)))
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

    use rustos_abi::hwtree::{HwDeviceClass, HwMatchKey, HwTreeHeader, HW_NODE_ROOT};
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
    /// with three interface nodes — a named Logitech composite receiver's
    /// boot-keyboard (#3) and boot-mouse (#4) interfaces sharing device
    /// address 1, and an unnamed vendor-specific single-interface device
    /// (#5) at address 2 — plus a non-USB timer (#6) the listing must
    /// ignore.
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
        keyboard.set_address(1);
        let mut mouse = HwNode::new(4, 2, HwDeviceClass::Input);
        mouse
            .push_match_key(HwMatchKey::usb(0x046d, 0xc534, 0x03_01_02))
            .expect("key fits");
        mouse.set_address(1);
        let mut unnamed = HwNode::new(5, 2, HwDeviceClass::Other);
        unnamed
            .push_match_key(HwMatchKey::usb(0xabcd, 0x1234, 0xff_00_00))
            .expect("key fits");
        unnamed.set_address(2);
        let mut timer = HwNode::new(6, 1, HwDeviceClass::Timer);
        timer
            .push_match_key(HwMatchKey::compatible(b"fixture,timer").expect("fits"))
            .expect("key fits");

        let nodes = [root, controller, keyboard, mouse, unnamed, timer];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&HwTreeHeader::new(1, nodes.len() as u64).to_le_bytes());
        for node in nodes {
            bytes.extend_from_slice(&node.to_le_bytes());
        }
        bytes
    }

    /// A transport serving one canned `HARDWARE_TREE` reply (or refusal).
    /// The fixture tree fits one page, so the paged fetch issues exactly
    /// one request.
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
        // The composite receiver's two interface nodes share device
        // address 1, so it lists once — one line per physical device,
        // never per interface — and bus/device numbers are small 1-based
        // ordinals, never hardware-tree node ids.
        let (out, result) = run_case(&[], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 001 Device 001: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "Bus 001 Device 002: ID abcd:1234",
            ]
        );
        // The unnamed vendor-specific device is advised on fd 3, and
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
                "Bus 001 Device 001: ID 046d:c534",
                "Bus 001 Device 002: ID abcd:1234",
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
        // A bare `-s` value is a rendered device number.
        let (out, result) = run_case(&["-s", "2"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["Bus 001 Device 002: ID abcd:1234"]);

        // `bus:` selects every device on the rendered bus.
        let (out, result) = run_case(&["-s", "1:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines().len(), 2);

        // A bus number no controller renders as matches nothing.
        let (out, result) = run_case(&["-s", "2:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert!(out.lines().is_empty());

        let (out, result) = run_case(&["-d", "046d:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            ["Bus 001 Device 001: ID 046d:c534 Logitech, Inc. Unifying Receiver"]
        );

        let (out, result) = run_case(&["-d", ":1234"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["Bus 001 Device 002: ID abcd:1234"]);
    }

    #[test]
    fn verbose_appends_the_class_identity() {
        // The composite receiver renders one class triple per interface
        // under its single device line (the boot-mouse protocol `2` has
        // no name in the fixture tables and none is fabricated).
        let (out, result) = run_case(&["-v", "-s", "1"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 001 Device 001: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "  bInterfaceClass 3 Human Interface Device",
                "  bInterfaceSubClass 1 Boot Interface Subclass",
                "  bInterfaceProtocol 1 Keyboard",
                "  bInterfaceClass 3 Human Interface Device",
                "  bInterfaceSubClass 1 Boot Interface Subclass",
                "  bInterfaceProtocol 2",
            ]
        );

        // An identity outside the class tables keeps its decimal values
        // with no fabricated names.
        let (out, result) = run_case(&["-v", "-s", "2"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 001 Device 002: ID abcd:1234",
                "  bInterfaceClass 255",
                "  bInterfaceSubClass 0",
                "  bInterfaceProtocol 0",
            ]
        );
    }

    #[test]
    fn tree_view_shows_buses_devices_and_interfaces() {
        let (out, result) = run_case(&["-t"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 001: fixture,xhci",
                "  Device 001: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "    Class=03:01:01 Human Interface Device",
                "    Class=03:01:02 Human Interface Device",
                "  Device 002: ID abcd:1234",
                "    Class=ff:00:00",
            ]
        );
    }

    #[test]
    fn identical_devices_group_by_address_and_an_unreported_address_never_groups() {
        // Two identical keyboards (same vid:pid, same controller) at
        // distinct addresses stay two devices; two address-less nodes —
        // an emitter that reported no device address — are never guessed
        // into one device, even with identical identities.
        let mut root = HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Bus);
        root.push_match_key(HwMatchKey::compatible(b"fixture,root").expect("fits"))
            .expect("key fits");
        let mut controller = HwNode::new(2, 1, HwDeviceClass::Bus);
        controller
            .push_match_key(HwMatchKey::compatible(b"fixture,xhci").expect("fits"))
            .expect("key fits");
        let mut twin_a = HwNode::new(3, 2, HwDeviceClass::Input);
        twin_a
            .push_match_key(HwMatchKey::usb(0x046d, 0xc534, 0x03_01_01))
            .expect("key fits");
        twin_a.set_address(1);
        let mut twin_b = HwNode::new(4, 2, HwDeviceClass::Input);
        twin_b
            .push_match_key(HwMatchKey::usb(0x046d, 0xc534, 0x03_01_01))
            .expect("key fits");
        twin_b.set_address(2);
        let mut bare_a = HwNode::new(5, 2, HwDeviceClass::Other);
        bare_a
            .push_match_key(HwMatchKey::usb(0xabcd, 0x1234, 0xff_00_00))
            .expect("key fits");
        let mut bare_b = HwNode::new(6, 2, HwDeviceClass::Other);
        bare_b
            .push_match_key(HwMatchKey::usb(0xabcd, 0x1234, 0xff_00_00))
            .expect("key fits");

        let nodes = [root, controller, twin_a, twin_b, bare_a, bare_b];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&HwTreeHeader::new(1, nodes.len() as u64).to_le_bytes());
        for node in nodes {
            bytes.extend_from_slice(&node.to_le_bytes());
        }

        let (out, result) = run_case(&[], Ok(bytes), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "Bus 001 Device 001: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "Bus 001 Device 002: ID 046d:c534 Logitech, Inc. Unifying Receiver",
                "Bus 001 Device 003: ID abcd:1234",
                "Bus 001 Device 004: ID abcd:1234",
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
        // Not a whole number of node records after the snapshot header.
        let (out, result) = run_case(&[], Ok(alloc::vec![0u8; HwNode::WIRE_LEN + 1]), true);
        assert_eq!(result, Err(LsusbError::Service(Errno::BadMagic)));
        assert!(out.lines().is_empty(), "no partial inventory is rendered");
    }

    #[test]
    fn an_empty_tree_lists_nothing_cleanly() {
        let empty = HwTreeHeader::new(1, 0).to_le_bytes().to_vec();
        let (out, result) = run_case(&[], Ok(empty), true);
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

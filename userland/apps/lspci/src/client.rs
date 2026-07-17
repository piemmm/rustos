//! The `lspci` engine: fetch the hardware tree, select the PCI functions,
//! and render the listing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_abi::hwtree::{HwMatchKind, HwNode, HwResource, HwResourceKind};
use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_devids::DevIds;
use tairix_help::{own_short_help, HelpSource};
use tairix_procinfo::{hwtree, Transport};

use crate::command::{Command, NameMode, Options};
use crate::error::LspciError;
use crate::io::Output;

/// The one-line usage banner, printed on a usage error and as the
/// fallback when the bundled help document is unavailable.
pub const USAGE: &str = "usage: lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "lspci";

/// One discovered PCI function: its tree identity and the numeric
/// identity its match key carries.
struct Function<'a> {
    node: &'a HwNode,
    vendor: u16,
    device: u16,
    class24: u32,
}

/// Run a parsed `lspci` command against the injected seams.
///
/// `database` is the bundle's compiled `pci.ids` table when it loaded and
/// validated; `None` renders every identity numerically (the caller has
/// already reported why on standard error — the listing itself is never
/// withheld over a naming aid).
///
/// # Errors
///
/// * [`LspciError::PermissionDenied`] — the caller lacks `CAP_SYSINFO_HW`.
/// * [`LspciError::Service`] — the query failed or the reply was malformed.
/// * [`LspciError::Output`] — a line (or the short help) could not be
///   written.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    database: Option<&DevIds<'_>>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LspciError> {
    let options = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(LspciError::Output)?;
            return Ok(());
        }
        Command::List(options) => options,
    };

    let nodes = hwtree::fetch_tree(transport).map_err(LspciError::from)?;
    let order = hwtree::bus_order(&nodes);

    // The PCI functions, in stable bus order (parent-chain order from the
    // tree), then filtered by `-s` / `-d`.
    let mut functions: Vec<Function<'_>> = Vec::new();
    for &index in &order {
        let node = &nodes[index];
        let Some(key) = node
            .match_keys()
            .iter()
            .find(|key| key.kind() == Some(HwMatchKind::Pci))
        else {
            continue;
        };
        let function = Function {
            node,
            vendor: key.vendor(),
            device: key.product(),
            class24: key.class(),
        };
        if !selected(&options, &function) {
            continue;
        }
        functions.push(function);
    }

    let mut unnamed = 0u64;
    if options.tree {
        render_tree(&options, &nodes, &functions, database, out, &mut unnamed)?;
    } else {
        for function in &functions {
            let mut line = format!("#{} ", function.node.id());
            push_identity(&mut line, &options, function, database, &mut unnamed);
            line.push('\n');
            out.write_all(line.as_bytes()).map_err(LspciError::Output)?;
            if options.verbose {
                render_resources(function.node.resources(), 1, out)?;
            }
        }
    }
    if unnamed > 0 && options.names != NameMode::Numeric {
        emit_unnamed_record(out, unnamed, database.is_some());
    }
    Ok(())
}

/// `true` if `function` passes the `-s` / `-d` filters.
fn selected(options: &Options, function: &Function<'_>) -> bool {
    if let Some(slot) = options.slot {
        if function.node.id() != slot {
            return false;
        }
    }
    if let Some(filter) = options.device {
        if filter.vendor.is_some_and(|v| v != function.vendor) {
            return false;
        }
        if filter.device.is_some_and(|d| d != function.device) {
            return false;
        }
    }
    true
}

/// Append `function`'s identity — class, vendor, and device, in the
/// selected [`NameMode`] — to `line`, counting an identity the database
/// could not fully name.
fn push_identity(
    line: &mut String,
    options: &Options,
    function: &Function<'_>,
    database: Option<&DevIds<'_>>,
    unnamed: &mut u64,
) {
    // The 24-bit PCI class code: base class, sub-class, programming
    // interface. The listing names the sub-class (the `pci.ids` row
    // `lspci` also uses), falling back to the base class, then numeric.
    let base = ((function.class24 >> 16) & 0xFF) as u8;
    let sub = ((function.class24 >> 8) & 0xFF) as u8;
    let class_hex = format!("{base:02x}{sub:02x}");
    let vendor_hex = format!("{:04x}", function.vendor);
    let device_hex = format!("{:04x}", function.device);

    let class_name = database.and_then(|db| db.subclass(base, sub).or_else(|| db.class(base)));
    let vendor_name = database.and_then(|db| db.vendor(function.vendor));
    let device_name = database.and_then(|db| db.device(function.vendor, function.device));
    if matches!(options.names, NameMode::Names | NameMode::Both)
        && (class_name.is_none() || vendor_name.is_none() || device_name.is_none())
    {
        *unnamed += 1;
    }

    // `write!` to a `String` cannot fail; the results are discarded on
    // that basis.
    match options.names {
        NameMode::Numeric => {
            let _ = write!(line, "{class_hex}: {vendor_hex}:{device_hex}");
        }
        NameMode::Names | NameMode::Both => {
            let class_text = class_name.map_or_else(|| format!("Class {class_hex}"), String::from);
            let vendor_text =
                vendor_name.map_or_else(|| format!("Vendor {vendor_hex}"), String::from);
            let device_text =
                device_name.map_or_else(|| format!("Device {device_hex}"), String::from);
            if options.names == NameMode::Names {
                let _ = write!(line, "{class_text}: {vendor_text} {device_text}");
            } else {
                let _ = write!(
                    line,
                    "{class_text} [{class_hex}]: {vendor_text} {device_text} [{vendor_hex}:{device_hex}]"
                );
            }
        }
    }
}

/// Render the `-t` topology view: every PCI function under its ancestor
/// chain, ancestors that lead to a PCI function shown as `#<id> <class>`
/// context lines, indentation one step per tree level.
fn render_tree(
    options: &Options,
    nodes: &[HwNode],
    functions: &[Function<'_>],
    database: Option<&DevIds<'_>>,
    out: &dyn Output,
    unnamed: &mut u64,
) -> Result<(), LspciError> {
    // Which nodes are (or contain) a selected PCI function.
    let selected_ids: Vec<u32> = functions.iter().map(|f| f.node.id()).collect();
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
        if let Some(function) = functions.iter().find(|f| f.node.id() == node.id()) {
            let _ = write!(line, "#{} ", node.id());
            push_identity(&mut line, options, function, database, unnamed);
        } else {
            let _ = write!(line, "#{} {}", node.id(), hwtree::class_label(node.class()));
        }
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(LspciError::Output)?;
        if options.verbose {
            if let Some(function) = functions.iter().find(|f| f.node.id() == node.id()) {
                render_resources(function.node.resources(), depth + 1, out)?;
            }
        }
    }
    Ok(())
}

/// Render a function's declared resources — the capability-grant
/// *requests* the tree records, no live state and no secrets — one
/// indented line per resource, in the `-v` view.
fn render_resources(
    resources: &[HwResource],
    depth: usize,
    out: &dyn Output,
) -> Result<(), LspciError> {
    for resource in resources {
        let mut line = String::new();
        for _ in 0..depth {
            line.push_str("  ");
        }
        let base = resource.base();
        let len = resource.length();
        // `write!` to a `String` cannot fail; the results are discarded
        // on that basis.
        match resource.kind() {
            Some(HwResourceKind::Mmio) => {
                let _ = write!(line, "MMIO window at 0x{base:x} [size=0x{len:x}]");
            }
            Some(HwResourceKind::Irq) => {
                if len > 1 {
                    let _ = write!(line, "IRQ lines {base} (count {len})");
                } else {
                    let _ = write!(line, "IRQ line {base}");
                }
            }
            Some(HwResourceKind::Port) => {
                let _ = write!(line, "I/O ports at 0x{base:x} [count=0x{len:x}]");
            }
            Some(HwResourceKind::Dma) => {
                if base == 0 && len == 0 {
                    line.push_str("DMA (no addressing constraint declared)");
                } else {
                    let _ = write!(
                        line,
                        "DMA constraint: addresses below 0x{base:x} [size=0x{len:x}]"
                    );
                }
            }
            Some(HwResourceKind::BusWindow) => {
                let _ = write!(
                    line,
                    "Bus window at 0x{base:x} [size=0x{len:x}] -> bus 0x{:x}",
                    resource.translated_base()
                );
            }
            Some(HwResourceKind::Endpoint) => {
                let _ = write!(line, "IPC endpoint {base}");
            }
            Some(HwResourceKind::Shared) => {
                let _ = write!(line, "Shared-memory region {base}");
            }
            Some(HwResourceKind::Framebuffer) => {
                match resource.framebuffer_mode() {
                    Ok(mode) => {
                        let _ = write!(
                            line,
                            "Framebuffer at 0x{base:x} [size=0x{len:x}] {}x{} stride {}",
                            mode.width_px, mode.height_px, mode.stride_bytes
                        );
                    }
                    // A malformed geometry still lists as a window; the
                    // mode is simply not shown (fail closed, never guess).
                    Err(_) => {
                        let _ = write!(line, "Framebuffer at 0x{base:x} [size=0x{len:x}]");
                    }
                }
            }
            None => line.push_str("resource (unknown kind)"),
        }
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(LspciError::Output)?;
    }
    Ok(())
}

/// Emit the `pci.names_unresolved` advisory (fd 3) when identities were
/// rendered numerically because the database has no entry (or did not
/// load): a tool or user then knows the numeric forms are not omissions
/// from the inventory itself. Advisory only — never affects the listing,
/// the exit status, or ordering.
fn emit_unnamed_record(out: &dyn Output, unnamed: u64, database_loaded: bool) {
    let message = if unnamed == 1 {
        String::from("1 device rendered with numeric ids.")
    } else {
        format!("{unnamed} devices rendered with numeric ids.")
    };
    let ai = format!(
        "{{\"subject\":\"pci_listing\",\
         \"omission\":{{\"reason\":\"name_not_in_database\",\
         \"entry_class\":\"device_name\",\"omitted_count\":{unnamed},\
         \"database_loaded\":{database_loaded},\
         \"stdout_is_exhaustive\":true}},\
         \"suggestion\":{{\"argv\":[\"lspci\",\"-nn\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        "pci.names_unresolved",
        Severity::Info,
        Human::with_suggestion(&message, "Use `lspci -nn` to see names and ids together."),
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

    use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey, HwTreeHeader, HW_NODE_ROOT};
    use tairix_abi::Errno;
    use tairix_devids::{textdb, DbKind};
    use tairix_help::SourceError;

    use super::*;
    use crate::command::parse;

    /// A `pci.ids`-grammar fixture compiled through the real import
    /// pipeline: one named vendor+device, one named class table. Intel and
    /// the SATA class are deliberately absent so the numeric fallbacks and
    /// the advisory record are exercised.
    const FIXTURE_DB: &str = "\
1af4  Red Hat, Inc.
\t1000  Virtio network device
C 02  Network controller
\t00  Ethernet controller
";

    fn fixture_table() -> Vec<u8> {
        textdb::parse(DbKind::Pci, FIXTURE_DB.as_bytes())
            .expect("fixture text vets")
            .encode()
    }

    /// The canned tree: a root bus (#1) carrying a PCI host bridge (#2)
    /// with two functions — a named virtio NIC (#3) and an unnamed AHCI
    /// controller (#4, with declared resources) — plus a non-PCI timer
    /// (#5) the listing must ignore.
    fn fixture_tree() -> Vec<u8> {
        let mut root = HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Bus);
        root.push_match_key(HwMatchKey::compatible(b"fixture,root").expect("fits"))
            .expect("key fits");
        let mut bridge = HwNode::new(2, 1, HwDeviceClass::Bus);
        bridge
            .push_match_key(HwMatchKey::compatible(b"fixture,pcie").expect("fits"))
            .expect("key fits");
        let mut nic = HwNode::new(3, 2, HwDeviceClass::Network);
        nic.push_match_key(HwMatchKey::pci(0x1af4, 0x1000, 0x02_00_00))
            .expect("key fits");
        let mut ahci = HwNode::new(4, 2, HwDeviceClass::Storage);
        ahci.push_match_key(HwMatchKey::pci(0x8086, 0x2922, 0x01_06_01))
            .expect("key fits");
        ahci.push_resource(HwResource::mmio(0xfe00_0000, 0x1000))
            .expect("resource fits");
        ahci.push_resource(HwResource::irq(33, 1))
            .expect("resource fits");
        let mut timer = HwNode::new(5, 1, HwDeviceClass::Timer);
        timer
            .push_match_key(HwMatchKey::compatible(b"fixture,timer").expect("fits"))
            .expect("key fits");

        let nodes = [root, bridge, nic, ahci, timer];
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
    ) -> (Recorder, Result<(), LspciError>) {
        let command = parse(args).expect("arguments parse");
        let transport = Fixture { reply };
        let table = fixture_table();
        let database = if with_db {
            Some(DevIds::parse(DbKind::Pci, &table).expect("fixture table decodes"))
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
                "#3 Ethernet controller: Red Hat, Inc. Virtio network device",
                "#4 Class 0106: Vendor 8086 Device 2922",
            ]
        );
        // The unnamed AHCI controller is advised on fd 3, and only it.
        let infos = out.infos.borrow();
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(
            infos[0].contains("\"pci.names_unresolved\""),
            "{}",
            infos[0]
        );
        assert!(infos[0].contains("\"omitted_count\":1"), "{}", infos[0]);
    }

    #[test]
    fn numeric_and_both_modes_render_ids() {
        let (out, result) = run_case(&["-n"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["#3 0200: 1af4:1000", "#4 0106: 8086:2922"]);
        // Numeric mode shows every id already; no advisory is emitted.
        assert!(out.infos.borrow().is_empty());

        let (out, result) = run_case(&["-nn"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "#3 Ethernet controller [0200]: Red Hat, Inc. Virtio network device [1af4:1000]",
                "#4 Class 0106 [0106]: Vendor 8086 Device 2922 [8086:2922]",
            ]
        );
    }

    #[test]
    fn a_missing_database_degrades_to_numeric_forms() {
        let (out, result) = run_case(&[], Ok(fixture_tree()), false);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "#3 Class 0200: Vendor 1af4 Device 1000",
                "#4 Class 0106: Vendor 8086 Device 2922",
            ]
        );
        let infos = out.infos.borrow();
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(
            infos[0].contains("\"database_loaded\":false"),
            "{}",
            infos[0]
        );
    }

    #[test]
    fn slot_and_device_filters_select() {
        let (out, result) = run_case(&["-s", "4"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["#4 Class 0106: Vendor 8086 Device 2922"]);

        let (out, result) = run_case(&["-d", "1af4:"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            ["#3 Ethernet controller: Red Hat, Inc. Virtio network device"]
        );

        let (out, result) = run_case(&["-d", ":2922"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(out.lines(), ["#4 Class 0106: Vendor 8086 Device 2922"]);
    }

    #[test]
    fn verbose_appends_the_declared_resources() {
        let (out, result) = run_case(&["-v", "-s", "4"], Ok(fixture_tree()), true);
        result.expect("listing succeeds");
        assert_eq!(
            out.lines(),
            [
                "#4 Class 0106: Vendor 8086 Device 2922",
                "  MMIO window at 0xfe000000 [size=0x1000]",
                "  IRQ line 33",
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
                "    #3 Ethernet controller: Red Hat, Inc. Virtio network device",
                "    #4 Class 0106: Vendor 8086 Device 2922",
            ]
        );
    }

    #[test]
    fn a_refused_query_is_fatal_with_the_reason() {
        let (out, result) = run_case(&[], Err(Errno::PermissionDenied), true);
        assert_eq!(result, Err(LspciError::PermissionDenied));
        assert!(out.lines().is_empty(), "nothing is fabricated");
    }

    #[test]
    fn a_malformed_reply_fails_closed() {
        // Not a whole number of node records after the snapshot header.
        let (out, result) = run_case(&[], Ok(alloc::vec![0u8; HwNode::WIRE_LEN + 1]), true);
        assert_eq!(result, Err(LspciError::Service(Errno::BadMagic)));
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
        for locale in tairix_help::REQUIRED_LOCALES {
            let path = alloc::format!("{help_root}/{locale}/lspci.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-n`",
                "`-nn`",
                "`-v`",
                "`-t`",
                "`-d [<vendor>]:[<device>]`",
                "`-s <node>`",
                "`-?, --help`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/lspci.md must document {switch}"
                );
            }
        }
    }
}

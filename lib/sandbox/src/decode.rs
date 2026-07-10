//! The executable-inspection decode service — the seam's first consumers.
//!
//! This module wires `rustos-binfmt` (container summaries) and
//! `rustos-disasm` (instruction windows) behind the sandbox seam: the
//! [`DecodeService`] runs *inside* the sandboxed worker, and the client
//! helpers ([`container_summary`], [`manifest_summary`], [`disassemble`])
//! run in the calling program, marshalling typed requests and validating
//! typed replies **fail-closed** — the worker has parsed hostile bytes and
//! may be compromised, so nothing it replies is trusted beyond these
//! bounds. The decoders themselves never run in the caller's address
//! space.
//!
//! Every list in a reply carries a fixed cap; a summary that could not
//! include everything says so through its `*_truncated` flags rather than
//! pretending completeness.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_binfmt::{elf, wasm, Format};
use rustos_disasm::{aarch64, riscv64, wasm as wasm_isa, x86_64, Insn, MAX_INSN_BYTES};

use crate::host::{Launcher, ParserSandbox, SandboxError};
use crate::wire::{Reader, Writer};
use crate::worker::Service;

/// Largest input (container image, manifest, or code window) a decode
/// request may carry. A fixed validation bound: a caller with a larger
/// file sends a prefix or a window, never the whole image.
pub const MAX_INPUT: usize = 4 << 20;

/// Most regions a container summary reports; more sets `regions_truncated`.
pub const MAX_REGIONS: usize = 4096;

/// Most symbols a container summary reports; more sets `symbols_truncated`.
pub const MAX_SYMBOLS: usize = 16384;

/// Longest region or symbol name carried in a reply, in bytes.
pub const MAX_NAME: usize = 256;

/// Longest mnemonic or operand text carried per instruction, in bytes.
pub const MAX_TEXT: usize = 256;

/// Most instructions one disassembly window may return.
pub const MAX_WINDOW_INSNS: usize = 4096;

/// The instruction-set architectures the disassemble request accepts.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Isa {
    /// x86_64 (variable length).
    X86_64,
    /// AArch64 A64 (fixed 32-bit).
    Aarch64,
    /// RISC-V RV64GC (16/32-bit parcels).
    Riscv64,
    /// wasm code-section body stream.
    Wasm,
}

impl Isa {
    const fn to_wire(self) -> u8 {
        match self {
            Self::X86_64 => 1,
            Self::Aarch64 => 2,
            Self::Riscv64 => 3,
            Self::Wasm => 4,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::X86_64),
            2 => Some(Self::Aarch64),
            3 => Some(Self::Riscv64),
            4 => Some(Self::Wasm),
            _ => None,
        }
    }
}

/// The container format a summary reports.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContainerFormat {
    /// A RustOS `rxe` load image.
    Rxe,
    /// A 64-bit little-endian ELF file.
    Elf64,
    /// A WebAssembly module.
    Wasm,
}

/// What a region holds, as far as inspection cares.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RegionKind {
    /// Executable code a disassembler can walk.
    Code,
    /// Anything else (data, bss, tables, custom sections).
    Data,
}

/// One segment, section, or function body of a container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Region {
    /// Name where the format carries one (section name, `func[i]`);
    /// empty for unnamed regions (rxe segments).
    pub name: String,
    /// Whether the region is executable code.
    pub kind: RegionKind,
    /// Virtual address, where the format defines one; otherwise 0.
    pub addr: u64,
    /// File offset of the region's bytes.
    pub file_offset: u64,
    /// Number of file bytes.
    pub file_size: u64,
    /// In-memory size (bytes past `file_size` are zero-filled).
    pub mem_size: u64,
    /// Readable when mapped.
    pub read: bool,
    /// Writable when mapped.
    pub write: bool,
    /// Executable when mapped.
    pub execute: bool,
}

/// One symbol-table entry of a container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolRecord {
    /// Symbol name (never empty; unnamed entries are omitted).
    pub name: String,
    /// Symbol value, usually a virtual address.
    pub addr: u64,
    /// Associated size in bytes.
    pub size: u64,
}

/// The typed container summary a sandboxed decode returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerSummary {
    /// The recognised container format.
    pub format: ContainerFormat,
    /// The instruction set the container's code targets, when the format
    /// names one (`None` for `rxe`, whose ISA is the installed system's).
    pub isa: Option<Isa>,
    /// Entry-point virtual address (0 where the format has none).
    pub entry: u64,
    /// Segments/sections/function bodies, capped at [`MAX_REGIONS`].
    pub regions: Vec<Region>,
    /// Symbols, capped at [`MAX_SYMBOLS`]; unnamed entries omitted.
    pub symbols: Vec<SymbolRecord>,
    /// True when not every region made the list (cap reached).
    pub regions_truncated: bool,
    /// True when not every symbol made the list (cap reached, or an
    /// entry's name did not resolve and was omitted).
    pub symbols_truncated: bool,
}

/// The typed rxe-manifest summary a sandboxed decode returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestInfo {
    /// The ABI version the manifest targets.
    pub abi_version: u32,
    /// The requested capability identifiers, in declaration order.
    pub capabilities: Vec<u16>,
}

/// One decoded instruction of a disassembly window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsnRecord {
    /// Address of the first encoding byte.
    pub address: u64,
    /// Full number of bytes consumed (≥ 1).
    pub length: u32,
    /// The encoding bytes, capped at [`MAX_INSN_BYTES`].
    pub bytes: Vec<u8>,
    /// Mnemonic text (wasm carries its nesting indentation).
    pub mnemonic: String,
    /// Operand text; empty when none.
    pub operands: String,
    /// Resolved absolute direct branch/call target, when encoded.
    pub branch_target: Option<u64>,
}

/// One disassembled window and where the next window starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisasmWindow {
    /// The decoded instructions, in address order.
    pub insns: Vec<InsnRecord>,
    /// Address just past the last decoded instruction — the next
    /// window's start.
    pub next_address: u64,
    /// The wasm nesting depth after the window (thread it into the next
    /// request); unused by the fixed-ISA decoders.
    pub next_depth: u32,
}

/// Why the service refused a request, carried typed over the wire.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecodeRefusal {
    /// The request payload violated the request grammar.
    MalformedRequest,
    /// No decoder recognises the input's container format.
    UnrecognisedContainer,
    /// The recognised container failed structural validation.
    MalformedContainer,
}

/// Typed failure a client decode helper can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DecodeFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// The worker refused the request with the carried typed reason.
    Refused(DecodeRefusal),
    /// The worker's reply violated the reply grammar: it cannot be
    /// believed, so the caller gets nothing (fail closed).
    ReplyMalformed,
}

/// Request opcodes.
const OP_SUMMARY: u8 = 1;
const OP_MANIFEST: u8 = 2;
const OP_DISASSEMBLE: u8 = 3;

/// Reply tags.
const REPLY_ERROR: u8 = 0;
const REPLY_SUMMARY: u8 = 1;
const REPLY_MANIFEST: u8 = 2;
const REPLY_DISASM: u8 = 3;

/// Refusal wire codes.
const REFUSAL_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_UNRECOGNISED: u8 = 2;
const REFUSAL_MALFORMED_CONTAINER: u8 = 3;

impl DecodeRefusal {
    const fn to_wire(self) -> u8 {
        match self {
            Self::MalformedRequest => REFUSAL_MALFORMED_REQUEST,
            Self::UnrecognisedContainer => REFUSAL_UNRECOGNISED,
            Self::MalformedContainer => REFUSAL_MALFORMED_CONTAINER,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            REFUSAL_MALFORMED_REQUEST => Some(Self::MalformedRequest),
            REFUSAL_UNRECOGNISED => Some(Self::UnrecognisedContainer),
            REFUSAL_MALFORMED_CONTAINER => Some(Self::MalformedContainer),
            _ => None,
        }
    }
}

/// The service the sandboxed worker runs: pure decode over the request
/// payload, a reply payload out. Total by construction — every failure is
/// a typed error reply.
#[derive(Debug, Default)]
pub struct DecodeService;

impl Service for DecodeService {
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        match dispatch(request) {
            Ok(reply) => reply,
            Err(refusal) => {
                let mut w = Writer::new();
                w.u8(REPLY_ERROR);
                w.u8(refusal.to_wire());
                w.finish()
            }
        }
    }
}

/// Decode the request, run the operation, and encode the reply.
fn dispatch(request: &[u8]) -> Result<Vec<u8>, DecodeRefusal> {
    let mut r = Reader::new(request);
    let op = r.u8().map_err(|_| DecodeRefusal::MalformedRequest)?;
    match op {
        OP_SUMMARY => {
            let image = r
                .bytes(MAX_INPUT)
                .map_err(|_| DecodeRefusal::MalformedRequest)?;
            if !r.is_exhausted() {
                return Err(DecodeRefusal::MalformedRequest);
            }
            let summary = summarise(image)?;
            Ok(encode_summary(&summary))
        }
        OP_MANIFEST => {
            let manifest = r
                .bytes(MAX_INPUT)
                .map_err(|_| DecodeRefusal::MalformedRequest)?;
            if !r.is_exhausted() {
                return Err(DecodeRefusal::MalformedRequest);
            }
            let info = rustos_binfmt::rxe::ManifestSummary::parse(manifest)
                .map_err(|_| DecodeRefusal::MalformedContainer)?;
            Ok(encode_manifest(&info))
        }
        OP_DISASSEMBLE => {
            let isa = Isa::from_wire(r.u8().map_err(|_| DecodeRefusal::MalformedRequest)?)
                .ok_or(DecodeRefusal::MalformedRequest)?;
            let address = r.u64().map_err(|_| DecodeRefusal::MalformedRequest)?;
            let depth = r.u32().map_err(|_| DecodeRefusal::MalformedRequest)?;
            let max_insns = r.u32().map_err(|_| DecodeRefusal::MalformedRequest)?;
            let code = r
                .bytes(MAX_INPUT)
                .map_err(|_| DecodeRefusal::MalformedRequest)?;
            if !r.is_exhausted() {
                return Err(DecodeRefusal::MalformedRequest);
            }
            let window = run_disassembly(isa, address, depth, max_insns, code);
            Ok(encode_disasm(&window))
        }
        _ => Err(DecodeRefusal::MalformedRequest),
    }
}

/// Build the container summary for `image`.
fn summarise(image: &[u8]) -> Result<ContainerSummary, DecodeRefusal> {
    match rustos_binfmt::detect(image) {
        Some(Format::Rxe) => summarise_rxe(image),
        Some(Format::Elf64) => summarise_elf(image),
        Some(Format::Wasm) => summarise_wasm(image),
        None => Err(DecodeRefusal::UnrecognisedContainer),
    }
}

/// Accumulates regions under the fixed cap, recording overflow honestly.
#[derive(Default)]
struct RegionList {
    regions: Vec<Region>,
    truncated: bool,
}

impl RegionList {
    fn push(&mut self, region: Region) {
        if self.regions.len() < MAX_REGIONS {
            self.regions.push(region);
        } else {
            self.truncated = true;
        }
    }
}

/// Clamp a name to the reply bound at a character boundary.
fn bounded_name(name: &str) -> String {
    let mut end = name.len().min(MAX_NAME);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&name[..end])
}

fn summarise_rxe(image: &[u8]) -> Result<ContainerSummary, DecodeRefusal> {
    let view =
        rustos_binfmt::rxe::RxeView::parse(image).map_err(|_| DecodeRefusal::MalformedContainer)?;
    let mut list = RegionList::default();
    for segment in view.segments() {
        let executable = segment.permission.is_executable();
        list.push(Region {
            name: String::new(),
            kind: if executable {
                RegionKind::Code
            } else {
                RegionKind::Data
            },
            addr: segment.vaddr,
            file_offset: segment.file_offset,
            file_size: segment.file_size,
            mem_size: segment.mem_size,
            read: true,
            write: segment.permission.is_writable(),
            execute: executable,
        });
    }
    Ok(ContainerSummary {
        format: ContainerFormat::Rxe,
        isa: None,
        entry: view.entry(),
        regions: list.regions,
        symbols: Vec::new(),
        regions_truncated: list.truncated,
        symbols_truncated: false,
    })
}

fn summarise_elf(image: &[u8]) -> Result<ContainerSummary, DecodeRefusal> {
    let view = elf::ElfView::parse(image).map_err(|_| DecodeRefusal::MalformedContainer)?;
    let header = *view.header();
    let isa = match header.machine {
        elf::Machine::X86_64 => Isa::X86_64,
        elf::Machine::Aarch64 => Isa::Aarch64,
        elf::Machine::Riscv64 => Isa::Riscv64,
    };
    let mut list = RegionList::default();
    if header.shnum > 0 {
        for index in 0..header.shnum {
            let Ok(section) = view.section(index) else {
                // The table bounds were validated whole at parse; a
                // per-entry refusal here means the file lies about an
                // entry, so the container cannot be believed.
                return Err(DecodeRefusal::MalformedContainer);
            };
            if section.sh_type == 0 {
                continue;
            }
            let name = view
                .section_name(&section)
                .map(bounded_name)
                .unwrap_or_default();
            let executable = section.flags & 0b100 != 0;
            list.push(Region {
                name,
                kind: if executable {
                    RegionKind::Code
                } else {
                    RegionKind::Data
                },
                addr: section.addr,
                file_offset: section.offset,
                file_size: if section.sh_type == elf::SHT_NOBITS {
                    0
                } else {
                    section.size
                },
                mem_size: section.size,
                read: true,
                write: section.flags & 0b001 != 0,
                execute: executable,
            });
        }
    } else {
        // A section-less file (a stripped loadable image) still shows its
        // program headers, so the viewer has regions to walk.
        for index in 0..header.phnum {
            let Ok(ph) = view.program_header(index) else {
                return Err(DecodeRefusal::MalformedContainer);
            };
            let executable = ph.flags & 0b001 != 0;
            list.push(Region {
                name: String::new(),
                kind: if executable {
                    RegionKind::Code
                } else {
                    RegionKind::Data
                },
                addr: ph.vaddr,
                file_offset: ph.offset,
                file_size: ph.file_size,
                mem_size: ph.mem_size,
                read: ph.flags & 0b100 != 0,
                write: ph.flags & 0b010 != 0,
                execute: executable,
            });
        }
    }
    let (symbols, symbols_truncated) = elf_symbols(&view, header.shnum);
    Ok(ContainerSummary {
        format: ContainerFormat::Elf64,
        isa: Some(isa),
        entry: header.entry,
        regions: list.regions,
        symbols,
        regions_truncated: list.truncated,
        symbols_truncated,
    })
}

/// Collect the named symbols of the first symbol table (`.symtab`
/// preferred over `.dynsym`), capped at [`MAX_SYMBOLS`].
fn elf_symbols(view: &elf::ElfView<'_>, shnum: u16) -> (Vec<SymbolRecord>, bool) {
    let table_index = (0..shnum).find_map(|index| {
        let section = view.section(index).ok()?;
        (section.sh_type == elf::SHT_SYMTAB).then_some(index)
    });
    let table_index = table_index.or_else(|| {
        (0..shnum).find_map(|index| {
            let section = view.section(index).ok()?;
            (section.sh_type == elf::SHT_DYNSYM).then_some(index)
        })
    });
    let Some(index) = table_index else {
        return (Vec::new(), false);
    };
    let Ok(table) = view.symbol_table(index) else {
        // The table names itself but does not decode: report no symbols,
        // honestly marked incomplete, rather than refusing the whole
        // summary a viewer can still use.
        return (Vec::new(), true);
    };
    let mut symbols = Vec::new();
    let mut truncated = false;
    for entry in 0..table.len() {
        let Ok(symbol) = table.symbol(entry) else {
            truncated = true;
            continue;
        };
        if symbol.name_offset == 0 {
            continue;
        }
        let Ok(name) = table.name(&symbol) else {
            truncated = true;
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if symbols.len() == MAX_SYMBOLS {
            truncated = true;
            break;
        }
        symbols.push(SymbolRecord {
            name: bounded_name(name),
            addr: symbol.value,
            size: symbol.size,
        });
    }
    (symbols, truncated)
}

/// Human name for a wasm section id, matching the spec's section names.
fn wasm_section_name(id: u8) -> &'static str {
    match id {
        0 => "custom",
        1 => "type",
        2 => "import",
        3 => "function",
        4 => "table",
        5 => "memory",
        6 => "global",
        7 => "export",
        8 => "start",
        9 => "element",
        10 => "code",
        11 => "data",
        12 => "data count",
        _ => "unknown",
    }
}

fn summarise_wasm(image: &[u8]) -> Result<ContainerSummary, DecodeRefusal> {
    let view = wasm::WasmView::parse(image).map_err(|_| DecodeRefusal::MalformedContainer)?;
    let mut list = RegionList::default();
    for entry in view.sections() {
        list.push(Region {
            name: String::from(wasm_section_name(entry.id)),
            kind: RegionKind::Data,
            addr: 0,
            file_offset: entry.offset as u64,
            file_size: entry.size as u64,
            mem_size: entry.size as u64,
            read: true,
            write: false,
            execute: false,
        });
    }
    // Each function body is a code region a disassembler can walk; the
    // whole walk fails closed on the first framing violation.
    if let Ok(Some(bodies)) = view.code_bodies() {
        let mut name = String::new();
        for body in bodies {
            let Ok(range) = body else {
                return Err(DecodeRefusal::MalformedContainer);
            };
            name.clear();
            // `func[i]` composed without a formatter dependency.
            name.push_str("func[");
            push_decimal(&mut name, u64::from(range.index));
            name.push(']');
            list.push(Region {
                name: bounded_name(&name),
                kind: RegionKind::Code,
                addr: 0,
                file_offset: range.offset as u64,
                file_size: range.size as u64,
                mem_size: range.size as u64,
                read: true,
                write: false,
                execute: true,
            });
        }
    }
    Ok(ContainerSummary {
        format: ContainerFormat::Wasm,
        isa: Some(Isa::Wasm),
        entry: 0,
        regions: list.regions,
        symbols: Vec::new(),
        regions_truncated: list.truncated,
        symbols_truncated: false,
    })
}

/// Append `value` in decimal to `out`.
fn push_decimal(out: &mut String, value: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut rest = value;
    loop {
        at -= 1;
        digits[at] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    for &digit in &digits[at..] {
        out.push(char::from(digit));
    }
}

/// Decode up to `max_insns` instructions from `code` at `address`.
fn run_disassembly(
    isa: Isa,
    address: u64,
    depth: u32,
    max_insns: u32,
    code: &[u8],
) -> DisasmWindow {
    let take = usize::try_from(max_insns)
        .unwrap_or(usize::MAX)
        .min(MAX_WINDOW_INSNS);
    let mut insns = Vec::new();
    let mut at = 0usize;
    let mut next_address = address;
    let mut next_depth = depth;
    while insns.len() < take && at < code.len() {
        let decoded = match isa {
            Isa::X86_64 => x86_64::decode(&code[at..], next_address),
            Isa::Aarch64 => aarch64::decode(&code[at..], next_address),
            Isa::Riscv64 => riscv64::decode(&code[at..], next_address),
            Isa::Wasm => wasm_isa::decode(&code[at..], next_address, next_depth).map(
                |(insn, depth_after)| {
                    next_depth = depth_after;
                    insn
                },
            ),
        };
        // Every decoder consumes at least one length unit when it decodes
        // at all; `None` only means the remaining input is empty.
        let Some(insn) = decoded else { break };
        let length = insn.length.max(1);
        at = at.saturating_add(length);
        next_address = next_address.wrapping_add(length as u64);
        insns.push(record_from(insn));
    }
    DisasmWindow {
        insns,
        next_address,
        next_depth,
    }
}

/// Convert a decoder [`Insn`] into the wire record shape.
fn record_from(insn: Insn) -> InsnRecord {
    let mut bytes = insn.bytes;
    bytes.truncate(MAX_INSN_BYTES);
    let mut mnemonic = insn.mnemonic;
    mnemonic.truncate(MAX_TEXT);
    let mut operands = insn.operands;
    operands.truncate(MAX_TEXT);
    InsnRecord {
        address: insn.address,
        length: u32::try_from(insn.length).unwrap_or(u32::MAX),
        bytes,
        mnemonic,
        operands,
        branch_target: insn.branch_target,
    }
}

fn encode_summary(summary: &ContainerSummary) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(REPLY_SUMMARY);
    w.u8(match summary.format {
        ContainerFormat::Rxe => 1,
        ContainerFormat::Elf64 => 2,
        ContainerFormat::Wasm => 3,
    });
    w.u8(summary.isa.map_or(0, Isa::to_wire));
    w.u64(summary.entry);
    let mut flags = 0u8;
    if summary.regions_truncated {
        flags |= 0b01;
    }
    if summary.symbols_truncated {
        flags |= 0b10;
    }
    w.u8(flags);
    w.u32(u32::try_from(summary.regions.len()).unwrap_or(0));
    for region in &summary.regions {
        w.str(&region.name);
        w.u8(match region.kind {
            RegionKind::Code => 1,
            RegionKind::Data => 2,
        });
        let mut perms = 0u8;
        if region.read {
            perms |= 0b001;
        }
        if region.write {
            perms |= 0b010;
        }
        if region.execute {
            perms |= 0b100;
        }
        w.u8(perms);
        w.u64(region.addr);
        w.u64(region.file_offset);
        w.u64(region.file_size);
        w.u64(region.mem_size);
    }
    w.u32(u32::try_from(summary.symbols.len()).unwrap_or(0));
    for symbol in &summary.symbols {
        w.str(&symbol.name);
        w.u64(symbol.addr);
        w.u64(symbol.size);
    }
    w.finish()
}

fn encode_manifest(info: &rustos_binfmt::rxe::ManifestSummary) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(REPLY_MANIFEST);
    w.u32(info.header().abi_version);
    w.u32(u32::try_from(info.capabilities().len()).unwrap_or(0));
    for id in info.capabilities() {
        w.u32(u32::from(id.as_u16()));
    }
    w.finish()
}

fn encode_disasm(window: &DisasmWindow) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(REPLY_DISASM);
    w.u64(window.next_address);
    w.u32(window.next_depth);
    w.u32(u32::try_from(window.insns.len()).unwrap_or(0));
    for insn in &window.insns {
        w.u64(insn.address);
        w.u32(insn.length);
        w.bytes(&insn.bytes);
        w.str(&insn.mnemonic);
        w.str(&insn.operands);
        match insn.branch_target {
            Some(target) => {
                w.u8(1);
                w.u64(target);
            }
            None => w.u8(0),
        }
    }
    w.finish()
}

/// Ask the sandboxed worker for the container summary of `image`.
///
/// # Errors
///
/// [`DecodeFailure`]: the sandbox failed, the worker refused typed, or
/// the worker's reply violated the reply grammar.
pub fn container_summary<L: Launcher, S: rustos_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    image: &[u8],
) -> Result<ContainerSummary, DecodeFailure> {
    if image.len() > MAX_INPUT {
        return Err(DecodeFailure::Sandbox(SandboxError::RequestTooLarge));
    }
    let mut w = Writer::new();
    w.u8(OP_SUMMARY);
    w.bytes(image);
    let reply = sandbox
        .request(&w.finish())
        .map_err(DecodeFailure::Sandbox)?;
    decode_summary_reply(&reply)
}

/// Ask the sandboxed worker for the manifest summary of `manifest`.
///
/// # Errors
///
/// As [`container_summary`].
pub fn manifest_summary<L: Launcher, S: rustos_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    manifest: &[u8],
) -> Result<ManifestInfo, DecodeFailure> {
    if manifest.len() > MAX_INPUT {
        return Err(DecodeFailure::Sandbox(SandboxError::RequestTooLarge));
    }
    let mut w = Writer::new();
    w.u8(OP_MANIFEST);
    w.bytes(manifest);
    let reply = sandbox
        .request(&w.finish())
        .map_err(DecodeFailure::Sandbox)?;
    decode_manifest_reply(&reply)
}

/// Ask the sandboxed worker to disassemble one window of `code`.
///
/// `address` is the first byte's address, `depth` the wasm nesting depth
/// carried from the previous window (0 elsewhere), `max_insns` the window
/// size (clamped to [`MAX_WINDOW_INSNS`] worker-side).
///
/// # Errors
///
/// As [`container_summary`].
pub fn disassemble<L: Launcher, S: rustos_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    isa: Isa,
    address: u64,
    depth: u32,
    max_insns: u32,
    code: &[u8],
) -> Result<DisasmWindow, DecodeFailure> {
    if code.len() > MAX_INPUT {
        return Err(DecodeFailure::Sandbox(SandboxError::RequestTooLarge));
    }
    let mut w = Writer::new();
    w.u8(OP_DISASSEMBLE);
    w.u8(isa.to_wire());
    w.u64(address);
    w.u32(depth);
    w.u32(max_insns);
    w.bytes(code);
    let reply = sandbox
        .request(&w.finish())
        .map_err(DecodeFailure::Sandbox)?;
    decode_disasm_reply(&reply)
}

/// Split a reply into its tag and body, resolving the error tag.
///
/// Returns the expected-body reader on the happy tag; any other shape is
/// the typed failure.
fn reply_body(reply: &[u8], expected_tag: u8) -> Result<Reader<'_>, DecodeFailure> {
    let mut r = Reader::new(reply);
    let tag = r.u8().map_err(|_| DecodeFailure::ReplyMalformed)?;
    if tag == REPLY_ERROR {
        let code = r.u8().map_err(|_| DecodeFailure::ReplyMalformed)?;
        if !r.is_exhausted() {
            return Err(DecodeFailure::ReplyMalformed);
        }
        let refusal = DecodeRefusal::from_wire(code).ok_or(DecodeFailure::ReplyMalformed)?;
        return Err(DecodeFailure::Refused(refusal));
    }
    if tag != expected_tag {
        return Err(DecodeFailure::ReplyMalformed);
    }
    Ok(r)
}

/// Decode a summary reply fail-closed (the worker is untrusted).
fn decode_summary_reply(reply: &[u8]) -> Result<ContainerSummary, DecodeFailure> {
    let mut r = reply_body(reply, REPLY_SUMMARY)?;
    let step = |_: crate::wire::WireError| DecodeFailure::ReplyMalformed;
    let format = match r.u8().map_err(step)? {
        1 => ContainerFormat::Rxe,
        2 => ContainerFormat::Elf64,
        3 => ContainerFormat::Wasm,
        _ => return Err(DecodeFailure::ReplyMalformed),
    };
    let isa = match r.u8().map_err(step)? {
        0 => None,
        raw => Some(Isa::from_wire(raw).ok_or(DecodeFailure::ReplyMalformed)?),
    };
    let entry = r.u64().map_err(step)?;
    let flags = r.u8().map_err(step)?;
    if flags & !0b11 != 0 {
        return Err(DecodeFailure::ReplyMalformed);
    }
    let region_count = r.u32().map_err(step)? as usize;
    if region_count > MAX_REGIONS {
        return Err(DecodeFailure::ReplyMalformed);
    }
    let mut regions = Vec::with_capacity(region_count);
    for _ in 0..region_count {
        let name = r.string(MAX_NAME).map_err(step)?;
        let kind = match r.u8().map_err(step)? {
            1 => RegionKind::Code,
            2 => RegionKind::Data,
            _ => return Err(DecodeFailure::ReplyMalformed),
        };
        let perms = r.u8().map_err(step)?;
        if perms & !0b111 != 0 {
            return Err(DecodeFailure::ReplyMalformed);
        }
        regions.push(Region {
            name,
            kind,
            addr: r.u64().map_err(step)?,
            file_offset: r.u64().map_err(step)?,
            file_size: r.u64().map_err(step)?,
            mem_size: r.u64().map_err(step)?,
            read: perms & 0b001 != 0,
            write: perms & 0b010 != 0,
            execute: perms & 0b100 != 0,
        });
    }
    let symbol_count = r.u32().map_err(step)? as usize;
    if symbol_count > MAX_SYMBOLS {
        return Err(DecodeFailure::ReplyMalformed);
    }
    let mut symbols = Vec::with_capacity(symbol_count);
    for _ in 0..symbol_count {
        let name = r.string(MAX_NAME).map_err(step)?;
        if name.is_empty() {
            return Err(DecodeFailure::ReplyMalformed);
        }
        symbols.push(SymbolRecord {
            name,
            addr: r.u64().map_err(step)?,
            size: r.u64().map_err(step)?,
        });
    }
    if !r.is_exhausted() {
        return Err(DecodeFailure::ReplyMalformed);
    }
    Ok(ContainerSummary {
        format,
        isa,
        entry,
        regions,
        symbols,
        regions_truncated: flags & 0b01 != 0,
        symbols_truncated: flags & 0b10 != 0,
    })
}

/// Decode a manifest reply fail-closed.
fn decode_manifest_reply(reply: &[u8]) -> Result<ManifestInfo, DecodeFailure> {
    let mut r = reply_body(reply, REPLY_MANIFEST)?;
    let abi_version = r.u32().map_err(|_| DecodeFailure::ReplyMalformed)?;
    let count = r.u32().map_err(|_| DecodeFailure::ReplyMalformed)? as usize;
    if count > usize::from(rustos_abi::MANIFEST_MAX_CAPABILITIES) {
        return Err(DecodeFailure::ReplyMalformed);
    }
    let mut capabilities = Vec::with_capacity(count);
    for _ in 0..count {
        let raw = r.u32().map_err(|_| DecodeFailure::ReplyMalformed)?;
        let id = u16::try_from(raw).map_err(|_| DecodeFailure::ReplyMalformed)?;
        capabilities.push(id);
    }
    if !r.is_exhausted() {
        return Err(DecodeFailure::ReplyMalformed);
    }
    Ok(ManifestInfo {
        abi_version,
        capabilities,
    })
}

/// Decode a disassembly reply fail-closed.
fn decode_disasm_reply(reply: &[u8]) -> Result<DisasmWindow, DecodeFailure> {
    let mut r = reply_body(reply, REPLY_DISASM)?;
    let step = |_: crate::wire::WireError| DecodeFailure::ReplyMalformed;
    let next_address = r.u64().map_err(step)?;
    let next_depth = r.u32().map_err(step)?;
    let count = r.u32().map_err(step)? as usize;
    if count > MAX_WINDOW_INSNS {
        return Err(DecodeFailure::ReplyMalformed);
    }
    let mut insns = Vec::with_capacity(count);
    for _ in 0..count {
        let address = r.u64().map_err(step)?;
        let length = r.u32().map_err(step)?;
        // A zero-length instruction would stall any pager walking the
        // window; a worker claiming one is lying.
        if length == 0 {
            return Err(DecodeFailure::ReplyMalformed);
        }
        let bytes = r.bytes(MAX_INSN_BYTES).map_err(step)?.to_vec();
        let mnemonic = r.string(MAX_TEXT).map_err(step)?;
        let operands = r.string(MAX_TEXT).map_err(step)?;
        let branch_target = match r.u8().map_err(step)? {
            0 => None,
            1 => Some(r.u64().map_err(step)?),
            _ => return Err(DecodeFailure::ReplyMalformed),
        };
        insns.push(InsnRecord {
            address,
            length,
            bytes,
            mnemonic,
            operands,
            branch_target,
        });
    }
    if !r.is_exhausted() {
        return Err(DecodeFailure::ReplyMalformed);
    }
    Ok(DisasmWindow {
        insns,
        next_address,
        next_depth,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        container_summary, decode_disasm_reply, decode_summary_reply, disassemble,
        manifest_summary, ContainerFormat, DecodeFailure, DecodeRefusal, DecodeService, Isa,
        RegionKind, MAX_INPUT, MAX_REGIONS,
    };
    use crate::host::ParserSandbox;
    use crate::loopback::LoopbackLauncher;
    use crate::wire::Writer;
    use crate::worker::Service;
    use alloc::vec;
    use alloc::vec::Vec;
    use rustos_abi::{
        CapabilityId, LoadHeader, ManifestHeader, RxePermission, Segment, LOAD_FLAG_PIE,
        LOAD_MAGIC, MANIFEST_MAGIC, RXE_PAGE_SIZE,
    };
    use rustos_log::{Event, Sink};

    /// Discards every event: these tests exercise healthy workers.
    struct SilentSink;

    impl Sink for SilentSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    type TestSandbox = ParserSandbox<LoopbackLauncher<fn() -> DecodeService>, SilentSink>;

    fn sandbox() -> TestSandbox {
        ParserSandbox::new(
            LoopbackLauncher::new(DecodeService::default as fn() -> DecodeService),
            SilentSink,
        )
    }

    /// A minimal valid rxe image: one RX code segment holding the entry,
    /// one RW data segment.
    fn rxe_image() -> Vec<u8> {
        let code = Segment {
            vaddr: RXE_PAGE_SIZE,
            file_offset: 0,
            file_size: 64,
            mem_size: RXE_PAGE_SIZE,
            permission: RxePermission::ReadExecute,
        };
        let data = Segment {
            vaddr: RXE_PAGE_SIZE * 2,
            file_offset: 64,
            file_size: 32,
            mem_size: RXE_PAGE_SIZE,
            permission: RxePermission::ReadWrite,
        };
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: 2,
            needed_count: 0,
            entry: RXE_PAGE_SIZE + 16,
            cfi_tag: [0xA5; 32],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        bytes.extend_from_slice(&code.to_le_bytes());
        bytes.extend_from_slice(&data.to_le_bytes());
        bytes
    }

    /// A manifest requesting the given capability ids.
    fn manifest_bytes(ids: &[CapabilityId]) -> Vec<u8> {
        let header = ManifestHeader {
            magic: MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: u16::try_from(ids.len()).expect("test counts fit"),
            reserved0: 0,
            syscall_table_hash: [0x11; 32],
            signer_pubkey: [0x22; 32],
            signature: [0x33; 64],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        for id in ids {
            bytes.extend_from_slice(&id.as_u16().to_le_bytes());
        }
        bytes
    }

    /// A minimal ELF64 x86_64 file: header plus one `PT_LOAD` RX program
    /// header, no sections (the stripped-image fallback path).
    fn elf_image() -> Vec<u8> {
        let mut bytes = vec![0u8; 64 + 56];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1; // EV_CURRENT
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type EXEC
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        bytes[24..32].copy_from_slice(&0x40_1000u64.to_le_bytes()); // e_entry
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
        let ph = 64;
        bytes[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        bytes[ph + 4..ph + 8].copy_from_slice(&0b101u32.to_le_bytes()); // R+X
        bytes[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        bytes[ph + 16..ph + 24].copy_from_slice(&0x40_0000u64.to_le_bytes()); // p_vaddr
        bytes[ph + 24..ph + 32].copy_from_slice(&0x40_0000u64.to_le_bytes()); // p_paddr
        bytes[ph + 32..ph + 40].copy_from_slice(&120u64.to_le_bytes()); // p_filesz
        bytes[ph + 40..ph + 48].copy_from_slice(&120u64.to_le_bytes()); // p_memsz
        bytes[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
        bytes
    }

    /// A wasm module holding one code section with `bodies` one-byte
    /// function bodies.
    fn wasm_module(bodies: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        push_leb(&mut payload, bodies);
        for _ in 0..bodies {
            payload.push(1); // body size
            payload.push(0x0B); // `end`
        }
        let mut bytes = b"\0asm\x01\0\0\0".to_vec();
        bytes.push(10); // code section id
        push_leb(&mut bytes, u32::try_from(payload.len()).expect("small"));
        bytes.extend_from_slice(&payload);
        bytes
    }

    fn push_leb(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let byte = u8::try_from(value & 0x7F).expect("masked");
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    #[test]
    fn an_rxe_summary_round_trips_through_the_sandbox() {
        let mut sandbox = sandbox();
        let summary = container_summary(&mut sandbox, &rxe_image()).expect("summary");
        assert_eq!(summary.format, ContainerFormat::Rxe);
        assert_eq!(summary.isa, None);
        assert_eq!(summary.entry, RXE_PAGE_SIZE + 16);
        assert_eq!(summary.regions.len(), 2);
        assert_eq!(summary.regions[0].kind, RegionKind::Code);
        assert!(summary.regions[0].execute && !summary.regions[0].write);
        assert_eq!(summary.regions[1].kind, RegionKind::Data);
        assert!(summary.regions[1].write && !summary.regions[1].execute);
        assert!(!summary.regions_truncated && !summary.symbols_truncated);
    }

    #[test]
    fn a_manifest_summary_round_trips_through_the_sandbox() {
        let mut sandbox = sandbox();
        let manifest = manifest_bytes(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let info = manifest_summary(&mut sandbox, &manifest).expect("manifest");
        assert_eq!(info.abi_version, rustos_abi::ABI_VERSION_CURRENT);
        assert_eq!(
            info.capabilities,
            vec![
                CapabilityId::FS_MOUNT.as_u16(),
                CapabilityId::NET_RAW.as_u16()
            ]
        );
    }

    #[test]
    fn an_elf_summary_reports_the_isa_and_the_program_header_fallback() {
        let mut sandbox = sandbox();
        let summary = container_summary(&mut sandbox, &elf_image()).expect("summary");
        assert_eq!(summary.format, ContainerFormat::Elf64);
        assert_eq!(summary.isa, Some(Isa::X86_64));
        assert_eq!(summary.entry, 0x40_1000);
        assert_eq!(summary.regions.len(), 1);
        assert_eq!(summary.regions[0].kind, RegionKind::Code);
        assert!(summary.regions[0].read && summary.regions[0].execute);
        assert!(!summary.regions[0].write);
    }

    #[test]
    fn a_wasm_summary_names_sections_and_function_bodies() {
        let mut sandbox = sandbox();
        let summary = container_summary(&mut sandbox, &wasm_module(2)).expect("summary");
        assert_eq!(summary.format, ContainerFormat::Wasm);
        assert_eq!(summary.isa, Some(Isa::Wasm));
        // One section region plus one code region per body.
        assert_eq!(summary.regions.len(), 3);
        assert_eq!(summary.regions[0].name, "code");
        assert_eq!(summary.regions[1].name, "func[0]");
        assert_eq!(summary.regions[1].kind, RegionKind::Code);
        assert_eq!(summary.regions[2].name, "func[1]");
    }

    #[test]
    fn region_overflow_is_reported_never_silent() {
        let mut sandbox = sandbox();
        let bodies = u32::try_from(MAX_REGIONS + 100).expect("fits");
        let summary = container_summary(&mut sandbox, &wasm_module(bodies)).expect("summary");
        assert_eq!(summary.regions.len(), MAX_REGIONS);
        assert!(summary.regions_truncated);
    }

    #[test]
    fn an_unrecognised_container_is_a_typed_refusal() {
        let mut sandbox = sandbox();
        assert_eq!(
            container_summary(&mut sandbox, b"plain text, not a binary"),
            Err(DecodeFailure::Refused(DecodeRefusal::UnrecognisedContainer))
        );
    }

    #[test]
    fn a_truncated_container_is_a_typed_refusal() {
        let mut sandbox = sandbox();
        let image = rxe_image();
        assert_eq!(
            container_summary(&mut sandbox, &image[..image.len() - 7]),
            Err(DecodeFailure::Refused(DecodeRefusal::MalformedContainer))
        );
    }

    #[test]
    fn a_malformed_request_is_refused_by_the_service() {
        let mut service = DecodeService;
        let reply = service.handle(&[0xFF, 1, 2, 3]);
        assert_eq!(
            decode_summary_reply(&reply),
            Err(DecodeFailure::Refused(DecodeRefusal::MalformedRequest))
        );
        // Trailing bytes after a well-formed request body are refused too.
        let mut w = Writer::new();
        w.u8(super::OP_SUMMARY);
        w.bytes(b"x");
        w.u8(0xEE);
        let reply = service.handle(&w.finish());
        assert_eq!(
            decode_summary_reply(&reply),
            Err(DecodeFailure::Refused(DecodeRefusal::MalformedRequest))
        );
    }

    #[test]
    fn disassembly_pages_by_max_insns_and_reports_the_next_address() {
        let mut sandbox = sandbox();
        // Two A64 `nop`s (0xD503201F little-endian).
        let nops = [0x1F, 0x20, 0x03, 0xD5, 0x1F, 0x20, 0x03, 0xD5];
        let window = disassemble(&mut sandbox, Isa::Aarch64, 0x1000, 0, 1, &nops).expect("window");
        assert_eq!(window.insns.len(), 1);
        assert_eq!(window.insns[0].address, 0x1000);
        assert_eq!(window.insns[0].length, 4);
        assert_eq!(window.insns[0].mnemonic, "nop");
        assert_eq!(window.next_address, 0x1004);
        // The next window resumes where the first stopped.
        let rest = disassemble(
            &mut sandbox,
            Isa::Aarch64,
            window.next_address,
            0,
            16,
            &nops[4..],
        )
        .expect("window");
        assert_eq!(rest.insns.len(), 1);
        assert_eq!(rest.next_address, 0x1008);
    }

    #[test]
    fn wasm_nesting_depth_threads_across_windows() {
        let mut sandbox = sandbox();
        // `block (empty) … end`: depth rises to 1 inside the block.
        let code = [0x02, 0x40, 0x01, 0x0B];
        let first = disassemble(&mut sandbox, Isa::Wasm, 0, 0, 1, &code).expect("window");
        assert_eq!(first.insns.len(), 1);
        assert_eq!(first.next_depth, 1);
        let rest = disassemble(
            &mut sandbox,
            Isa::Wasm,
            first.next_address,
            first.next_depth,
            16,
            &code[usize::try_from(first.next_address).expect("small")..],
        )
        .expect("window");
        assert_eq!(rest.insns.len(), 2);
        assert_eq!(rest.next_depth, 0);
    }

    #[test]
    fn hostile_replies_are_refused_fail_closed() {
        // A summary reply declaring more regions than the cap.
        let mut w = Writer::new();
        w.u8(super::REPLY_SUMMARY);
        w.u8(1); // rxe
        w.u8(0); // no isa
        w.u64(0);
        w.u8(0);
        w.u32(u32::try_from(MAX_REGIONS + 1).expect("fits"));
        assert_eq!(
            decode_summary_reply(&w.finish()),
            Err(DecodeFailure::ReplyMalformed)
        );
        // A disassembly reply claiming a zero-length instruction.
        let mut w = Writer::new();
        w.u8(super::REPLY_DISASM);
        w.u64(0);
        w.u32(0);
        w.u32(1);
        w.u64(0);
        w.u32(0); // zero length: a lie
        assert_eq!(
            decode_disasm_reply(&w.finish()),
            Err(DecodeFailure::ReplyMalformed)
        );
        // An unknown reply tag.
        assert_eq!(
            decode_summary_reply(&[0x77]),
            Err(DecodeFailure::ReplyMalformed)
        );
        // An empty reply.
        assert_eq!(
            decode_summary_reply(&[]),
            Err(DecodeFailure::ReplyMalformed)
        );
    }

    #[test]
    fn an_oversize_input_is_refused_before_any_request() {
        let mut sandbox = sandbox();
        let oversize = vec![0u8; MAX_INPUT + 1];
        assert!(matches!(
            container_summary(&mut sandbox, &oversize),
            Err(DecodeFailure::Sandbox(_))
        ));
    }
}

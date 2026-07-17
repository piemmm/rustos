//! The disassembly viewer: container summary pages and per-region paged
//! disassembly, decoded **only** behind the parser sandbox.
//!
//! The viewer never parses a container or instruction stream in its own
//! address space: every container summary, manifest summary, and
//! disassembly window is produced by the sandboxed decode service
//! (`tairix_sandbox::decode`) through the [`Decode`] seam, and every reply
//! is already validated fail-closed by the caller-side helpers there. The
//! only in-process inspection is the magic-prefix recognition
//! (`tairix_binfmt::detect`, a bounded byte compare) that routes a file to
//! this viewer at all.
//!
//! Instructions are decoded per screenful from the viewed region's window
//! — never the whole binary up front. Scrolling keeps a history of visited
//! line tops; moving above the history resynchronises from a nearby anchor
//! (fixed-length ISAs re-align within a short backscan; wasm walks forward
//! from a recorded anchor, threading the nesting depth the service
//! reports). Goto and search run as bounded background jobs on the same
//! tick machinery the text and hex viewers use, so a large region never
//! stalls the key loop and Esc stops the walk.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use tairix_abi::{CapabilityId, Errno};
use tairix_log::Sink;
use tairix_sandbox::decode::{
    self, ContainerFormat, ContainerSummary, DecodeFailure, DisasmWindow, InsnRecord, Isa,
    ManifestInfo, Region, RegionKind, SymbolRecord, MAX_WINDOW_INSNS,
};
use tairix_sandbox::{Launcher, ParserSandbox, SandboxError};

use crate::fs::Fs;
use crate::view_text::JobOutcome;

/// Code bytes fetched per disassembly request: comfortably more than the
/// longest instruction times the tallest screen, so one read serves one
/// window.
const WINDOW_BYTES: usize = 4096;

/// Instructions decoded per goto/search walk tick — the bound that keeps
/// a tick's sandbox round-trip small so the key loop stays responsive.
const WALK_CHUNK: u32 = 512;

/// Bytes before the current top a fixed-length ISA scroll-up walk may
/// start from when no nearer anchor is recorded; variable-length decoding
/// re-aligns well within this distance.
const BACKSCAN_BYTES: u64 = 256;

/// Most resynchronisation anchors kept per region; past the cap every
/// second anchor is dropped, coarsening the walk start honestly rather
/// than growing without bound.
const ANCHOR_CAP: usize = 1024;

/// Most chunks one scroll-up walk may decode before it lands on the
/// nearest anchor instead — the bound that keeps a resynchronisation
/// finite even against a pathological region.
const MAX_WALK_CHUNKS: usize = 64;

/// Everything the viewer asks of the sandboxed decode service.
///
/// Object-safe so the app threads one `&mut dyn Decode` the way it
/// threads `&mut dyn Fs`; the production implementation is the
/// [`ParserSandbox`] over the program's own re-spawned worker, and the
/// host tests use the same seam over the in-process loopback worker.
pub trait Decode {
    /// The container summary of `image` (see [`decode::container_summary`]).
    ///
    /// # Errors
    ///
    /// The [`DecodeFailure`] the caller-side helper reports.
    fn container_summary(&mut self, image: &[u8]) -> Result<ContainerSummary, DecodeFailure>;

    /// The manifest summary of `manifest` (see [`decode::manifest_summary`]).
    ///
    /// # Errors
    ///
    /// The [`DecodeFailure`] the caller-side helper reports.
    fn manifest_summary(&mut self, manifest: &[u8]) -> Result<ManifestInfo, DecodeFailure>;

    /// One disassembled window of `code` (see [`decode::disassemble`]).
    ///
    /// # Errors
    ///
    /// The [`DecodeFailure`] the caller-side helper reports.
    fn disassemble(
        &mut self,
        isa: Isa,
        address: u64,
        depth: u32,
        max_insns: u32,
        code: &[u8],
    ) -> Result<DisasmWindow, DecodeFailure>;
}

impl<L: Launcher, S: Sink> Decode for ParserSandbox<L, S> {
    fn container_summary(&mut self, image: &[u8]) -> Result<ContainerSummary, DecodeFailure> {
        decode::container_summary(self, image)
    }

    fn manifest_summary(&mut self, manifest: &[u8]) -> Result<ManifestInfo, DecodeFailure> {
        decode::manifest_summary(self, manifest)
    }

    fn disassemble(
        &mut self,
        isa: Isa,
        address: u64,
        depth: u32,
        max_insns: u32,
        code: &[u8],
    ) -> Result<DisasmWindow, DecodeFailure> {
        decode::disassemble(self, isa, address, depth, max_insns, code)
    }
}

/// Why the disassembly viewer could not serve: the filesystem refused a
/// read, or the sandboxed decode failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisasmError {
    /// A window read through the [`Fs`] seam was refused.
    Fs(Errno),
    /// The sandboxed decode failed (refusal, crash, or a reply that
    /// could not be believed).
    Decode(DecodeFailure),
}

/// One line the viewer could not produce, in words for the message line.
#[must_use]
pub fn describe(error: DisasmError) -> String {
    match error {
        DisasmError::Fs(errno) => format!("read failed: {errno:?}"),
        DisasmError::Decode(DecodeFailure::Refused(refusal)) => match refusal {
            decode::DecodeRefusal::UnrecognisedContainer => {
                String::from("not a recognised executable")
            }
            decode::DecodeRefusal::MalformedContainer => String::from("malformed executable"),
            decode::DecodeRefusal::MalformedRequest => String::from("decode request refused"),
        },
        DisasmError::Decode(DecodeFailure::Sandbox(SandboxError::RequestTooLarge)) => {
            String::from("too large to decode")
        }
        DisasmError::Decode(DecodeFailure::Sandbox(_)) => {
            String::from("the decode sandbox is unavailable")
        }
        DisasmError::Decode(DecodeFailure::ReplyMalformed) => {
            String::from("the decode worker replied nonsense")
        }
    }
}

/// What the viewer is showing: a recognised container, a standalone
/// signed manifest, or a raw fragment the user forced open with a chosen
/// ISA.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisasmBody {
    /// A container `lib/binfmt` recognises; the summary page lists its
    /// regions and the code pane disassembles them.
    Container(ContainerSummary),
    /// A standalone signed manifest (`RXM1`); the summary page lists what
    /// it requests. The manifest travels beside an `rxe` image (a spawn
    /// argument), never inside it, so it is only ever viewed as its own
    /// file.
    Manifest(ManifestInfo),
    /// An unrecognised file force-opened as code at a user-chosen ISA.
    Raw,
}

/// Which page of the viewer is showing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisasmPane {
    /// The container/manifest summary page.
    Summary,
    /// One code region's disassembly.
    Code,
}

/// Where the code pane stands: the open region and the address (plus
/// wasm nesting depth) of the top display line.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CodePlace {
    /// Index into [`DisasmView::regions`].
    pub region: usize,
    /// Address of the first shown instruction.
    pub top: u64,
    /// The wasm nesting depth at `top` (0 for the fixed ISAs).
    pub depth: u32,
}

/// A goto walk's state: the destination and the walking front.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct GotoWalk {
    /// The address to land on (or the region end when past it).
    target: u64,
    /// The walking front's address.
    at: u64,
    /// The wasm nesting depth at `at`.
    depth: u32,
}

/// A search walk's state: the walking front and the starting place whose
/// own line never counts as a hit.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SearchWalk {
    /// The walking front's address.
    at: u64,
    /// The wasm nesting depth at `at`.
    depth: u32,
    /// Hits at or before this address are the current place, skipped.
    from: u64,
}

/// A live goto/search walk, advanced one bounded chunk per tick.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum DisasmJob {
    /// Walking forward to a target address.
    Goto(GotoWalk),
    /// Scanning instruction text for the stored needle.
    Search(SearchWalk),
}

/// One display line of the code pane: a symbol label or an instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisasmRow {
    /// The rendered text.
    pub text: String,
    /// True for a `<symbol>:` label line (rendered emphasised).
    pub is_label: bool,
}

/// The disassembly viewer's whole state: the decoded body, the summary
/// cursor, the code pane's place with its visit history and
/// resynchronisation anchors, the last rendered rows, and the live job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisasmView {
    /// The viewed file's path (the box title and error subject).
    pub path: String,
    /// Apparent byte size of the viewed file.
    pub size: u64,
    /// What is being shown.
    pub body: DisasmBody,
    /// The container's regions, or the one synthetic whole-file region in
    /// raw mode; empty for a manifest.
    pub regions: Vec<Region>,
    /// The container's symbols, address-sorted at open.
    pub symbols: Vec<SymbolRecord>,
    /// The user's manual ISA choice (raw mode always has one; a container
    /// with no machine field gets one before its first region opens; `I`
    /// overrides any).
    pub isa_choice: Option<Isa>,
    /// Which page is showing.
    pub pane: DisasmPane,
    /// The summary page's selected region row.
    pub sum_cursor: usize,
    /// The summary page's first shown line.
    pub sum_scroll: usize,
    /// The code pane's place; meaningful while `pane` is `Code`.
    pub place: CodePlace,
    /// Known `(address, depth)` resynchronisation anchors of the open
    /// region, address-sorted; scroll-up and goto walk forward from the
    /// nearest one.
    anchors: Vec<(u64, u32)>,
    /// The last refreshed display rows of the code pane.
    pub rows: Vec<DisasmRow>,
    /// The `(region, top, depth, rows)` the current `rows` were decoded
    /// for; `None` forces the next refresh to decode.
    window_key: Option<(usize, u64, u32, usize)>,
    /// True when the window reached the region's last instruction.
    pub at_end: bool,
    /// Interior height of the last refresh, for page-sized jumps.
    pub viewport_rows: usize,
    /// The live goto/search walk, if any.
    job: Option<DisasmJob>,
    /// The last search needle (lowercased), for `n`.
    needle: Option<String>,
    /// The last search hit address, for the message line.
    pub last_hit: Option<u64>,
}

impl DisasmView {
    /// Open `path` (its full `bytes`) as a recognised container or a
    /// standalone manifest, through the sandboxed decode.
    ///
    /// # Errors
    ///
    /// [`DisasmError::Decode`] when the sandboxed decode refuses or
    /// fails; the caller falls back to the hex viewer with the
    /// [`describe`]d notice.
    pub fn open(
        decode: &mut dyn Decode,
        path: &str,
        size: u64,
        bytes: &[u8],
    ) -> Result<Self, DisasmError> {
        let (body, regions, symbols) = if is_manifest_head(bytes) {
            let info = decode
                .manifest_summary(bytes)
                .map_err(DisasmError::Decode)?;
            (DisasmBody::Manifest(info), Vec::new(), Vec::new())
        } else {
            let summary = decode
                .container_summary(bytes)
                .map_err(DisasmError::Decode)?;
            let regions = summary.regions.clone();
            let mut symbols = summary.symbols.clone();
            symbols.sort_by_key(|symbol| symbol.addr);
            (DisasmBody::Container(summary), regions, symbols)
        };
        let mut view = Self::assemble(path, size, body, regions, symbols, None);
        // The default selection is the first code region, so Enter's
        // first meaning is "show me the code", not a data-section jump.
        view.sum_cursor = view
            .regions
            .iter()
            .position(|region| region.kind == RegionKind::Code)
            .unwrap_or(0);
        Ok(view)
    }

    /// Open `path` as a raw code fragment at the user-chosen `isa`,
    /// starting at byte offset `top` (the place a hex view handed over).
    #[must_use]
    pub fn raw(path: &str, size: u64, isa: Isa, top: u64) -> Self {
        let region = Region {
            name: String::from("raw"),
            kind: RegionKind::Code,
            addr: 0,
            file_offset: 0,
            file_size: size,
            mem_size: size,
            read: true,
            write: false,
            execute: true,
        };
        let mut view = Self::assemble(
            path,
            size,
            DisasmBody::Raw,
            vec![region],
            Vec::new(),
            Some(isa),
        );
        view.pane = DisasmPane::Code;
        view.place = CodePlace {
            region: 0,
            top: top.min(size.saturating_sub(1)),
            depth: 0,
        };
        view
    }

    fn assemble(
        path: &str,
        size: u64,
        body: DisasmBody,
        regions: Vec<Region>,
        symbols: Vec<SymbolRecord>,
        isa_choice: Option<Isa>,
    ) -> Self {
        Self {
            path: String::from(path),
            size,
            body,
            regions,
            symbols,
            isa_choice,
            pane: DisasmPane::Summary,
            sum_cursor: 0,
            sum_scroll: 0,
            place: CodePlace {
                region: 0,
                top: 0,
                depth: 0,
            },
            anchors: Vec::new(),
            rows: Vec::new(),
            window_key: None,
            at_end: false,
            viewport_rows: 0,
            job: None,
            needle: None,
            last_hit: None,
        }
    }

    /// The ISA the code pane decodes with: the manual choice first, then
    /// the container's own machine field.
    #[must_use]
    pub fn isa(&self) -> Option<Isa> {
        self.isa_choice.or(match &self.body {
            DisasmBody::Container(summary) => summary.isa,
            DisasmBody::Manifest(_) | DisasmBody::Raw => None,
        })
    }

    /// True while a goto/search walk is live.
    #[must_use]
    pub fn ticking(&self) -> bool {
        self.job.is_some()
    }

    /// Stop the live walk in place.
    pub fn cancel_job(&mut self) {
        self.job = None;
    }
}

impl DisasmView {
    /// The summary page's lines: each row's text plus the region index it
    /// selects (headers and the manifest body select nothing). Pure over
    /// the decoded state, so the render and the tests read one builder.
    #[must_use]
    pub fn summary_rows(&self) -> Vec<(String, Option<usize>)> {
        let mut rows = Vec::new();
        match &self.body {
            DisasmBody::Container(summary) => {
                let format = match summary.format {
                    ContainerFormat::Rxe => "rxe",
                    ContainerFormat::Elf64 => "elf64",
                    ContainerFormat::Wasm => "wasm",
                };
                let isa = match self.isa() {
                    Some(isa) => isa_name(isa),
                    None => "system (choose on open)",
                };
                rows.push((
                    format!("format {format}  isa {isa}  entry {:#x}", summary.entry),
                    None,
                ));
                if summary.format == ContainerFormat::Rxe {
                    rows.push((
                        String::from("manifest: carried beside the image, not embedded"),
                        None,
                    ));
                }
                let truncated = if summary.regions_truncated {
                    " (list truncated)"
                } else {
                    ""
                };
                rows.push((format!("regions: {}{truncated}", self.regions.len()), None));
                for (index, region) in self.regions.iter().enumerate() {
                    rows.push((region_row(region), Some(index)));
                }
                let symbols_truncated = if summary.symbols_truncated {
                    " (list truncated)"
                } else {
                    ""
                };
                rows.push((
                    format!("symbols: {}{symbols_truncated}", self.symbols.len()),
                    None,
                ));
            }
            DisasmBody::Manifest(info) => {
                rows.push((String::from("signed manifest (RXM1)"), None));
                rows.push((format!("abi version {}", info.abi_version), None));
                rows.push((
                    format!("requested capabilities: {}", info.capabilities.len()),
                    None,
                ));
                for &raw in &info.capabilities {
                    let text = CapabilityId::from_raw(raw)
                        .ok()
                        .and_then(CapabilityId::name)
                        .map_or_else(|| format!("  capability #{raw}"), |n| format!("  {n}"));
                    rows.push((text, None));
                }
            }
            DisasmBody::Raw => {
                // Raw mode opens straight into the code pane; the summary
                // page never shows.
            }
        }
        rows
    }

    /// Move the summary cursor by `delta` region rows, clamped.
    pub fn move_summary_cursor(&mut self, delta: isize) {
        let count = self.regions.len();
        if count == 0 {
            return;
        }
        let current = isize::try_from(self.sum_cursor).unwrap_or(isize::MAX);
        let moved = current.saturating_add(delta).max(0);
        self.sum_cursor = usize::try_from(moved).unwrap_or(usize::MAX).min(count - 1);
    }

    /// The summary-selected region, if the page has any.
    #[must_use]
    pub fn selected_region(&self) -> Option<&Region> {
        self.regions.get(self.sum_cursor)
    }

    /// Open code region `index` at its first instruction. The caller has
    /// already ensured an ISA is chosen ([`DisasmView::isa`]).
    pub fn enter_region(&mut self, index: usize) {
        let Some(region) = self.regions.get(index) else {
            return;
        };
        self.pane = DisasmPane::Code;
        self.place = CodePlace {
            region: index,
            top: region.addr,
            depth: 0,
        };
        self.anchors.clear();
        self.anchors.push((region.addr, 0));
        self.rows.clear();
        self.window_key = None;
        self.at_end = false;
        self.job = None;
        self.last_hit = None;
    }

    /// Leave the code pane for the summary page (raw mode has no summary;
    /// the caller exits the viewer instead).
    pub fn leave_code(&mut self) {
        self.pane = DisasmPane::Summary;
        self.job = None;
    }

    /// The open region, when the code pane is showing.
    #[must_use]
    pub fn open_region(&self) -> Option<&Region> {
        self.regions.get(self.place.region)
    }

    /// The current file offset of the code pane's top line (the place a
    /// hex/text switch keeps), or 0 from the summary page.
    #[must_use]
    pub fn switch_offset(&self) -> u64 {
        if self.pane != DisasmPane::Code {
            return 0;
        }
        match self.open_region() {
            Some(region) => region
                .file_offset
                .saturating_add(self.place.top.saturating_sub(region.addr)),
            None => 0,
        }
    }

    /// Keep the summary scroll containing the cursor's row.
    fn clamp_summary_scroll(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        let line = self
            .summary_rows()
            .iter()
            .position(|(_, region)| *region == Some(self.sum_cursor))
            .unwrap_or(0);
        if line < self.sum_scroll {
            self.sum_scroll = line;
        }
        if line >= self.sum_scroll + rows {
            self.sum_scroll = line + 1 - rows;
        }
    }
}

impl DisasmView {
    /// Re-decode the page for the frame about to render: clamp the
    /// summary scroll, or fetch and disassemble the code window when the
    /// place or the viewport changed since the last refresh.
    ///
    /// # Errors
    ///
    /// [`DisasmError`]: the window read or the sandboxed decode failed;
    /// the caller closes the viewer with the reason rather than showing a
    /// stale page as live.
    pub fn refresh(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        rows: usize,
    ) -> Result<(), DisasmError> {
        self.viewport_rows = rows.max(1);
        match self.pane {
            DisasmPane::Summary => {
                self.clamp_summary_scroll(rows);
                let lines = self.summary_rows().len();
                self.sum_scroll = self.sum_scroll.min(lines.saturating_sub(1));
                Ok(())
            }
            DisasmPane::Code => self.refresh_window(fs, decode),
        }
    }

    /// Fetch and decode the current window unless the cached one already
    /// matches the place and viewport.
    fn refresh_window(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
    ) -> Result<(), DisasmError> {
        let key = (
            self.place.region,
            self.place.top,
            self.place.depth,
            self.viewport_rows,
        );
        if self.window_key == Some(key) {
            return Ok(());
        }
        let Some(isa) = self.isa() else {
            // No ISA chosen yet: nothing decodable to show (the app opens
            // the chooser before entering a region, so this is the empty
            // frame between those steps, never a lasting state).
            self.rows.clear();
            self.window_key = Some(key);
            return Ok(());
        };
        let max = u32::try_from(self.viewport_rows.min(MAX_WINDOW_INSNS)).unwrap_or(1);
        let window = self.decode_at(fs, decode, isa, self.place.top, self.place.depth, max)?;
        let end = self.open_region().map_or(0, region_code_end);
        self.at_end = window.next_address >= end;
        self.rows = build_rows(&window.insns, &self.symbols);
        self.record_anchor(self.place.top, self.place.depth);
        if window.next_address < end {
            self.record_anchor(window.next_address, window.next_depth);
        }
        self.window_key = Some(key);
        Ok(())
    }

    /// One sandboxed decode of up to `max_insns` instructions at `addr`,
    /// over a bounded window read from the open region's file bytes.
    fn decode_at(
        &self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        isa: Isa,
        addr: u64,
        depth: u32,
        max_insns: u32,
    ) -> Result<DisasmWindow, DisasmError> {
        let Some(region) = self.open_region() else {
            // No open region: nothing to decode (fail closed, not panic).
            return Ok(DisasmWindow {
                insns: Vec::new(),
                next_address: addr,
                next_depth: depth,
            });
        };
        let end = region_code_end(region);
        if addr < region.addr || addr >= end {
            return Ok(DisasmWindow {
                insns: Vec::new(),
                next_address: addr,
                next_depth: depth,
            });
        }
        let offset = region.file_offset.saturating_add(addr - region.addr);
        let len = usize::try_from((end - addr).min(WINDOW_BYTES as u64)).unwrap_or(WINDOW_BYTES);
        let mut bytes = vec![0_u8; len];
        let mut filled = 0;
        while filled < len {
            let read = fs
                .read(&self.path, offset + filled as u64, &mut bytes[filled..])
                .map_err(DisasmError::Fs)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        bytes.truncate(filled);
        decode
            .disassemble(isa, addr, depth, max_insns, &bytes)
            .map_err(DisasmError::Decode)
    }

    /// Remember `(addr, depth)` as a resynchronisation anchor of the open
    /// region, keeping the list sorted, deduplicated, and bounded.
    fn record_anchor(&mut self, addr: u64, depth: u32) {
        if let Err(index) = self.anchors.binary_search_by_key(&addr, |&(a, _)| a) {
            self.anchors.insert(index, (addr, depth));
        }
        if self.anchors.len() > ANCHOR_CAP {
            // Coarsen by keeping every second anchor; walks start a little
            // farther back but stay correct.
            let mut keep = false;
            self.anchors.retain(|_| {
                keep = !keep;
                keep
            });
        }
    }

    /// The nearest recorded anchor strictly below `addr`, or the fixed-ISA
    /// backscan start, or the region start.
    fn walk_start(&self, addr: u64) -> (u64, u32) {
        let Some(region) = self.open_region() else {
            return (addr, 0);
        };
        let anchor = match self.anchors.binary_search_by_key(&addr, |&(a, _)| a) {
            Ok(index) | Err(index) => index
                .checked_sub(1)
                .and_then(|below| self.anchors.get(below))
                .copied(),
        };
        let anchor = anchor.unwrap_or((region.addr, 0));
        if self.isa() == Some(Isa::Wasm) {
            // Only a recorded anchor carries an honest wasm depth.
            return anchor;
        }
        // A fixed-length ISA re-aligns quickly, so a nearer synthetic
        // start beats a distant anchor.
        let backscan = addr.saturating_sub(BACKSCAN_BYTES).max(region.addr);
        if backscan > anchor.0 {
            (backscan, 0)
        } else {
            anchor
        }
    }

    /// Scroll the code pane down by `lines` instructions.
    ///
    /// # Errors
    ///
    /// [`DisasmError`] from the window read or decode; the place is kept.
    pub fn scroll_down(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        lines: usize,
    ) -> Result<(), DisasmError> {
        let Some(isa) = self.isa() else {
            return Ok(());
        };
        let end = self.open_region().map_or(0, region_code_end);
        let max = u32::try_from(lines.clamp(1, MAX_WINDOW_INSNS)).unwrap_or(1);
        let w = self.decode_at(fs, decode, isa, self.place.top, self.place.depth, max)?;
        if w.insns.is_empty() {
            self.at_end = true;
            return Ok(());
        }
        if w.insns.len() == max as usize && w.next_address < end {
            // A full step: the decode's own next-window hand-off is the
            // exact new top (with its wasm depth threaded through).
            self.record_anchor(w.next_address, w.next_depth);
            self.place.top = w.next_address;
            self.place.depth = w.next_depth;
            self.window_key = None;
            return Ok(());
        }
        // The region's tail: land on its last instruction.
        let last = w.insns.len() - 1;
        if last == 0 {
            self.at_end = true;
            return Ok(());
        }
        let depth = self.depth_at_step(fs, decode, isa, last)?;
        self.place.top = w.insns[last].address;
        self.place.depth = depth;
        self.window_key = None;
        Ok(())
    }

    /// The wasm depth after `steps` instructions from the current place
    /// (0 for the fixed ISAs, which carry no nesting).
    fn depth_at_step(
        &self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        isa: Isa,
        steps: usize,
    ) -> Result<u32, DisasmError> {
        if isa != Isa::Wasm || steps == 0 {
            return Ok(if isa == Isa::Wasm {
                self.place.depth
            } else {
                0
            });
        }
        let max = u32::try_from(steps.min(MAX_WINDOW_INSNS)).unwrap_or(1);
        let w = self.decode_at(fs, decode, isa, self.place.top, self.place.depth, max)?;
        Ok(w.next_depth)
    }

    /// Scroll the code pane up by `lines` instructions: walk forward from
    /// the nearest anchor to find the instructions preceding the top,
    /// bounded by a fixed chunk budget.
    ///
    /// # Errors
    ///
    /// [`DisasmError`] from the window read or decode; the place is kept.
    pub fn scroll_up(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        lines: usize,
    ) -> Result<(), DisasmError> {
        let Some(isa) = self.isa() else {
            return Ok(());
        };
        let Some(region) = self.open_region() else {
            return Ok(());
        };
        let start = region.addr;
        if self.place.top <= start || lines == 0 {
            return Ok(());
        }
        let top = self.place.top;
        let (mut at, mut depth) = self.walk_start(top);
        // Two sliding chunks bracket the top: the predecessors are in
        // them once a chunk reaches it.
        let mut previous: Option<(u64, u32, DisasmWindow)> = None;
        for _ in 0..MAX_WALK_CHUNKS {
            let w = self.decode_at(fs, decode, isa, at, depth, WALK_CHUNK)?;
            if w.insns.is_empty() {
                break;
            }
            let reached =
                w.next_address >= top || w.insns.last().is_some_and(|insn| insn.address >= top);
            if reached {
                let mut candidates: Vec<(u64, u32, usize, u64)> = Vec::new();
                if let Some((p_at, p_depth, p_w)) = &previous {
                    for (index, insn) in p_w.insns.iter().enumerate() {
                        if insn.address < top {
                            candidates.push((*p_at, *p_depth, index, insn.address));
                        }
                    }
                }
                for (index, insn) in w.insns.iter().enumerate() {
                    if insn.address < top {
                        candidates.push((at, depth, index, insn.address));
                    }
                }
                let Some(&(c_at, c_depth, c_index, c_addr)) =
                    candidates.get(candidates.len().saturating_sub(lines))
                else {
                    // Fewer predecessors than asked: land on the first
                    // known one (or the walk start).
                    self.move_top(at, depth);
                    return Ok(());
                };
                let new_depth = if isa == Isa::Wasm && c_index > 0 {
                    let max = u32::try_from(c_index.min(MAX_WINDOW_INSNS)).unwrap_or(1);
                    let sub = self.decode_at(fs, decode, isa, c_at, c_depth, max)?;
                    sub.next_depth
                } else if isa == Isa::Wasm {
                    c_depth
                } else {
                    0
                };
                self.move_top(c_addr, new_depth);
                return Ok(());
            }
            self.record_anchor(w.next_address, w.next_depth);
            previous = Some((at, depth, w.clone()));
            at = w.next_address;
            depth = w.next_depth;
        }
        // The walk budget ran out: land on the nearest anchor — an honest
        // move up, if a coarse one.
        self.move_top(at.min(top), if at <= top { depth } else { 0 });
        Ok(())
    }

    /// Set the code pane's top line, invalidating the cached window.
    fn move_top(&mut self, addr: u64, depth: u32) {
        self.place.top = addr;
        self.place.depth = depth;
        self.at_end = false;
        self.window_key = None;
    }

    /// Jump to the open region's first instruction.
    pub fn go_home(&mut self) {
        if let Some(region) = self.open_region() {
            let start = region.addr;
            self.move_top(start, 0);
        }
    }
}

impl DisasmView {
    /// Override the decode ISA (`I`). The wasm nesting baseline restarts
    /// at the current top — a depth from another ISA's stream would be a
    /// fabrication.
    pub fn set_isa(&mut self, isa: Isa) {
        self.isa_choice = Some(isa);
        if self.pane == DisasmPane::Code {
            self.place.depth = 0;
            self.anchors.clear();
            if let Some(region) = self.open_region() {
                let start = region.addr;
                self.anchors.push((start, 0));
            }
            self.record_anchor(self.place.top, 0);
            self.window_key = None;
            self.at_end = false;
        }
        self.job = None;
    }

    /// Start a walk to `target`: the code region containing it (the open
    /// one preferred) is entered and the walk lands on the instruction
    /// covering the address. Returns `false` — with nothing changed —
    /// when no code region contains `target`.
    pub fn start_goto(&mut self, target: u64) -> bool {
        let containing = |region: &Region| {
            region.kind == RegionKind::Code
                && region.addr <= target
                && target < region_code_end(region)
        };
        let index = if self.open_region().is_some_and(containing) {
            self.place.region
        } else {
            let Some(index) = self.regions.iter().position(containing) else {
                return false;
            };
            self.switch_region(index);
            index
        };
        debug_assert_eq!(index, self.place.region);
        let (at, depth) = self.walk_start(target.saturating_add(1));
        self.job = Some(DisasmJob::Goto(GotoWalk { target, at, depth }));
        true
    }

    /// Walk to the open region's end, landing on its final page.
    pub fn go_end(&mut self) {
        let Some(region) = self.open_region() else {
            return;
        };
        let end = region_code_end(region);
        let (at, depth) = self.walk_start(end);
        self.job = Some(DisasmJob::Goto(GotoWalk {
            target: u64::MAX,
            at,
            depth,
        }));
    }

    /// Move the place into region `index` (a goto crossing regions),
    /// resetting the per-region anchors.
    fn switch_region(&mut self, index: usize) {
        let Some(region) = self.regions.get(index) else {
            return;
        };
        let start = region.addr;
        self.place = CodePlace {
            region: index,
            top: start,
            depth: 0,
        };
        self.anchors.clear();
        self.anchors.push((start, 0));
        self.window_key = None;
        self.at_end = false;
        self.sum_cursor = index;
    }

    /// Start a mnemonic/operand text search forward from the current top
    /// within the open region. Returns `false` for an empty needle.
    pub fn start_search(&mut self, typed: &str) -> bool {
        let needle = typed.trim().to_lowercase();
        if needle.is_empty() {
            return false;
        }
        self.needle = Some(needle);
        self.begin_search();
        true
    }

    /// Repeat the last search past the current top (`n`). Returns `false`
    /// when no search has been made.
    pub fn search_next(&mut self) -> bool {
        if self.needle.is_none() {
            return false;
        }
        self.begin_search();
        true
    }

    fn begin_search(&mut self) {
        self.job = Some(DisasmJob::Search(SearchWalk {
            at: self.place.top,
            depth: self.place.depth,
            from: self.place.top,
        }));
    }

    /// Advance the live walk by one bounded chunk.
    ///
    /// # Errors
    ///
    /// [`DisasmError`]: the walk's window read or decode failed; the job
    /// is cancelled and the place kept.
    pub fn tick(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
    ) -> Result<JobOutcome, DisasmError> {
        let Some(job) = self.job else {
            return Ok(JobOutcome::Pending);
        };
        let Some(isa) = self.isa() else {
            self.job = None;
            return Ok(JobOutcome::NotFound);
        };
        let end = self.open_region().map_or(0, region_code_end);
        let outcome = match job {
            DisasmJob::Goto(walk) => self.tick_goto(fs, decode, isa, end, walk),
            DisasmJob::Search(walk) => self.tick_search(fs, decode, isa, end, walk),
        };
        if outcome.is_err() {
            self.job = None;
        }
        outcome
    }

    /// One goto chunk: land on the instruction covering the target, or
    /// walk on, or land on the region's final page past the end.
    fn tick_goto(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        isa: Isa,
        end: u64,
        walk: GotoWalk,
    ) -> Result<JobOutcome, DisasmError> {
        let GotoWalk { target, at, depth } = walk;
        let w = self.decode_at(fs, decode, isa, at, depth, WALK_CHUNK)?;
        if w.insns.is_empty() {
            self.job = None;
            return Ok(JobOutcome::NotFound);
        }
        let covering = w
            .insns
            .iter()
            .position(|insn| covers(insn, target) || insn.address >= target);
        if let Some(index) = covering {
            let depth_at = self.depth_within(fs, decode, isa, at, depth, index)?;
            self.move_top(w.insns[index].address, depth_at);
            self.job = None;
            return Ok(JobOutcome::Moved);
        }
        if w.next_address >= end {
            // Past the region's last instruction: show its final page.
            let index = w.insns.len().saturating_sub(self.viewport_rows.max(1));
            let depth_at = self.depth_within(fs, decode, isa, at, depth, index)?;
            self.move_top(w.insns[index].address, depth_at);
            self.at_end = true;
            self.job = None;
            return Ok(JobOutcome::PastEnd);
        }
        self.record_anchor(w.next_address, w.next_depth);
        self.job = Some(DisasmJob::Goto(GotoWalk {
            target,
            at: w.next_address,
            depth: w.next_depth,
        }));
        Ok(JobOutcome::Pending)
    }

    /// One search chunk: land on the first instruction past the walk's
    /// start whose text carries the needle, or walk on, or report not
    /// found at the region end.
    fn tick_search(
        &mut self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        isa: Isa,
        end: u64,
        walk: SearchWalk,
    ) -> Result<JobOutcome, DisasmError> {
        let SearchWalk { at, depth, from } = walk;
        let Some(needle) = self.needle.clone() else {
            self.job = None;
            return Ok(JobOutcome::NotFound);
        };
        let w = self.decode_at(fs, decode, isa, at, depth, WALK_CHUNK)?;
        let hit = w.insns.iter().enumerate().find(|(_, insn)| {
            insn.address > from
                && format!("{} {}", insn.mnemonic, insn.operands)
                    .to_lowercase()
                    .contains(needle.as_str())
        });
        if let Some((index, insn)) = hit {
            let address = insn.address;
            let depth_at = self.depth_within(fs, decode, isa, at, depth, index)?;
            self.move_top(address, depth_at);
            self.last_hit = Some(address);
            self.job = None;
            return Ok(JobOutcome::Moved);
        }
        if w.insns.is_empty() || w.next_address >= end {
            self.job = None;
            return Ok(JobOutcome::NotFound);
        }
        self.record_anchor(w.next_address, w.next_depth);
        self.job = Some(DisasmJob::Search(SearchWalk {
            at: w.next_address,
            depth: w.next_depth,
            from,
        }));
        Ok(JobOutcome::Pending)
    }

    /// The wasm depth `steps` instructions after `(at, depth)` (0 for the
    /// fixed ISAs).
    fn depth_within(
        &self,
        fs: &mut dyn Fs,
        decode: &mut dyn Decode,
        isa: Isa,
        at: u64,
        depth: u32,
        steps: usize,
    ) -> Result<u32, DisasmError> {
        if isa != Isa::Wasm {
            return Ok(0);
        }
        if steps == 0 {
            return Ok(depth);
        }
        let max = u32::try_from(steps.min(MAX_WINDOW_INSNS)).unwrap_or(1);
        let w = self.decode_at(fs, decode, isa, at, depth, max)?;
        Ok(w.next_depth)
    }
}

/// Whether `insn`'s encoding covers `target`.
fn covers(insn: &InsnRecord, target: u64) -> bool {
    target >= insn.address && target - insn.address < u64::from(insn.length)
}

/// First address past a region's decodable code bytes (the in-memory
/// tail past `file_size` is zero fill, not code).
fn region_code_end(region: &Region) -> u64 {
    region.addr.saturating_add(region.file_size)
}

/// Build the code pane's display rows: a `<symbol>:` label line before
/// the instruction starting a known symbol, then the instruction row
/// with its encoding bytes and a symbolised branch target.
fn build_rows(insns: &[InsnRecord], symbols: &[SymbolRecord]) -> Vec<DisasmRow> {
    let mut rows = Vec::new();
    for insn in insns {
        let at = symbols.binary_search_by_key(&insn.address, |symbol| symbol.addr);
        if let Ok(mut index) = at {
            // Every symbol at this address labels the line (the search
            // may land mid-run of equal addresses).
            while index > 0 && symbols[index - 1].addr == insn.address {
                index -= 1;
            }
            for symbol in symbols[index..]
                .iter()
                .take_while(|symbol| symbol.addr == insn.address)
            {
                rows.push(DisasmRow {
                    text: format!("{:016x} <{}>:", symbol.addr, symbol.name),
                    is_label: true,
                });
            }
        }
        rows.push(DisasmRow {
            text: insn_row(insn, symbols),
            is_label: false,
        });
    }
    rows
}

/// Encoding bytes shown per instruction row; a longer encoding elides
/// with `…` (the full length is decoded regardless).
const SHOWN_BYTES: usize = 8;

/// One instruction's display row.
fn insn_row(insn: &InsnRecord, symbols: &[SymbolRecord]) -> String {
    let mut bytes = String::new();
    for byte in insn.bytes.iter().take(SHOWN_BYTES) {
        let _ = core::fmt::Write::write_fmt(&mut bytes, format_args!("{byte:02x} "));
    }
    if insn.bytes.len() > SHOWN_BYTES {
        bytes.pop();
        bytes.push('…');
    }
    let target = insn
        .branch_target
        .and_then(|target| symbolise(target, symbols))
        .unwrap_or_default();
    let operands = if insn.operands.is_empty() {
        String::new()
    } else {
        format!(" {}", insn.operands)
    };
    format!(
        "{:8x}:  {bytes:<width$} {}{operands}{target}",
        insn.address,
        insn.mnemonic,
        width = SHOWN_BYTES * 3,
    )
}

/// The `<name>` / `<name+0xoff>` a branch target lands in, when a symbol
/// covers it.
fn symbolise(target: u64, symbols: &[SymbolRecord]) -> Option<String> {
    let index = match symbols.binary_search_by_key(&target, |symbol| symbol.addr) {
        Ok(index) => index,
        Err(0) => return None,
        Err(index) => index - 1,
    };
    let symbol = &symbols[index];
    let extent = symbol.size.max(1);
    if target < symbol.addr || target - symbol.addr >= extent {
        return None;
    }
    if target == symbol.addr {
        Some(format!(" <{}>", symbol.name))
    } else {
        Some(format!(" <{}+{:#x}>", symbol.name, target - symbol.addr))
    }
}

/// One region's summary line: kind, name, address, file extent, memory
/// size, and permissions, in fixed columns.
fn region_row(region: &Region) -> String {
    let kind = match region.kind {
        RegionKind::Code => "code",
        RegionKind::Data => "data",
    };
    let name = if region.name.is_empty() {
        "-"
    } else {
        region.name.as_str()
    };
    format!(
        "  {kind}  {:<20} addr {:#010x}  file {:#x}+{:#x}  mem {:#x}  {}{}{}",
        name,
        region.addr,
        region.file_offset,
        region.file_size,
        region.mem_size,
        if region.read { 'r' } else { '-' },
        if region.write { 'w' } else { '-' },
        if region.execute { 'x' } else { '-' },
    )
}

/// The display name of an ISA (also the chooser's vocabulary).
#[must_use]
pub fn isa_name(isa: Isa) -> &'static str {
    match isa {
        Isa::X86_64 => "x86-64",
        Isa::Aarch64 => "aarch64",
        Isa::Riscv64 => "riscv64",
        Isa::Wasm => "wasm",
    }
}

/// Whether a file head is a standalone signed manifest (`RXM1`): the
/// same shallow magic recognition `tairix_binfmt::detect` performs for
/// the containers — routing only, never a parse.
#[must_use]
pub fn is_manifest_head(head: &[u8]) -> bool {
    head.len() >= 4 && head[0..4] == tairix_abi::MANIFEST_MAGIC.to_le_bytes()
}

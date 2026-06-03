//! Behaviour tests for the `top` viewer: the model's selection/scroll/scope
//! logic and the renderer/loop driven over in-memory `sysinfo` and tty
//! channels (`AGENTS.md` §7).

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use rustos_abi::sysinfo::{
    ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId, SysinfoRequestHeader,
};
use rustos_abi::Errno;
use rustos_curses::{Event, Screen, Size, Tty};
use rustos_procinfo::Transport;
use rustos_termcap::TermType;

use crate::app::{list_capacity, render, run};
use crate::error::TopError;
use crate::model::{Action, Model, Scope};

// ---- Fixtures --------------------------------------------------------------

/// An in-memory `sysinfod` stand-in answering process-list queries from a
/// fixed record set, decoding the request exactly as the real service.
struct FakeService {
    records: Vec<ProcessRecord>,
    deny_global: bool,
    seen: RefCell<Vec<SysinfoQueryId>>,
}

impl FakeService {
    fn new(records: Vec<ProcessRecord>) -> Self {
        Self {
            records,
            deny_global: false,
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for FakeService {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        let header = SysinfoRequestHeader::from_bytes(request)?;
        self.seen.borrow_mut().push(header.query);
        if self.deny_global && header.query == SysinfoQueryId::GLOBAL_PROCESS_LIST {
            return Err(Errno::PermissionDenied);
        }
        let payload = &request[SysinfoRequestHeader::WIRE_LEN
            ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
        let req = ProcessListRequest::from_bytes(payload)?;
        let offset = req.offset as usize;
        if offset >= self.records.len() {
            return Ok(Vec::new());
        }
        let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
        let mut out = Vec::with_capacity(take * ProcessRecord::WIRE_LEN);
        for record in &self.records[offset..offset + take] {
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }
}

/// An in-memory tty: queued input bytes, captured output bytes.
struct FakeTty {
    input: Vec<u8>,
    output: Vec<u8>,
}

impl FakeTty {
    fn with_input(bytes: &[u8]) -> Self {
        Self {
            input: bytes.to_vec(),
            output: Vec::new(),
        }
    }
}

impl Tty for FakeTty {
    fn write(&mut self, bytes: &[u8]) -> rustos_curses::Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self) -> rustos_curses::Result<Vec<u8>> {
        Ok(core::mem::take(&mut self.input))
    }
}

fn record(pid: u64, name: &[u8]) -> ProcessRecord {
    ProcessRecord::new(pid, 1, 1000, 1000, ProcessState::Running, 0, name).expect("record")
}

fn records(n: u64) -> Vec<ProcessRecord> {
    (1..=n).map(|pid| record(pid, b"proc")).collect()
}

/// Whether `haystack` contains the byte run `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---- Model -----------------------------------------------------------------

#[test]
fn an_empty_model_has_no_selection() {
    let model = Model::new(Scope::Own);
    assert_eq!(model.selected(), None);
    assert!(model.processes().is_empty());
}

#[test]
fn refresh_populates_and_selects_the_first_row() {
    let service = FakeService::new(vec![record(1, b"init"), record(2, b"shell")]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    assert_eq!(model.processes().len(), 2);
    assert_eq!(model.selected(), Some(0));
    assert_eq!(
        service.seen.borrow().as_slice(),
        &[SysinfoQueryId::SELF_PROCESS_LIST]
    );
}

#[test]
fn arrows_move_and_clamp_the_selection() {
    let service = FakeService::new(records(3));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(10);

    assert_eq!(model.handle_event(&Event::Up), Action::Ignore); // already at top
    assert_eq!(model.handle_event(&Event::Down), Action::Redraw);
    assert_eq!(model.selected(), Some(1));
    assert_eq!(model.handle_event(&Event::End), Action::Redraw);
    assert_eq!(model.selected(), Some(2));
    assert_eq!(model.handle_event(&Event::Down), Action::Ignore); // clamped at bottom
    assert_eq!(model.handle_event(&Event::Home), Action::Redraw);
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn the_selection_scrolls_to_stay_visible() {
    let service = FakeService::new(records(20));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(5);
    assert_eq!(model.scroll_top(), 0);

    model.handle_event(&Event::End);
    assert_eq!(model.selected(), Some(19));
    // The last row must be visible: top is clamped so the viewport ends at 19.
    assert_eq!(model.scroll_top(), 15);

    model.handle_event(&Event::Home);
    assert_eq!(model.scroll_top(), 0);
}

#[test]
fn page_keys_move_a_viewport_at_a_time() {
    let service = FakeService::new(records(20));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(5);
    model.handle_event(&Event::PageDown);
    assert_eq!(model.selected(), Some(5));
    model.handle_event(&Event::PageUp);
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn toggling_scope_asks_for_a_refresh_and_flips_the_view() {
    let mut model = Model::new(Scope::Own);
    assert_eq!(model.handle_event(&Event::Char('a')), Action::Refresh);
    assert_eq!(model.scope(), Scope::All);
    assert_eq!(model.handle_event(&Event::Char('a')), Action::Refresh);
    assert_eq!(model.scope(), Scope::Own);
}

#[test]
fn the_help_key_toggles_the_overlay() {
    let mut model = Model::new(Scope::Own);
    assert!(!model.help_visible());
    assert_eq!(model.handle_event(&Event::Char('?')), Action::Redraw);
    assert!(model.help_visible());
    assert_eq!(model.handle_event(&Event::Char('h')), Action::Redraw);
    assert!(!model.help_visible());
}

#[test]
fn quitting_is_reported() {
    let mut model = Model::new(Scope::Own);
    assert_eq!(model.handle_event(&Event::Char('q')), Action::Quit);
}

#[test]
fn an_unmapped_key_is_ignored() {
    let mut model = Model::new(Scope::Own);
    assert_eq!(model.handle_event(&Event::Tab), Action::Ignore);
}

#[test]
fn refresh_clamps_a_selection_past_a_shrunken_list() {
    let service = FakeService::new(records(10));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(10);
    model.handle_event(&Event::End);
    assert_eq!(model.selected(), Some(9));

    // The list shrinks under the cursor; refresh must clamp the selection.
    let smaller = FakeService::new(records(3));
    model.refresh(&smaller).expect("ok");
    assert_eq!(model.selected(), Some(2));
}

#[test]
fn a_denied_global_refresh_reports_permission_denied() {
    let mut service = FakeService::new(records(2));
    service.deny_global = true;
    let mut model = Model::new(Scope::All);
    assert_eq!(model.refresh(&service), Err(TopError::PermissionDenied));
}

// ---- Rendering -------------------------------------------------------------

#[test]
fn list_capacity_subtracts_header_and_footer() {
    assert_eq!(list_capacity(Size::new(24, 80)), 21);
    // A screen with no room for the list still reports zero, never underflows.
    assert_eq!(list_capacity(Size::new(2, 80)), 0);
}

#[test]
fn render_draws_the_title_header_and_rows() {
    let service = FakeService::new(vec![record(1, b"init"), record(2, b"shell")]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(10, 60),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");

    let out = screen.into_tty().output;
    assert!(contains(&out, b"RustOS top"));
    assert!(contains(&out, b"PID"));
    assert!(contains(&out, b"init"));
    assert!(contains(&out, b"shell"));
}

#[test]
fn render_shows_the_help_overlay_when_toggled() {
    let service = FakeService::new(records(3));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.handle_event(&Event::Char('?'));
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(16, 60),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    // The renderer skips unchanged blank cells, so only contiguous
    // non-space runs are guaranteed to appear verbatim in the byte stream.
    assert!(contains(&out, b"Keys"));
    assert!(contains(&out, b"refresh"));
}

#[test]
fn a_wide_process_name_does_not_break_rendering() {
    // A double-width (CJK) process name must render without panicking and the
    // narrow column header must still be present.
    let service = FakeService::new(vec![record(1, "世界".as_bytes())]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(8, 40),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    assert!(contains(&out, "世界".as_bytes()));
}

// ---- The run loop ----------------------------------------------------------

#[test]
fn run_quits_on_q_after_refreshing() {
    let service = FakeService::new(vec![record(1, b"init")]);
    let mut model = Model::new(Scope::Own);
    let mut screen = Screen::new(
        FakeTty::with_input(b"q"),
        TermType::Xterm256Color,
        Size::new(10, 60),
    );
    assert_eq!(run(&mut model, &service, &mut screen), Ok(()));
    // It refreshed before drawing, so the snapshot is populated.
    assert_eq!(model.processes().len(), 1);
    let out = screen.into_tty().output;
    assert!(contains(&out, b"init"));
}

#[test]
fn run_returns_when_input_is_exhausted() {
    // No quit key: the loop ends when the channel yields nothing more.
    let service = FakeService::new(records(3));
    let mut model = Model::new(Scope::Own);
    let mut screen = Screen::new(
        FakeTty::with_input(b"\x1b[B"),
        TermType::Xterm256Color,
        Size::new(10, 60),
    );
    assert_eq!(run(&mut model, &service, &mut screen), Ok(()));
    // The down-arrow moved the selection before input ran out.
    assert_eq!(model.selected(), Some(1));
}

#[test]
fn run_toggling_to_a_denied_global_view_surfaces_the_error() {
    let mut service = FakeService::new(records(2));
    service.deny_global = true;
    let mut model = Model::new(Scope::Own);
    // 'a' toggles to the system-wide scope and triggers a refresh, which the
    // service denies.
    let mut screen = Screen::new(
        FakeTty::with_input(b"a"),
        TermType::Xterm256Color,
        Size::new(10, 60),
    );
    assert_eq!(
        run(&mut model, &service, &mut screen),
        Err(TopError::PermissionDenied)
    );
}

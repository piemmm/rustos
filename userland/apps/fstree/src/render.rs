//! The curses frame: the boxed tree and file panes, the flattened branch
//! view, the file viewers, the status bar, the message line, and the
//! sort-menu, help, and report overlays.
//!
//! The body is drawn inside a bordered box whose top edge carries the pane
//! titles (the focused pane's title in bold); the tree pane, a full-height
//! divider, and the file pane share it, while the flattened view, the
//! viewers, and the overlays take the whole box. The last two rows are the
//! white-on-blue status bar and the message line (bold prompts, bold-yellow
//! notices, dim key hints); rows are colour-coded by class (directories
//! blue, tagged entries yellow, the focused row reverse). Pure drawing over
//! the [`Model`] — no I/O, so golden-grid tests drive it directly.

use alloc::format;
use alloc::string::String;

use tairix_abi::time::Time64;
use tairix_curses::{truncate_to_width, Pos, Window};
use tairix_vt::{Attributes, BasicColor, Color};

use crate::model::{
    InputOp, IsaPurpose, Model, Overlay, Pane, Prompt, SortKey, View, Viewer, ViewerKind,
};
use crate::view_disasm::{isa_name, DisasmPane, DisasmView};
use crate::view_hex::{dump_row, offset_digits, HexView};
use crate::view_text::TextView;
use crate::walk::{relative_to, WalkPurpose};

/// The vertical split of the two-pane view: a full-width disk-statistics
/// header, then the boxed directory-tree window stacked above the boxed
/// file window — the `XTree Gold` arrangement.
pub(crate) struct PanesLayout {
    /// Top border row of the tree box (the header is the row above it).
    pub tree_top: u16,
    /// Total rows of the tree box, borders included.
    pub tree_height: u16,
    /// Top border row of the file box.
    pub file_top: u16,
    /// Total rows of the file box, borders included.
    pub file_height: u16,
}

impl PanesLayout {
    /// Content rows inside the tree box (its height minus the two borders).
    pub(crate) fn tree_interior(&self) -> usize {
        usize::from(self.tree_height.saturating_sub(2))
    }

    /// Content rows inside the file box.
    pub(crate) fn file_interior(&self) -> usize {
        usize::from(self.file_height.saturating_sub(2))
    }
}

/// Split a `body`-row two-pane region (the grid minus the status and
/// message lines) into the stats header and the two stacked boxes.
/// `None` when the body is too short to seat a header and both boxes;
/// the renderer then falls back to a bare listing.
pub(crate) fn panes_layout(body: u16) -> Option<PanesLayout> {
    // One header row, then two boxes each needing at least three rows
    // (top border, one content row, bottom border).
    if body < 1 + 3 + 3 {
        return None;
    }
    let below_header = body - 1;
    // The tree window takes the upper half (rounded up), the file window
    // the remainder; each keeps its own pair of border rows.
    let tree_height = below_header.div_ceil(2);
    let file_height = below_header - tree_height;
    Some(PanesLayout {
        tree_top: 1,
        tree_height,
        file_top: 1 + tree_height,
        file_height,
    })
}

// --- Styles ---------------------------------------------------------------
//
// One place defines every attribute the frame uses, so the palette is
// changed here and nowhere else. Colours degrade through the curses
// renderer's per-terminal downgrade (a monochrome terminal keeps the
// bold/dim/reverse shape).

/// The pane borders: cyan lines around every body region.
fn border_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.foreground = Color::Basic(BasicColor::Cyan);
    attrs
}

/// A pane title on the top border: bold cyan for the focused pane,
/// dim cyan for the inactive one.
fn title_attrs(focused: bool) -> Attributes {
    let mut attrs = border_attrs();
    if focused {
        attrs.bold = true;
    } else {
        attrs.dim = true;
    }
    attrs
}

/// A directory row: bold blue, the conventional directory colour.
fn dir_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.bold = true;
    attrs.foreground = Color::Basic(BasicColor::Blue);
    attrs
}

/// A tagged row: yellow, so the tagged set reads at a glance.
fn tag_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.foreground = Color::Basic(BasicColor::Yellow);
    attrs
}

/// The status bar: white on blue, the classic full-width band.
fn status_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.foreground = Color::Basic(BasicColor::White);
    attrs.background = Color::Basic(BasicColor::Blue);
    attrs
}

/// The key-hint line: dim, so hints never compete with content.
fn hint_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.dim = true;
    attrs
}

/// A notice or error on the message line: bold yellow, so a refusal is
/// never mistaken for a hint.
fn message_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.bold = true;
    attrs.foreground = Color::Basic(BasicColor::Yellow);
    attrs
}

/// An open prompt on the message line: bold, with the default colours.
fn prompt_attrs() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.bold = true;
    attrs
}

// --- The frame --------------------------------------------------------------

/// Draw one full frame of `model` into `window`.
pub fn render(model: &Model, window: &mut Window) {
    let size = window.size();
    let rows = size.rows;
    let cols = size.cols;
    window.erase();
    if rows < 5 || cols < 8 {
        return;
    }
    let body = rows - 2;
    match model.overlay {
        Overlay::Help => render_help(model, window, body, cols),
        Overlay::Report => render_report(model, window, body, cols),
        Overlay::Volumes => render_volumes(model, window, body, cols),
        Overlay::Settings => render_settings(model, window, body, cols),
        Overlay::Attrs => render_attrs(model, window, body, cols),
        Overlay::None | Overlay::SortMenu => match model.view {
            View::Panes => render_panes(model, window, body, cols),
            View::Flat => render_flat(model, window, body, cols),
            View::Viewer => render_viewer(model, window, body, cols),
        },
    }
    render_status(model, window, rows, cols);
    render_message(model, window, rows, cols);
}

/// Draw one bordered window: a box `height` rows tall whose top border sits
/// at row `top`, carrying `title` on its top border (bold when `focused`).
/// Content is drawn *inside* (rows `top + 1 ..= top + height - 2`).
fn draw_box(window: &mut Window, top: u16, height: u16, cols: u16, title: &str, focused: bool) {
    if height < 2 {
        return;
    }
    let border = border_attrs();
    window.set_attributes(border);
    let inner = usize::from(cols.saturating_sub(2));
    let horizontal = "─".repeat(inner);
    let bottom = top + height - 1;
    let _ = window.move_add_str(Pos::new(top, 0), &format!("┌{horizontal}┐"));
    let _ = window.move_add_str(Pos::new(bottom, 0), &format!("└{horizontal}┘"));
    for row in (top + 1)..bottom {
        let _ = window.move_add_str(Pos::new(row, 0), "│");
        let _ = window.move_add_str(Pos::new(row, cols - 1), "│");
    }
    window.set_attributes(title_attrs(focused));
    let _ = window.move_add_str(Pos::new(top, 2), title);
    window.set_attributes(Attributes::PLAIN);
}

/// The XTree-style disk-statistics header: the shown directory's path,
/// the volume's free/total bytes (when the mount reports them), the item
/// count, and the tagged count — a full-width band across the top.
fn render_header(model: &Model, window: &mut Window, cols: u16) {
    use core::fmt::Write as _;
    let mut text = format!(" Path: {}", model.files_dir);
    if let Some(space) = model.space {
        let _ = write!(
            text,
            "   Avail {} of {} bytes",
            space.free_bytes, space.total_bytes
        );
    }
    let _ = write!(text, "   {} items", model.visible_files().len());
    if !model.tags.is_empty() {
        let _ = write!(text, "   {} tagged", model.tags.count());
    }
    let mut padded = String::from(truncate_to_width(&text, usize::from(cols)));
    while padded.len() < usize::from(cols) {
        padded.push(' ');
    }
    window.set_attributes(status_attrs());
    let _ = window.move_add_str(Pos::new(0, 0), &padded);
    window.set_attributes(Attributes::PLAIN);
}

/// The `XTree Gold` main screen: a disk-statistics header over the boxed
/// directory-tree window (branch-line tree) stacked above the boxed file
/// window. The active window's title is bold; its highlight bar is
/// reversed while the other window's is dimmed, so the eye always finds
/// the focus.
fn render_panes(model: &Model, window: &mut Window, body: u16, cols: u16) {
    render_header(model, window, cols);
    let Some(layout) = panes_layout(body) else {
        // Too short for the boxed layout: fall back to a bare tree listing
        // under the header so a tiny grid still shows something usable.
        render_bare_tree(model, window, body, cols);
        return;
    };

    let plain = Attributes::PLAIN;
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let mut dim = Attributes::PLAIN;
    dim.dim = true;

    draw_box(
        window,
        layout.tree_top,
        layout.tree_height,
        cols,
        " Directory Tree ",
        model.pane == Pane::Tree,
    );
    let tree_inner = layout.tree_interior();
    let tw = usize::from(cols.saturating_sub(2));
    let tree_rows = model.tree_rows();
    let visible_tree = tree_rows.iter().enumerate().skip(model.tree_scroll);
    for (line, (index, row)) in (0..tree_inner).zip(visible_tree) {
        let text = format!("{}{} {}", row.branch, row.fold, row.name);
        let focused = index == model.tree_cursor;
        let attrs = match (focused, model.pane) {
            (true, Pane::Tree) => reverse,
            (true, Pane::Files) => dim,
            // Every tree row is a directory; colour it as one.
            _ => dir_attrs(),
        };
        window.set_attributes(attrs);
        let _ = window.move_add_str(
            Pos::new(layout.tree_top + 1 + u16::try_from(line).unwrap_or(0), 1),
            truncate_to_width(&text, tw),
        );
        window.set_attributes(plain);
    }

    draw_box(
        window,
        layout.file_top,
        layout.file_height,
        cols,
        " Files ",
        model.pane == Pane::Files,
    );
    let file_inner = layout.file_interior();
    let fw = usize::from(cols.saturating_sub(2));
    let files = model.visible_files();
    let visible_files = files.iter().enumerate().skip(model.file_scroll);
    for (line, (index, entry)) in (0..file_inner).zip(visible_files) {
        let tagged = model
            .tags
            .contains(&crate::model::join(&model.files_dir, &entry.name));
        let text = file_row(
            tagged,
            entry.kind.is_dir(),
            &entry.name,
            entry.size,
            entry.modified,
            fw,
        );
        let focused = index == model.file_cursor;
        let attrs = match (focused, model.pane) {
            (true, Pane::Files) => reverse,
            (true, Pane::Tree) => dim,
            // Unfocused rows read by class: tagged first (the working
            // set), then directories, then plain files.
            _ if tagged => tag_attrs(),
            _ if entry.kind.is_dir() => dir_attrs(),
            _ => plain,
        };
        window.set_attributes(attrs);
        let _ = window.move_add_str(
            Pos::new(layout.file_top + 1 + u16::try_from(line).unwrap_or(0), 1),
            truncate_to_width(&text, fw),
        );
        window.set_attributes(plain);
    }
}

/// The fallback listing when the grid is too short for the boxed layout:
/// the tree rows drawn plainly under the header, cursor highlighted.
fn render_bare_tree(model: &Model, window: &mut Window, body: u16, cols: u16) {
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let width = usize::from(cols);
    let tree_rows = model.tree_rows();
    let visible = tree_rows.iter().enumerate().skip(model.tree_scroll);
    for (line, (index, row)) in (1..body).zip(visible) {
        let text = format!("{}{} {}", row.branch, row.fold, row.name);
        let attrs = if index == model.tree_cursor {
            reverse
        } else {
            dir_attrs()
        };
        window.set_attributes(attrs);
        let _ = window.move_add_str(Pos::new(line, 0), truncate_to_width(&text, width));
        window.set_attributes(Attributes::PLAIN);
    }
}

/// One file-pane row: the tag marker, name left, size and stamp
/// right-aligned.
fn file_row(
    tagged: bool,
    is_dir: bool,
    name: &str,
    size: u64,
    modified: Time64,
    width: usize,
) -> String {
    let marker = if tagged { "*" } else { " " };
    let stamp = format_stamp(modified);
    let size_text = if is_dir {
        String::from("<dir>")
    } else {
        format!("{size}")
    };
    let tail = format!("{size_text:>12} {stamp:>16}");
    let name_width = width.saturating_sub(tail.len() + 3);
    let shown = truncate_to_width(name, name_width);
    format!("{marker}{shown:<name_width$} {tail}")
}

/// The flattened branch view: one full-width row per found file — the tag
/// marker, the branch-relative path, size and stamp right-aligned — while
/// the walk fills the list page by page.
fn render_flat(model: &Model, window: &mut Window, body: u16, cols: u16) {
    let Some(walk) = &model.walk else {
        return;
    };
    let plain = Attributes::PLAIN;
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let title = format!(" {} ", walk.label());
    draw_box(window, 0, body, cols, &title, true);
    let width = usize::from(cols - 2);
    let visible = walk.entries.iter().enumerate().skip(model.flat_scroll);
    for (line, (index, entry)) in (0..body - 2).zip(visible) {
        let rel = relative_to(&walk.root, &entry.path);
        let shown = match &entry.note {
            Some(note) => format!("{rel}  [{note}]"),
            None => String::from(rel),
        };
        let tagged = model.tags.contains(&entry.path);
        let text = file_row(tagged, false, &shown, entry.size, entry.modified, width);
        let attrs = if index == model.flat_cursor {
            reverse
        } else if tagged {
            tag_attrs()
        } else {
            plain
        };
        window.set_attributes(attrs);
        let _ = window.move_add_str(Pos::new(line + 1, 1), truncate_to_width(&text, width));
        window.set_attributes(plain);
    }
}

/// The viewer body: the text pager's decoded page, the hex dump's
/// offset/hex/ASCII rows, or the disassembly viewer's summary/code page.
/// Pure drawing over the page the last refresh loaded.
fn render_viewer(model: &Model, window: &mut Window, body: u16, cols: u16) {
    match &model.viewer {
        Some(Viewer::Text(view)) => render_text_view(view, window, body, cols),
        Some(Viewer::Hex(view)) => render_hex_view(view, window, body, cols),
        Some(Viewer::Disasm(view)) => render_disasm_view(view, window, body, cols),
        None => {}
    }
}

/// The disassembly viewer's page inside its box: the summary page's
/// header and region rows (selected region reversed), or the code pane's
/// label and instruction rows (labels in the directory register so the
/// eye finds function starts).
fn render_disasm_view(view: &DisasmView, window: &mut Window, body: u16, cols: u16) {
    let title = format!(" {} ", view.path);
    draw_box(window, 0, body, cols, &title, true);
    let width = usize::from(cols - 2);
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    match view.pane {
        DisasmPane::Summary => {
            let rows = view.summary_rows();
            let shown = rows.iter().skip(view.sum_scroll);
            for (line, (text, region)) in (0..body - 2).zip(shown) {
                let selected = *region == Some(view.sum_cursor);
                if selected {
                    window.set_attributes(reverse);
                }
                let _ = window.move_add_str(Pos::new(line + 1, 1), truncate_to_width(text, width));
                window.set_attributes(Attributes::PLAIN);
            }
        }
        DisasmPane::Code => {
            for (line, row) in (0..body - 2).zip(view.rows.iter()) {
                if row.is_label {
                    window.set_attributes(dir_attrs());
                }
                let _ =
                    window.move_add_str(Pos::new(line + 1, 1), truncate_to_width(&row.text, width));
                window.set_attributes(Attributes::PLAIN);
            }
        }
    }
}

/// The text pager's page inside its box, one sanitised row per line.
fn render_text_view(view: &TextView, window: &mut Window, body: u16, cols: u16) {
    let title = format!(" {} ", view.path);
    draw_box(window, 0, body, cols, &title, true);
    for (line, row) in (0..body - 2).zip(view.rows.iter()) {
        let _ = window.move_add_str(
            Pos::new(line + 1, 1),
            truncate_to_width(&row.text, usize::from(cols - 2)),
        );
    }
}

/// The hex dump's page inside its box, one offset/hex/ASCII row per
/// line; the offset column reads in cyan so the eye tracks it.
fn render_hex_view(view: &HexView, window: &mut Window, body: u16, cols: u16) {
    let title = format!(" {} ", view.path);
    draw_box(window, 0, body, cols, &title, true);
    let digits = offset_digits(view.size);
    for line in 0..body - 2 {
        let Some(text) = dump_row(view.top, &view.bytes, usize::from(line), digits) else {
            break;
        };
        let shown = truncate_to_width(&text, usize::from(cols - 2));
        let _ = window.move_add_str(Pos::new(line + 1, 1), shown);
        // Re-tint the offset column (the row's first `digits` characters,
        // always ASCII hex) without re-shaping the row.
        let offset_len = digits.min(shown.len());
        window.set_attributes(border_attrs());
        let _ = window.move_add_str(Pos::new(line + 1, 1), &shown[..offset_len]);
        window.set_attributes(Attributes::PLAIN);
    }
}

/// The report overlay: a batch's per-file failure lines (or a walk's
/// unreadable directories), paged inside its box; any key dismisses it.
fn render_report(model: &Model, window: &mut Window, body: u16, cols: u16) {
    draw_box(window, 0, body, cols, " Report ", true);
    for (line, text) in (0..body - 2).zip(model.report_lines.iter()) {
        let _ = window.move_add_str(
            Pos::new(line + 1, 1),
            truncate_to_width(text, usize::from(cols - 2)),
        );
    }
}

/// The volume list (`V`): one row per published storage root — mount
/// target, filesystem type, and free/total bytes (absent figures shown
/// as `-`, never fabricated). The cursor row is reversed; Enter opens
/// the root, Esc leaves.
fn render_volumes(model: &Model, window: &mut Window, body: u16, cols: u16) {
    draw_box(window, 0, body, cols, " Volumes ", true);
    let width = usize::from(cols - 2);
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    for (line, (index, volume)) in (0..body - 2).zip(model.volumes.iter().enumerate()) {
        let space = match volume.space {
            Some(space) => format!("{}/{}", space.free_bytes, space.total_bytes),
            None => String::from("-"),
        };
        let text = format!("{:<24}  {:<8}  free {space}", volume.target, volume.fstype);
        if index == model.volume_cursor {
            window.set_attributes(reverse);
        }
        let _ = window.move_add_str(Pos::new(line + 1, 1), truncate_to_width(&text, width));
        window.set_attributes(Attributes::PLAIN);
    }
}

/// The attributes editor (`a`): the entry's mode line, then one row per
/// extended attribute with the cursor row reversed. A backing without
/// attribute storage says so in place of an empty list.
fn render_attrs(model: &Model, window: &mut Window, body: u16, cols: u16) {
    let Some(view) = model.attrs.as_ref() else {
        return;
    };
    draw_box(window, 0, body, cols, " Attributes ", true);
    let width = usize::from(cols - 2);
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let header = format!(
        "{}  mode {:o}   m mode  n new  Enter edit  d delete",
        view.name, view.mode
    );
    let _ = window.move_add_str(Pos::new(1, 1), truncate_to_width(&header, width));
    if view.unsupported {
        let _ = window.move_add_str(
            Pos::new(3, 1),
            truncate_to_width("attributes not supported by this filesystem", width),
        );
        return;
    }
    if view.entries.is_empty() {
        let _ = window.move_add_str(Pos::new(3, 1), truncate_to_width("no attributes", width));
        return;
    }
    for (line, (index, (key, value))) in
        (0..body.saturating_sub(4)).zip(view.entries.iter().enumerate())
    {
        let text = format!("{key} = {}", display_value(value));
        if index == view.cursor {
            window.set_attributes(reverse);
        }
        let _ = window.move_add_str(Pos::new(line + 3, 1), truncate_to_width(&text, width));
        window.set_attributes(Attributes::PLAIN);
    }
}

/// An attribute value's display form: printable text is shown as typed;
/// anything else is escaped byte by byte (`\xNN`), never trusted raw onto
/// the terminal.
fn display_value(value: &[u8]) -> String {
    match core::str::from_utf8(value) {
        Ok(text) if text.chars().all(|c| !c.is_control()) => String::from(text),
        _ => {
            let mut out = String::new();
            for byte in value {
                if (0x20..=0x7e).contains(byte) {
                    out.push(char::from(*byte));
                } else {
                    let _ = core::fmt::Write::write_fmt(&mut out, format_args!("\\x{byte:02x}"));
                }
            }
            out
        }
    }
}

/// The settings menu (`S`): the persisted confirmation toggles and where
/// they persist to (or that they cannot).
fn render_settings(model: &Model, window: &mut Window, body: u16, cols: u16) {
    draw_box(window, 0, body, cols, " Settings ", true);
    let width = usize::from(cols - 2);
    let on_off = |on: bool| if on { "on" } else { "off" };
    let lines = [
        format!(
            "1  confirm delete            {}",
            on_off(model.settings.confirm_delete)
        ),
        format!(
            "2  confirm batch delete      {}",
            on_off(model.settings.confirm_batch_delete)
        ),
        String::new(),
        match &model.settings_home {
            Some(home) => format!("saved in {home}/Settings/fstree/"),
            None => String::from("no home directory — changes last this session only"),
        },
    ];
    for (line, text) in (0..body - 2).zip(lines.iter()) {
        let _ = window.move_add_str(Pos::new(line + 1, 1), truncate_to_width(text, width));
    }
}

/// The status bar: path, entry count, sort order, free space, and the
/// hidden toggle.
fn render_status(model: &Model, window: &mut Window, rows: u16, cols: u16) {
    if model.view == View::Viewer {
        render_viewer_status(model, window, rows, cols);
        return;
    }
    let key = match model.sort_key {
        SortKey::Name => "name",
        SortKey::Extension => "ext",
        SortKey::Size => "size",
        SortKey::Modified => "mtime",
    };
    let direction = if model.sort_desc { "desc" } else { "asc" };
    let hidden = if model.show_hidden { "  hidden" } else { "" };
    let filter = match &model.filter {
        Some(filter) if filter.pattern.is_none() => {
            format!("  filter {} (bad pattern)", filter.text)
        }
        Some(filter) => format!("  filter {}", filter.text),
        None => String::new(),
    };
    let tags = if model.tags.is_empty() {
        String::new()
    } else {
        format!(
            "  {} tagged ({} bytes)",
            model.tags.count(),
            model.tags.total_bytes()
        )
    };
    let status = if let (View::Flat, Some(walk)) = (model.view, &model.walk) {
        let state = if walk.done {
            ""
        } else if walk.paused {
            "  more with Space"
        } else {
            "  searching… Esc stops"
        };
        format!(
            "{} {}  {} entries{state}{tags}",
            walk.root,
            walk.label(),
            walk.entries.len(),
        )
    } else {
        // Path, free space, and item counts ride the top disk-statistics
        // header; the band carries the active window and view state so the
        // two never repeat each other.
        let active = match model.pane {
            Pane::Tree => "Tree",
            Pane::Files => "Files",
        };
        format!("{active}  sort {key} {direction}{hidden}{filter}{tags}")
    };
    let mut padded = String::from(truncate_to_width(&status, usize::from(cols)));
    while padded.len() < usize::from(cols) {
        padded.push(' ');
    }
    window.set_attributes(status_attrs());
    let _ = window.move_add_str(Pos::new(rows - 2, 0), &padded);
    window.set_attributes(Attributes::PLAIN);
}

/// The viewer's status bar: path, size, place, and the live-scan state.
fn render_viewer_status(model: &Model, window: &mut Window, rows: u16, cols: u16) {
    let status = match &model.viewer {
        Some(Viewer::Text(view)) => {
            let place = match view.top.line {
                Some(line) => format!("line {}", line + 1),
                None => format!("offset 0x{:x}", view.top.offset),
            };
            let wrap = if view.wrap { "wrap" } else { "nowrap" };
            let end = if view.at_end { "  end" } else { "" };
            let scanning = if view.ticking() {
                "  searching… Esc stops"
            } else {
                ""
            };
            format!(
                "{}  text  {} bytes  {place}  {wrap}{end}{scanning}",
                view.path, view.size
            )
        }
        Some(Viewer::Hex(view)) => {
            let end = if view.at_end { "  end" } else { "" };
            let scanning = if view.ticking() {
                "  searching… Esc stops"
            } else {
                ""
            };
            format!(
                "{}  hex  {} bytes  offset 0x{:x}{end}{scanning}",
                view.path, view.size, view.top
            )
        }
        Some(Viewer::Disasm(view)) => {
            let isa = match view.isa() {
                Some(isa) => isa_name(isa),
                None => "isa?",
            };
            let scanning = if view.ticking() {
                "  walking… Esc stops"
            } else {
                ""
            };
            match view.pane {
                DisasmPane::Summary => format!(
                    "{}  disasm  {}  {} regions{scanning}",
                    view.path,
                    isa,
                    view.regions.len(),
                ),
                DisasmPane::Code => {
                    let name = view
                        .open_region()
                        .map(|region| {
                            if region.name.is_empty() {
                                String::from("region")
                            } else {
                                region.name.clone()
                            }
                        })
                        .unwrap_or_default();
                    let end = if view.at_end { "  end" } else { "" };
                    format!(
                        "{}  disasm  {isa}  {name}  address {:#x}{end}{scanning}",
                        view.path, view.place.top,
                    )
                }
            }
        }
        None => String::new(),
    };
    let mut padded = String::from(truncate_to_width(&status, usize::from(cols)));
    while padded.len() < usize::from(cols) {
        padded.push(' ');
    }
    window.set_attributes(status_attrs());
    let _ = window.move_add_str(Pos::new(rows - 2, 0), &padded);
    window.set_attributes(Attributes::PLAIN);
}

/// The message line: the open prompt's question, an error/notice, the
/// sort-menu prompt, or key hints — each in its own register (bold
/// prompts, bold-yellow notices, dim hints) so the eye reads the kind
/// before the words.
fn render_message(model: &Model, window: &mut Window, rows: u16, cols: u16) {
    let usage = model
        .walk
        .as_ref()
        .filter(|walk| walk.purpose == WalkPurpose::Usage)
        .map(|walk| format!("usage of {}: {} (Esc cancels)", walk.root, walk.figures()));
    let (text, attrs) = if let Some(prompt) = &model.prompt {
        (prompt_line(prompt), prompt_attrs())
    } else if model.overlay == Overlay::SortMenu {
        (
            String::from("sort: n)ame  e)xtension  s)ize  m)odified  r)everse  Esc cancels"),
            prompt_attrs(),
        )
    } else if let Some(message) = &model.message {
        (message.clone(), message_attrs())
    } else if let Some(usage) = usage {
        (usage, message_attrs())
    } else {
        let hint = if model.view == View::Viewer {
            match &model.viewer {
                Some(Viewer::Text(_)) => String::from(
                    "arrows scroll  Space/b page  Home/End  g line  / search  n next  w wrap  \
                     x hex  d disasm  q back",
                ),
                Some(Viewer::Disasm(view)) if view.pane == DisasmPane::Summary => {
                    String::from("arrows move  Enter open region  x hex  t text  q back")
                }
                Some(Viewer::Disasm(_)) => String::from(
                    "arrows scroll  Space/b page  Home/End  g address  / search  n next  \
                     I isa  x hex  t text  Esc summary  q back",
                ),
                _ => String::from(
                    "arrows scroll  Space/b page  Home/End  g offset  / search (text or 0x…)  \
                     n next  t text  d disasm  q back",
                ),
            }
        } else if model.view == View::Flat {
            String::from(
                "arrows move  Enter open dir  t tag  T glob  i invert  C clear  c copy  m move  \
                 d delete  Space more  Esc back",
            )
        } else if model.pane == Pane::Tree {
            // The active window's own command set, XTree-style: the tree
            // window navigates and folds; Enter/Tab/→ steps into the files.
            String::from(
                "↑↓ move  →/+ expand  ←/- collapse  Enter/Tab files  t tag  c copy  m move  \
                 r rename  d delete  M mkdir  a attrib  s sort  u usage  v branch  V vols  \
                 S settings  ? help  q quit",
            )
        } else {
            String::from(
                "↑↓ move  Enter open  →/l enter dir  Esc/Tab/← tree  o open as  t tag  T glob  \
                 i invert  C clear  c copy  m move  r rename  d delete  a attrib  f filter  \
                 / find  F contents  s sort  . repeat  ? help  q quit",
            )
        };
        (hint, hint_attrs())
    };
    window.set_attributes(attrs);
    let _ = window.move_add_str(
        Pos::new(rows - 1, 0),
        truncate_to_width(&text, usize::from(cols)),
    );
    window.set_attributes(Attributes::PLAIN);
}

/// The one-line question an open prompt shows; the trailing underscore is
/// the input point of the text prompts.
fn prompt_line(prompt: &Prompt) -> String {
    match prompt {
        // The bracketed figure is the entry's current mode for reference.
        Prompt::Mode(mode) => format!(
            "mode {} [{:o}]: {}_  (octal, Enter applies, Esc cancels)",
            mode.name, mode.current, mode.input
        ),
        Prompt::AttrEdit(edit) => format!(
            "attribute on {} (key=value): {}_  (Enter applies, Esc cancels)",
            edit.name, edit.input
        ),
        Prompt::Input(input) => input_prompt_line(input),
        Prompt::OpenAs(prompt) => format!(
            "open {} as: t)ext  x) hex  d)isassembly  Esc cancels",
            prompt.path
        ),
        Prompt::IsaPick(prompt) => {
            let what = match &prompt.purpose {
                IsaPurpose::OpenRaw { path, .. } => format!("isa for {path}"),
                IsaPurpose::EnterRegion { .. } => String::from("isa for this region"),
                IsaPurpose::Override => String::from("isa override"),
            };
            format!("{what}: x)86-64  a)arch64  r)iscv64  w)asm  Esc cancels")
        }
        Prompt::ConfirmDelete(confirm) => {
            if confirm.kind.is_dir() {
                format!("delete directory {} and its contents? y/N", confirm.name)
            } else {
                format!("delete {}? y/N", confirm.name)
            }
        }
        Prompt::Overwrite(paused) => match paused.op.conflict() {
            Some(conflict) => {
                format!("{} exists — o)verwrite  s)kip  c)ancel", conflict.dst)
            }
            // Unreachable by construction (the prompt exists only while a
            // conflict is paused), but fail closed rather than panic.
            None => String::from("o)verwrite  s)kip  c)ancel"),
        },
        Prompt::ConfirmBatchDelete { count } => {
            format!("delete {count} tagged entries (directories recursively)? y/N")
        }
        Prompt::BatchOverwrite(paused) => match paused.batch.conflict() {
            Some(conflict) => {
                format!("{} exists — o)verwrite  s)kip  c)ancel batch", conflict.dst)
            }
            // Unreachable by construction (the prompt exists only while a
            // conflict is paused), but fail closed rather than panic.
            None => String::from("o)verwrite  s)kip  c)ancel batch"),
        },
    }
}

/// The question an open text prompt asks, per its subject.
fn input_prompt_line(input: &crate::model::InputPrompt) -> String {
    match &input.op {
        InputOp::CopyDest { name, .. } => {
            format!(
                "copy {name} to: {}_  (Enter copies, Esc cancels)",
                input.input
            )
        }
        InputOp::MoveDest { name, .. } => {
            format!(
                "move {name} to: {}_  (Enter moves, Esc cancels)",
                input.input
            )
        }
        InputOp::RenameTo { name, .. } => {
            format!(
                "rename {name} to: {}_  (Enter renames, Esc cancels)",
                input.input
            )
        }
        InputOp::MkdirName => {
            format!("mkdir: {}_  (Enter creates, Esc cancels)", input.input)
        }
        InputOp::TagGlob => {
            format!(
                "tag pattern: {}_  (glob, Enter tags, Esc cancels)",
                input.input
            )
        }
        InputOp::FilterPattern { .. } => {
            format!(
                "filter: {}_  (glob, applied as typed; Enter keeps, Esc restores)",
                input.input
            )
        }
        InputOp::SearchGlob => {
            format!(
                "search below this branch: {}_  (glob, Enter searches, Esc cancels)",
                input.input
            )
        }
        InputOp::ViewerGoto { kind } => match kind {
            ViewerKind::Hex => format!(
                "goto offset: {}_  (decimal or 0x-hex, Enter jumps, Esc cancels)",
                input.input
            ),
            ViewerKind::Text => {
                format!("goto line: {}_  (Enter jumps, Esc cancels)", input.input)
            }
            ViewerKind::Disasm => format!(
                "goto address: {}_  (decimal or 0x-hex, Enter jumps, Esc cancels)",
                input.input
            ),
        },
        InputOp::ViewerSearch { kind } => match kind {
            ViewerKind::Hex => format!(
                "find bytes: {}_  (text or 0x hex pairs, Enter searches, Esc cancels)",
                input.input
            ),
            ViewerKind::Text => {
                format!("find text: {}_  (Enter searches, Esc cancels)", input.input)
            }
            ViewerKind::Disasm => format!(
                "find instruction text: {}_  (Enter searches, Esc cancels)",
                input.input
            ),
        },
        InputOp::ContentNeedle { tagged } => {
            let scope = if *tagged {
                "the tagged set"
            } else {
                "this branch"
            };
            format!(
                "find in files of {scope}: {}_  (Enter searches, Esc cancels)",
                input.input
            )
        }
        InputOp::BatchDest { moving, count } => {
            let verb = if *moving { "move" } else { "copy" };
            format!(
                "{verb} {count} tagged into directory: {}_  (Enter runs, Esc cancels)",
                input.input
            )
        }
    }
}

/// The help overlay: the bundle's own Help document, paged inside its box
/// (never embedded text — the string arrives through the help seam).
fn render_help(model: &Model, window: &mut Window, body: u16, cols: u16) {
    draw_box(window, 0, body, cols, " Help ", true);
    for (line, text) in (0..body - 2).zip(model.help_text.lines()) {
        let _ = window.move_add_str(
            Pos::new(line + 1, 1),
            truncate_to_width(text, usize::from(cols - 2)),
        );
    }
}

/// A stamp rendered as `YYYY-MM-DD HH:MM`, or `-` for the epoch (the
/// documented "no stamp" value — an absent figure, never a fabricated
/// 1970 date).
fn format_stamp(stamp: Time64) -> String {
    if stamp == Time64::UNIX_EPOCH {
        return String::from("-");
    }
    tairix_fsmeta::calendar::CivilTime::from_time64(stamp).iso_minute()
}

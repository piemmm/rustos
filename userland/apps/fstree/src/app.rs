//! The session: typed key events mutating the [`Model`], and the screen
//! loop the `Run` binary drives.
//!
//! Every wait parks in the kernel: the loop blocks in [`Screen::getch`],
//! and while a walk (`u`/`v`) is live the wait carries a short timeout so
//! the walk advances one bounded tick per elapsed bound — the kernel still
//! parks the read for the interval; there is no polling loop. The terminal
//! is restored by the `Run` binary's alternate-screen bracketing, not here.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use core::time::Duration;

use rustos_abi::{Errno, FileKind};
use rustos_curses::{Event, InputMode, Pos, Screen, Tty, Window};
use rustos_glob::Pattern;
use rustos_sandbox::decode::{Isa, RegionKind, MAX_INPUT};

use crate::fs::Fs;
use crate::info::{note_hidden_entries, Info};
use crate::model::{
    child_dirs_of, join, merge_child_dirs, AttrEditPrompt, AttrEntries, AttrsView, BatchPrompt,
    ConfirmPrompt, DirNode, InputOp, InputPrompt, IsaPrompt, IsaPurpose, ModePrompt, Model,
    NameFilter, OpenAsPrompt, Overlay, OverwritePrompt, Pane, Prompt, RepeatOp, SortKey, View,
    Viewer, ViewerKind,
};
use crate::ops::{parent_of, plan_target, resolve_destination, Decision, FileOp, OpProgress};
use crate::render::render;
use crate::search::{ContentScan, Needle};
use crate::tag::{Batch, BatchProgress, TagEntry, TagRange};
use crate::view_disasm::{describe, is_manifest_head, Decode, DisasmBody, DisasmPane, DisasmView};
use crate::view_hex::{parse_offset, HexView};
use crate::view_text::{JobOutcome, TextView};
use crate::walk::{relative_to, FlatEntry, WalkPurpose, WalkState, Walker};

/// Longest mode the prompt accepts: four octal digits (`7777`), the full
/// permission word — a fixed validation bound on typed input, matching the
/// kernel's own `FS_MODE_MASK` ceiling.
const MODE_DIGITS_MAX: usize = 4;

/// Longest text a destination or name prompt accepts — a typing bound on
/// the prompt line; the shared path grammar and the kernel enforce the
/// real path limits on submission.
const INPUT_MAX: usize = 512;

/// The input bound while a walk is live: the kernel parks the read for
/// this interval, and an elapsed bound advances the walk one tick — short
/// enough that the view fills briskly, long enough that a held key still
/// outruns the ticks.
const WALK_TICK: Duration = Duration::from_millis(25);

/// Directories one walk tick may read — the bound that keeps a tick's
/// filesystem work small so the key loop stays responsive between ticks.
const WALK_DIRS_PER_TICK: usize = 16;

/// Entries per flattened-view expansion page: the walk pauses each time
/// this many further entries have accumulated, and the load-more key
/// (Space) releases the next page — a huge branch never fills memory
/// unasked.
const FLAT_PAGE: usize = 512;

/// File bytes one content-search tick may scan — the bound that keeps a
/// tick's read work small so the key loop stays responsive while a large
/// file streams through the scanner.
const SCAN_BYTES_PER_TICK: usize = 256 * 1024;

/// Bytes of the head sample deciding a file's opening viewer: text when
/// the sample is NUL-free, valid UTF-8 (a truncated final character
/// allowed), hex otherwise.
const HEAD_SAMPLE: usize = 4096;

/// The session outcome the `Run` binary maps to an exit code.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FstreeError {
    /// The terminal path failed (a write error or a closed input).
    Terminal,
}

/// Rows available to a pane body: the grid minus the status and message
/// lines and the box's top and bottom border rows.
fn body_rows(screen_rows: u16) -> usize {
    usize::from(screen_rows.saturating_sub(4))
}

/// Drive the session against the seams until the user quits.
///
/// # Errors
///
/// [`FstreeError::Terminal`] when the screen write path or the blocking
/// input path fails — the session ends loudly rather than spinning on a
/// dead channel.
pub fn run<T: Tty>(
    model: &mut Model,
    fs: &mut dyn Fs,
    decode: &mut dyn Decode,
    screen: &mut Screen<T>,
    info: &mut dyn Info,
) -> Result<i32, FstreeError> {
    let mut window = Window::new(Pos::new(0, 0), screen.size());
    // The omission state already reported on fd 3, so a record goes out
    // once per change, not per keystroke.
    let mut noted_hidden = None;
    loop {
        note_hidden_entries(model, info, &mut noted_hidden);
        clamp_scroll(model, body_rows(screen.size().rows));
        refresh_viewer(
            model,
            fs,
            decode,
            body_rows(screen.size().rows),
            // The viewer wraps to the boxed interior, inside the side
            // borders.
            usize::from(screen.size().cols.saturating_sub(2)),
        );
        render(model, &mut window);
        screen.refresh(&window).map_err(|_| FstreeError::Terminal)?;
        // While a walk or a viewer scan is live the wait carries a short
        // bound so an elapsed read advances it one tick; otherwise the
        // read blocks until a key arrives. Either way the kernel parks
        // the task — never a poll.
        let walk_live = model.walk.as_ref().is_some_and(WalkState::ticking);
        let viewer_live = viewer_ticking(model);
        screen.set_input_mode(if walk_live || viewer_live {
            InputMode::Timeout(WALK_TICK)
        } else {
            InputMode::Blocking
        });
        let Some(event) = screen.getch().map_err(|_| FstreeError::Terminal)? else {
            // No event: the tick bound elapsed, or a split escape
            // sequence continues on the next read.
            if walk_live {
                walk_tick(model, fs);
            }
            if viewer_live {
                viewer_tick(model, fs, decode);
            }
            continue;
        };
        handle_event(model, fs, decode, &event);
        if model.quit {
            return Ok(0);
        }
    }
}

/// Apply one typed key event to the session state.
pub fn handle_event(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode, event: &Event) {
    model.message = None;
    if let Some(prompt) = model.prompt.take() {
        match prompt {
            Prompt::Mode(mode) => handle_mode_prompt(model, fs, mode, event),
            Prompt::AttrEdit(edit) => handle_attr_edit_prompt(model, fs, edit, event),
            Prompt::Input(input) => handle_input_prompt(model, fs, input, event),
            Prompt::ConfirmDelete(confirm) => handle_confirm_prompt(model, fs, &confirm, event),
            Prompt::Overwrite(paused) => handle_overwrite_prompt(model, fs, paused, event),
            Prompt::ConfirmBatchDelete { count } => {
                handle_confirm_batch_delete(model, fs, count, event);
            }
            Prompt::BatchOverwrite(paused) => handle_batch_overwrite(model, fs, paused, event),
            Prompt::OpenAs(prompt) => handle_open_as(model, fs, decode, prompt, event),
            Prompt::IsaPick(prompt) => handle_isa_pick(model, prompt, event),
        }
        return;
    }
    match model.overlay {
        Overlay::Help | Overlay::Report => {
            // Any key dismisses a covering overlay.
            model.overlay = Overlay::None;
            return;
        }
        Overlay::SortMenu => {
            handle_sort_menu(model, event);
            return;
        }
        Overlay::Volumes => {
            handle_volumes_overlay(model, fs, event);
            return;
        }
        Overlay::Settings => {
            handle_settings_overlay(model, fs, event);
            return;
        }
        Overlay::Attrs => {
            handle_attrs_overlay(model, fs, event);
            return;
        }
        Overlay::None => {}
    }
    if model.view == View::Viewer {
        handle_viewer_key(model, fs, decode, event);
        return;
    }
    if model.view == View::Flat {
        handle_flat_key(model, fs, event);
        return;
    }
    handle_panes_key(model, fs, decode, event);
}

/// Apply one key in the plain panes view (no prompt, overlay, or
/// covering view is open).
fn handle_panes_key(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode, event: &Event) {
    match event {
        Event::Char('q') => model.quit = true,
        Event::Char('?') => model.overlay = Overlay::Help,
        Event::Char('s') => model.overlay = Overlay::SortMenu,
        Event::Char('a') => open_attrs_view(model, fs),
        Event::Char('c') => open_copy_move(model, false),
        Event::Char('m') => open_copy_move(model, true),
        Event::Char('r') => open_rename_prompt(model),
        Event::Char('d') => open_delete(model, fs),
        Event::Char('M') => {
            model.prompt = Some(Prompt::Input(InputPrompt {
                op: InputOp::MkdirName,
                input: String::new(),
            }));
        }
        Event::Char('t') => toggle_tag(model),
        Event::Char('T') => open_tag_glob(model),
        Event::Char('i') => invert_tags(model),
        Event::Char('C') => {
            model.tags.clear();
            model.message = Some(String::from("tags cleared"));
        }
        Event::Char('o') => open_open_as(model),
        Event::Char('u') => start_walk(model, WalkPurpose::Usage),
        Event::Char('v') => start_walk(model, WalkPurpose::Flat),
        Event::Char('f') => open_filter_prompt(model),
        Event::Char('/') => open_search_prompt(model),
        Event::Char('F') => open_content_prompt(model),
        Event::Char('V') => open_volumes(model, fs),
        Event::Char('S') => model.overlay = Overlay::Settings,
        Event::Esc => cancel_usage_walk(model),
        Event::Char('.') => repeat_last_op(model, fs),
        Event::Char('H') => {
            model.show_hidden = !model.show_hidden;
            clamp_cursors(model);
        }
        Event::Tab => {
            model.pane = match model.pane {
                Pane::Tree => Pane::Files,
                Pane::Files => Pane::Tree,
            };
        }
        Event::Up | Event::Char('k') => move_cursor(model, fs, -1),
        Event::Down | Event::Char('j') => move_cursor(model, fs, 1),
        Event::Left | Event::Char('h') => {
            if model.pane == Pane::Tree {
                set_expanded(model, fs, false);
            }
        }
        Event::Right | Event::Char('l') => {
            if model.pane == Pane::Tree {
                set_expanded(model, fs, true);
            }
        }
        Event::Enter => match model.pane {
            Pane::Tree => toggle_expanded(model, fs),
            Pane::Files => enter_file_row(model, fs, decode),
        },
        _ => {}
    }
}

/// The focused pane's selection: the file pane's entry, or the tree
/// pane's directory. `None` when the focused pane is empty.
fn focused_selection(model: &Model) -> Option<(String, String, FileKind)> {
    match model.pane {
        Pane::Files => {
            let entry = model.visible_files().get(model.file_cursor).copied()?;
            Some((
                join(&model.files_dir, &entry.name),
                entry.name.clone(),
                entry.kind,
            ))
        }
        Pane::Tree => {
            let rows = model.tree_rows();
            let row = rows.get(model.tree_cursor)?;
            Some((row.path.clone(), row.name.clone(), FileKind::Directory))
        }
    }
}

/// Open the attributes editor (`a`) on the focused pane's selection: the
/// entry's mode bits (a resolve-only stat through the seam) plus its
/// extended attributes. A refused stat or listing surfaces its error and
/// opens nothing; a backing without attribute storage opens the view with
/// that fact stated (the mode editor still works there).
fn open_attrs_view(model: &mut Model, fs: &mut dyn Fs) {
    let Some((path, name, _)) = focused_selection(model) else {
        return;
    };
    let mode = match fs.stat_mode(&path) {
        Ok(mode) => mode,
        Err(errno) => {
            model.report(&path, errno);
            return;
        }
    };
    match load_attr_entries(fs, &path) {
        Ok((entries, unsupported)) => {
            model.attrs = Some(AttrsView {
                path,
                name,
                mode,
                entries,
                cursor: 0,
                unsupported,
            });
            model.overlay = Overlay::Attrs;
        }
        Err(errno) => model.report(&path, errno),
    }
}

/// The visible extended attributes of `path` as `(key, value)` pairs,
/// paired with whether the backing stores no attributes at all (the
/// honest "unsupported" answer, shown in place of an empty list).
fn load_attr_entries(fs: &mut dyn Fs, path: &str) -> Result<(AttrEntries, bool), Errno> {
    let keys = match fs.attr_list(path) {
        Ok(keys) => keys,
        Err(Errno::NotSupported) => return Ok((Vec::new(), true)),
        Err(errno) => return Err(errno),
    };
    let mut entries = Vec::with_capacity(keys.len());
    for key in keys {
        let value = fs.attr_get(path, &key)?;
        entries.push((key, value));
    }
    Ok((entries, false))
}

/// Re-read the open attributes view's entries after an applied change,
/// keeping the cursor on a valid row. A refused re-read surfaces its
/// error; the view keeps its previous rows rather than lying with an
/// empty list.
fn refresh_attrs_view(model: &mut Model, fs: &mut dyn Fs) {
    let Some(view) = model.attrs.as_ref() else {
        return;
    };
    let path = view.path.clone();
    match load_attr_entries(fs, &path) {
        Ok((entries, unsupported)) => {
            if let Some(view) = model.attrs.as_mut() {
                view.cursor = view.cursor.min(entries.len().saturating_sub(1));
                view.entries = entries;
                view.unsupported = unsupported;
            }
        }
        Err(errno) => model.report(&path, errno),
    }
}

/// Apply one key in the attributes editor: arrows move over the entries,
/// `m` opens the octal mode prompt, `n` asks for a new `key=value`,
/// Enter edits the selected attribute, `d` removes it, Esc leaves. The
/// kernel decides every change; a refusal is surfaced verbatim.
fn handle_attrs_overlay(model: &mut Model, fs: &mut dyn Fs, event: &Event) {
    let Some(view) = model.attrs.as_mut() else {
        model.overlay = Overlay::None;
        return;
    };
    match event {
        Event::Esc | Event::Char('q' | 'a') => {
            model.overlay = Overlay::None;
            model.attrs = None;
        }
        Event::Up | Event::Char('k') => view.cursor = view.cursor.saturating_sub(1),
        Event::Down | Event::Char('j') => {
            if view.cursor + 1 < view.entries.len() {
                view.cursor += 1;
            }
        }
        Event::Char('m') => {
            let current = view.mode;
            model.prompt = Some(Prompt::Mode(ModePrompt {
                path: view.path.clone(),
                name: view.name.clone(),
                current,
                input: format!("{current:o}"),
            }));
        }
        Event::Char('n') => {
            if view.unsupported {
                model.message = Some(String::from("attributes not supported by this filesystem"));
                return;
            }
            model.prompt = Some(Prompt::AttrEdit(AttrEditPrompt {
                path: view.path.clone(),
                name: view.name.clone(),
                input: String::new(),
            }));
        }
        Event::Enter | Event::Char('e') => {
            let Some((key, value)) = view.entries.get(view.cursor) else {
                return;
            };
            // A text value pre-fills for in-place editing; a binary one
            // pre-fills the key alone — bytes that cannot round-trip
            // through the line are never lossily offered back.
            let input = match core::str::from_utf8(value) {
                Ok(text) if text.chars().all(|c| !c.is_control()) => format!("{key}={text}"),
                _ => format!("{key}="),
            };
            model.prompt = Some(Prompt::AttrEdit(AttrEditPrompt {
                path: view.path.clone(),
                name: view.name.clone(),
                input,
            }));
        }
        Event::Char('d') => {
            let Some((key, _)) = view.entries.get(view.cursor) else {
                return;
            };
            let key = key.clone();
            let path = view.path.clone();
            match fs.attr_remove(&path, &key) {
                Ok(()) => {
                    model.message = Some(format!("attribute {key} removed"));
                    refresh_attrs_view(model, fs);
                }
                Err(errno) => model.report(&path, errno),
            }
        }
        _ => {}
    }
}

/// Apply one key to the open `key=value` attribute prompt: printable
/// characters and Backspace edit, Enter applies through the seam, Esc
/// cancels. The kernel owns the key grammar and every permission and
/// size bound; a refusal is surfaced verbatim and nothing changes.
fn handle_attr_edit_prompt(
    model: &mut Model,
    fs: &mut dyn Fs,
    mut prompt: AttrEditPrompt,
    event: &Event,
) {
    match event {
        Event::Esc => {}
        Event::Backspace => {
            prompt.input.pop();
            model.prompt = Some(Prompt::AttrEdit(prompt));
        }
        Event::Enter => {
            let Some((key, value)) = prompt.input.split_once('=') else {
                model.message = Some(String::from(
                    "attribute: key=value expected — nothing applied",
                ));
                return;
            };
            if key.is_empty() {
                model.message = Some(String::from("attribute: empty key — nothing applied"));
                return;
            }
            match fs.attr_set(&prompt.path, key, value.as_bytes()) {
                Ok(()) => {
                    model.message = Some(format!("attribute {key} set on {}", prompt.name));
                    refresh_attrs_view(model, fs);
                }
                Err(errno) => model.report(&prompt.path, errno),
            }
        }
        Event::Char(c) if !c.is_control() => {
            prompt.input.push(*c);
            model.prompt = Some(Prompt::AttrEdit(prompt));
        }
        // Any other key is ignored — the prompt accepts only what the
        // kernel could accept.
        _ => model.prompt = Some(Prompt::AttrEdit(prompt)),
    }
}

/// The `c`/`m` key: a batch destination prompt when entries are tagged
/// (the tagged set is the operand), the single-selection transfer prompt
/// otherwise.
fn open_copy_move(model: &mut Model, moving: bool) {
    if model.tags.is_empty() {
        open_transfer_prompt(model, moving);
        return;
    }
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::BatchDest {
            moving,
            count: model.tags.count(),
        },
        input: String::new(),
    }));
}

/// The `d` key: the batch delete when entries are tagged, the
/// single-selection delete otherwise. Each asks first unless its
/// persisted confirmation toggle is off.
fn open_delete(model: &mut Model, fs: &mut dyn Fs) {
    if model.tags.is_empty() {
        open_delete_prompt(model, fs);
        return;
    }
    if !model.settings.confirm_batch_delete {
        batch_delete_now(model, fs);
        return;
    }
    model.prompt = Some(Prompt::ConfirmBatchDelete {
        count: model.tags.count(),
    });
}

/// Open the copy (`c`) or move (`m`) destination prompt on the focused
/// selection. Moving the session root is refused — the tree must keep its
/// anchor; copying it elsewhere is legitimate.
fn open_transfer_prompt(model: &mut Model, moving: bool) {
    let Some((src, name, kind)) = focused_selection(model) else {
        return;
    };
    if moving && src == model.root.path {
        model.message = Some(String::from("cannot move the session root"));
        return;
    }
    let op = if moving {
        InputOp::MoveDest { src, name, kind }
    } else {
        InputOp::CopyDest { src, name, kind }
    };
    model.prompt = Some(Prompt::Input(InputPrompt {
        op,
        input: String::new(),
    }));
}

/// Open the rename (`r`) prompt on the focused selection, prefilled with
/// the current name. Renaming the session root is refused.
fn open_rename_prompt(model: &mut Model) {
    let Some((src, name, kind)) = focused_selection(model) else {
        return;
    };
    if src == model.root.path {
        model.message = Some(String::from("cannot rename the session root"));
        return;
    }
    let input = name.clone();
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::RenameTo { src, name, kind },
        input,
    }));
}

/// Open the delete (`d`) confirmation on the focused selection — or,
/// when the persisted toggle turned the question off, delete straight
/// away. Deleting the session root is refused either way.
fn open_delete_prompt(model: &mut Model, fs: &mut dyn Fs) {
    let Some((path, name, kind)) = focused_selection(model) else {
        return;
    };
    if path == model.root.path {
        model.message = Some(String::from("cannot delete the session root"));
        return;
    }
    if !model.settings.confirm_delete {
        delete_now(model, fs, &path, kind);
        return;
    }
    model.prompt = Some(Prompt::ConfirmDelete(ConfirmPrompt { path, name, kind }));
}

/// Delete `path` now: the confirmed (or unconfirmed-by-setting) single
/// delete, remembered for the repeat key.
fn delete_now(model: &mut Model, fs: &mut dyn Fs, path: &str, kind: FileKind) {
    model.last_op = Some(RepeatOp::Delete);
    let refresh = alloc::vec![String::from(parent_of(path))];
    run_op(model, fs, FileOp::delete(path, kind), refresh);
    prune_tag_if_gone(model, fs, path);
}

/// Run the batch delete of the tagged set now (the confirmed — or
/// unconfirmed-by-setting — batch), remembered for the repeat key.
fn batch_delete_now(model: &mut Model, fs: &mut dyn Fs) {
    model.last_op = Some(RepeatOp::Delete);
    let items = model.tags.entries().to_vec();
    let refresh: Vec<String> = items
        .iter()
        .map(|item| String::from(parent_of(&item.path)))
        .collect();
    run_batch(model, fs, Batch::delete(&items), refresh);
}

/// The `.` key: re-apply the last completed file operation to the
/// focused selection — a copy or move into the remembered destination
/// directory, or a delete (which still asks per the confirmation
/// setting).
fn repeat_last_op(model: &mut Model, fs: &mut dyn Fs) {
    let Some(op) = model.last_op.clone() else {
        model.message = Some(String::from("nothing to repeat"));
        return;
    };
    match op {
        RepeatOp::CopyInto(dest) => repeat_transfer(model, fs, &dest, false),
        RepeatOp::MoveInto(dest) => repeat_transfer(model, fs, &dest, true),
        RepeatOp::Delete => open_delete(model, fs),
    }
}

/// Repeat a copy/move of the focused selection into `dest` — the same
/// planning, refusals, and overwrite questions as a typed destination.
fn repeat_transfer(model: &mut Model, fs: &mut dyn Fs, dest: &str, moving: bool) {
    let Some((src, _, kind)) = focused_selection(model) else {
        return;
    };
    if moving && src == model.root.path {
        model.message = Some(String::from("cannot move the session root"));
        return;
    }
    submit_transfer(model, fs, &src, kind, dest, moving);
}

/// The `V` key: fetch the published storage roots and open the volume
/// list. An empty report is a message, not an empty screen.
fn open_volumes(model: &mut Model, fs: &mut dyn Fs) {
    let volumes = fs.list_volumes();
    if volumes.is_empty() {
        model.message = Some(String::from("no volumes reported"));
        return;
    }
    model.volumes = volumes;
    model.volume_cursor = 0;
    model.overlay = Overlay::Volumes;
}

/// Apply one key to the volume list: arrows/`j`/`k` move, Enter opens
/// the chosen root, Esc/`q`/`V` closes the list.
fn handle_volumes_overlay(model: &mut Model, fs: &mut dyn Fs, event: &Event) {
    match event {
        Event::Esc | Event::Char('q' | 'V') => model.overlay = Overlay::None,
        Event::Up | Event::Char('k') => {
            model.volume_cursor = step(model.volume_cursor, -1, model.volumes.len());
        }
        Event::Down | Event::Char('j') => {
            model.volume_cursor = step(model.volume_cursor, 1, model.volumes.len());
        }
        Event::Enter => {
            let Some(volume) = model.volumes.get(model.volume_cursor) else {
                return;
            };
            let target = volume.target.clone();
            open_volume_root(model, fs, &target);
        }
        _ => {}
    }
}

/// Re-root the session at `target` (a chosen volume's mount point): the
/// tree, file pane, cursors, and space figure all restart there. A
/// refused listing keeps the session where it stands — the list stays
/// open and the errno lands on the message line.
fn open_volume_root(model: &mut Model, fs: &mut dyn Fs, target: &str) {
    match fs.list_dir(target) {
        Ok(entries) => {
            model.root = DirNode {
                name: String::from(target),
                path: String::from(target),
                expanded: true,
                children: Some(child_dirs_of(target, &entries)),
            };
            model.files_dir = String::from(target);
            model.files = entries;
            model.sort_files();
            model.pane = Pane::Tree;
            model.tree_cursor = 0;
            model.tree_scroll = 0;
            model.file_cursor = 0;
            model.file_scroll = 0;
            model.filter = None;
            model.space = fs.volume_space(target);
            model.overlay = Overlay::None;
            model.message = Some(format!("opened {target}"));
        }
        Err(errno) => model.report(target, errno),
    }
}

/// Apply one key to the settings menu: `1`/`2` toggle a confirmation
/// (persisted immediately), Esc/`q`/`S` closes.
fn handle_settings_overlay(model: &mut Model, fs: &mut dyn Fs, event: &Event) {
    match event {
        Event::Esc | Event::Char('q' | 'S') => model.overlay = Overlay::None,
        Event::Char('1') => {
            model.settings.confirm_delete = !model.settings.confirm_delete;
            persist_settings(model, fs);
        }
        Event::Char('2') => {
            model.settings.confirm_batch_delete = !model.settings.confirm_batch_delete;
            persist_settings(model, fs);
        }
        _ => {}
    }
}

/// Write the settings to the user's `Settings/fstree/` through the seam.
/// A failure (or an unknown home) keeps the change for this session and
/// says so — the toggle the user made is never silently reverted.
fn persist_settings(model: &mut Model, fs: &mut dyn Fs) {
    match model.settings_home.clone() {
        Some(home) => match model.settings.store(fs, &home) {
            Ok(()) => model.message = Some(String::from("settings saved")),
            Err(errno) => {
                model.message = Some(format!(
                    "settings not saved ({errno:?}) — in effect this session"
                ));
            }
        },
        None => {
            model.message = Some(String::from(
                "no home directory — settings apply to this session only",
            ));
        }
    }
}

/// Apply one key to the open mode prompt: octal digits and Backspace
/// edit, Enter applies through the seam, Esc cancels. The kernel decides
/// whether the change is allowed; a refusal is surfaced verbatim and the
/// entry is left unchanged (the prompt closes either way — the user's
/// input survives on the message line's report, not as hidden state).
fn handle_mode_prompt(model: &mut Model, fs: &mut dyn Fs, mut prompt: ModePrompt, event: &Event) {
    match event {
        Event::Esc => {}
        Event::Backspace => {
            prompt.input.pop();
            model.prompt = Some(Prompt::Mode(prompt));
        }
        Event::Char(digit @ '0'..='7') => {
            if prompt.input.len() < MODE_DIGITS_MAX {
                prompt.input.push(*digit);
            }
            model.prompt = Some(Prompt::Mode(prompt));
        }
        Event::Enter => {
            // At most four octal digits can be typed, so the parse fails
            // only on empty input and the value never exceeds `0o7777`.
            let Ok(mode) = u32::from_str_radix(&prompt.input, 8) else {
                model.message = Some(String::from("mode: empty — nothing applied"));
                return;
            };
            match fs.set_mode(&prompt.path, mode) {
                Ok(()) => {
                    model.message = Some(format!("mode of {} now {mode:o}", prompt.name));
                    // The attributes editor shows the same entry's bits;
                    // keep its mode line current with the applied change.
                    if let Some(view) = model.attrs.as_mut() {
                        if view.path == prompt.path {
                            view.mode = mode;
                        }
                    }
                }
                Err(errno) => model.report(&prompt.path, errno),
            }
        }
        // Any other key (a non-octal digit included) is ignored — the
        // prompt accepts only what the kernel could accept.
        _ => model.prompt = Some(Prompt::Mode(prompt)),
    }
}

/// Apply one key to an open text prompt: printable keys and Backspace
/// edit, Enter submits the answer, Esc cancels with nothing changed —
/// except the live filter prompt, whose edits show immediately and whose
/// Esc restores the filter held before the prompt opened.
fn handle_input_prompt(model: &mut Model, fs: &mut dyn Fs, mut prompt: InputPrompt, event: &Event) {
    match event {
        Event::Esc => {
            if let InputOp::FilterPattern { previous } = prompt.op {
                model.filter = previous;
                clamp_cursors(model);
            }
        }
        Event::Backspace => {
            prompt.input.pop();
            live_apply_filter(model, &prompt);
            model.prompt = Some(Prompt::Input(prompt));
        }
        Event::Char(ch) if !ch.is_control() => {
            if prompt.input.len() < INPUT_MAX {
                prompt.input.push(*ch);
            }
            live_apply_filter(model, &prompt);
            model.prompt = Some(Prompt::Input(prompt));
        }
        Event::Tab => {
            complete_destination(model, fs, &mut prompt);
            model.prompt = Some(Prompt::Input(prompt));
        }
        Event::Enter => submit_input(model, fs, prompt),
        _ => model.prompt = Some(Prompt::Input(prompt)),
    }
}

/// The shared completion engine's directory seam over the app's [`Fs`]:
/// the engine lists through a shared reference, the seam mutates through
/// its one-slot caches, so a `RefCell` bridges the two — borrowed only
/// inside `list_dir`, never held across calls. A relative directory is
/// joined onto the prompt's base, exactly as the submit path resolves
/// the typed destination.
struct SeamLister<'a> {
    fs: core::cell::RefCell<&'a mut dyn Fs>,
    base: &'a str,
}

impl rustos_complete::DirLister for SeamLister<'_> {
    fn list_dir(&self, dir: &str) -> Result<Vec<rustos_complete::DirEntryInfo>, Errno> {
        let resolved = if dir.starts_with('/') {
            String::from(dir)
        } else {
            join(self.base, dir)
        };
        let entries = self.fs.borrow_mut().list_dir(&resolved)?;
        Ok(entries
            .into_iter()
            .map(|entry| rustos_complete::DirEntryInfo {
                is_dir: entry.kind == FileKind::Directory,
                name: entry.name,
            })
            .collect())
    }
}

/// How many candidate names the message line lists before eliding the
/// rest — a display bound, not a completion bound.
const COMPLETION_LISTED_MAX: usize = 6;

/// Tab in a destination prompt: complete the typed path against the
/// directory it names (relative spellings against the same base the
/// submit resolves onto). A unique candidate replaces the word (a
/// directory staying open with its `/`); several extend to their longest
/// common prefix or are listed on the message line; none is said plainly.
/// Prompts asking for something other than an existing path (a new name,
/// a pattern, a needle) do not complete.
fn complete_destination(model: &mut Model, fs: &mut dyn Fs, prompt: &mut InputPrompt) {
    let base = match &prompt.op {
        InputOp::CopyDest { .. } | InputOp::MoveDest { .. } => model.files_dir.clone(),
        InputOp::BatchDest { .. } => current_base(model),
        _ => return,
    };
    let lister = SeamLister {
        fs: core::cell::RefCell::new(fs),
        base: &base,
    };
    let matches = rustos_complete::path_matches(&prompt.input, &base, &lister);
    let (dir_part, _) = rustos_complete::split_path_word(&prompt.input);
    let completed = |name: &str, is_dir: bool| {
        let mut text = String::from(dir_part);
        text.push_str(name);
        if is_dir {
            text.push('/');
        }
        text
    };
    match matches.as_slice() {
        [] => model.message = Some(String::from("no completion")),
        [only] => {
            let text = completed(&only.name, only.is_dir);
            if text.len() <= INPUT_MAX {
                prompt.input = text;
            }
        }
        many => {
            let common = rustos_complete::common_prefix(many.iter().map(|e| e.name.as_str()));
            let extended = format!("{dir_part}{common}");
            if extended.len() > prompt.input.len() && extended.len() <= INPUT_MAX {
                prompt.input = extended;
                return;
            }
            let mut shown: Vec<&str> = many
                .iter()
                .take(COMPLETION_LISTED_MAX)
                .map(|e| e.name.as_str())
                .collect();
            let more = many.len() - shown.len();
            let mut text = shown.join("  ");
            if more > 0 {
                shown.pop();
                text = format!("{} (+{more} more)", shown.join("  "));
            }
            model.message = Some(text);
        }
    }
}

/// Re-apply the filter prompt's text to the file pane as typed, so the
/// narrowing is visible live while the prompt is open.
fn live_apply_filter(model: &mut Model, prompt: &InputPrompt) {
    if matches!(prompt.op, InputOp::FilterPattern { .. }) {
        model.filter = NameFilter::from_text(&prompt.input);
        clamp_cursors(model);
    }
}

/// Act on a submitted text prompt: plan, validate, and run the operation
/// it names. Every refusal lands on the message line with nothing changed.
fn submit_input(model: &mut Model, fs: &mut dyn Fs, prompt: InputPrompt) {
    match prompt.op {
        InputOp::CopyDest { src, kind, .. } => {
            submit_transfer(model, fs, &src, kind, &prompt.input, false);
        }
        InputOp::MoveDest { src, kind, .. } => {
            submit_transfer(model, fs, &src, kind, &prompt.input, true);
        }
        InputOp::RenameTo { src, name, kind } => {
            let typed = prompt.input;
            if typed.is_empty() || typed == "." || typed == ".." || typed.contains('/') {
                model.message = Some(String::from("rename: invalid name"));
                return;
            }
            if typed == name {
                model.message = Some(String::from("rename: name unchanged"));
                return;
            }
            let target = join(parent_of(&src), &typed);
            run_op(
                model,
                fs,
                FileOp::move_to(&src, kind, &target),
                alloc::vec![String::from(parent_of(&src))],
            );
        }
        InputOp::MkdirName => {
            let typed = prompt.input;
            if typed.is_empty() || typed == "." || typed == ".." || typed.contains('/') {
                model.message = Some(String::from("mkdir: invalid name"));
                return;
            }
            let path = join(&model.files_dir, &typed);
            match fs.mkdir(&path) {
                Ok(()) => {
                    model.message = Some(format!("created directory {typed}"));
                    let listed = [model.files_dir.clone()];
                    refresh_after(model, fs, &listed);
                }
                Err(errno) => model.report(&path, errno),
            }
        }
        InputOp::TagGlob => submit_tag_glob(model, &prompt.input),
        InputOp::FilterPattern { .. } => submit_filter(model, &prompt.input),
        InputOp::SearchGlob => submit_name_search(model, &prompt.input),
        InputOp::ViewerGoto { kind } => submit_viewer_goto(model, fs, kind, &prompt.input),
        InputOp::ViewerSearch { kind } => submit_viewer_search(model, fs, kind, &prompt.input),
        InputOp::ContentNeedle { tagged } => submit_content_search(model, tagged, &prompt.input),
        InputOp::BatchDest { moving, .. } => submit_batch_dest(model, fs, moving, &prompt.input),
    }
}

/// Plan and run a copy or move of `src` to the typed destination: the
/// spelling is normalised through the shared path grammar (relative
/// spellings land in the listed directory), the target planned (an
/// existing directory receives the source inside it), and the self/subtree
/// refusals applied before any I/O.
fn submit_transfer(
    model: &mut Model,
    fs: &mut dyn Fs,
    src: &str,
    kind: FileKind,
    typed: &str,
    moving: bool,
) {
    if typed.is_empty() {
        model.message = Some(String::from("empty destination — nothing done"));
        return;
    }
    let dst = match resolve_destination(&model.files_dir, typed) {
        Ok(dst) => dst,
        Err(error) => {
            model.message = Some(error.describe());
            return;
        }
    };
    let target = match plan_target(fs, src, kind, &dst) {
        Ok(target) => target,
        Err(error) => {
            model.message = Some(error.describe());
            return;
        }
    };
    // The repeat key re-applies the operation into the same directory.
    let dest_dir = String::from(parent_of(&target));
    model.last_op = Some(if moving {
        RepeatOp::MoveInto(dest_dir.clone())
    } else {
        RepeatOp::CopyInto(dest_dir.clone())
    });
    let refresh = alloc::vec![String::from(parent_of(src)), dest_dir];
    let op = if moving {
        FileOp::move_to(src, kind, &target)
    } else {
        FileOp::copy(src, kind, &target)
    };
    run_op(model, fs, op, refresh);
    if moving {
        prune_tag_if_gone(model, fs, src);
    }
}

/// Apply one key to the delete confirmation: only `y`/`Y` proceeds; any
/// other key declines with nothing changed — never an assumed yes.
fn handle_confirm_prompt(
    model: &mut Model,
    fs: &mut dyn Fs,
    prompt: &ConfirmPrompt,
    event: &Event,
) {
    match event {
        Event::Char('y' | 'Y') => delete_now(model, fs, &prompt.path, prompt.kind),
        _ => model.message = Some(format!("{} not deleted", prompt.name)),
    }
}

/// Apply one key to the batch delete confirmation: only `y`/`Y` proceeds
/// with a batch delete of the tagged set; any other key declines with
/// nothing changed — never an assumed yes.
fn handle_confirm_batch_delete(model: &mut Model, fs: &mut dyn Fs, count: usize, event: &Event) {
    match event {
        Event::Char('y' | 'Y') => batch_delete_now(model, fs),
        _ => model.message = Some(format!("{count} tagged entries not deleted")),
    }
}

/// Apply one key to a paused batch's overwrite question: o)verwrite,
/// s)kip, or c)ancel (Esc cancels too — the batch's *remaining entries*
/// are dropped, work already applied stays); any other key keeps asking.
fn handle_batch_overwrite(
    model: &mut Model,
    fs: &mut dyn Fs,
    mut paused: BatchPrompt,
    event: &Event,
) {
    let decision = match event {
        Event::Char('o' | 'O') => Decision::Overwrite,
        Event::Char('s' | 'S') => Decision::Skip,
        Event::Char('c' | 'C') | Event::Esc => Decision::Cancel,
        _ => {
            model.prompt = Some(Prompt::BatchOverwrite(paused));
            return;
        }
    };
    paused.batch.resolve(decision);
    run_batch(model, fs, paused.batch, paused.refresh);
}

/// Drive `batch` until every entry is processed or an overwrite question
/// pauses it; on completion the report lands on the message line (and the
/// report overlay when any entry failed), succeeded sources are untagged,
/// and the panes refresh.
fn run_batch(model: &mut Model, fs: &mut dyn Fs, mut batch: Batch, refresh: Vec<String>) {
    match batch.advance(fs) {
        BatchProgress::Done => {
            for src in batch.succeeded() {
                model.tags.remove_under(src);
            }
            model.message = Some(batch.summary());
            if !batch.failures().is_empty() {
                model.report_lines = batch.failures().to_vec();
                model.overlay = Overlay::Report;
            }
            refresh_after(model, fs, &refresh);
            // The flat listing may now name moved or deleted entries;
            // a fresh walk keeps it honest rather than showing stale rows.
            if model.view == View::Flat {
                if let Some(walk) = &model.walk {
                    let root = walk.root.clone();
                    start_flat_walk(model, &root);
                }
            }
        }
        BatchProgress::NeedsDecision => {
            model.prompt = Some(Prompt::BatchOverwrite(BatchPrompt { batch, refresh }));
        }
    }
}

/// Untag `path` (and anything beneath it) once it is verifiably gone —
/// after a delete or move — so tags never point at removed entries. A
/// path that still exists (the operation failed or was skipped) keeps its
/// tag; only a confirmed absence prunes.
fn prune_tag_if_gone(model: &mut Model, fs: &mut dyn Fs, path: &str) {
    if matches!(fs.stat_kind(path), Err(Errno::NotFound)) {
        model.tags.remove_under(path);
    }
}

/// The directory a walk (`u`/`v`) descends below: the tree pane's
/// selected row, or the listed directory when the file pane has focus.
fn focused_dir(model: &Model) -> String {
    match model.pane {
        Pane::Tree => model
            .tree_rows()
            .get(model.tree_cursor)
            .map_or_else(|| model.files_dir.clone(), |row| row.path.clone()),
        Pane::Files => model.files_dir.clone(),
    }
}

/// The directory a typed relative destination joins onto: the walk root
/// in the flattened view, the listed directory otherwise.
fn current_base(model: &Model) -> String {
    if model.view == View::Flat {
        if let Some(walk) = &model.walk {
            return walk.root.clone();
        }
    }
    model.files_dir.clone()
}

/// The `t` key in the panes: toggle the tag on the file pane's selected
/// entry and step the cursor down (so repeated presses mark a run). The
/// tree pane's rows are the navigation skeleton, not taggable entries.
fn toggle_tag(model: &mut Model) {
    if model.pane != Pane::Files {
        model.message = Some(String::from("tags mark file-pane entries"));
        return;
    }
    let Some((path, kind, size)) = model
        .visible_files()
        .get(model.file_cursor)
        .map(|entry| (join(&model.files_dir, &entry.name), entry.kind, entry.size))
    else {
        return;
    };
    model.tags.toggle(TagEntry { path, kind, size });
    let count = model.visible_files().len();
    model.file_cursor = step(model.file_cursor, 1, count);
}

/// The `T` key: open the tag-by-pattern prompt.
fn open_tag_glob(model: &mut Model) {
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::TagGlob,
        input: String::new(),
    }));
}

/// The `i` key in the panes: toggle the tag of every visible file-pane
/// entry, so the tagged and untagged sets swap.
fn invert_tags(model: &mut Model) {
    let items: Vec<TagEntry> = model
        .visible_files()
        .iter()
        .map(|entry| TagEntry {
            path: join(&model.files_dir, &entry.name),
            kind: entry.kind,
            size: entry.size,
        })
        .collect();
    for item in items {
        model.tags.toggle(item);
    }
    model.message = Some(format!("{} tagged", model.tags.count()));
}

/// A submitted tag-by-pattern answer: tag every visible entry whose name
/// (panes) or branch-relative path (flattened view) matches the glob —
/// or, for a `size:`/`date:` range spelling, whose listed figures fall
/// inside the range. A malformed pattern or range is reported and tags
/// nothing.
fn submit_tag_glob(model: &mut Model, typed: &str) {
    match TagRange::parse(typed) {
        Ok(Some(range)) => {
            tag_by_range(model, &range);
            return;
        }
        Ok(None) => {}
        Err(reason) => {
            model.message = Some(reason);
            return;
        }
    }
    let pattern = match Pattern::new(typed) {
        Ok(pattern) => pattern,
        Err(error) => {
            model.message = Some(format!("bad pattern: {error}"));
            return;
        }
    };
    let items: Vec<TagEntry> = if model.view == View::Flat {
        let Some(walk) = &model.walk else {
            return;
        };
        walk.entries
            .iter()
            .filter(|entry| pattern.matches(relative_to(&walk.root, &entry.path)))
            .map(|entry| TagEntry {
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
            })
            .collect()
    } else {
        model
            .visible_files()
            .iter()
            .filter(|entry| pattern.matches(&entry.name))
            .map(|entry| TagEntry {
                path: join(&model.files_dir, &entry.name),
                kind: entry.kind,
                size: entry.size,
            })
            .collect()
    };
    let matched = items.len();
    for item in items {
        model.tags.insert(item);
    }
    model.message = Some(format!(
        "tagged {matched} matching, {} tagged in all",
        model.tags.count()
    ));
}

/// Tag every visible entry (panes) or listed row (flattened view) whose
/// listed size and modification stamp fall inside `range` — the same
/// scope the glob form tags over.
fn tag_by_range(model: &mut Model, range: &TagRange) {
    let items: Vec<TagEntry> = if model.view == View::Flat {
        let Some(walk) = &model.walk else {
            return;
        };
        walk.entries
            .iter()
            .filter(|entry| range.matches(entry.size, entry.modified))
            .map(|entry| TagEntry {
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
            })
            .collect()
    } else {
        model
            .visible_files()
            .iter()
            .filter(|entry| range.matches(entry.size, entry.modified))
            .map(|entry| TagEntry {
                path: join(&model.files_dir, &entry.name),
                kind: entry.kind,
                size: entry.size,
            })
            .collect()
    };
    let matched = items.len();
    for item in items {
        model.tags.insert(item);
    }
    model.message = Some(format!(
        "tagged {matched} in range, {} tagged in all",
        model.tags.count()
    ));
}

/// A submitted batch destination: the spelling is normalised through the
/// shared path grammar, must name an existing directory (a batch lands
/// its entries *inside* it), and the batch then runs over the tagged set
/// in tag order.
fn submit_batch_dest(model: &mut Model, fs: &mut dyn Fs, moving: bool, typed: &str) {
    if typed.is_empty() {
        model.message = Some(String::from("empty destination — nothing done"));
        return;
    }
    let dst = match resolve_destination(&current_base(model), typed) {
        Ok(dst) => dst,
        Err(error) => {
            model.message = Some(error.describe());
            return;
        }
    };
    match fs.stat_kind(&dst) {
        Ok(FileKind::Directory) => {}
        Ok(_) => {
            model.message = Some(String::from(
                "batch destination must be an existing directory",
            ));
            return;
        }
        Err(errno) => {
            model.report(&dst, errno);
            return;
        }
    }
    // The repeat key re-applies the operation into the same directory.
    model.last_op = Some(if moving {
        RepeatOp::MoveInto(dst.clone())
    } else {
        RepeatOp::CopyInto(dst.clone())
    });
    let items = model.tags.entries().to_vec();
    let mut refresh: Vec<String> = items
        .iter()
        .map(|item| String::from(parent_of(&item.path)))
        .collect();
    refresh.push(dst.clone());
    let batch = if moving {
        Batch::move_to(&items, &dst)
    } else {
        Batch::copy(&items, &dst)
    };
    run_batch(model, fs, batch, refresh);
}

/// The `f` key: open the live filter prompt, pre-filled with the active
/// filter so it can be edited; the filter as it stood is restored on Esc.
fn open_filter_prompt(model: &mut Model) {
    let previous = model.filter.clone();
    let input = previous
        .as_ref()
        .map_or_else(String::new, |f| f.text.clone());
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::FilterPattern { previous },
        input,
    }));
}

/// The `/` key: open the branch filename-search prompt.
fn open_search_prompt(model: &mut Model) {
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::SearchGlob,
        input: String::new(),
    }));
}

/// The `F` key: open the content-search prompt. The scope is decided
/// here — the tagged set when anything is tagged, the focused branch
/// otherwise — and the question says which.
fn open_content_prompt(model: &mut Model) {
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::ContentNeedle {
            tagged: !model.tags.is_empty(),
        },
        input: String::new(),
    }));
}

/// A submitted (Entered) filter: it is already applied live; the message
/// line reports what it shows.
fn submit_filter(model: &mut Model, typed: &str) {
    model.message = Some(match &model.filter {
        None => String::from("filter cleared"),
        Some(filter) if filter.pattern.is_none() => {
            format!("filter {typed}: bad pattern — showing everything")
        }
        Some(_) => format!("filter {typed}: {} shown", model.visible_files().len()),
    });
}

/// A submitted filename-search pattern: walk the focused branch listing
/// every file whose branch-relative path matches. A malformed pattern is
/// reported and searches nothing.
fn submit_name_search(model: &mut Model, typed: &str) {
    if typed.is_empty() {
        model.message = Some(String::from("empty pattern — nothing searched"));
        return;
    }
    let pattern = match Pattern::new(typed) {
        Ok(pattern) => pattern,
        Err(error) => {
            model.message = Some(format!("bad pattern: {error}"));
            return;
        }
    };
    let root = focused_dir(model);
    enter_flat(model);
    model.walk = Some(WalkState::name_search(&root, typed, pattern, FLAT_PAGE));
}

/// A submitted content-search needle: stream through the tagged set
/// (tagged files directly, tagged directories walked) or the focused
/// branch, listing every file whose contents contain the needle.
fn submit_content_search(model: &mut Model, tagged: bool, typed: &str) {
    let Some(needle) = Needle::new(typed) else {
        model.message = Some(String::from("empty text — nothing searched"));
        return;
    };
    let scan = ContentScan::new(needle);
    let (root, walker, seeds) = if tagged {
        let mut dirs = Vec::new();
        let mut seeds = Vec::new();
        for item in model.tags.entries() {
            if item.kind.is_dir() {
                dirs.push(item.path.clone());
            } else {
                seeds.push(FlatEntry {
                    path: item.path.clone(),
                    kind: item.kind,
                    size: item.size,
                    modified: rustos_abi::time::Time64::UNIX_EPOCH,
                    note: None,
                });
            }
        }
        (model.root.path.clone(), Walker::from_seeds(dirs), seeds)
    } else {
        let root = focused_dir(model);
        let walker = Walker::new(&root);
        (root, walker, Vec::new())
    };
    enter_flat(model);
    model.walk = Some(WalkState::content_search(
        &root, typed, scan, walker, seeds, FLAT_PAGE,
    ));
}

/// The `u`/`v` keys: start a walk of the focused directory — counting
/// only (`u`), or feeding the flattened branch view (`v`).
fn start_walk(model: &mut Model, purpose: WalkPurpose) {
    let root = focused_dir(model);
    match purpose {
        WalkPurpose::Flat => start_flat_walk(model, &root),
        WalkPurpose::Usage => {
            model.walk = Some(WalkState::new(&root, WalkPurpose::Usage, FLAT_PAGE));
        }
    }
}

/// Enter (or restart) the flattened branch view below `root`.
fn start_flat_walk(model: &mut Model, root: &str) {
    enter_flat(model);
    model.walk = Some(WalkState::new(root, WalkPurpose::Flat, FLAT_PAGE));
}

/// Switch to the flattened view with its cursor reset.
fn enter_flat(model: &mut Model) {
    model.view = View::Flat;
    model.flat_cursor = 0;
    model.flat_scroll = 0;
}

/// Esc in the panes: cancel a live usage walk, keeping (and reporting)
/// the figures counted so far.
fn cancel_usage_walk(model: &mut Model) {
    if let Some(walk) = model.walk.take() {
        model.message = Some(format!(
            "usage of {} cancelled at {}",
            walk.root,
            walk.figures()
        ));
    }
}

/// Advance a live walk by one bounded tick. A finished usage walk lands
/// its figures on the message line (and its unreadable-directory lines on
/// the report overlay); a finished flat walk keeps its listing shown.
pub fn walk_tick(model: &mut Model, fs: &mut dyn Fs) {
    let Some(mut walk) = model.walk.take() else {
        return;
    };
    walk.tick(fs, WALK_DIRS_PER_TICK, SCAN_BYTES_PER_TICK);
    if walk.done && walk.purpose == WalkPurpose::Usage {
        model.message = Some(format!("usage of {}: {}", walk.root, walk.figures()));
        if !walk.walker.errors.is_empty() {
            model.report_lines.clone_from(&walk.walker.errors);
            model.overlay = Overlay::Report;
        }
    } else {
        model.walk = Some(walk);
    }
}

/// Apply one key inside the flattened branch view.
fn handle_flat_key(model: &mut Model, fs: &mut dyn Fs, event: &Event) {
    match event {
        Event::Esc => {
            // Esc first stops a live walk in place (the results found so
            // far stand and stay browsable); a second Esc leaves the view.
            if model.walk.as_ref().is_some_and(WalkState::ticking) {
                if let Some(walk) = &mut model.walk {
                    walk.stop();
                    model.message = Some(format!("stopped: {}", walk.figures()));
                }
                return;
            }
            exit_flat(model);
        }
        Event::Char('q') => exit_flat(model),
        Event::Enter => jump_to_flat_hit(model, fs),
        Event::Up | Event::Char('k') => move_flat_cursor(model, -1),
        Event::Down | Event::Char('j') => move_flat_cursor(model, 1),
        Event::Char(' ') => {
            if let Some(walk) = &mut model.walk {
                walk.resume();
            }
        }
        Event::Char('t') => flat_toggle_tag(model),
        Event::Char('T') => open_tag_glob(model),
        Event::Char('i') => flat_invert_tags(model),
        Event::Char('C') => {
            model.tags.clear();
            model.message = Some(String::from("tags cleared"));
        }
        Event::Char('c') => open_flat_batch(model, false),
        Event::Char('m') => open_flat_batch(model, true),
        Event::Char('d') => open_flat_delete(model, fs),
        Event::Char('?') => model.overlay = Overlay::Help,
        _ => {}
    }
}

/// Leave the flattened view for the panes, dropping the walk.
fn exit_flat(model: &mut Model) {
    model.view = View::Panes;
    model.walk = None;
    model.flat_cursor = 0;
    model.flat_scroll = 0;
}

/// Enter on a flattened row: jump to the hit's directory in the panes,
/// landing the file cursor on the hit. A directory that refuses to list
/// reports its error and keeps the flattened view — fail closed, never a
/// blank pane.
fn jump_to_flat_hit(model: &mut Model, fs: &mut dyn Fs) {
    let Some((path, root)) = model.walk.as_ref().and_then(|walk| {
        walk.entries
            .get(model.flat_cursor)
            .map(|entry| (entry.path.clone(), walk.root.clone()))
    }) else {
        return;
    };
    let parent = String::from(parent_of(&path));
    let name = path.rsplit('/').next().unwrap_or_default();
    if !select_dir(model, fs, &parent) {
        return;
    }
    exit_flat(model);
    reveal_in_tree(model, fs, &root, &parent);
    reveal_in_files(model, name);
    model.pane = Pane::Files;
}

/// Expand the tree along `root`..`parent` so the jumped-to directory has
/// a visible row, and put the tree cursor on it. Expansion is best-effort:
/// an ancestor that refuses to list simply stays collapsed — the file
/// pane already shows the destination.
fn reveal_in_tree(model: &mut Model, fs: &mut dyn Fs, root: &str, parent: &str) {
    let mut path = String::from(root);
    let rest = relative_to(root, parent);
    let steps = rest.split('/').filter(|c| !c.is_empty());
    for component in steps {
        if !populate_children(model, fs, &path) {
            break;
        }
        if let Some(node) = find_node(&mut model.root, &path) {
            node.expanded = true;
        }
        path = join(&path, component);
    }
    if let Some(index) = model.tree_rows().iter().position(|row| row.path == parent) {
        model.tree_cursor = index;
    }
    clamp_cursors(model);
}

/// Land the file cursor on `name`, lifting the hidden toggle or the
/// filter when either hides the hit — a jump lands on its subject, and
/// the change is visible in the status line, never silent.
fn reveal_in_files(model: &mut Model, name: &str) {
    let visible = |m: &Model| m.visible_files().iter().any(|e| e.name == name);
    if !visible(model) && name.starts_with('.') && !model.show_hidden {
        model.show_hidden = true;
    }
    if !visible(model) && model.filter.is_some() {
        model.filter = None;
        model.message = Some(String::from("filter cleared to reveal the hit"));
    }
    if let Some(index) = model.visible_files().iter().position(|e| e.name == name) {
        model.file_cursor = index;
    }
    clamp_cursors(model);
}

/// Move the flattened view's cursor by `delta` within the listed entries.
fn move_flat_cursor(model: &mut Model, delta: isize) {
    let count = model.walk.as_ref().map_or(0, |walk| walk.entries.len());
    model.flat_cursor = step(model.flat_cursor, delta, count);
}

/// The `t` key in the flattened view: toggle the tag on the selected row
/// and step the cursor down.
fn flat_toggle_tag(model: &mut Model) {
    let Some((path, kind, size, count)) = model.walk.as_ref().and_then(|walk| {
        walk.entries.get(model.flat_cursor).map(|entry| {
            (
                entry.path.clone(),
                entry.kind,
                entry.size,
                walk.entries.len(),
            )
        })
    }) else {
        return;
    };
    model.tags.toggle(TagEntry { path, kind, size });
    model.flat_cursor = step(model.flat_cursor, 1, count);
}

/// The `i` key in the flattened view: toggle the tag of every listed row.
fn flat_invert_tags(model: &mut Model) {
    let items: Vec<TagEntry> = model.walk.as_ref().map_or_else(Vec::new, |walk| {
        walk.entries
            .iter()
            .map(|entry| TagEntry {
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
            })
            .collect()
    });
    for item in items {
        model.tags.toggle(item);
    }
    model.message = Some(format!("{} tagged", model.tags.count()));
}

/// The `c`/`m` keys in the flattened view: a batch over the tagged set
/// (the view's rows are batch operands; there is no single-selection
/// transfer here).
fn open_flat_batch(model: &mut Model, moving: bool) {
    if model.tags.is_empty() {
        model.message = Some(String::from("no tagged entries"));
        return;
    }
    model.prompt = Some(Prompt::Input(InputPrompt {
        op: InputOp::BatchDest {
            moving,
            count: model.tags.count(),
        },
        input: String::new(),
    }));
}

/// The `d` key in the flattened view: the batch delete confirmation over
/// the tagged set.
fn open_flat_delete(model: &mut Model, fs: &mut dyn Fs) {
    if model.tags.is_empty() {
        model.message = Some(String::from("no tagged entries"));
        return;
    }
    if !model.settings.confirm_batch_delete {
        batch_delete_now(model, fs);
        return;
    }
    model.prompt = Some(Prompt::ConfirmBatchDelete {
        count: model.tags.count(),
    });
}

/// Open the viewer the head sample picks on the regular file at `path`:
/// the disassembly viewer for a recognised executable container or a
/// standalone signed manifest, the text pager for NUL-free, valid-UTF-8
/// heads, the hex dump for everything else. A refused read reports and
/// opens nothing.
fn open_viewer(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode, path: &str, size: u64) {
    let mut buf = [0_u8; HEAD_SAMPLE];
    let read = match fs.read(path, 0, &mut buf) {
        Ok(read) => read,
        Err(errno) => {
            model.report(path, errno);
            return;
        }
    };
    let head = &buf[..read];
    if rustos_binfmt::detect(head).is_some() || is_manifest_head(head) {
        open_disasm(model, fs, decode, path, size);
        return;
    }
    let viewer = if is_text_head(head) {
        Viewer::Text(TextView::new(path, size))
    } else {
        Viewer::Hex(HexView::new(path, size))
    };
    model.viewer = Some(viewer);
    model.view = View::Viewer;
}

/// Open the disassembly viewer on `path` through the sandboxed decode; a
/// refused or failed decode falls back to the hex view with a one-line
/// notice — never an error dialog, never a crash.
fn open_disasm(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode, path: &str, size: u64) {
    if size > MAX_INPUT as u64 {
        model.viewer = Some(Viewer::Hex(HexView::new(path, size)));
        model.view = View::Viewer;
        model.message = Some(String::from("too large to decode — showing hex"));
        return;
    }
    let bytes = match read_all(fs, path, size) {
        Ok(bytes) => bytes,
        Err(errno) => {
            model.report(path, errno);
            return;
        }
    };
    match DisasmView::open(decode, path, size, &bytes) {
        Ok(view) => {
            model.viewer = Some(Viewer::Disasm(view));
            model.view = View::Viewer;
        }
        Err(error) => {
            model.viewer = Some(Viewer::Hex(HexView::new(path, size)));
            model.view = View::Viewer;
            model.message = Some(format!("{} — showing hex", describe(error)));
        }
    }
}

/// Read the whole of `path` (at most [`MAX_INPUT`] bytes — the caller
/// checked) for the container/manifest summary decode.
fn read_all(fs: &mut dyn Fs, path: &str, size: u64) -> Result<Vec<u8>, Errno> {
    let len = usize::try_from(size.min(MAX_INPUT as u64)).unwrap_or(MAX_INPUT);
    let mut bytes = alloc::vec![0_u8; len];
    let mut filled = 0;
    while filled < len {
        let read = fs.read(path, filled as u64, &mut bytes[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// The `o` key: ask which viewer to open the focused regular file in.
fn open_open_as(model: &mut Model) {
    let entry = if model.pane == Pane::Files {
        model.visible_files().get(model.file_cursor).copied()
    } else {
        None
    };
    let Some(entry) = entry else {
        model.message = Some(String::from("open as: select a regular file"));
        return;
    };
    if entry.kind.is_dir() {
        model.message = Some(String::from("open as: select a regular file"));
        return;
    }
    model.prompt = Some(Prompt::OpenAs(OpenAsPrompt {
        path: join(&model.files_dir, &entry.name),
        size: entry.size,
    }));
}

/// Apply one key to the open-as chooser: t)ext, he(x), d)isassembly, Esc
/// cancels; any other key keeps asking.
fn handle_open_as(
    model: &mut Model,
    fs: &mut dyn Fs,
    decode: &mut dyn Decode,
    prompt: OpenAsPrompt,
    event: &Event,
) {
    match event {
        Event::Esc => {}
        Event::Char('t' | 'T') => {
            model.viewer = Some(Viewer::Text(TextView::new(&prompt.path, prompt.size)));
            model.view = View::Viewer;
        }
        Event::Char('x' | 'X') => {
            model.viewer = Some(Viewer::Hex(HexView::new(&prompt.path, prompt.size)));
            model.view = View::Viewer;
        }
        Event::Char('d' | 'D') => force_disasm(model, fs, decode, &prompt.path, prompt.size, 0),
        _ => model.prompt = Some(Prompt::OpenAs(prompt)),
    }
}

/// Force-open `path` in the disassembly viewer: a recognised container
/// or manifest decodes as itself; anything else asks for the ISA and
/// opens as a raw fragment at `offset`.
fn force_disasm(
    model: &mut Model,
    fs: &mut dyn Fs,
    decode: &mut dyn Decode,
    path: &str,
    size: u64,
    offset: u64,
) {
    let mut buf = [0_u8; HEAD_SAMPLE];
    let read = match fs.read(path, 0, &mut buf) {
        Ok(read) => read,
        Err(errno) => {
            model.report(path, errno);
            return;
        }
    };
    let head = &buf[..read];
    if rustos_binfmt::detect(head).is_some() || is_manifest_head(head) {
        open_disasm(model, fs, decode, path, size);
        return;
    }
    model.prompt = Some(Prompt::IsaPick(IsaPrompt {
        purpose: IsaPurpose::OpenRaw {
            path: String::from(path),
            size,
            offset,
        },
    }));
}

/// Apply one key to the ISA chooser: x)86-64, a)arch64, r)iscv64, w)asm,
/// Esc cancels; any other key keeps asking.
fn handle_isa_pick(model: &mut Model, prompt: IsaPrompt, event: &Event) {
    let isa = match event {
        Event::Esc => return,
        Event::Char('x' | 'X') => Isa::X86_64,
        Event::Char('a' | 'A') => Isa::Aarch64,
        Event::Char('r' | 'R') => Isa::Riscv64,
        Event::Char('w' | 'W') => Isa::Wasm,
        _ => {
            model.prompt = Some(Prompt::IsaPick(prompt));
            return;
        }
    };
    match prompt.purpose {
        IsaPurpose::OpenRaw { path, size, offset } => {
            model.viewer = Some(Viewer::Disasm(DisasmView::raw(&path, size, isa, offset)));
            model.view = View::Viewer;
        }
        IsaPurpose::EnterRegion { index } => {
            if let Some(Viewer::Disasm(view)) = &mut model.viewer {
                view.isa_choice = Some(isa);
                view.enter_region(index);
            }
        }
        IsaPurpose::Override => {
            if let Some(Viewer::Disasm(view)) = &mut model.viewer {
                view.set_isa(isa);
            }
        }
    }
}

/// Whether a head sample reads as text: no NUL byte, and valid UTF-8
/// except for a character the sample may have truncated.
fn is_text_head(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match core::str::from_utf8(head) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

/// Leave the viewer for the panes.
fn exit_viewer(model: &mut Model) {
    model.view = View::Panes;
    model.viewer = None;
}

/// The viewed file's path, for error reports.
fn viewer_path(model: &Model) -> String {
    match &model.viewer {
        Some(Viewer::Text(view)) => view.path.clone(),
        Some(Viewer::Hex(view)) => view.path.clone(),
        Some(Viewer::Disasm(view)) => view.path.clone(),
        None => String::new(),
    }
}

/// Whether the open viewer has a live background scan.
fn viewer_ticking(model: &Model) -> bool {
    match &model.viewer {
        Some(Viewer::Text(view)) => view.ticking(),
        Some(Viewer::Hex(view)) => view.ticking(),
        Some(Viewer::Disasm(view)) => view.ticking(),
        None => false,
    }
}

/// Apply one key inside the viewer.
fn handle_viewer_key(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode, event: &Event) {
    let page = match &model.viewer {
        Some(Viewer::Text(view)) => view.viewport_rows.max(1),
        Some(Viewer::Hex(view)) => view.viewport_rows.max(1),
        Some(Viewer::Disasm(view)) => view.viewport_rows.max(1),
        None => {
            exit_viewer(model);
            return;
        }
    };
    let page_rows = i64::try_from(page).unwrap_or(i64::MAX);
    match event {
        Event::Esc => {
            // Esc first stops a live scan in place; a second Esc leaves
            // the viewer.
            if viewer_ticking(model) {
                match &mut model.viewer {
                    Some(Viewer::Text(view)) => view.cancel_job(),
                    Some(Viewer::Hex(view)) => view.cancel_job(),
                    Some(Viewer::Disasm(view)) => view.cancel_job(),
                    None => {}
                }
                model.message = Some(String::from("stopped"));
                return;
            }
            // A container's code pane steps back to its summary page
            // first; raw mode has no summary and leaves the viewer.
            if let Some(Viewer::Disasm(view)) = &mut model.viewer {
                if view.pane == DisasmPane::Code && !matches!(view.body, DisasmBody::Raw) {
                    view.leave_code();
                    return;
                }
            }
            exit_viewer(model);
        }
        Event::Char('q') => exit_viewer(model),
        Event::Char('?') => model.overlay = Overlay::Help,
        Event::Up | Event::Char('k') => viewer_scroll(model, fs, decode, -1),
        Event::Down | Event::Char('j') => viewer_scroll(model, fs, decode, 1),
        Event::PageUp | Event::Char('b') => viewer_scroll(model, fs, decode, -page_rows),
        Event::PageDown | Event::Char(' ') => viewer_scroll(model, fs, decode, page_rows),
        Event::Home => match &mut model.viewer {
            Some(Viewer::Text(view)) => view.go_home(),
            Some(Viewer::Hex(view)) => view.go_home(),
            Some(Viewer::Disasm(view)) => match view.pane {
                DisasmPane::Summary => {
                    view.sum_cursor = 0;
                    view.sum_scroll = 0;
                }
                DisasmPane::Code => view.go_home(),
            },
            None => {}
        },
        Event::End => viewer_go_end(model, fs, page),
        Event::Char('g') => {
            let Some(kind) = viewer_prompt_kind(model) else {
                return;
            };
            model.prompt = Some(Prompt::Input(InputPrompt {
                op: InputOp::ViewerGoto { kind },
                input: String::new(),
            }));
        }
        Event::Char('/') => {
            let Some(kind) = viewer_prompt_kind(model) else {
                return;
            };
            model.prompt = Some(Prompt::Input(InputPrompt {
                op: InputOp::ViewerSearch { kind },
                input: String::new(),
            }));
        }
        Event::Char('n') => viewer_search_next(model, fs),
        Event::Char('w') => {
            if let Some(Viewer::Text(view)) = &mut model.viewer {
                view.wrap = !view.wrap;
                model.message = Some(String::from(if view.wrap { "wrap on" } else { "wrap off" }));
            }
        }
        Event::Char('x') => viewer_switch_hex(model),
        Event::Char('t') => viewer_switch_text(model, fs),
        Event::Char('d') => viewer_switch_disasm(model, fs, decode),
        Event::Char('I') => {
            if matches!(model.viewer, Some(Viewer::Disasm(_))) {
                model.prompt = Some(Prompt::IsaPick(IsaPrompt {
                    purpose: IsaPurpose::Override,
                }));
            }
        }
        Event::Enter => enter_summary_row(model),
        _ => {}
    }
}

/// Which viewer the goto/search prompt should ask for; the disassembly
/// summary page has neither (`None` opens no prompt).
fn viewer_prompt_kind(model: &Model) -> Option<ViewerKind> {
    match &model.viewer {
        Some(Viewer::Text(_)) => Some(ViewerKind::Text),
        Some(Viewer::Hex(_)) => Some(ViewerKind::Hex),
        Some(Viewer::Disasm(view)) if view.pane == DisasmPane::Code => Some(ViewerKind::Disasm),
        _ => None,
    }
}

/// Enter on the disassembly summary page: a code region opens in the
/// code pane (asking for the ISA when the container names none); a data
/// region shows as a hex dump at its file bytes.
fn enter_summary_row(model: &mut Model) {
    let Some(Viewer::Disasm(view)) = &mut model.viewer else {
        return;
    };
    if view.pane != DisasmPane::Summary {
        return;
    }
    let Some(region) = view.selected_region() else {
        return;
    };
    if region.kind == RegionKind::Data {
        if region.file_size == 0 {
            model.message = Some(String::from("region has no file bytes"));
            return;
        }
        let (path, size, offset) = (view.path.clone(), view.size, region.file_offset);
        model.viewer = Some(Viewer::Hex(HexView::at_offset(&path, size, offset)));
        return;
    }
    if region.file_size == 0 {
        model.message = Some(String::from("region has no code bytes"));
        return;
    }
    let index = view.sum_cursor;
    if view.isa().is_some() {
        view.enter_region(index);
    } else {
        model.prompt = Some(Prompt::IsaPick(IsaPrompt {
            purpose: IsaPurpose::EnterRegion { index },
        }));
    }
}

/// Scroll the viewer by `delta` display rows (negative is up). A refused
/// read reports and keeps the place.
fn viewer_scroll(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode, delta: i64) {
    let rows = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
    if let Some(Viewer::Disasm(view)) = &mut model.viewer {
        let result = match view.pane {
            DisasmPane::Summary => {
                let step = isize::try_from(delta).unwrap_or(isize::MAX);
                view.move_summary_cursor(step);
                Ok(())
            }
            DisasmPane::Code => {
                if delta.is_negative() {
                    view.scroll_up(fs, decode, rows)
                } else {
                    view.scroll_down(fs, decode, rows)
                }
            }
        };
        if let Err(error) = result {
            model.message = Some(describe(error));
        }
        return;
    }
    let result = match &mut model.viewer {
        Some(Viewer::Text(view)) => {
            if delta.is_negative() {
                view.scroll_up(fs, rows)
            } else {
                view.scroll_down(fs, rows)
            }
        }
        Some(Viewer::Hex(view)) => view.scroll(fs, delta),
        Some(Viewer::Disasm(_)) | None => Ok(()),
    };
    if let Err(errno) = result {
        let path = viewer_path(model);
        model.report(&path, errno);
    }
}

/// Jump the viewer to the file's end (`End`); the disassembly code pane
/// walks to its region's final page in the background.
fn viewer_go_end(model: &mut Model, fs: &mut dyn Fs, page: usize) {
    let result = match &mut model.viewer {
        Some(Viewer::Text(view)) => view.go_end(fs, page),
        Some(Viewer::Hex(view)) => view.go_end(fs, page),
        Some(Viewer::Disasm(view)) => {
            match view.pane {
                DisasmPane::Summary => {
                    let last = view.regions.len().saturating_sub(1);
                    view.sum_cursor = last;
                }
                DisasmPane::Code => view.go_end(),
            }
            Ok(())
        }
        None => Ok(()),
    };
    if let Err(errno) = result {
        let path = viewer_path(model);
        model.report(&path, errno);
    }
}

/// The `n` key: repeat the viewer's last search past its previous hit.
fn viewer_search_next(model: &mut Model, fs: &mut dyn Fs) {
    let started = match &mut model.viewer {
        Some(Viewer::Text(view)) => view.search_next(fs),
        Some(Viewer::Hex(view)) => view.search_next(),
        Some(Viewer::Disasm(view)) => view.pane == DisasmPane::Code && view.search_next(),
        None => return,
    };
    if !started {
        model.message = Some(String::from("no previous search"));
    }
}

/// The `x` key: show the same place as a hex dump.
fn viewer_switch_hex(model: &mut Model) {
    if let Some(Viewer::Text(view)) = &model.viewer {
        model.viewer = Some(Viewer::Hex(HexView::at_offset(
            &view.path,
            view.size,
            view.top.offset,
        )));
    } else if let Some(Viewer::Disasm(view)) = &model.viewer {
        model.viewer = Some(Viewer::Hex(HexView::at_offset(
            &view.path,
            view.size,
            view.switch_offset(),
        )));
    }
}

/// The `t` key: show the same place as text, snapped to its row start;
/// the line number is unknown until a goto-line re-anchors it.
fn viewer_switch_text(model: &mut Model, fs: &mut dyn Fs) {
    if let Some(Viewer::Hex(view)) = &model.viewer {
        let (path, size, top) = (view.path.clone(), view.size, view.top);
        model.viewer = Some(Viewer::Text(TextView::at_offset(fs, &path, size, top)));
    } else if let Some(Viewer::Disasm(view)) = &model.viewer {
        let (path, size, top) = (view.path.clone(), view.size, view.switch_offset());
        model.viewer = Some(Viewer::Text(TextView::at_offset(fs, &path, size, top)));
    }
}

/// The `d` key: show the same file in the disassembly viewer — the
/// container summary for a recognised executable/manifest, or (after an
/// ISA choice) a raw fragment starting at the current place.
fn viewer_switch_disasm(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode) {
    let (path, size, offset) = match &model.viewer {
        Some(Viewer::Text(view)) => (view.path.clone(), view.size, view.top.offset),
        Some(Viewer::Hex(view)) => (view.path.clone(), view.size, view.top),
        Some(Viewer::Disasm(_)) | None => return,
    };
    force_disasm(model, fs, decode, &path, size, offset);
}

/// A submitted viewer goto: a byte offset (decimal or `0x`-hex) for the
/// hex dump, a 1-based line number for the text pager, or an address for
/// the disassembly code pane (both scans run in the background).
fn submit_viewer_goto(model: &mut Model, fs: &mut dyn Fs, kind: ViewerKind, typed: &str) {
    match kind {
        ViewerKind::Hex => {
            let Some(offset) = parse_offset(typed) else {
                model.message = Some(String::from("goto: not an offset (decimal or 0x-hex)"));
                return;
            };
            let result = match &mut model.viewer {
                Some(Viewer::Hex(view)) => view.go_to(fs, offset),
                _ => Ok(()),
            };
            if let Err(errno) = result {
                let path = viewer_path(model);
                model.report(&path, errno);
            }
        }
        ViewerKind::Text => {
            let Ok(target) = typed.parse::<u64>() else {
                model.message = Some(String::from("goto: not a line number"));
                return;
            };
            if target == 0 {
                model.message = Some(String::from("goto: lines count from 1"));
                return;
            }
            if let Some(Viewer::Text(view)) = &mut model.viewer {
                view.start_goto(target);
            }
        }
        ViewerKind::Disasm => {
            let Some(target) = parse_offset(typed) else {
                model.message = Some(String::from("goto: not an address (decimal or 0x-hex)"));
                return;
            };
            if let Some(Viewer::Disasm(view)) = &mut model.viewer {
                if !view.start_goto(target) {
                    model.message = Some(String::from("goto: no code region holds that address"));
                }
            }
        }
    }
}

/// A submitted viewer search: literal text for the text pager; text or a
/// `0x…` byte sequence for the hex dump; mnemonic/operand text for the
/// disassembly code pane. The scan runs in the background; Esc stops it.
fn submit_viewer_search(model: &mut Model, fs: &mut dyn Fs, kind: ViewerKind, typed: &str) {
    let started = match (&mut model.viewer, kind) {
        (Some(Viewer::Text(view)), ViewerKind::Text) => view.start_search(fs, typed),
        (Some(Viewer::Hex(view)), ViewerKind::Hex) => view.start_search(typed),
        (Some(Viewer::Disasm(view)), ViewerKind::Disasm) => view.start_search(typed),
        _ => return,
    };
    if !started {
        model.message = Some(match kind {
            ViewerKind::Hex => String::from("search: text, or 0x followed by hex byte pairs"),
            ViewerKind::Text | ViewerKind::Disasm => String::from("empty text — nothing searched"),
        });
    }
}

/// Advance the viewer's live scan by one bounded tick, surfacing its
/// outcome on the message line.
pub fn viewer_tick(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode) {
    let outcome = match &mut model.viewer {
        Some(Viewer::Text(view)) => view.tick(fs, SCAN_BYTES_PER_TICK),
        Some(Viewer::Hex(view)) => view.tick(fs, SCAN_BYTES_PER_TICK),
        Some(Viewer::Disasm(view)) => match view.tick(fs, decode) {
            Ok(outcome) => outcome,
            Err(error) => {
                model.message = Some(describe(error));
                return;
            }
        },
        None => return,
    };
    match outcome {
        JobOutcome::Pending => {}
        JobOutcome::Moved => {
            model.message = Some(match &model.viewer {
                Some(Viewer::Text(view)) => match view.top.line {
                    Some(line) => format!("line {}", line + 1),
                    None => String::from("match found"),
                },
                Some(Viewer::Hex(view)) => match view.last_hit {
                    Some(hit) => format!("found at 0x{hit:x}"),
                    None => format!("offset 0x{:x}", view.top),
                },
                Some(Viewer::Disasm(view)) => match view.last_hit {
                    Some(hit) => format!("found at {hit:#x}"),
                    None => format!("address {:#x}", view.place.top),
                },
                None => return,
            });
        }
        JobOutcome::NotFound => model.message = Some(String::from("not found")),
        JobOutcome::PastEnd => {
            model.message = Some(String::from("past the last line — showing the end"));
        }
        JobOutcome::Failed(errno) => {
            let path = viewer_path(model);
            model.report(&path, errno);
        }
    }
}

/// Re-read the open viewer's page for the frame about to render. A
/// refused read (or a failed decode) closes the viewer and reports —
/// stale content is never shown as live.
pub fn refresh_viewer(
    model: &mut Model,
    fs: &mut dyn Fs,
    decode: &mut dyn Decode,
    rows: usize,
    cols: usize,
) {
    if model.view != View::Viewer {
        return;
    }
    if let Some(Viewer::Disasm(view)) = &mut model.viewer {
        if let Err(error) = view.refresh(fs, decode, rows) {
            exit_viewer(model);
            model.message = Some(describe(error));
        }
        return;
    }
    let result = match &mut model.viewer {
        Some(Viewer::Text(view)) => view.refresh(fs, rows, cols),
        Some(Viewer::Hex(view)) => view.refresh(fs, rows),
        Some(Viewer::Disasm(_)) | None => Ok(()),
    };
    if let Err(errno) = result {
        let path = viewer_path(model);
        exit_viewer(model);
        model.report(&path, errno);
    }
}

/// Apply one key to a paused operation's overwrite question: o)verwrite,
/// s)kip, or c)ancel (Esc cancels too); any other key keeps asking.
fn handle_overwrite_prompt(
    model: &mut Model,
    fs: &mut dyn Fs,
    mut paused: OverwritePrompt,
    event: &Event,
) {
    let decision = match event {
        Event::Char('o' | 'O') => Decision::Overwrite,
        Event::Char('s' | 'S') => Decision::Skip,
        Event::Char('c' | 'C') | Event::Esc => Decision::Cancel,
        _ => {
            model.prompt = Some(Prompt::Overwrite(paused));
            return;
        }
    };
    paused.op.resolve(decision);
    run_op(model, fs, paused.op, paused.refresh);
}

/// Drive `op` until it finishes, fails, or pauses on an overwrite
/// question; the panes are refreshed when it ends either way, because a
/// failed or cancelled operation may already have applied earlier steps.
fn run_op(model: &mut Model, fs: &mut dyn Fs, mut op: FileOp, refresh: Vec<String>) {
    match op.advance(fs) {
        OpProgress::Done => {
            model.message = Some(op.summary());
            refresh_after(model, fs, &refresh);
        }
        OpProgress::NeedsDecision => {
            model.prompt = Some(Prompt::Overwrite(OverwritePrompt { op, refresh }));
        }
        OpProgress::Failed(error) => {
            model.message = Some(error.describe());
            refresh_after(model, fs, &refresh);
        }
    }
}

/// Refresh the panes after an operation: re-list every affected tree node
/// that had been read, then revalidate the file pane (climbing to the
/// nearest listable ancestor when the listed directory itself is gone).
fn refresh_after(model: &mut Model, fs: &mut dyn Fs, dirs: &[String]) {
    for dir in dirs {
        reload_tree_node(model, fs, dir);
    }
    revalidate_file_pane(model, fs);
    clamp_cursors(model);
}

/// Re-read the children of the tree node at `path`, preserving the
/// expansion state of surviving branches. A node never read stays lazy; a
/// listing now refused empties the node and collapses it (fail closed —
/// stale rows are never shown as live).
fn reload_tree_node(model: &mut Model, fs: &mut dyn Fs, path: &str) {
    let populated = matches!(
        find_node(&mut model.root, path),
        Some(DirNode {
            children: Some(_),
            ..
        })
    );
    if !populated {
        return;
    }
    let listing = fs.list_dir(path);
    if let Some(node) = find_node(&mut model.root, path) {
        if let Ok(entries) = listing {
            let old = node.children.take().unwrap_or_default();
            node.children = Some(merge_child_dirs(path, &entries, old));
        } else {
            node.children = None;
            node.expanded = false;
        }
    }
}

/// Re-list the file pane's directory; when it no longer lists (deleted or
/// moved), climb to the nearest ancestor that does, stopping at the
/// session root.
fn revalidate_file_pane(model: &mut Model, fs: &mut dyn Fs) {
    let mut path = model.files_dir.clone();
    loop {
        match fs.list_dir(&path) {
            Ok(entries) => {
                model.files = entries;
                model.files_dir = path;
                model.sort_files();
                model.space = fs.volume_space(&model.files_dir);
                return;
            }
            Err(_) if path != model.root.path => {
                path = String::from(parent_of(&path));
            }
            // The session root itself no longer lists; the previous
            // listing is kept rather than blanked — the next navigation
            // will surface the error in place.
            Err(_) => return,
        }
    }
}

/// Apply the key that answers the sort menu.
fn handle_sort_menu(model: &mut Model, event: &Event) {
    model.overlay = Overlay::None;
    let key = match event {
        Event::Char('n') => Some(SortKey::Name),
        Event::Char('e') => Some(SortKey::Extension),
        Event::Char('s') => Some(SortKey::Size),
        Event::Char('m') => Some(SortKey::Modified),
        Event::Char('r') => {
            model.sort_desc = !model.sort_desc;
            model.sort_files();
            clamp_cursors(model);
            return;
        }
        // Esc — or any unassigned key — cancels the menu, changing nothing.
        _ => None,
    };
    if let Some(key) = key {
        model.sort_key = key;
        model.sort_files();
        clamp_cursors(model);
    }
}

/// Move the focused pane's cursor by `delta`, reloading the file pane when
/// the tree selection changes. A refused listing restores the cursor and
/// surfaces the error — the selection never lands on a directory whose
/// contents cannot be shown as if it were empty.
fn move_cursor(model: &mut Model, fs: &mut dyn Fs, delta: isize) {
    match model.pane {
        Pane::Tree => {
            let rows = model.tree_rows();
            let previous = model.tree_cursor;
            let next = step(previous, delta, rows.len());
            if next == previous {
                return;
            }
            model.tree_cursor = next;
            let path = rows[next].path.clone();
            if !select_dir(model, fs, &path) {
                model.tree_cursor = previous;
            }
        }
        Pane::Files => {
            let count = model.visible_files().len();
            model.file_cursor = step(model.file_cursor, delta, count);
        }
    }
}

/// Load `path` into the file pane; on failure report and keep the old
/// listing. Returns whether the selection moved.
fn select_dir(model: &mut Model, fs: &mut dyn Fs, path: &str) -> bool {
    match fs.list_dir(path) {
        Ok(entries) => {
            model.files = entries;
            model.files_dir = String::from(path);
            model.file_cursor = 0;
            model.file_scroll = 0;
            model.sort_files();
            model.space = fs.volume_space(path);
            true
        }
        Err(errno) => {
            model.report(path, errno);
            false
        }
    }
}

/// Expand or collapse the tree row under the cursor.
fn set_expanded(model: &mut Model, fs: &mut dyn Fs, expanded: bool) {
    let rows = model.tree_rows();
    let Some(row) = rows.get(model.tree_cursor) else {
        return;
    };
    let path = row.path.clone();
    if expanded == row.expanded {
        return;
    }
    if expanded && !populate_children(model, fs, &path) {
        return;
    }
    if let Some(node) = find_node(&mut model.root, &path) {
        node.expanded = expanded;
    }
    clamp_cursors(model);
}

/// Enter on a tree row: collapse an expanded node, expand a collapsed one.
fn toggle_expanded(model: &mut Model, fs: &mut dyn Fs) {
    let rows = model.tree_rows();
    let expanded = rows.get(model.tree_cursor).is_some_and(|row| row.expanded);
    set_expanded(model, fs, !expanded);
}

/// Enter on a file-pane row: a directory descends into it (selecting it
/// in the tree); a regular file opens in the viewer the head sample
/// picks.
fn enter_file_row(model: &mut Model, fs: &mut dyn Fs, decode: &mut dyn Decode) {
    let Some(entry) = model.visible_files().get(model.file_cursor).copied() else {
        return;
    };
    if !entry.kind.is_dir() {
        let path = join(&model.files_dir, &entry.name);
        open_viewer(model, fs, decode, &path, entry.size);
        return;
    }
    let parent = model.files_dir.clone();
    let child_path = join(&parent, &entry.name);
    // Show the descent in the tree: the parent expands and the cursor
    // lands on the child row, so both panes agree on the selection.
    if !populate_children(model, fs, &parent) {
        return;
    }
    if let Some(node) = find_node(&mut model.root, &parent) {
        node.expanded = true;
    }
    if !select_dir(model, fs, &child_path) {
        return;
    }
    if let Some(index) = model
        .tree_rows()
        .iter()
        .position(|row| row.path == child_path)
    {
        model.tree_cursor = index;
    }
}

/// Ensure the node at `path` has its children read. Returns `false` (with
/// the error reported) when the listing is refused, leaving the tree
/// untouched.
fn populate_children(model: &mut Model, fs: &mut dyn Fs, path: &str) -> bool {
    let needs_read = matches!(
        find_node(&mut model.root, path),
        Some(DirNode { children: None, .. })
    );
    if !needs_read {
        return true;
    }
    match fs.list_dir(path) {
        Ok(entries) => {
            let children = child_dirs_of(path, &entries);
            if let Some(node) = find_node(&mut model.root, path) {
                node.children = Some(children);
            }
            true
        }
        Err(errno) => {
            model.report(path, errno);
            false
        }
    }
}

/// The mutable node at `path`, found by walking the tree.
fn find_node<'m>(node: &'m mut DirNode, path: &str) -> Option<&'m mut DirNode> {
    if node.path == path {
        return Some(node);
    }
    node.children
        .as_mut()?
        .iter_mut()
        .find_map(|child| find_node(child, path))
}

/// Clamp the cursors into their (possibly shrunken) row counts.
fn clamp_cursors(model: &mut Model) {
    let tree_len = model.tree_rows().len();
    if model.tree_cursor >= tree_len {
        model.tree_cursor = tree_len.saturating_sub(1);
    }
    let files_len = model.visible_files().len();
    if model.file_cursor >= files_len {
        model.file_cursor = files_len.saturating_sub(1);
    }
    let flat_len = model.walk.as_ref().map_or(0, |walk| walk.entries.len());
    if model.flat_cursor >= flat_len {
        model.flat_cursor = flat_len.saturating_sub(1);
    }
}

/// Keep each pane's scroll window containing its cursor.
fn clamp_scroll(model: &mut Model, height: usize) {
    if height == 0 {
        return;
    }
    if model.tree_cursor < model.tree_scroll {
        model.tree_scroll = model.tree_cursor;
    }
    if model.tree_cursor >= model.tree_scroll + height {
        model.tree_scroll = model.tree_cursor + 1 - height;
    }
    if model.file_cursor < model.file_scroll {
        model.file_scroll = model.file_cursor;
    }
    if model.file_cursor >= model.file_scroll + height {
        model.file_scroll = model.file_cursor + 1 - height;
    }
    if model.flat_cursor < model.flat_scroll {
        model.flat_scroll = model.flat_cursor;
    }
    if model.flat_cursor >= model.flat_scroll + height {
        model.flat_scroll = model.flat_cursor + 1 - height;
    }
}

/// Step `index` by `delta` within `0..count`, staying put at the edges.
fn step(index: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let last = count - 1;
    if delta.is_negative() {
        index.saturating_sub(delta.unsigned_abs())
    } else {
        index.saturating_add(delta.unsigned_abs()).min(last)
    }
}

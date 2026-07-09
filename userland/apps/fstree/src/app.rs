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

use crate::fs::Fs;
use crate::model::{
    child_dirs_of, join, merge_child_dirs, BatchPrompt, ConfirmPrompt, DirNode, InputOp,
    InputPrompt, ModePrompt, Model, Overlay, OverwritePrompt, Pane, Prompt, SortKey, View,
};
use crate::ops::{parent_of, plan_target, resolve_destination, Decision, FileOp, OpProgress};
use crate::render::render;
use crate::tag::{Batch, BatchProgress, TagEntry};
use crate::walk::{relative_to, WalkPurpose, WalkState};

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

/// The session outcome the `Run` binary maps to an exit code.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FstreeError {
    /// The terminal path failed (a write error or a closed input).
    Terminal,
}

/// Rows available to a pane body: the grid minus the status and message
/// lines.
fn body_rows(screen_rows: u16) -> usize {
    usize::from(screen_rows.saturating_sub(2))
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
    screen: &mut Screen<T>,
) -> Result<i32, FstreeError> {
    let mut window = Window::new(Pos::new(0, 0), screen.size());
    loop {
        clamp_scroll(model, body_rows(screen.size().rows));
        render(model, &mut window);
        screen.refresh(&window).map_err(|_| FstreeError::Terminal)?;
        // While a walk is live the wait carries a short bound so an
        // elapsed read advances the walk one tick; otherwise the read
        // blocks until a key arrives. Either way the kernel parks the
        // task — never a poll.
        let ticking = model.walk.as_ref().is_some_and(WalkState::ticking);
        screen.set_input_mode(if ticking {
            InputMode::Timeout(WALK_TICK)
        } else {
            InputMode::Blocking
        });
        let Some(event) = screen.getch().map_err(|_| FstreeError::Terminal)? else {
            // No event: the walk bound elapsed (its tick), or a split
            // escape sequence continues on the next read.
            if ticking {
                walk_tick(model, fs);
            }
            continue;
        };
        handle_event(model, fs, &event);
        if model.quit {
            return Ok(0);
        }
    }
}

/// Apply one typed key event to the session state.
pub fn handle_event(model: &mut Model, fs: &mut dyn Fs, event: &Event) {
    model.message = None;
    if let Some(prompt) = model.prompt.take() {
        match prompt {
            Prompt::Mode(mode) => handle_mode_prompt(model, fs, mode, event),
            Prompt::Input(input) => handle_input_prompt(model, fs, input, event),
            Prompt::ConfirmDelete(confirm) => handle_confirm_prompt(model, fs, &confirm, event),
            Prompt::Overwrite(paused) => handle_overwrite_prompt(model, fs, paused, event),
            Prompt::ConfirmBatchDelete { count } => {
                handle_confirm_batch_delete(model, fs, count, event);
            }
            Prompt::BatchOverwrite(paused) => handle_batch_overwrite(model, fs, paused, event),
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
        Overlay::None => {}
    }
    if model.view == View::Flat {
        handle_flat_key(model, fs, event);
        return;
    }
    match event {
        Event::Char('q') => model.quit = true,
        Event::Char('?') => model.overlay = Overlay::Help,
        Event::Char('s') => model.overlay = Overlay::SortMenu,
        Event::Char('a') => open_mode_prompt(model, fs),
        Event::Char('c') => open_copy_move(model, false),
        Event::Char('m') => open_copy_move(model, true),
        Event::Char('r') => open_rename_prompt(model),
        Event::Char('d') => open_delete(model),
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
        Event::Char('u') => start_walk(model, WalkPurpose::Usage),
        Event::Char('v') => start_walk(model, WalkPurpose::Flat),
        Event::Esc => cancel_usage_walk(model),
        Event::Char('.') => {
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
            Pane::Files => enter_file_row(model, fs),
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

/// Open the mode-editor prompt on the focused pane's selection. The
/// prompt starts from the entry's *current* bits (a resolve-only stat
/// through the seam); a refused stat surfaces its error and opens nothing.
fn open_mode_prompt(model: &mut Model, fs: &mut dyn Fs) {
    let Some((path, name, _)) = focused_selection(model) else {
        return;
    };
    match fs.stat_mode(&path) {
        Ok(current) => {
            model.prompt = Some(Prompt::Mode(ModePrompt {
                path,
                name,
                current,
                input: format!("{current:o}"),
            }));
        }
        Err(errno) => model.report(&path, errno),
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

/// The `d` key: the batch delete confirmation when entries are tagged,
/// the single-selection confirmation otherwise.
fn open_delete(model: &mut Model) {
    if model.tags.is_empty() {
        open_delete_prompt(model);
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

/// Open the delete (`d`) confirmation on the focused selection. Deleting
/// the session root is refused.
fn open_delete_prompt(model: &mut Model) {
    let Some((path, name, kind)) = focused_selection(model) else {
        return;
    };
    if path == model.root.path {
        model.message = Some(String::from("cannot delete the session root"));
        return;
    }
    model.prompt = Some(Prompt::ConfirmDelete(ConfirmPrompt { path, name, kind }));
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
/// edit, Enter submits the answer, Esc cancels with nothing changed.
fn handle_input_prompt(model: &mut Model, fs: &mut dyn Fs, mut prompt: InputPrompt, event: &Event) {
    match event {
        Event::Esc => {}
        Event::Backspace => {
            prompt.input.pop();
            model.prompt = Some(Prompt::Input(prompt));
        }
        Event::Char(ch) if !ch.is_control() => {
            if prompt.input.len() < INPUT_MAX {
                prompt.input.push(*ch);
            }
            model.prompt = Some(Prompt::Input(prompt));
        }
        Event::Enter => submit_input(model, fs, prompt),
        _ => model.prompt = Some(Prompt::Input(prompt)),
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
    let refresh = alloc::vec![
        String::from(parent_of(src)),
        String::from(parent_of(&target))
    ];
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
        Event::Char('y' | 'Y') => {
            let refresh = alloc::vec![String::from(parent_of(&prompt.path))];
            run_op(
                model,
                fs,
                FileOp::delete(&prompt.path, prompt.kind),
                refresh,
            );
            prune_tag_if_gone(model, fs, &prompt.path);
        }
        _ => model.message = Some(format!("{} not deleted", prompt.name)),
    }
}

/// Apply one key to the batch delete confirmation: only `y`/`Y` proceeds
/// with a batch delete of the tagged set; any other key declines with
/// nothing changed — never an assumed yes.
fn handle_confirm_batch_delete(model: &mut Model, fs: &mut dyn Fs, count: usize, event: &Event) {
    match event {
        Event::Char('y' | 'Y') => {
            let items = model.tags.entries().to_vec();
            let refresh: Vec<String> = items
                .iter()
                .map(|item| String::from(parent_of(&item.path)))
                .collect();
            run_batch(model, fs, Batch::delete(&items), refresh);
        }
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
/// (panes) or branch-relative path (flattened view) matches the glob. A
/// malformed pattern is reported and tags nothing.
fn submit_tag_glob(model: &mut Model, typed: &str) {
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
    model.view = View::Flat;
    model.flat_cursor = 0;
    model.flat_scroll = 0;
    model.walk = Some(WalkState::new(root, WalkPurpose::Flat, FLAT_PAGE));
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
    walk.tick(fs, WALK_DIRS_PER_TICK);
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
        Event::Esc | Event::Char('q') => {
            model.view = View::Panes;
            model.walk = None;
            model.flat_cursor = 0;
            model.flat_scroll = 0;
        }
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
        Event::Char('d') => open_flat_delete(model),
        Event::Char('?') => model.overlay = Overlay::Help,
        _ => {
            let _ = fs;
        }
    }
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
fn open_flat_delete(model: &mut Model) {
    if model.tags.is_empty() {
        model.message = Some(String::from("no tagged entries"));
        return;
    }
    model.prompt = Some(Prompt::ConfirmBatchDelete {
        count: model.tags.count(),
    });
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

/// Enter on a file-pane row: a directory descends into it (selecting it in
/// the tree); a regular file is left alone until the viewers arrive.
fn enter_file_row(model: &mut Model, fs: &mut dyn Fs) {
    let Some(entry) = model.visible_files().get(model.file_cursor).copied() else {
        return;
    };
    if !entry.kind.is_dir() {
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

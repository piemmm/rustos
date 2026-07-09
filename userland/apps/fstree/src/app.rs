//! The session: typed key events mutating the [`Model`], and the blocking
//! screen loop the `Run` binary drives.
//!
//! Every wait blocks in [`Screen::getch`] (the kernel parks the task until
//! input arrives); there is no polling loop. The terminal is restored by
//! the `Run` binary's alternate-screen bracketing, not here.

use alloc::string::String;

use rustos_curses::{Event, Pos, Screen, Tty, Window};

use crate::fs::Fs;
use crate::model::{child_dirs_of, join, DirNode, Model, Overlay, Pane, SortKey};
use crate::render::render;

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
        // `getch` blocks in the kernel until input arrives; `None` means
        // the read's bytes completed no event yet (a split escape
        // sequence), so the next read continues the decode.
        let Some(event) = screen.getch().map_err(|_| FstreeError::Terminal)? else {
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
    match model.overlay {
        Overlay::Help => {
            // Any key dismisses the help overlay.
            model.overlay = Overlay::None;
            return;
        }
        Overlay::SortMenu => {
            handle_sort_menu(model, event);
            return;
        }
        Overlay::None => {}
    }
    match event {
        Event::Char('q') => model.quit = true,
        Event::Char('?') => model.overlay = Overlay::Help,
        Event::Char('s') => model.overlay = Overlay::SortMenu,
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

/// Clamp both cursors into their (possibly shrunken) row counts.
fn clamp_cursors(model: &mut Model) {
    let tree_len = model.tree_rows().len();
    if model.tree_cursor >= tree_len {
        model.tree_cursor = tree_len.saturating_sub(1);
    }
    let files_len = model.visible_files().len();
    if model.file_cursor >= files_len {
        model.file_cursor = files_len.saturating_sub(1);
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

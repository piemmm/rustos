//! The session loop: events in, frames out, until the editor quits.
//!
//! [`run`] owns the cycle every full-screen tool shares: draw the
//! [`Editor`] into a window, flush the minimal diff to the terminal, then
//! block on the next input event. Blocking is the kernel's park on the
//! inherited standard input — there is no polling loop; the editor only
//! runs when a key arrives.

use tairix_curses::{Event, Pos, Screen, Tty, Window};

use crate::command::Start;
use crate::editor::Editor;
use crate::error::VimError;
use crate::fileio::FileIo;
use crate::render::render;

/// Run the editor session to completion. Returns the exit code the
/// session chose (`:q`, `ZZ`, …) or the terminal failure that ended it.
///
/// The first file of the argument list is loaded before the first frame;
/// `start` then applies the `+num` / `+` / `+/pattern` startup position.
///
/// # Errors
///
/// [`VimError::Terminal`] when the terminal byte channel fails (including
/// the session's input closing underneath it).
pub fn run<T: Tty>(
    editor: &mut Editor,
    io: &dyn FileIo,
    screen: &mut Screen<T>,
    start: Option<Start>,
) -> Result<i32, VimError> {
    if let Some(path) = editor.files.first().cloned() {
        editor.load_file(&path, io);
    }
    match start {
        Some(Start::Line(line)) => {
            let target = crate::motion::goto_line(&editor.buffer, line);
            editor.cursor = target.pos;
            editor.clamp_cursor();
        }
        Some(Start::LastLine) => {
            let target = crate::motion::goto_line(&editor.buffer, editor.buffer.len_lines());
            editor.cursor = target.pos;
            editor.clamp_cursor();
        }
        Some(Start::Pattern(pattern)) => editor.run_search_command(&pattern, true),
        None => {}
    }
    let mut window = Window::new(Pos::new(0, 0), screen.size());
    loop {
        render(editor, &mut window);
        screen.refresh(&window).map_err(|_| VimError::Terminal)?;
        // `getch` blocks in the kernel until input arrives; `None` means
        // the read's bytes completed no event yet (a split escape
        // sequence), so the next read continues the decode.
        let Some(event) = screen.getch().map_err(|_| VimError::Terminal)? else {
            continue;
        };
        // The window outlives each frame, so it is the one piece of layout
        // that does not re-derive itself; resize it and redraw. The
        // renderer recomputes the scroll view from the window it is given,
        // so the cursor stays visible at the new geometry.
        if let Event::Resize(size) = event {
            window.resize(size);
            continue;
        }
        editor.handle_event(&event, io);
        if let Some(code) = editor.quit {
            return Ok(code);
        }
    }
}

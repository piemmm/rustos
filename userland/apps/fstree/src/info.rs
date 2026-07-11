//! The Standard Information Stream seam (fd 3): structured, advisory,
//! ignorable records about what the session shows.
//!
//! `fstree` emits exactly one record class: an `omission` when the file
//! pane hides dot-named entries under the default hidden-entries toggle,
//! so a capturing consumer (`fstree 3>info.jsonl`) knows the pane was not
//! exhaustive and how to see the rest. Advisory only by the stream's
//! contract: a record never affects the session, the exit status, or the
//! screen, and an unattached fd 3 is a silent no-op.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};

use crate::model::Model;

/// The advisory-output seam: deliver one framed JSONL record. The `Run`
/// binary writes fd 3 best-effort; tests capture the bytes.
pub trait Info {
    /// Deliver one JSONL-framed record (trailing newline included).
    fn info(&mut self, record: &[u8]);
}

/// An [`Info`] that discards every record — for callers with no advisory
/// consumer (and the tests that do not exercise the stream).
pub struct NullInfo;

impl Info for NullInfo {
    fn info(&mut self, _record: &[u8]) {}
}

/// What the file pane currently omits under the hidden-entries toggle:
/// the listed directory and its hidden-entry count. `None` when nothing
/// is omitted (the toggle shows them, or none exist).
#[must_use]
pub fn hidden_omission(model: &Model) -> Option<(String, u64)> {
    if model.show_hidden {
        return None;
    }
    let omitted = model
        .files
        .iter()
        .filter(|entry| entry.name.starts_with('.'))
        .count() as u64;
    if omitted == 0 {
        return None;
    }
    Some((model.files_dir.clone(), omitted))
}

/// Emit the `fs.hidden_entries_omitted` advisory when the pane's omission
/// state changed since `last` (a new directory, a changed count) — once
/// per change, so browsing does not spam the stream. `last` carries the
/// state already reported.
pub fn note_hidden_entries(model: &Model, info: &mut dyn Info, last: &mut Option<(String, u64)>) {
    let current = hidden_omission(model);
    if current == *last {
        return;
    }
    if let Some((dir, omitted)) = &current {
        info.info(&omission_record(dir, *omitted));
    }
    *last = current;
}

/// The framed `fs.hidden_entries_omitted` record for `omitted` hidden
/// entries in `dir`.
fn omission_record(dir: &str, omitted: u64) -> Vec<u8> {
    let message = if omitted == 1 {
        String::from("1 hidden entry not shown.")
    } else {
        format!("{omitted} hidden entries not shown.")
    };
    let ai = format!(
        "{{\"subject\":\"file_pane\",\
         \"omission\":{{\"reason\":\"hidden_by_default\",\
         \"entry_class\":\"dotfile\",\"omitted_count\":{omitted},\
         \"directory\":{},\
         \"pane_is_exhaustive\":false}}}}",
        json_string(dir)
    );
    let record = StdInfoRecord::new(
        "fstree",
        StdInfoKind::Omission,
        "fs.hidden_entries_omitted",
        Severity::Info,
        Human::with_suggestion(&message, "Press H to show them."),
    )
    .with_ai(&ai);
    // Sized to the payload (the directory path is the only unbounded
    // part), so a deep path never truncates the frame.
    let mut buf = alloc::vec![0u8; 512 + ai.len()];
    match record.write_jsonl(&mut buf) {
        Ok(len) => {
            buf.truncate(len);
            buf
        }
        // A record that cannot frame is dropped whole — fd 3 is advisory
        // and never worth a partial or malformed line.
        Err(_) => Vec::new(),
    }
}

/// A minimal JSON string literal for `text`: quotes, backslashes, and
/// control bytes escaped so a hostile directory name cannot break the
/// record's framing.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                // Writing to a `String` cannot fail.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

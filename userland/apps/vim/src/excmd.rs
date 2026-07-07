//! The ex (`:`) command language: ranges and the command set.
//!
//! [`execute`] parses one command line and drives the [`Editor`]. The
//! grammar is the vim core:
//!
//! ```text
//! [range] command[!] [argument]
//! range   := '%' | addr | addr ',' addr
//! addr    := number | '.' | '$'  [ ('+'|'-') number ]
//! ```
//!
//! Commands: `w[!]`, `wq`/`x`, `q[!]`, `e[!]`, `enew`, `r`, `n[ext]`,
//! `prev[ious]`, `noh[lsearch]`, `set` (`number`/`nonumber` and their
//! abbreviations), `d[elete]` over a range, `s/pat/rep/[g]` over a range,
//! and a bare address (`:12`, `:$`) as a goto. Anything else is vim's
//! `E492: Not an editor command`. The wider ex set is staged in
//! `plans/VIM.md`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::buffer::Position;
use crate::editor::Editor;
use crate::fileio::FileIo;
use crate::motion;
use crate::pattern::Pattern;

/// A parsed line range, 0-based and inclusive.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Range {
    start: usize,
    end: usize,
}

/// Execute one `:` command line.
pub fn execute(editor: &mut Editor, text: &str, io: &dyn FileIo) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let (range, rest) = match parse_range(editor, text) {
        Ok(parsed) => parsed,
        Err(message) => {
            editor.error(message);
            return;
        }
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        // A bare address is a goto.
        if let Some(range) = range {
            let target = motion::goto_line(&editor.buffer, range.end + 1);
            editor.cursor = target.pos;
            editor.sticky_col = None;
            editor.clamp_cursor();
        }
        return;
    }
    let (name, bang, argument) = split_command(rest);
    match name {
        "w" | "write" => {
            let path = non_empty(argument);
            editor.write_buffer(path, bang, io);
        }
        "q" | "quit" => quit(editor, bang),
        "wq" | "x" | "xit" => {
            let path = non_empty(argument);
            // `:x` writes only when modified; `:wq` always writes.
            let must_write = name == "wq" || editor.buffer.is_modified();
            if !must_write || editor.write_buffer(path, bang, io) {
                editor.quit = Some(0);
            }
        }
        "e" | "edit" => match non_empty(argument) {
            Some(path) => editor.edit_file(path, bang, io),
            None => match editor.buffer.name().map(String::from) {
                Some(path) => editor.edit_file(&path, bang, io),
                None => editor.error(String::from("E32: No file name")),
            },
        },
        "enew" => {
            if editor.buffer.is_modified() && !bang {
                editor.error(String::from(
                    "E37: No write since last change (add ! to override)",
                ));
            } else {
                let readonly = editor.buffer.is_readonly();
                editor.buffer = crate::buffer::Buffer::empty();
                editor.buffer.set_readonly(readonly);
                editor.cursor = Position::default();
                editor.view.top = 0;
            }
        }
        "r" | "read" => match non_empty(argument) {
            Some(path) => editor.read_file_into(path, io),
            None => editor.error(String::from("E32: No file name")),
        },
        "n" | "next" => editor.goto_arg(true, bang, io),
        "prev" | "previous" | "N" => editor.goto_arg(false, bang, io),
        "noh" | "nohl" | "nohlsearch" => editor.hlsearch = false,
        "set" | "se" => set_option(editor, argument),
        "d" | "delete" => {
            let range = range.unwrap_or(Range {
                start: editor.cursor.line,
                end: editor.cursor.line,
            });
            delete_lines(editor, range);
        }
        "s" => {
            let range = range.unwrap_or(Range {
                start: editor.cursor.line,
                end: editor.cursor.line,
            });
            // The delimiter and everything after it sit in `rest` right
            // after the leading `s`.
            substitute(editor, range, &rest[1..]);
        }
        _ => editor.error(format!("E492: Not an editor command: {rest}")),
    }
}

/// `:q` — quit, guarding unwritten changes.
fn quit(editor: &mut Editor, bang: bool) {
    if editor.buffer.is_modified() && !bang {
        editor.error(String::from(
            "E37: No write since last change (add ! to override)",
        ));
    } else {
        editor.quit = Some(0);
    }
}

/// Split `rest` into the command word, its `!` flag, and the argument
/// text. The command word is the leading run of letters; `!` may follow
/// it directly.
fn split_command(rest: &str) -> (&str, bool, &str) {
    let end = rest
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_alphabetic())
        .map_or(rest.len(), |(at, _)| at);
    let name = &rest[..end];
    let tail = &rest[end..];
    match tail.strip_prefix('!') {
        Some(argument) => (name, true, argument),
        None => (name, false, tail),
    }
}

/// `argument` trimmed, or [`None`] when empty.
fn non_empty(argument: &str) -> Option<&str> {
    let trimmed = argument.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Parse the optional leading range. Returns the range (if any) and the
/// remaining text.
fn parse_range<'a>(editor: &Editor, text: &'a str) -> Result<(Option<Range>, &'a str), String> {
    let last = editor.buffer.len_lines() - 1;
    if let Some(rest) = text.strip_prefix('%') {
        return Ok((
            Some(Range {
                start: 0,
                end: last,
            }),
            rest,
        ));
    }
    let (first, rest) = parse_addr(editor, text)?;
    let Some(first) = first else {
        return Ok((None, text));
    };
    if let Some(after_comma) = rest.strip_prefix(',') {
        let (second, rest) = parse_addr(editor, after_comma)?;
        let Some(second) = second else {
            return Err(String::from("E14: Invalid address"));
        };
        let (start, end) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };
        return Ok((
            Some(Range {
                start: start.min(last),
                end: end.min(last),
            }),
            rest,
        ));
    }
    Ok((
        Some(Range {
            start: first.min(last),
            end: first.min(last),
        }),
        rest,
    ))
}

/// Parse one address (`number`, `.`, `$`, with an optional `+`/`-`
/// offset). Returns the 0-based line and the remaining text; `None` when
/// no address is present.
fn parse_addr<'a>(editor: &Editor, text: &'a str) -> Result<(Option<usize>, &'a str), String> {
    let last = editor.buffer.len_lines() - 1;
    let mut rest = text;
    let base: Option<usize> = if let Some(after) = rest.strip_prefix('.') {
        rest = after;
        Some(editor.cursor.line)
    } else if let Some(after) = rest.strip_prefix('$') {
        rest = after;
        Some(last)
    } else {
        let digits = rest
            .char_indices()
            .find(|(_, ch)| !ch.is_ascii_digit())
            .map_or(rest.len(), |(at, _)| at);
        if digits == 0 {
            None
        } else {
            let number: usize = rest[..digits]
                .parse()
                .map_err(|_| String::from("E14: Invalid address"))?;
            rest = &rest[digits..];
            Some(number.saturating_sub(1))
        }
    };
    let Some(base) = base else {
        return Ok((None, text));
    };
    // An optional +N / -N offset.
    if let Some(after) = rest.strip_prefix('+') {
        let (offset, tail) = parse_number(after);
        return Ok((Some(base.saturating_add(offset.unwrap_or(1))), tail));
    }
    if let Some(after) = rest.strip_prefix('-') {
        let (offset, tail) = parse_number(after);
        return Ok((Some(base.saturating_sub(offset.unwrap_or(1))), tail));
    }
    Ok((Some(base), rest))
}

/// A leading decimal number, if present.
fn parse_number(text: &str) -> (Option<usize>, &str) {
    let digits = text
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map_or(text.len(), |(at, _)| at);
    if digits == 0 {
        return (None, text);
    }
    match text[..digits].parse() {
        Ok(number) => (Some(number), &text[digits..]),
        Err(_) => (None, text),
    }
}

/// `:set` — the supported options.
fn set_option(editor: &mut Editor, argument: &str) {
    match argument.trim() {
        "nu" | "number" => editor.number = true,
        "nonu" | "nonumber" => editor.number = false,
        "" => editor.error(String::from("E518: Unknown option: ")),
        other => editor.error(format!("E518: Unknown option: {other}")),
    }
}

/// `:[range]d` — delete whole lines into the unnamed register.
fn delete_lines(editor: &mut Editor, range: Range) {
    let start = Position::new(range.start, 0);
    let end = Position::new(
        range.end,
        editor.buffer.line_len(range.end).saturating_sub(1),
    );
    editor.delete_span(start, end, true, true);
}

/// `:[range]s/pat/rep/[g]` — substitute within the range.
fn substitute(editor: &mut Editor, range: Range, body: &str) {
    let mut chars = body.chars();
    let Some(delimiter) = chars.next() else {
        editor.error(String::from("E471: Argument required"));
        return;
    };
    if delimiter.is_ascii_alphanumeric() || delimiter == '\\' {
        editor.error(format!("E492: Not an editor command: s{body}"));
        return;
    }
    let parts = split_substitute(chars.as_str(), delimiter);
    let (pattern_text, replacement, flags) = parts;
    let source = if pattern_text.is_empty() {
        let Some(search) = &editor.search else {
            editor.error(String::from("E35: No previous regular expression"));
            return;
        };
        String::from(search.pattern.source())
    } else {
        pattern_text
    };
    let Ok(pattern) = Pattern::compile(&source) else {
        editor.error(format!("E383: Invalid pattern: {source}"));
        return;
    };
    let global = flags.contains('g');
    let mut replaced = 0usize;
    let mut touched_lines = 0usize;
    let mut last_line = editor.cursor.line;
    editor.buffer.begin_edit(editor.cursor);
    for line in range.start..=range.end.min(editor.buffer.len_lines() - 1) {
        let original = String::from(editor.buffer.line(line));
        let (new, count) = substitute_line(&pattern, &original, &replacement, global);
        if count > 0 {
            editor
                .buffer
                .replace_lines(line, line + 1, alloc::vec![new]);
            replaced += count;
            touched_lines += 1;
            last_line = line;
        }
    }
    let cursor = Position::new(last_line, 0);
    editor.cursor = cursor;
    editor.buffer.commit_edit(cursor);
    editor.clamp_cursor();
    if replaced == 0 {
        editor.error(format!("E486: Pattern not found: {source}"));
    } else if replaced > 1 {
        editor.info(format!("{replaced} substitutions on {touched_lines} lines"));
    }
    // The substitute pattern becomes the current search, as in vim.
    editor.search = Some(crate::editor::Search {
        pattern,
        forward: true,
    });
    editor.hlsearch = true;
}

/// Split the substitute body into pattern, replacement, and flags on the
/// (escapable) delimiter.
fn split_substitute(body: &str, delimiter: char) -> (String, String, String) {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in body.chars() {
        if escaped {
            if ch != delimiter {
                current.push('\\');
            }
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == delimiter && parts.len() < 2 {
            parts.push(core::mem::take(&mut current));
            continue;
        }
        current.push(ch);
    }
    if escaped {
        current.push('\\');
    }
    parts.push(current);
    let mut it = parts.into_iter();
    let pattern = it.next().unwrap_or_default();
    let replacement = it.next().unwrap_or_default();
    let flags = it.next().unwrap_or_default();
    (pattern, replacement, flags)
}

/// Apply the substitution to one line; returns the new line and the
/// number of replacements.
fn substitute_line(
    pattern: &Pattern,
    line: &str,
    replacement: &str,
    global: bool,
) -> (String, usize) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut at = 0usize;
    let mut count = 0usize;
    while at <= chars.len() {
        let Some((start, end)) = pattern.find_at(line, at) else {
            break;
        };
        // Copy the unmatched prefix.
        for &ch in &chars[at..start] {
            out.push(ch);
        }
        let matched: String = chars[start..end].iter().collect();
        push_replacement(&mut out, replacement, &matched);
        count += 1;
        // A zero-width match must still advance the scan.
        at = if end > start {
            end
        } else {
            if let Some(&ch) = chars.get(end) {
                out.push(ch);
            }
            end + 1
        };
        if !global {
            break;
        }
    }
    for &ch in chars.get(at..).unwrap_or(&[]) {
        out.push(ch);
    }
    (out, count)
}

/// Expand the replacement text: `&` inserts the whole match, `\&` a
/// literal ampersand, `\\` a literal backslash.
fn push_replacement(out: &mut String, replacement: &str, matched: &str) {
    let mut escaped = false;
    for ch in replacement.chars() {
        if escaped {
            match ch {
                '&' => out.push('&'),
                '\\' => out.push('\\'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '&' => out.push_str(matched),
            other => out.push(other),
        }
    }
    if escaped {
        out.push('\\');
    }
}

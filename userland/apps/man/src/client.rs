//! The resolve-and-render engine: find the command's owning bundle, load
//! its Help document in the active locale, and write the rendered page —
//! paginated on an interactive terminal — to standard output.

use alloc::format;
use alloc::string::String;

use rustos_abi::stdinfo::{Human, StdInfoKind, StdInfoRecord};
use rustos_abi::{Errno, BUNDLE_SUFFIX};
use rustos_cmdres::bundle_candidates;
use rustos_help::{
    load, render_full, render_short, DocumentName, Fallback, LoadError, Loaded, Locale,
};
use rustos_vt::{encode_all_into, Op};

use crate::command::Command;
use crate::error::ManError;
use crate::io::{BundleStore, Console};
use crate::source::ScopedHelp;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `man`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: man [-h | -?] <command> [topic]";

/// `man`'s own command word: its short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "man";

/// The pager prompt shown at the foot of each screenful.
const MORE_PROMPT: &[u8] = b"--More--";

/// The bytes that erase [`MORE_PROMPT`]: return to column one, overwrite
/// with spaces, return again so the next line starts clean.
const MORE_ERASE: &[u8] = b"\r        \r";

/// The environment the shell resolved once for this invocation: the active
/// locale preference and the `PATH` search list, both read from the
/// inherited environment by the `Run` binary and injected here so the
/// engine itself performs no ambient lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct Request<'a> {
    /// The user's locale preference (the `LANG` variable, a BCP-47 tag,
    /// plans/APPS.md §5), if set. A missing or malformed preference falls
    /// back to the canonical `en-US/` documents — a bad
    /// preference must never make help unreadable.
    pub locale: Option<&'a str>,
    /// The `PATH` variable, if set: the user-extendable half of the
    /// store-then-`PATH` bundle search (plans/APPS.md §8).
    pub path: Option<&'a str>,
}

/// Run one [`Command`] against the injected store and console.
///
/// # Errors
///
/// Every [`ManError`] the command can produce; the `Run` binary reports it
/// on standard error and maps it to the exit status.
pub fn run(
    command: &Command,
    request: &Request<'_>,
    store: &dyn BundleStore,
    console: &dyn Console,
) -> Result<(), ManError> {
    match command {
        Command::ShortHelp => short_help(request, store, console),
        Command::Page { word, topic } => page(word, topic.as_deref(), request, store, console),
    }
}

/// Render `man`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree; when that tree is absent (a build without the
/// bundle's documents) the usage banner stands in, so `-h` never fails.
fn short_help(
    request: &Request<'_>,
    store: &dyn BundleStore,
    console: &dyn Console,
) -> Result<(), ManError> {
    let loaded = resolve(OWN_WORD, request.path, store).and_then(|bundle_dir| {
        let name = DocumentName::parse(OWN_WORD)?;
        let source = ScopedHelp::new(store, &bundle_dir);
        load(&source, &active_locale(request.locale), &name).map_err(from_load(OWN_WORD, OWN_WORD))
    });
    match loaded {
        Ok(loaded) => {
            let bytes = encode_ops(&render_short(&loaded.doc));
            console.write_all(&bytes).map_err(ManError::Output)
        }
        // The tool's own page being missing must not make `-h` fail: the
        // usage banner is the tool's own text, not fabricated help content.
        Err(ManError::Output(err)) => Err(ManError::Output(err)),
        Err(_) => {
            let line = format!("{USAGE}\n");
            console.write_all(line.as_bytes()).map_err(ManError::Output)
        }
    }
}

/// Render one command's Help document in full, paginated when the console
/// is an interactive terminal.
fn page(
    word: &str,
    topic: Option<&str>,
    request: &Request<'_>,
    store: &dyn BundleStore,
    console: &dyn Console,
) -> Result<(), ManError> {
    let bundle_dir = resolve(word, request.path, store)?;
    let name_str = topic.unwrap_or_else(|| document_word(word));
    let name = DocumentName::parse(name_str)?;
    let requested = active_locale(request.locale);
    let source = ScopedHelp::new(store, &bundle_dir);
    let loaded = load(&source, &requested, &name).map_err(from_load(word, name_str))?;
    emit_fallback_record(console, &requested, &loaded);
    let bytes = encode_ops(&render_full(&loaded.doc));
    write_paged(&bytes, console)
}

/// Encode a rendered `Op` sequence to bytes over the allocation-free
/// [`encode_all_into`] sink API.
fn encode_ops(ops: &[Op]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    encode_all_into(ops, &mut out);
    out
}

/// The document a command word names inside its bundle: the word itself
/// for a bare command, and the command name — the path leaf minus the
/// bundle suffix — when the word spells the bundle (`top.app`,
/// `/Apps/Example.app`), so both invocations of one program open the same
/// page.
fn document_word(word: &str) -> &str {
    let leaf = word.rsplit('/').next().unwrap_or(word);
    leaf.strip_suffix(BUNDLE_SUFFIX).unwrap_or(leaf)
}

/// Resolve the command word to its owning bundle directory: the first
/// candidate of the shared store-then-`PATH` order that exists. `NotFound`
/// moves to the next candidate; any other refusal is final — exactly the
/// shell's launch rule, so the page shown always documents the program the
/// shell would run.
fn resolve(word: &str, path: Option<&str>, store: &dyn BundleStore) -> Result<String, ManError> {
    let candidates = bundle_candidates(word, path);
    if candidates.is_empty() {
        return Err(ManError::NotABundle(String::from(word)));
    }
    for candidate in candidates {
        match store.bundle_exists(&candidate) {
            Ok(true) => return Ok(candidate),
            Ok(false) => {}
            Err(err) => return Err(ManError::Store(err)),
        }
    }
    Err(ManError::CommandNotFound(String::from(word)))
}

/// The locale the engine is asked for: the user's preference when it is a
/// well-formed tag, the canonical `en-US/` otherwise. A malformed
/// preference degrades to the canonical documents rather than making every
/// page unreadable — the fallback chain itself stays the engine's.
fn active_locale(tag: Option<&str>) -> Locale {
    tag.and_then(|tag| Locale::parse(tag).ok())
        .unwrap_or_default()
}

/// Map a [`LoadError`] onto the command's error vocabulary: a missing
/// document is the ordinary "no help" outcome; everything else reports the
/// tree as unusable.
fn from_load(word: &str, name: &str) -> impl FnOnce(LoadError) -> ManError {
    let word = String::from(word);
    let name = String::from(name);
    move |err| match err {
        LoadError::NotFound => ManError::NoHelp { word, name },
        other => ManError::Tree(other),
    }
}

/// Emit the locale-fallback `stdinfo` advisory (fd 3) when the served
/// locale is not the one the user asked for (plans/APPS.md §7): a tool or
/// user then knows the page was not shown in the requested language.
/// Advisory only — never affects the page, the exit status, or ordering.
fn emit_fallback_record(console: &dyn Console, requested: &Locale, loaded: &Loaded) {
    if requested.is_default() || loaded.selection.fallback == Fallback::Exact {
        return;
    }
    let served = loaded.selection.locale_dir.as_str();
    let message = format!(
        "Help shown in {served}; no {} translation.",
        requested.as_str()
    );
    // Both tags are engine-validated spellings (letters, digits, `-`), so
    // embedding them in the JSON object cannot break its framing.
    let ai = format!(
        "{{\"locale\":{{\"requested\":\"{}\",\"served\":\"{served}\"}}}}",
        requested.as_str()
    );
    let record = StdInfoRecord::new(
        "man",
        StdInfoKind::Context,
        "help.locale_fallback",
        rustos_abi::stdinfo::Severity::Info,
        Human::message(&message),
    )
    .with_ai(&ai);
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        console.info(&buf[..len]);
    }
}

/// Write the rendered page, a screenful at a time on an interactive
/// terminal.
///
/// The pager is the historical `more` contract: after each screenful the
/// prompt offers space (next screenful), return (one more line), or `q`
/// (stop); any other key turns the next screenful too, and end of input
/// streams the remainder without prompting. A console that reports no row
/// count (a redirection, a pipe) gets the whole page in order, unprompted.
fn write_paged(bytes: &[u8], console: &dyn Console) -> Result<(), ManError> {
    let page_rows = match console.rows() {
        Some(rows) if rows >= 2 => usize::from(rows) - 1,
        _ => return console.write_all(bytes).map_err(ManError::Output),
    };
    let mut written_rows = 0usize;
    let mut paging = true;
    let mut lines = bytes.split_inclusive(|&byte| byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        console.write_all(line).map_err(ManError::Output)?;
        written_rows += 1;
        if paging && written_rows >= page_rows && lines.peek().is_some() {
            match prompt(console)? {
                PagerStep::Screenful => written_rows = 0,
                PagerStep::Line => written_rows = page_rows.saturating_sub(1),
                PagerStep::Quit => return Ok(()),
                PagerStep::Stream => paging = false,
            }
        }
    }
    Ok(())
}

/// What the user asked the pager to do next.
enum PagerStep {
    /// Show the next screenful.
    Screenful,
    /// Show one more line.
    Line,
    /// Stop rendering.
    Quit,
    /// Input ended: stream the rest without prompting.
    Stream,
}

/// Show the `--More--` prompt, wait for one key, and erase the prompt.
fn prompt(console: &dyn Console) -> Result<PagerStep, ManError> {
    console.write_all(MORE_PROMPT).map_err(ManError::Output)?;
    let key = console.read_key().map_err(pager_input_error)?;
    console.write_all(MORE_ERASE).map_err(ManError::Output)?;
    Ok(match key {
        Some(b'q' | b'Q') => PagerStep::Quit,
        Some(b'\r' | b'\n') => PagerStep::Line,
        Some(_) => PagerStep::Screenful,
        None => PagerStep::Stream,
    })
}

/// A pager-input failure is an output-path failure from the caller's view:
/// the page could not be delivered interactively.
fn pager_input_error(err: Errno) -> ManError {
    ManError::Output(err)
}

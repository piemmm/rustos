//! Tab completion: the word under the cursor to the candidates that could
//! finish it (`plans/SHELL.md`, "Tab expansion and completion").
//!
//! The engine is pure and read-only: it inspects the line with the shell's
//! own quoting-aware lexer ([`crate::lexer::tokenize_with_spans`]) — never a
//! second, completion-only tokeniser — and reaches the outside world only
//! through the injected [`DirLister`] and [`ResourceLister`] seams, so it can
//! be tested without a kernel and can never run a command, write a file, or
//! change `$?`.
//!
//! The *path* completion policy (the directory-part/leaf split, the dotfile
//! rule, the prefix filter, the common-prefix Tab extension) is the shared
//! `lib/complete` engine; this module owns only what is the shell's —
//! word roles from the shell's own lexer, shell-escaping of inserts, and
//! the command and resource-reference candidate classes.
//!
//! What is completed, by the word's role in the line:
//!
//! * **Command position** (first non-assignment word of a simple command):
//!   builtin names (the shared `builtin::BUILTIN_NAMES` table) and the `.app` bundles
//!   of the shared command-search directories
//!   ([`tairix_cmdres::command_search_dirs`] — the two system stores, the
//!   user's own two stores, then the `PATH` entries), so completion offers
//!   exactly the names the shell would resolve. A word spelling a path (it
//!   contains `/`) completes as a path.
//! * **Redirection source or target**: filesystem paths *and* the resource
//!   references openable in that direction — the registered namespaces
//!   (`sys:` …) and their catalogued selectors
//!   ([`tairix_resref::KnownNamespace`]), the same registry the redirection
//!   classifier applies. The directions differ by one class: a value-backed
//!   namespace ([`tairix_resref::NamespaceBacking::Value`]) is offered after a
//!   *read*, which the shell serves through the System Information API, and
//!   never after a *write*, where it is a dead end — such a resource is
//!   changed by a typed service command (`plans/ALIAS.md` §6.2, §15.3).
//! * **Any other argument**: filesystem paths, plus resource references once
//!   the word could begin one (a registered-namespace prefix).
//!
//! # Resource references
//!
//! A word the shared resolution rule
//! ([`tairix_resref::names_resource_reference`] — the same predicate
//! [`tairix_resref::classify_target`] routes on) reads as a resource
//! reference is completed *only* as one, in every role including command
//! position: it can never denote a path, so offering path candidates for it
//! would offer something the shell would not open.
//!
//! Within a namespace the selector completes one segment at a time, exactly
//! as a path does, from the registry's selector catalogue
//! ([`tairix_resref::KnownNamespace::selector_catalogue`]) — so
//! `state:<Tab>` offers `irq/` and `net/`, and `state:net/wan/<Tab>` offers
//! that interface's four state leaves. A catalogue segment spelled `<iface>`
//! is a *placeholder*: it names a [`SelectorDomain`] rather than one
//! resource. At such a position the engine asks the injected
//! [`ResourceLister`] for that domain's real names and offers *those* — so
//! Tab yields `wan lan0`, never the text `<iface>`, which the shell could
//! not insert and the user could not use.
//!
//! A lister that answers with nothing, or refuses, yields **no candidates**
//! there. That is the right answer rather than a degradation: the domains
//! that need a capability to enumerate (an interface list needs
//! `CAP_SYSINFO_HW`) are exactly the ones whose resources the session could
//! not read either, so offering nothing offers precisely what this session
//! can use. The *catalogue* is never filtered — a selector the session
//! cannot read is still completed, and the read then fails with an error
//! naming the capability — because discovery is not authorization and a
//! spelling grants nothing (`plans/ALIAS.md` §6.2).
//!
//! Degradation is deliberate and fail-closed: a line whose prefix does not
//! lex (an open quote), or a word already carrying quoting or expansion
//! syntax, completes to nothing rather than to a guess.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_cmdres::CommandEnv;
use tairix_resref::{
    is_placeholder, placeholder_domain, KnownNamespace, NamespaceBacking, SelectorDomain,
    SelectorEntry,
};

pub use tairix_complete::{DirEntryInfo, DirLister};

use crate::builtin::BUILTIN_NAMES;
use crate::lexer::{tokenize_with_spans, RedirOp, Token};
use crate::parser::OpenMode;

/// One completion candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// The full replacement for the word under completion (shell-escaped).
    pub insert: String,
    /// What a candidate listing shows (the bare name, `/`-suffixed for a
    /// directory).
    pub display: String,
    /// A closing character appended *only* when this candidate completes
    /// uniquely: a space after a finished word, nothing after a directory or
    /// a namespace prefix the user will keep typing into.
    pub closing: Option<char>,
}

/// The result of one completion request: the word's character span on the
/// line and the candidates that could replace it (sorted by display text,
/// deduplicated; empty when nothing applies).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    /// First character index of the word under completion.
    pub start: usize,
    /// One past the last character index (the cursor).
    pub end: usize,
    /// The candidates, sorted and deduplicated.
    ///
    /// Every entry is text the shell will insert verbatim: there is no
    /// display-only class. A placeholder position offers the real names
    /// behind it (through [`ResourceLister`]) or nothing at all.
    pub candidates: Vec<Candidate>,
}

impl Completion {
    /// The empty result at `cursor` (nothing applies).
    fn none(cursor: usize) -> Self {
        Self {
            start: cursor,
            end: cursor,
            candidates: Vec::new(),
        }
    }
}

/// The engine's read-only seam onto the *live* names a selector placeholder
/// stands for — the interfaces, bonds, interrupt lines, and CPUs of this
/// machine, plus the closed name tables another crate owns.
///
/// Mirrors [`DirLister`]: an object-safe read-only lookup, injected, so this
/// module stays pure and kernel-free and the production implementation (which
/// speaks to the System Information API) lives at the shell's edge. A test
/// injects a fake and drives every domain without a service.
///
/// # Capability adaptivity is the contract, not a fallback
///
/// An implementation is expected to answer with an **empty list** for a
/// domain this session could not enumerate — an interface list costs
/// `CAP_SYSINFO_HW`, which an ordinary session does not hold — and to do so
/// *without* issuing a request it knows will be refused, so a Tab press never
/// produces a denied query or an audit refusal record. The engine offers no
/// candidates there, which is exactly right: a session that cannot read
/// `info:net/<iface>/mac` gains nothing from being shown interface names.
pub trait ResourceLister {
    /// The live names in `domain`, in whatever order the source reports them
    /// (the engine sorts).
    ///
    /// # Errors
    ///
    /// The [`Errno`] the lookup failed with. An error and an empty list are
    /// treated alike by the engine — no candidates — so an implementation
    /// need not distinguish "cannot ask" from "nothing there".
    fn list(&self, domain: SelectorDomain) -> Result<Vec<String>, Errno>;
}

/// The syntactic role of the word under completion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Role {
    /// The command word of a simple command.
    Command,
    /// An ordinary argument word.
    Argument,
    /// The source word of a read-only redirection (`<`, `n<`).
    ///
    /// Distinguished from [`RedirTarget`](Self::RedirTarget) because the
    /// directions open different sets: only a read may name a value-backed
    /// reference, which the shell serves through the System Information API
    /// (`plans/ALIAS.md` §6.2).
    RedirSource,
    /// The target word of a writing redirection (`>`, `>>`, `<>`, `&>`) or a
    /// here-string.
    RedirTarget,
    /// A here-document delimiter (never completed).
    HereDocDelim,
}

/// Compute the completion for `line` with the cursor after character
/// `cursor` (character index, not byte). Read-only; never changes `$?`.
///
/// `env` carries the session's `HOME` and `PATH`: both are needed because the
/// user's own command and application stores are part of the search order, so
/// a completion given no home would offer fewer names than the shell can
/// actually run.
pub fn complete(
    line: &str,
    cursor: usize,
    env: CommandEnv<'_>,
    lister: &dyn DirLister,
    resources: &dyn ResourceLister,
) -> Completion {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let prefix: String = chars[..cursor].iter().collect();

    // The shell's own lexer decides the word boundaries. A prefix that does
    // not lex (an open quote, a dangling escape) completes to nothing.
    let Ok(tokens) = tokenize_with_spans(&prefix) else {
        return Completion::none(cursor);
    };

    let (start, before) = match tokens.last() {
        Some((Token::Word(_), span)) if span.end == cursor => {
            (span.start, &tokens[..tokens.len() - 1])
        }
        _ => (cursor, &tokens[..]),
    };
    let word: String = chars[start..cursor].iter().collect();

    // A word already carrying quoting or expansion syntax degrades to no
    // candidates rather than a guess at its expansion.
    if word
        .chars()
        .any(|c| matches!(c, '\'' | '"' | '\\' | '$' | '{' | '}'))
    {
        return Completion::none(cursor);
    }

    let role = word_role(before);
    let mut candidates = Vec::new();
    // A word the shared resolution rule reads as a resource reference is a
    // reference in every role — it can never name a path — so it completes
    // from the registry alone.
    let reference = tairix_resref::names_resource_reference(&word);
    // A *writing* redirection can only open a stream, so a value-backed
    // namespace is out of play there. A *reading* one may name either.
    let streams_only = role == Role::RedirTarget;
    match role {
        Role::HereDocDelim => {}
        Role::Command => {
            if reference {
                selector_candidates(&word, false, resources, &mut candidates);
            } else if word.contains('/') {
                path_candidates(&word, lister, &mut candidates);
            } else {
                command_candidates(&word, env, lister, &mut candidates);
            }
        }
        Role::Argument | Role::RedirSource | Role::RedirTarget => {
            if reference {
                selector_candidates(&word, streams_only, resources, &mut candidates);
            } else {
                // A redirection's blank slate offers every namespace it could
                // open; an empty argument word offers none (it would bury the
                // paths).
                namespace_candidates(
                    &word,
                    matches!(role, Role::RedirSource | Role::RedirTarget),
                    streams_only,
                    &mut candidates,
                );
                path_candidates(&word, lister, &mut candidates);
            }
        }
    }

    candidates.sort_by(|a, b| a.display.cmp(&b.display));
    candidates.dedup_by(|a, b| a.insert == b.insert);
    Completion {
        start,
        end: cursor,
        candidates,
    }
}

/// Decide the role of the word being completed from the tokens before it.
fn word_role(before: &[(Token, core::ops::Range<usize>)]) -> Role {
    // A redirection operator immediately before the word claims it.
    if let Some((Token::Redirect(op), _)) = before.last() {
        return match op {
            // Only a plain read (`<`, `n<`) may name a value-backed
            // reference. `<>` is a write direction too, and the shell refuses
            // a value-backed reference there rather than silently serving
            // half of what was asked for, so it completes as a target.
            RedirOp::File {
                mode: OpenMode::Read,
                ..
            } => Role::RedirSource,
            RedirOp::File { .. } | RedirOp::Combined { .. } | RedirOp::HereString { .. } => {
                Role::RedirTarget
            }
            RedirOp::HereDoc { .. } => Role::HereDocDelim,
            // `2>&1` / `>&-` need no target; the next word is ordinary.
            RedirOp::Dup { .. } | RedirOp::Close { .. } => role_from_command_words(before),
        };
    }
    role_from_command_words(before)
}

/// Command position versus argument: walk the current simple command (since
/// the last control operator) and see whether a command word was already
/// typed. Leading `NAME=value` assignment words do not take the command
/// position, matching the interpreter's own prefix-assignment rule.
fn role_from_command_words(before: &[(Token, core::ops::Range<usize>)]) -> Role {
    let mut saw_command_word = false;
    for (token, _) in before {
        match token {
            Token::Pipe
            | Token::PipeBoth
            | Token::AndIf
            | Token::OrIf
            | Token::Semicolon
            | Token::Ampersand
            | Token::Bang => saw_command_word = false,
            Token::Word(word) => {
                if saw_command_word {
                    continue;
                }
                // An assignment word leaves the command position open.
                if crate::env::assignment_split(word).is_none() {
                    saw_command_word = true;
                }
            }
            Token::Redirect(_) => {}
        }
    }
    if saw_command_word {
        Role::Argument
    } else {
        Role::Command
    }
}

/// Escape a candidate name so inserting it keeps the line lexing as one
/// word: whitespace, quotes, and the shell's operator/expansion characters
/// are backslash-escaped.
fn escape_word(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(
            c,
            ' ' | '\t'
                | '\''
                | '"'
                | '\\'
                | '$'
                | '|'
                | '&'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '{'
                | '}'
                | '!'
                | '#'
                | '*'
                | '?'
                | '['
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Candidates for a bare command word: builtins plus the `.app` bundles of
/// the shared command-search directories, so exactly the resolvable names
/// are offered.
fn command_candidates(
    word: &str,
    env: CommandEnv<'_>,
    lister: &dyn DirLister,
    out: &mut Vec<Candidate>,
) {
    for &name in BUILTIN_NAMES {
        if name.starts_with(word) {
            out.push(Candidate {
                insert: String::from(name),
                display: String::from(name),
                closing: Some(' '),
            });
        }
    }
    for dir in tairix_cmdres::command_search_dirs(env) {
        let Ok(entries) = lister.list_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let Some(name) = entry.name.strip_suffix(".app") else {
                continue;
            };
            if !entry.is_dir || name.is_empty() || !name.starts_with(word) {
                continue;
            }
            out.push(Candidate {
                insert: escape_word(name),
                display: String::from(name),
                closing: Some(' '),
            });
        }
    }
}

/// The registered namespace prefixes a plain (not yet reference-shaped) word
/// could begin: `st` offers `state:` and `stats:`.
///
/// The two flags are independent, and conflating them would be a bug:
///
/// * `redirection` marks any redirection's source or target, where an empty
///   word offers every namespace — as likely there as a filename, whereas an
///   empty *argument* word offering namespaces would bury the paths.
/// * `streams_only` restricts the offer to *stream-backed* namespaces, set for
///   a **writing** redirection where a value-backed one is a dead end
///   (`plans/ALIAS.md` §15.3, and the kernel resolver's own refusal). A read
///   leaves it clear: the shell serves that itself.
///
/// The candidate stays open (no closing character): the user carries on into
/// the selector.
fn namespace_candidates(
    word: &str,
    redirection: bool,
    streams_only: bool,
    out: &mut Vec<Candidate>,
) {
    if word.is_empty() && !redirection {
        return;
    }
    for namespace in KnownNamespace::ALL {
        if streams_only && namespace.backing() != NamespaceBacking::Stream {
            continue;
        }
        let name = namespace.as_str();
        if name.starts_with(word) {
            out.push(Candidate {
                insert: format!("{name}:"),
                display: format!("{name}:"),
                closing: None,
            });
        }
    }
}

/// Candidates for a reference-shaped `word` (`state:net/wan/li`): the next
/// selector segment, from the namespace's registry catalogue.
///
/// The walk mirrors path completion — the segments before the last `/` are
/// fixed, the text after it is the prefix being completed — so a deep
/// selector is reached one segment at a time. A catalogue placeholder at the
/// completing position is expanded through `resources` into the live names of
/// its [`SelectorDomain`], each offered as an ordinary candidate; a domain
/// that lists nothing (or cannot be listed at all) contributes nothing. See
/// [`ResourceLister`] for why that silence is the correct answer rather than
/// a degradation.
///
/// A domain is listed at most once per call, however many catalogue entries
/// share the placeholder — `stats:net/<iface>/…` has ten leaves but one
/// interface list — so a Tab press costs one lookup per distinct domain. No
/// result is cached across calls: a hot-plugged interface appears on the
/// user's next Tab, not after a restart.
///
/// `streams_only` marks a redirection target, where a value-backed namespace
/// offers nothing at all: `echo hi > info:` names something no descriptor can
/// be opened on, so completing its selectors would only lead the user into a
/// dead end (`plans/ALIAS.md` §15.3).
///
/// Inserts are built verbatim rather than through [`escape_word`]: every
/// character of the reference grammar (`a-z0-9-_./:?=`) is an ordinary word
/// character to the shell's lexer, so escaping would only inject backslashes
/// into the reference. A *listed* name is both filtered to the selector
/// grammar ([`is_selector_segment`]) and shell-escaped, so a name from outside
/// this crate can neither produce a reference the parser would reject nor
/// inject shell syntax.
fn selector_candidates(
    word: &str,
    streams_only: bool,
    resources: &dyn ResourceLister,
    out: &mut Vec<Candidate>,
) {
    // The rule that classified `word` guarantees a `:` with a registered
    // namespace before it, but this function must stand on its own.
    let Some(colon) = word.find(':') else {
        return;
    };
    let (prefix, rest) = (&word[..colon], &word[colon + 1..]);
    let Some(namespace) = KnownNamespace::from_name(prefix) else {
        return;
    };
    if streams_only && namespace.backing() != NamespaceBacking::Stream {
        return;
    }
    let (fixed, leaf) = tairix_complete::split_path_word(rest);
    // `fixed` is empty or ends in `/`, so the split's last element is always
    // the empty tail after that separator. Any *other* empty element is a
    // malformed selector (`net//link`): complete it to nothing rather than to
    // a spelling the reference parser would reject.
    let mut typed: Vec<&str> = fixed.split('/').collect();
    typed.pop();
    if typed.iter().any(|segment| segment.is_empty()) {
        return;
    }
    let catalogue = namespace.selector_catalogue();
    // Names already fetched in this call, so several entries sharing a
    // placeholder cost one lookup rather than one each.
    let mut listed: Vec<(SelectorDomain, Vec<String>)> = Vec::new();
    for entry in catalogue {
        let segments: Vec<&str> = entry.segments().collect();
        // Nothing left to offer once the whole selector is typed.
        if segments.len() <= typed.len() {
            continue;
        }
        if !matches_typed(&segments, &typed, catalogue) {
            continue;
        }
        let next = segments[typed.len()];
        let last = segments.len() == typed.len() + 1;
        if let Some(domain) = placeholder_domain(next) {
            // A name only the running machine (or another crate's closed
            // table) knows: offer the real names, never the placeholder text
            // — the shell cannot insert `<iface>` and the user cannot use it.
            for name in domain_names(&mut listed, domain, resources) {
                if name.is_empty() || !name.starts_with(leaf) || !is_selector_segment(name) {
                    continue;
                }
                let mut insert = format!("{prefix}:{fixed}{}", escape_word(name));
                let mut display = String::from(name);
                let closing = close_segment(last, entry.mandatory_param, &mut insert, &mut display);
                out.push(Candidate {
                    insert,
                    display,
                    closing,
                });
            }
            continue;
        }
        if !next.starts_with(leaf) {
            continue;
        }
        let mut insert = format!("{prefix}:{fixed}{next}");
        let mut display = String::from(next);
        let closing = close_segment(last, entry.mandatory_param, &mut insert, &mut display);
        out.push(Candidate {
            insert,
            display,
            closing,
        });
    }
}

/// Finish one completed selector segment, returning its closing character.
///
/// The one rule for both a literal catalogue segment and a listed name, so
/// the two can never disagree about when a reference is finished:
///
/// * a non-final segment gains `/` and stays open, exactly as a directory
///   does;
/// * a final segment closes the word with a space;
/// * a final segment whose reference is invalid without a query parameter (a
///   windowed rate, undefined without its sampling window) instead carries
///   that parameter and stays open for the value.
///
/// A placeholder can be either: `stats:net/<iface>/rx.bytes` continues past
/// the interface name, while `stats:limits/<kind>` *is* the whole reference —
/// and `stats:mem/reclaim/<class>` is both, since a sibling entry continues to
/// `/self`, so that name is offered twice (finished and open) just as a
/// literal segment in the same shape already is.
fn close_segment(
    last: bool,
    mandatory_param: Option<&'static str>,
    insert: &mut String,
    display: &mut String,
) -> Option<char> {
    if !last {
        insert.push('/');
        display.push('/');
        return None;
    }
    match mandatory_param {
        Some(param) => {
            for text in [insert, display] {
                text.push('?');
                text.push_str(param);
                text.push('=');
            }
            None
        }
        None => Some(' '),
    }
}

/// The live names of `domain`, listed at most once per completion however
/// many catalogue entries share the placeholder: `cache` holds what has
/// already been asked for.
///
/// A refusal and an empty list are the same answer — no names — so the caller
/// need not distinguish "cannot ask" from "nothing there".
fn domain_names<'a>(
    cache: &'a mut Vec<(SelectorDomain, Vec<String>)>,
    domain: SelectorDomain,
    resources: &dyn ResourceLister,
) -> &'a [String] {
    if !cache.iter().any(|(known, _)| *known == domain) {
        cache.push((domain, resources.list(domain).unwrap_or_default()));
    }
    cache
        .iter()
        .find(|(known, _)| *known == domain)
        .map_or(&[], |(_, names)| names.as_slice())
}

/// Whether `name` is spellable as one selector segment: within the parser's
/// own length bound ([`tairix_resref::MAX_SEGMENT_LEN`]) and made only of
/// characters the grammar allows in a segment (`a-z A-Z 0-9 - _ .`), so a
/// completed reference always re-parses.
///
/// Applied to *listed* names only: catalogue spellings are the registry's own
/// and need no filtering.
fn is_selector_segment(name: &str) -> bool {
    name.len() <= tairix_resref::MAX_SEGMENT_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `segments` (one catalogue entry) is still in play given the
/// non-empty `typed` segments: each fixed position must match, literally for
/// a literal segment and by any name for a placeholder.
///
/// A literal sibling wins its position: when some entry in `catalogue`
/// spells `typed[i]` literally there, entries that would match it only
/// through a placeholder are out. That mirrors the resolvers' own arm
/// order — `state:net/resolver/…` and `stats:net/stack/…` are matched before
/// `net/<iface>/…`, which makes `resolver` and `stack` reserved interface
/// names — so completion offers exactly what would resolve.
fn matches_typed(segments: &[&str], typed: &[&str], catalogue: &[SelectorEntry]) -> bool {
    for (index, &fixed) in typed.iter().enumerate() {
        let Some(&segment) = segments.get(index) else {
            return false;
        };
        if is_placeholder(segment) {
            if literal_claims(catalogue, typed, index, fixed) {
                return false;
            }
        } else if segment != fixed {
            return false;
        }
    }
    true
}

/// Whether any catalogue entry spells `fixed` literally at `index`, having
/// itself matched every earlier typed segment. Such an entry claims the
/// position, so a placeholder cannot also match there.
fn literal_claims(catalogue: &[SelectorEntry], typed: &[&str], index: usize, fixed: &str) -> bool {
    catalogue.iter().any(|entry| {
        let segments: Vec<&str> = entry.segments().collect();
        segments.get(index) == Some(&fixed)
            && segments
                .iter()
                .zip(typed)
                .take(index)
                .all(|(&segment, &earlier)| is_placeholder(segment) || segment == earlier)
    })
}

/// Filesystem path candidates: the shared `lib/complete` policy (the
/// word's directory part is listed — the working directory for a bare
/// name — leaf-prefix matches are offered, and hidden (dot) entries only
/// when the prefix asks for them), dressed in the shell's presentation: a
/// shell-escaped insert, and a directory candidate ending in `/` that
/// stays open for further completion.
fn path_candidates(word: &str, lister: &dyn DirLister, out: &mut Vec<Candidate>) {
    let (dir_part, _) = tairix_complete::split_path_word(word);
    for entry in tairix_complete::path_matches(word, ".", lister) {
        let mut insert = String::from(dir_part);
        insert.push_str(&escape_word(&entry.name));
        let (display, closing) = if entry.is_dir {
            insert.push('/');
            (format!("{}/", entry.name), None)
        } else {
            (entry.name.clone(), Some(' '))
        };
        out.push(Candidate {
            insert,
            display,
            closing,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Candidate, CommandEnv, Completion, DirEntryInfo, DirLister, ResourceLister, SelectorDomain,
    };
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use tairix_abi::Errno;

    /// A resource lister with nothing to offer in any domain — the shape of a
    /// session holding no enumeration capability, and the default for the
    /// scenarios that are about paths, commands, and catalogue spellings.
    struct NoResources;

    impl ResourceLister for NoResources {
        fn list(&self, _domain: SelectorDomain) -> Result<Vec<String>, Errno> {
            Ok(Vec::new())
        }
    }

    /// A lister with fixed names per domain; any domain not listed answers
    /// empty. `refuse` makes the named domain fail instead, so a refusal and
    /// an empty list can be told apart in a test even though the engine
    /// treats them alike.
    struct MapResources {
        names: Vec<(SelectorDomain, Vec<String>)>,
        refuse: Option<SelectorDomain>,
        asked: core::cell::RefCell<Vec<SelectorDomain>>,
    }

    impl MapResources {
        fn new(names: &[(SelectorDomain, &[&str])]) -> Self {
            Self {
                names: names
                    .iter()
                    .map(|(domain, names)| {
                        (
                            *domain,
                            names.iter().map(|name| (*name).to_string()).collect(),
                        )
                    })
                    .collect(),
                refuse: None,
                asked: core::cell::RefCell::new(Vec::new()),
            }
        }

        fn refusing(domain: SelectorDomain) -> Self {
            Self {
                refuse: Some(domain),
                ..Self::new(&[])
            }
        }
    }

    impl ResourceLister for MapResources {
        fn list(&self, domain: SelectorDomain) -> Result<Vec<String>, Errno> {
            self.asked.borrow_mut().push(domain);
            if self.refuse == Some(domain) {
                return Err(Errno::PermissionDenied);
            }
            Ok(self
                .names
                .iter()
                .find(|(candidate, _)| *candidate == domain)
                .map(|(_, names)| names.clone())
                .unwrap_or_default())
        }
    }

    /// The engine with no live selector names available: the default for
    /// every scenario that is not about placeholder expansion, so those read
    /// exactly as before.
    fn complete(
        line: &str,
        cursor: usize,
        env: CommandEnv<'_>,
        lister: &dyn DirLister,
    ) -> Completion {
        super::complete(line, cursor, env, lister, &NoResources)
    }

    /// An in-memory directory tree: `(path, entries)` pairs.
    struct MapLister {
        dirs: Vec<(String, Vec<DirEntryInfo>)>,
    }

    impl MapLister {
        fn new(dirs: &[(&str, &[(&str, bool)])]) -> Self {
            Self {
                dirs: dirs
                    .iter()
                    .map(|(path, entries)| {
                        (
                            (*path).to_string(),
                            entries
                                .iter()
                                .map(|(name, is_dir)| DirEntryInfo {
                                    name: (*name).to_string(),
                                    is_dir: *is_dir,
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            }
        }
    }

    impl DirLister for MapLister {
        fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>, Errno> {
            self.dirs
                .iter()
                .find(|(path, _)| path == dir)
                .map(|(_, entries)| entries.clone())
                .ok_or(Errno::NotFound)
        }
    }

    fn inserts(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.insert.as_str()).collect()
    }

    fn displays(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.display.as_str()).collect()
    }

    /// A command-position word completes from builtins and the `.app`
    /// bundles of every search directory — both system stores, the user's
    /// own two, and `PATH` — exactly the resolvable names.
    #[test]
    fn command_position_offers_builtins_and_bundles() {
        let lister = MapLister::new(&[
            (
                "/System/Commands",
                &[("cat.app", true), ("cp.app", true), ("notes.txt", false)],
            ),
            ("/System/Applications", &[("calendar.app", true)]),
            ("/Users/ada/Commands", &[("collate.app", true)]),
            ("/Users/ada/Applications", &[("chart.app", true)]),
            ("/Users/ada/bin", &[("cargo.app", true)]),
        ]);
        let result = complete(
            "c",
            1,
            CommandEnv {
                home: Some("/Users/ada"),
                path_var: Some("/Users/ada/bin"),
            },
            &lister,
        );
        assert_eq!(result.start, 0);
        assert_eq!(result.end, 1);
        assert_eq!(
            inserts(&result.candidates),
            ["calendar", "cargo", "cat", "cd", "chart", "collate", "cp"]
        );
        assert!(result.candidates.iter().all(|c| c.closing == Some(' ')));

        // A session with no home has no user stores to search, so the same
        // tree offers only the system stores and `PATH`.
        let homeless = complete(
            "c",
            1,
            CommandEnv {
                home: None,
                path_var: Some("/Users/ada/bin"),
            },
            &lister,
        );
        assert_eq!(
            inserts(&homeless.candidates),
            ["calendar", "cargo", "cat", "cd", "cp"]
        );
    }

    /// An argument word completes as a path; directories gain `/` and stay
    /// open, files close with a space, dotfiles hide unless asked for.
    #[test]
    fn argument_completes_paths() {
        let lister = MapLister::new(&[(
            ".",
            &[("notes.txt", false), ("notebooks", true), (".notrc", false)],
        )]);
        let result = complete("cat no", 6, CommandEnv::default(), &lister);
        assert_eq!(result.start, 4);
        assert_eq!(
            inserts(&result.candidates),
            ["notebooks/", "notes.txt"],
            "dotfiles stay hidden"
        );
        assert_eq!(result.candidates[0].closing, None);
        assert_eq!(result.candidates[1].closing, Some(' '));

        let hidden = complete("cat .no", 7, CommandEnv::default(), &lister);
        assert_eq!(inserts(&hidden.candidates), [".notrc"]);
    }

    /// A sub-path word lists its directory part and keeps it on the insert.
    #[test]
    fn argument_completes_subdirectory_paths() {
        let lister = MapLister::new(&[("/Users", &[("ada", true), ("bob", true)])]);
        let result = complete("ls /Users/a", 11, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["/Users/ada/"]);
    }

    /// A redirection target offers resource namespaces alongside files, and
    /// a typed namespace completes its catalogued selectors.
    #[test]
    fn redirection_target_offers_resources() {
        let lister = MapLister::new(&[(".", &[("sysinfo.txt", false)])]);
        let result = complete("echo hi > sys", 13, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["sys:", "sysinfo.txt"]);

        let selectors = complete("cat < sys:", 10, CommandEnv::default(), &lister);
        assert_eq!(inserts(&selectors.candidates), ["sys:null", "sys:random"]);
        assert_eq!(displays(&selectors.candidates), ["null", "random"]);

        let narrowed = complete("cat < sys:r", 11, CommandEnv::default(), &lister);
        assert_eq!(inserts(&narrowed.candidates), ["sys:random"]);
    }

    /// A *reading* redirection offers value-backed namespaces as well as
    /// streams, because the shell can now serve one: it reads the value
    /// through the System Information API and feeds it to the child down a
    /// pipe, so `cat < info:mem/physical` works and completing towards it
    /// leads somewhere.
    #[test]
    fn a_redirection_source_also_offers_value_namespaces() {
        let lister = MapLister::new(&[(".", &[])]);
        let resources = MapResources::new(&[(SelectorDomain::Interface, &["wan"])]);

        // The blank slate after `<` lists every namespace a read can open —
        // the streams and the value-backed trio alike.
        let blank = super::complete("cat < ", 6, CommandEnv::default(), &lister, &resources);
        let offered = inserts(&blank.candidates);
        for namespace in ["sys:", "info:", "state:", "stats:"] {
            assert!(offered.contains(&namespace), "got {offered:?}");
        }

        // And a typed value-backed prefix completes its selectors, segment by
        // segment, exactly as an argument does.
        let namespace = super::complete(
            "cat < info:",
            11,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert!(
            !namespace.candidates.is_empty(),
            "`< info:` must offer its selectors"
        );
        let mem = super::complete(
            "cat < info:mem/",
            15,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert!(
            displays(&mem.candidates).contains(&"physical"),
            "got {:?}",
            displays(&mem.candidates)
        );

        // An explicit-fd read (`3< ref`) is the same direction, so it offers
        // the same set.
        let explicit = super::complete(
            "cat 3< stats:",
            13,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert!(
            !explicit.candidates.is_empty(),
            "an explicit-fd read offers value selectors too"
        );

        // A placeholder segment still resolves to the live names, never the
        // literal `<iface>` text — `/`-suffixed, since the interface's own
        // leaves come next.
        let iface = super::complete(
            "cat < info:net/",
            15,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(displays(&iface.candidates), ["wan/"]);
    }

    /// A *writing* redirection can only open a stream, so a value-backed
    /// namespace is offered nowhere in that role — neither as a prefix nor as
    /// a reference-shaped word — while a stream namespace still completes
    /// exactly as before. The read direction is the sibling test
    /// [`a_redirection_source_also_offers_value_namespaces`].
    #[test]
    fn a_redirection_target_offers_only_stream_namespaces() {
        let lister = MapLister::new(&[(".", &[])]);
        let resources = MapResources::new(&[(SelectorDomain::Interface, &["wan"])]);

        // The blank slate lists the namespaces a redirection could open, and
        // no others.
        let blank = super::complete("echo hi > ", 10, CommandEnv::default(), &lister, &resources);
        let offered = inserts(&blank.candidates);
        assert!(offered.contains(&"sys:"), "got {offered:?}");
        for value in ["info:", "state:", "stats:"] {
            assert!(!offered.contains(&value), "{value} cannot be opened");
        }

        // A typed value-backed prefix offers nothing, and neither do its
        // selectors: the whole namespace is a dead end here.
        for line in [
            "echo hi > info",
            "echo hi > info:",
            "echo hi > info:net/",
            "echo hi >> stats:",
            "echo hi 2> state:net/",
            // `<>` asks to write as well as read, and the shell refuses a
            // value-backed reference there rather than serving half the
            // request, so it is a dead end too.
            "cat <> info:",
            // A here-string's word is content, not a target to open.
            "cat <<< info:",
        ] {
            let cursor = line.chars().count();
            let result = super::complete(line, cursor, CommandEnv::default(), &lister, &resources);
            assert!(
                result.candidates.is_empty(),
                "{line:?} must offer nothing, got {:?}",
                displays(&result.candidates)
            );
        }

        // `sys:` is a stream: unchanged.
        let stream = super::complete(
            "echo hi > sys:",
            14,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(displays(&stream.candidates), ["null", "random"]);

        // As an *argument* the same reference still completes: reading a
        // value is what `sysinfo show info:…` does, and only redirection is
        // restricted to streams.
        let argument = super::complete(
            "cat info:net/",
            13,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(inserts(&argument.candidates), ["info:net/wan/"]);
    }

    /// An argument word also completes a resource reference once it could
    /// begin one — `cat sys:r` → `cat sys:random`.
    #[test]
    fn argument_completes_resource_references() {
        let lister = MapLister::new(&[(".", &[])]);
        let result = complete("cat sys:r", 9, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["sys:random"]);
        // But an empty argument word does not spam namespaces.
        let empty = complete("cat ", 4, CommandEnv::default(), &lister);
        assert!(empty.candidates.is_empty());
    }

    /// The registry's whole namespace set is offered from a shared prefix, so
    /// `st` reaches `state:` and `stats:` as well as any file.
    #[test]
    fn namespace_prefixes_are_offered_from_a_partial_word() {
        let lister = MapLister::new(&[(".", &[("stack.txt", false)])]);
        let result = complete("cat st", 6, CommandEnv::default(), &lister);
        assert_eq!(
            inserts(&result.candidates),
            ["stack.txt", "state:", "stats:"]
        );
        // A namespace prefix stays open: the user carries on into a selector.
        for candidate in &result.candidates {
            if candidate.insert.ends_with(':') {
                assert_eq!(candidate.closing, None);
            }
        }
    }

    /// A namespace completes its selectors one segment at a time, exactly as
    /// a path does: a non-final segment gains `/` and stays open, a leaf
    /// closes the word.
    #[test]
    fn namespace_selectors_complete_one_segment_at_a_time() {
        let lister = MapLister::new(&[(".", &[])]);

        let top = complete("cat state:", 10, CommandEnv::default(), &lister);
        assert_eq!(inserts(&top.candidates), ["state:irq/", "state:net/"]);
        assert_eq!(displays(&top.candidates), ["irq/", "net/"]);
        assert!(top.candidates.iter().all(|c| c.closing.is_none()));

        let leaves = complete("cat state:net/wan/", 18, CommandEnv::default(), &lister);
        assert_eq!(
            displays(&leaves.candidates),
            ["active-member", "address", "link", "member-health"]
        );
        assert_eq!(leaves.candidates[2].insert, "state:net/wan/link");
        assert!(leaves.candidates.iter().all(|c| c.closing == Some(' ')));

        // A partial leaf narrows, and the span covers the whole word so the
        // insert replaces it entire.
        let narrowed = complete("cat state:net/wan/li", 20, CommandEnv::default(), &lister);
        assert_eq!(inserts(&narrowed.candidates), ["state:net/wan/link"]);
        assert_eq!((narrowed.start, narrowed.end), (4, 20));
    }

    /// A `.`-separated leaf is one segment, so the counter family narrows on
    /// its own prefix.
    #[test]
    fn dotted_leaves_narrow_within_one_segment() {
        let lister = MapLister::new(&[(".", &[])]);
        let result = complete("cat stats:net/wan/rx.", 21, CommandEnv::default(), &lister);
        assert_eq!(
            displays(&result.candidates),
            [
                "rx.bps?window=",
                "rx.bytes",
                "rx.dropped",
                "rx.filtered",
                "rx.packets",
                "rx.pps?window="
            ]
        );
    }

    /// A rate is undefined without a sampling window, so its completion
    /// carries the mandatory parameter and stays open for the value rather
    /// than closing as a finished word.
    #[test]
    fn a_windowed_rate_completes_with_its_mandatory_parameter() {
        let lister = MapLister::new(&[(".", &[])]);
        let result = complete(
            "cat stats:net/wan/rx.pp",
            23,
            CommandEnv::default(),
            &lister,
        );
        assert_eq!(
            inserts(&result.candidates),
            ["stats:net/wan/rx.pps?window="]
        );
        assert_eq!(result.candidates[0].closing, None);
    }

    /// A placeholder position offers the *live names* behind it, never the
    /// `<iface>` text: `info:net/<Tab>` lists this machine's interfaces, each
    /// an ordinary candidate that stays open for the leaf that follows.
    #[test]
    fn a_placeholder_offers_the_listed_names() {
        let lister = MapLister::new(&[(".", &[])]);
        let resources = MapResources::new(&[(SelectorDomain::Interface, &["wan", "lan0"])]);

        let result = super::complete(
            "cat info:net/",
            13,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(
            inserts(&result.candidates),
            ["info:net/lan0/", "info:net/wan/"]
        );
        assert_eq!(displays(&result.candidates), ["lan0/", "wan/"]);
        assert!(
            result.candidates.iter().all(|c| c.closing.is_none()),
            "a listed name stays open for the leaf that follows"
        );
        // Ten `stats:net/<iface>/…` entries share one placeholder, so one
        // Tab press costs one lookup, not one per entry.
        let counted = MapResources::new(&[(SelectorDomain::Interface, &["wan"])]);
        let rates = super::complete(
            "cat stats:net/",
            14,
            CommandEnv::default(),
            &lister,
            &counted,
        );
        assert!(inserts(&rates.candidates).contains(&"stats:net/wan/"));
        assert_eq!(
            counted
                .asked
                .borrow()
                .iter()
                .filter(|domain| **domain == SelectorDomain::Interface)
                .count(),
            1
        );

        // A partly typed name narrows the listing, exactly as a path does.
        let narrowed = super::complete(
            "cat info:net/l",
            14,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(inserts(&narrowed.candidates), ["info:net/lan0/"]);

        // A literal sibling still claims its own position beside the names.
        let one_iface = MapResources::new(&[(SelectorDomain::Interface, &["wan"])]);
        let both = super::complete(
            "cat state:net/",
            14,
            CommandEnv::default(),
            &lister,
            &one_iface,
        );
        assert_eq!(
            inserts(&both.candidates),
            ["state:net/resolver/", "state:net/wan/"]
        );
    }

    /// A closed table another crate owns needs no service and no capability,
    /// so its names are always offered — `info:limits/<Tab>` reaches the
    /// resource-limit kinds for any session.
    #[test]
    fn a_closed_table_placeholder_expands() {
        let lister = MapLister::new(&[(".", &[])]);
        let resources = MapResources::new(&[(
            SelectorDomain::LimitKind,
            &["open-streams", "processes", "stack-bytes"],
        )]);
        let result = super::complete(
            "cat info:limits/",
            16,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(
            inserts(&result.candidates),
            [
                "info:limits/open-streams/",
                "info:limits/processes/",
                "info:limits/stack-bytes/"
            ]
        );
        // The leaves resume past the listed name, so the whole reference is
        // reachable one segment at a time.
        let leaves = super::complete(
            "cat info:limits/processes/",
            26,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(displays(&leaves.candidates), ["hard", "soft"]);
    }

    /// A placeholder can be the *whole* remaining reference rather than a
    /// step towards one, and a listed name must then close the word instead of
    /// gaining a `/`: `stats:limits/<kind>` is a complete metric. Where a
    /// sibling entry continues past the same placeholder
    /// (`stats:mem/reclaim/<class>` and `…/<class>/self`) the name is offered
    /// both ways, exactly as a literal segment in that shape already is.
    #[test]
    fn a_final_placeholder_closes_the_word() {
        let lister = MapLister::new(&[(".", &[])]);
        let resources = MapResources::new(&[
            (SelectorDomain::LimitKind, &["open-streams", "processes"]),
            (SelectorDomain::ReclaimClass, &["clean-file-data"]),
        ]);

        // The placeholder *is* the last segment: a finished reference.
        let limits = super::complete(
            "cat stats:limits/",
            17,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(
            inserts(&limits.candidates),
            ["stats:limits/open-streams", "stats:limits/processes"]
        );
        assert!(
            limits.candidates.iter().all(|c| c.closing == Some(' ')),
            "a complete reference closes as a finished word"
        );

        // Both shapes at once: the class alone is a metric, and `/self` is a
        // narrower one under the same name.
        let reclaim = super::complete(
            "cat stats:mem/reclaim/",
            22,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(
            inserts(&reclaim.candidates),
            [
                "stats:mem/reclaim/clean-file-data",
                "stats:mem/reclaim/clean-file-data/",
                "stats:mem/reclaim/total",
                "stats:mem/reclaim/total/"
            ]
        );

        // And the leaf under a listed name still resolves.
        let leaf = super::complete(
            "cat stats:mem/reclaim/clean-file-data/",
            38,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(displays(&leaf.candidates), ["self"]);
    }

    /// A domain this session cannot enumerate yields **nothing** — and a
    /// refusal is the same answer as an empty list. That is not a
    /// degradation: a session that cannot list interfaces cannot read an
    /// interface's facts either, so there was nothing there for it.
    #[test]
    fn an_unlistable_domain_offers_nothing() {
        let lister = MapLister::new(&[(".", &[])]);

        // A domain that lists nothing, and one that cannot be listed at all:
        // the engine treats them alike, and neither ever falls back to
        // offering the placeholder text.
        let nothing = MapResources::new(&[(SelectorDomain::Interface, &[])]);
        let denied = MapResources::refusing(SelectorDomain::Interface);
        for resources in [
            &nothing as &dyn ResourceLister,
            &denied as &dyn ResourceLister,
        ] {
            let result = super::complete(
                "cat info:net/",
                13,
                CommandEnv::default(),
                &lister,
                resources,
            );
            assert!(
                result.candidates.is_empty(),
                "got {:?}",
                displays(&result.candidates)
            );
        }
        // The refusal really was reached, rather than the domain never being
        // asked for. Both shapes this position accepts are asked — a plain
        // interface and a bond alias — so the check is membership, not the
        // whole list.
        assert!(denied.asked.borrow().contains(&SelectorDomain::Interface));
        assert!(denied.asked.borrow().contains(&SelectorDomain::Bond));
    }

    /// A listed name that could not be spelled back as one selector segment
    /// is dropped: completion never offers a reference the parser would
    /// reject, whatever the service reported.
    #[test]
    fn a_listed_name_outside_the_grammar_is_dropped() {
        let lister = MapLister::new(&[(".", &[])]);
        let resources = MapResources::new(&[(
            SelectorDomain::Interface,
            &["wan", "", "with space", "sla/sh", "semi;colon", "quo\"te"],
        )]);
        let result = super::complete(
            "cat info:net/",
            13,
            CommandEnv::default(),
            &lister,
            &resources,
        );
        assert_eq!(inserts(&result.candidates), ["info:net/wan/"]);
    }

    /// A literal sibling claims its position, mirroring the resolvers' own
    /// arm order: `net/stack` and `net/resolver` are reserved names, so they
    /// never also match as an interface, and their leaves are not offered
    /// under an interface that happens to be typed there.
    #[test]
    fn a_literal_segment_claims_its_position_from_a_placeholder() {
        let lister = MapLister::new(&[(".", &[])]);

        // `stack` resolves only its own defence counters.
        let stack = complete("cat stats:net/stack/", 20, CommandEnv::default(), &lister);
        assert_eq!(stack.candidates.len(), 11, "the defence counters");
        assert!(
            stack
                .candidates
                .iter()
                .all(|c| c.insert.starts_with("stats:net/stack/")),
            "no interface counters leak in: {:?}",
            displays(&stack.candidates)
        );

        // An interface's counters still complete under a real name.
        let iface = complete("cat stats:net/wan/", 18, CommandEnv::default(), &lister);
        assert_eq!(iface.candidates.len(), 11, "7 counters and 4 rates");

        // A fully typed leaf is a dead end, not a placeholder match.
        for line in ["cat stats:irq/count/", "cat stats:cpu/load/"] {
            let cursor = line.chars().count();
            let result = complete(line, cursor, CommandEnv::default(), &lister);
            assert!(
                result.candidates.is_empty(),
                "{line:?} should be a dead end, got {:?}",
                displays(&result.candidates)
            );
        }
    }

    /// A reference-shaped word completes as a reference at command position
    /// too — a bare `state:<Tab>` at the prompt lists the namespace — while a
    /// word that is not reference-shaped still offers only runnable names.
    #[test]
    fn command_position_completes_a_reference_shaped_word() {
        let lister = MapLister::new(&[("/System/Commands", &[("stat.app", true)]), (".", &[])]);

        let reference = complete("state:", 6, CommandEnv::default(), &lister);
        assert_eq!(inserts(&reference.candidates), ["state:irq/", "state:net/"]);

        // Without the `:` the word is a command name: no namespace noise.
        let command = complete("stat", 4, CommandEnv::default(), &lister);
        assert_eq!(inserts(&command.candidates), ["stat"]);
    }

    /// The `Alias:/path` spelling is a path, not a reference, so it completes
    /// through the filesystem — the shared classification rule, never a
    /// second copy of it.
    #[test]
    fn an_alias_path_still_completes_as_a_path() {
        let lister = MapLister::new(&[("sys:", &[("etc", true)])]);
        let result = complete("cat sys:/e", 10, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["sys:/etc/"]);
    }

    /// A selector with an empty segment is malformed — the reference parser
    /// rejects it — so it completes to nothing rather than to a spelling that
    /// could never resolve.
    #[test]
    fn a_malformed_selector_completes_to_nothing() {
        let lister = MapLister::new(&[(".", &[])]);
        for line in ["cat state:net//", "cat state:net//li", "cat stats://mem/"] {
            let cursor = line.chars().count();
            let result = complete(line, cursor, CommandEnv::default(), &lister);
            assert!(
                result.candidates.is_empty(),
                "{line:?} is malformed and must offer nothing, got {:?}",
                displays(&result.candidates)
            );
        }
    }

    /// A namespace with no resolver wired advertises nothing, so completion
    /// cannot offer a name that would resolve to nothing.
    #[test]
    fn an_unserved_namespace_completes_to_nothing() {
        let lister = MapLister::new(&[(".", &[])]);
        for line in ["cat disk:", "cat tty:", "cat proc:"] {
            let cursor = line.chars().count();
            let result = complete(line, cursor, CommandEnv::default(), &lister);
            assert!(
                result.candidates.is_empty(),
                "{line:?} has no wired resolver and must offer nothing"
            );
        }
    }

    /// The word before a pipe or `;` is a fresh command position.
    #[test]
    fn command_position_resets_after_control_operators() {
        let lister = MapLister::new(&[("/System/Commands", &[("cat.app", true)])]);
        for line in ["ls | ca", "ls; ca", "ls && ca", "FOO=1 ca"] {
            let cursor = line.chars().count();
            let result = complete(line, cursor, CommandEnv::default(), &lister);
            assert_eq!(
                inserts(&result.candidates),
                ["cat"],
                "{line:?} should complete the command position"
            );
        }
    }

    /// Fail-closed degradation: an unlexable prefix, a quoted word, and a
    /// here-doc delimiter complete to nothing.
    #[test]
    fn degrades_to_nothing_fail_closed() {
        let lister = MapLister::new(&[(".", &[("notes.txt", false)])]);
        for (line, cursor) in [("echo 'no", 8), ("cat \"no", 7), ("cat <<E", 7)] {
            let result = complete(line, cursor, CommandEnv::default(), &lister);
            assert!(
                result.candidates.is_empty(),
                "{line:?} should complete to nothing"
            );
        }
        // A refused listing degrades to no path candidates.
        let result = complete("cat no", 6, CommandEnv::default(), &MapLister::new(&[]));
        assert!(result.candidates.is_empty());
    }

    /// A candidate whose name needs escaping is inserted shell-safe.
    #[test]
    fn candidates_are_shell_escaped() {
        let lister = MapLister::new(&[(".", &[("my notes.txt", false)])]);
        let result = complete("cat my", 6, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["my\\ notes.txt"]);
        assert_eq!(result.candidates[0].display, "my notes.txt");
    }

    /// The dup/close redirections take no target: the following word is an
    /// ordinary argument, not a redirection target.
    #[test]
    fn dup_redirections_do_not_claim_the_next_word() {
        let lister = MapLister::new(&[(".", &[("notes.txt", false)])]);
        let result = complete("cat 2>&1 no", 11, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["notes.txt"]);
    }
}

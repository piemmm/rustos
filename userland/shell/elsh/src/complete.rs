//! Tab completion: the word under the cursor to the candidates that could
//! finish it (`plans/SHELL.md`, "Tab expansion and completion").
//!
//! The engine is pure and read-only: it inspects the line with the shell's
//! own quoting-aware lexer ([`crate::lexer::tokenize_with_spans`]) — never a
//! second, completion-only tokeniser — and reaches the filesystem only
//! through the injected [`DirLister`] seam, so it can be tested without a
//! kernel and can never run a command, write a file, or change `$?`.
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
//! * **Redirection target**: filesystem paths *and* resource references —
//!   registered namespaces (`sys:` …) and their catalogued selectors
//!   ([`tairix_resref::KnownNamespace`]), the same registry the redirection
//!   classifier applies.
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
//! is a *placeholder*: a per-machine name the registry cannot enumerate. It
//! becomes a display-only [`Completion::hints`] entry rather than a
//! candidate — the user is shown what comes next without the shell inserting
//! a name it does not know — and completion resumes past it once the name is
//! typed.
//!
//! Degradation is deliberate and fail-closed: a line whose prefix does not
//! lex (an open quote), or a word already carrying quoting or expansion
//! syntax, completes to nothing rather than to a guess.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_cmdres::CommandEnv;
use tairix_resref::{is_placeholder, KnownNamespace, SelectorEntry};

pub use tairix_complete::{DirEntryInfo, DirLister};

use crate::builtin::BUILTIN_NAMES;
use crate::lexer::{tokenize_with_spans, RedirOp, Token};

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
    pub candidates: Vec<Candidate>,
    /// Display-only entries: what could come next where the shell cannot
    /// supply the text itself — today a resource-selector placeholder
    /// (`<iface>`), a per-machine name only the running system knows.
    ///
    /// A hint is listed beside the candidates and never inserted. Because a
    /// lone real candidate would otherwise be inserted silently and hide the
    /// alternative, a result carrying hints is always *listed* rather than
    /// completed; hints are therefore raised only at a segment boundary (an
    /// empty leaf), so completing a partly typed name is never held up.
    pub hints: Vec<String>,
}

impl Completion {
    /// The empty result at `cursor` (nothing applies).
    fn none(cursor: usize) -> Self {
        Self {
            start: cursor,
            end: cursor,
            candidates: Vec::new(),
            hints: Vec::new(),
        }
    }
}

/// The syntactic role of the word under completion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Role {
    /// The command word of a simple command.
    Command,
    /// An ordinary argument word.
    Argument,
    /// The target word of a file-opening redirection.
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
    let mut hints = Vec::new();
    // A word the shared resolution rule reads as a resource reference is a
    // reference in every role — it can never name a path — so it completes
    // from the registry alone.
    let reference = tairix_resref::names_resource_reference(&word);
    match role {
        Role::HereDocDelim => {}
        Role::Command => {
            if reference {
                selector_candidates(&word, &mut candidates, &mut hints);
            } else if word.contains('/') {
                path_candidates(&word, lister, &mut candidates);
            } else {
                command_candidates(&word, env, lister, &mut candidates);
            }
        }
        Role::Argument | Role::RedirTarget => {
            if reference {
                selector_candidates(&word, &mut candidates, &mut hints);
            } else {
                // A redirection target's blank slate offers every namespace;
                // an empty argument word does not (it would bury the paths).
                namespace_candidates(&word, role == Role::RedirTarget, &mut candidates);
                path_candidates(&word, lister, &mut candidates);
            }
        }
    }

    candidates.sort_by(|a, b| a.display.cmp(&b.display));
    candidates.dedup_by(|a, b| a.insert == b.insert);
    hints.sort();
    hints.dedup();
    Completion {
        start,
        end: cursor,
        candidates,
        hints,
    }
}

/// Decide the role of the word being completed from the tokens before it.
fn word_role(before: &[(Token, core::ops::Range<usize>)]) -> Role {
    // A redirection operator immediately before the word claims it.
    if let Some((Token::Redirect(op), _)) = before.last() {
        return match op {
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
/// could begin: `st` offers `state:` and `stats:`. `offer_all` additionally
/// offers every namespace for an empty word — a redirection target's blank
/// slate, where a namespace is as likely as a filename.
///
/// The candidate stays open (no closing character): the user carries on into
/// the selector.
fn namespace_candidates(word: &str, offer_all: bool, out: &mut Vec<Candidate>) {
    if word.is_empty() && !offer_all {
        return;
    }
    for namespace in KnownNamespace::ALL {
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
/// completing position becomes a `hints` entry instead of a candidate; see
/// [`Completion::hints`] for why that is display-only and why it is raised
/// only at a segment boundary.
///
/// Inserts are built verbatim rather than through [`escape_word`]: every
/// character of the reference grammar (`a-z0-9-_./:?=`) is an ordinary word
/// character to the shell's lexer, so escaping would only inject backslashes
/// into the reference.
fn selector_candidates(word: &str, out: &mut Vec<Candidate>, hints: &mut Vec<String>) {
    // The rule that classified `word` guarantees a `:` with a registered
    // namespace before it, but this function must stand on its own.
    let Some(colon) = word.find(':') else {
        return;
    };
    let (prefix, rest) = (&word[..colon], &word[colon + 1..]);
    let Some(namespace) = KnownNamespace::from_name(prefix) else {
        return;
    };
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
        if is_placeholder(next) {
            // A name only the running machine knows. Shown at a segment
            // boundary so the shape is discoverable, never inserted.
            if leaf.is_empty() {
                hints.push(String::from(next));
            }
            continue;
        }
        if !next.starts_with(leaf) {
            continue;
        }
        let mut insert = format!("{prefix}:{fixed}{next}");
        let mut display = String::from(next);
        let closing = if last {
            match entry.mandatory_param {
                // A rate is undefined without its sampling window, so the
                // completed reference carries the parameter the user must
                // fill in rather than closing as a finished word.
                Some(param) => {
                    for text in [&mut insert, &mut display] {
                        text.push('?');
                        text.push_str(param);
                        text.push('=');
                    }
                    None
                }
                None => Some(' '),
            }
        } else {
            insert.push('/');
            display.push('/');
            None
        };
        out.push(Candidate {
            insert,
            display,
            closing,
        });
    }
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
    use super::{complete, Candidate, CommandEnv, DirEntryInfo, DirLister};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use tairix_abi::Errno;

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

    /// A per-machine name the registry cannot enumerate is offered as a
    /// display-only hint beside the real candidates, never as an insert — and
    /// only at a segment boundary, so a partly typed name still completes.
    #[test]
    fn placeholder_segments_become_display_only_hints() {
        let lister = MapLister::new(&[(".", &[])]);

        // Both shapes the position accepts are shown: a plain interface, and
        // a bond alias (whose aggregation leaves only a bond serves).
        let boundary = complete("cat state:net/", 14, CommandEnv::default(), &lister);
        assert_eq!(inserts(&boundary.candidates), ["state:net/resolver/"]);
        assert_eq!(boundary.hints, ["<bond>", "<iface>"]);

        // Typing into the segment is completion, not discovery: the hint
        // steps aside so `resolver/` can be inserted.
        let typing = complete("cat state:net/r", 15, CommandEnv::default(), &lister);
        assert_eq!(inserts(&typing.candidates), ["state:net/resolver/"]);
        assert!(typing.hints.is_empty());

        // Two distinct placeholders at one position are both shown.
        let irq = complete("cat info:", 9, CommandEnv::default(), &lister);
        assert!(irq.hints.is_empty(), "the first segment is all literal");
        let limits = complete("cat info:limits/", 16, CommandEnv::default(), &lister);
        assert!(limits.candidates.is_empty());
        assert_eq!(limits.hints, ["<kind>"]);
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
        assert_eq!(iface.candidates.len(), 10, "6 counters and 4 rates");

        // A fully typed leaf is a dead end, not a placeholder match.
        for line in ["cat stats:irq/count/", "cat stats:cpu/load/"] {
            let cursor = line.chars().count();
            let result = complete(line, cursor, CommandEnv::default(), &lister);
            assert!(
                result.candidates.is_empty() && result.hints.is_empty(),
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
                result.candidates.is_empty() && result.hints.is_empty(),
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
                result.candidates.is_empty() && result.hints.is_empty(),
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

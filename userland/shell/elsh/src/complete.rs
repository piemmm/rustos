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
//!   registered namespaces (`sys:` …) and their well-known selectors
//!   ([`tairix_resref::KnownNamespace`]), the same registry the redirection
//!   classifier applies.
//! * **Any other argument**: filesystem paths, plus resource references once
//!   the word could begin one (a registered-namespace prefix).
//!
//! Degradation is deliberate and fail-closed: a line whose prefix does not
//! lex (an open quote), or a word already carrying quoting or expansion
//! syntax, completes to nothing rather than to a guess.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_cmdres::CommandEnv;
use tairix_resref::KnownNamespace;

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
    match role {
        Role::HereDocDelim => {}
        Role::Command => {
            if word.contains('/') {
                path_candidates(&word, lister, &mut candidates);
            } else {
                command_candidates(&word, env, lister, &mut candidates);
            }
        }
        Role::Argument => {
            resource_candidates(&word, false, &mut candidates);
            path_candidates(&word, lister, &mut candidates);
        }
        Role::RedirTarget => {
            resource_candidates(&word, true, &mut candidates);
            path_candidates(&word, lister, &mut candidates);
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

/// Candidates from the resource-reference registry: registered namespaces
/// for a plain prefix, and a namespace's well-known selectors once the `:`
/// is typed. `offer_all` additionally offers every namespace for an empty
/// word (a redirection target's blank slate).
fn resource_candidates(word: &str, offer_all: bool, out: &mut Vec<Candidate>) {
    if let Some(colon) = word.find(':') {
        let (prefix, rest) = (&word[..colon], &word[colon + 1..]);
        let Some(namespace) = KnownNamespace::from_name(prefix) else {
            return;
        };
        // `sys:/…` is an alias-path spelling, not a reference.
        if rest.starts_with('/') || rest.contains('/') {
            return;
        }
        for &selector in namespace.well_known_selectors() {
            if selector.starts_with(rest) {
                out.push(Candidate {
                    insert: format!("{prefix}:{selector}"),
                    display: format!("{prefix}:{selector}"),
                    closing: Some(' '),
                });
            }
        }
        return;
    }
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
    /// a typed namespace completes its well-known selectors.
    #[test]
    fn redirection_target_offers_resources() {
        let lister = MapLister::new(&[(".", &[("sysinfo.txt", false)])]);
        let result = complete("echo hi > sys", 13, CommandEnv::default(), &lister);
        assert_eq!(inserts(&result.candidates), ["sys:", "sysinfo.txt"]);

        let selectors = complete("cat < sys:", 10, CommandEnv::default(), &lister);
        assert_eq!(inserts(&selectors.candidates), ["sys:null", "sys:random"]);

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

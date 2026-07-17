//! TAIRiX shared filename-completion engine (`lib/complete`).
//!
//! More than one interactive TAIRiX program completes a partially typed
//! filesystem path: the shell's Tab completion first, and the tree file
//! manager's destination prompts beside it. The policy is *identical*
//! wherever it happens — split the word at its last `/` into a directory
//! part and a leaf prefix, list that directory, offer the entries whose
//! names extend the prefix, hide dot-named entries unless the prefix asks
//! for them, and extend a multi-candidate word to its longest common
//! prefix — so it lives here once and every consumer imports it, rather
//! than each embedding a private engine that would drift.
//!
//! What stays with the consumer is *presentation*: the shell escapes a
//! candidate so the completed line still lexes as one word, decides
//! closing characters, and merges path candidates with its command and
//! resource-reference candidates; the file manager inserts candidates
//! verbatim into a plain path prompt. Neither re-derives the policy above.
//!
//! # Read-only, fail-closed
//!
//! The engine reaches the filesystem only through the injected
//! [`DirLister`] seam, and listing is the only operation the seam offers —
//! completion can never create, write, or run anything. A listing the
//! kernel refuses completes to nothing (an empty candidate set), never to
//! a guess; the caller's permission to *see* a directory is decided
//! kernel-side, exactly as for any other listing.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

/// One directory entry a [`DirLister`] reports: the name and whether it
/// is a directory (a directory candidate stays open for further
/// completion; a file candidate finishes the word).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntryInfo {
    /// The entry's name within its directory (a single component).
    pub name: String,
    /// `true` if the entry is a directory.
    pub is_dir: bool,
}

/// The engine's read-only filesystem seam: list a directory's entries.
///
/// On a running kernel this is backed by the `fs_*` listing syscalls
/// (every permission check stays kernel-side); in tests it is an
/// in-memory fixture. Listing is the only filesystem operation completion
/// may perform.
pub trait DirLister {
    /// List the entries of `dir`.
    ///
    /// # Errors
    ///
    /// The host's [`Errno`]; the engine degrades to no candidates.
    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>, Errno>;
}

/// Split a path word at its last `/` into the directory part (kept on the
/// insert, trailing `/` included) and the leaf prefix being completed.
/// A word with no `/` is all leaf.
#[must_use]
pub fn split_path_word(word: &str) -> (&str, &str) {
    match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    }
}

/// The directory a word's candidates are listed from: `bare_dir` for a
/// word with no directory part (the consumer's notion of "here" — the
/// shell's working directory, the file manager's listed directory), the
/// root for `/`, and otherwise the directory part without its trailing
/// separator.
#[must_use]
pub fn list_target<'w>(dir_part: &'w str, bare_dir: &'w str) -> &'w str {
    if dir_part.is_empty() {
        bare_dir
    } else if dir_part == "/" {
        "/"
    } else {
        dir_part.trim_end_matches('/')
    }
}

/// The candidates that could complete the path word `word`: the entries
/// of its directory whose names extend the leaf prefix, name-ordered.
/// Dot-named (hidden) entries are offered only when the prefix itself
/// starts with a dot. A listing the seam refuses yields no candidates —
/// fail closed, never a guess.
#[must_use]
pub fn path_matches(word: &str, bare_dir: &str, lister: &dyn DirLister) -> Vec<DirEntryInfo> {
    let (dir_part, leaf) = split_path_word(word);
    let Ok(mut entries) = lister.list_dir(list_target(dir_part, bare_dir)) else {
        return Vec::new();
    };
    entries.retain(|entry| {
        entry.name.starts_with(leaf) && (!entry.name.starts_with('.') || leaf.starts_with('.'))
    });
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// The longest common prefix of the candidate texts, by character — the
/// Tab discipline's extension when several candidates share a stem.
#[must_use]
pub fn common_prefix<'a>(mut items: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = items.next() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for item in items {
        let mut keep = 0;
        for (a, b) in prefix.iter().zip(item.chars()) {
            if *a != b {
                break;
            }
            keep += 1;
        }
        prefix.truncate(keep);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{common_prefix, path_matches, split_path_word, DirEntryInfo, DirLister};
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

    fn names(entries: &[DirEntryInfo]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The word splits at its last separator; a bare word is all leaf.
    #[test]
    fn the_word_splits_into_directory_part_and_leaf() {
        assert_eq!(split_path_word("no"), ("", "no"));
        assert_eq!(split_path_word("/Users/a"), ("/Users/", "a"));
        assert_eq!(split_path_word("/"), ("/", ""));
        assert_eq!(split_path_word("a/b/"), ("a/b/", ""));
    }

    /// A bare word lists the consumer's "here"; candidates extend the
    /// leaf prefix name-ordered, and dotfiles stay hidden unless asked.
    #[test]
    fn bare_words_complete_from_the_bare_directory() {
        let lister = MapLister::new(&[(
            "/work",
            &[("notes.txt", false), ("notebooks", true), (".notrc", false)],
        )]);
        let matches = path_matches("no", "/work", &lister);
        assert_eq!(names(&matches), ["notebooks", "notes.txt"]);
        assert!(matches[0].is_dir && !matches[1].is_dir);

        let hidden = path_matches(".no", "/work", &lister);
        assert_eq!(names(&hidden), [".notrc"]);
    }

    /// A word carrying a directory part lists that directory, root
    /// included.
    #[test]
    fn sub_path_words_list_their_directory_part() {
        let lister = MapLister::new(&[
            ("/Users", &[("ada", true), ("bob", true)]),
            ("/", &[("System", true), ("Users", true)]),
        ]);
        assert_eq!(names(&path_matches("/Users/a", ".", &lister)), ["ada"]);
        assert_eq!(
            names(&path_matches("/Sys", ".", &lister)),
            ["System"],
            "the root lists as `/`"
        );
    }

    /// A refused listing completes to nothing — fail closed, never a
    /// guess.
    #[test]
    fn a_refused_listing_yields_no_candidates() {
        let lister = MapLister::new(&[]);
        assert!(path_matches("no", ".", &lister).is_empty());
    }

    /// The longest common prefix drives the Tab extension; disjoint
    /// candidates share nothing and an empty set has no prefix.
    #[test]
    fn common_prefix_extends_shared_stems() {
        assert_eq!(
            common_prefix(["notes.txt", "notebooks/"].into_iter()),
            "note"
        );
        assert_eq!(common_prefix(["a", "b"].into_iter()), "");
        assert_eq!(common_prefix(core::iter::empty()), "");
        assert_eq!(common_prefix(["only"].into_iter()), "only");
    }
}

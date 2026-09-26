//! Where the program bundles' source crates live, and how their manifest
//! source reads.
//!
//! One walk and one line grammar, compiled into this crate's build script
//! (which embeds each bundle's `Help/` and `Resources/`) and exported to the
//! bundle composer (which builds each bundle's `Run` and signs its
//! `AppInfo`), so the payload a bundle carries and the bundle itself are
//! found and named by one rule and can never describe different programs.

use std::io;
use std::path::{Path, PathBuf};
use std::vec::Vec;

/// The manifest source whose presence makes a crate a program bundle.
pub const MANIFEST_SOURCE: &str = "AppInfo.toml";

/// Each line of `manifest` that is neither blank nor a `#` comment, numbered
/// from one, with the trimmed `(key, value)` it spells, or `None` for a line
/// that is not `key = value`.
///
/// The grammar is flat, so a table header is such a line.
pub fn manifest_entries(
    manifest: &str,
) -> impl Iterator<Item = (usize, Option<(&str, &str)>)> + '_ {
    manifest.lines().enumerate().filter_map(|(index, raw)| {
        let line = raw.trim();
        (!line.is_empty() && !line.starts_with('#')).then(|| {
            let entry = line
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()));
            (index + 1, entry)
        })
    })
}

/// The text of a double-quoted manifest string, or `None` for a value that is
/// not one or that holds a quote or backslash, which the grammar gives no
/// meaning.
#[must_use]
pub fn quoted_string(value: &str) -> Option<&str> {
    let text = value.strip_prefix('"')?.strip_suffix('"')?;
    (!text.contains(['"', '\\'])).then_some(text)
}

/// The string `manifest` gives `key`, from the entries above its first line
/// that is not one; `None` where the first of them naming `key` exactly does
/// not give it a quoted string, or none does.
#[must_use]
pub fn string_entry<'a>(manifest: &'a str, key: &str) -> Option<&'a str> {
    manifest_entries(manifest)
        .map_while(|(_, entry)| entry)
        .find(|&(name, _)| name == key)
        .and_then(|(_, value)| quoted_string(value))
}

/// Every program crate under `userland`, in a deterministic order.
///
/// A program crate holds [`MANIFEST_SOURCE`] at `<class>/<crate>`, or at
/// `<class>/<group>/<crate>` beneath a grouping directory that is not itself
/// a crate — a game's own subtree, whose crates compose each other. `read`
/// is told every directory whose listing the answer depends on, so a build
/// script can rerun when a program is added anywhere the walk looks.
///
/// # Errors
///
/// A directory the walk cannot list.
pub fn program_crates(userland: &Path, mut read: impl FnMut(&Path)) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for class in sorted_dirs(userland, &mut read)? {
        for member in sorted_dirs(&class, &mut read)? {
            if member.join("Cargo.toml").is_file() {
                if member.join(MANIFEST_SOURCE).is_file() {
                    found.push(member);
                }
                continue;
            }
            for member_crate in sorted_dirs(&member, &mut read)? {
                if member_crate.join(MANIFEST_SOURCE).is_file() {
                    found.push(member_crate);
                }
            }
        }
    }
    Ok(found)
}

/// The immediate subdirectories of `root`, sorted, so the walk is the same
/// on every filesystem.
fn sorted_dirs(root: &Path, read: &mut impl FnMut(&Path)) -> io::Result<Vec<PathBuf>> {
    read(root);
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use std::vec;
    use std::vec::Vec;

    use super::{manifest_entries, quoted_string, string_entry};

    /// Blank and comment lines are skipped, entries are numbered by their
    /// line and trimmed, and a line that is not `key = value` says so.
    #[test]
    fn entries_are_every_line_that_is_not_blank_or_a_comment() {
        let manifest = "# a program\n\nname = \"ls\"\n  kind=\"command\"  \n[table]\n";
        let entries: Vec<_> = manifest_entries(manifest).collect();
        assert_eq!(
            entries,
            vec![
                (3, Some(("name", "\"ls\""))),
                (4, Some(("kind", "\"command\""))),
                (5, None),
            ]
        );
    }

    /// Only a whole double-quoted string with nothing inside it the grammar
    /// cannot mean is a string.
    #[test]
    fn a_quoted_string_is_exactly_one_pair_of_quotes() {
        assert_eq!(quoted_string("\"ls\""), Some("ls"));
        assert_eq!(quoted_string("\"\""), Some(""));
        for refused in ["ls", "\"ls", "ls\"", "\"", "\"l\"s\"", "\"l\\s\"", "'ls'"] {
            assert_eq!(quoted_string(refused), None, "{refused}");
        }
    }

    /// A key is matched exactly, before the first line that is not an entry,
    /// and its first entry decides.
    #[test]
    fn a_string_entry_is_the_first_exact_top_level_match() {
        let manifest = "names = \"no\"\nname = \"ls\"\nname = \"cat\"\n";
        assert_eq!(string_entry(manifest, "name"), Some("ls"));
        assert_eq!(string_entry(manifest, "nam"), None);
        assert_eq!(string_entry("[table]\nname = \"ls\"\n", "name"), None);
        assert_eq!(string_entry("name = ls\nname = \"ls\"\n", "name"), None);
        assert_eq!(string_entry("kind = \"command\"\n", "name"), None);
    }
}

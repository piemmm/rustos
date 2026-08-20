//! The seams through which `ln` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the planning logic in [`crate::client`] run against in-memory
//! fixtures with no kernel, mirroring the seam design of the other userland
//! crates (`cp`'s `FileSystem`/`Prompt`, `ls`'s `Listing`, `rm`'s
//! `Removal`).
//!
//! The vocabulary is deliberately the *name as typed*: `ln` creates and
//! replaces **names**, never what a name leads to, so the one question it
//! asks the filesystem is what a name holds.

use tairix_abi::Errno;

/// What a link name already holds, as `ln` must see it.
///
/// The distinctions are exactly the ones `ln`'s operand rules turn on. A
/// destination that is a **directory** receives the links inside it; one that
/// is a **link to a directory** does too, unless `-n` says to treat it as the
/// plain name it also is; anything else present is a name `-f`/`-i` may
/// replace and every other invocation refuses.
///
/// A link is never *followed* for the replacement decision: replacing a name
/// removes the link, so a planted link can never redirect the new name
/// somewhere else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Occupant {
    /// Nothing of that name.
    Vacant,
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link whose target resolves to a directory.
    LinkToDirectory,
    /// A symbolic link that resolves to something other than a directory,
    /// or that dangles.
    Link,
}

impl Occupant {
    /// Whether a link created at this name would land *inside* it — a
    /// directory, or a link to one when `-n` was not given.
    ///
    /// This is the whole of GNU's two-operand rule, decided in one place so
    /// the destination reading and the `-n` switch can never disagree.
    #[must_use]
    pub const fn receives_links(self, no_dereference: bool) -> bool {
        match self {
            Self::Directory => true,
            Self::LinkToDirectory => !no_dereference,
            Self::Vacant | Self::File | Self::Link => false,
        }
    }

    /// Whether the name is already taken, so creating a link there needs
    /// `-f` or `-i` first.
    #[must_use]
    pub const fn is_taken(self) -> bool {
        match self {
            Self::Vacant => false,
            Self::Directory | Self::File | Self::LinkToDirectory | Self::Link => true,
        }
    }
}

/// Inspects link names, creates links, and removes a name a replacement
/// needs freed.
pub trait FileSystem {
    /// What the name `path` holds ([`Occupant`]).
    ///
    /// The final component is described as typed, and a symbolic link is
    /// additionally resolved just far enough to say whether it names a
    /// directory — the only thing the destination reading and `-n` need.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reaching `path` — e.g.
    /// [`Errno::PermissionDenied`] when the caller may not search the way
    /// there. An absent name is [`Occupant::Vacant`], never an error.
    fn occupant(&self, path: &str) -> Result<Occupant, Errno>;

    /// Create the symbolic link `link` whose stored target is `target`.
    ///
    /// `target` is stored verbatim and is never resolved here: it may be
    /// relative, may carry `..`, and may name nothing at all, so the created
    /// link may legitimately dangle. Creating it grants no authority over
    /// what it names.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — [`Errno::AlreadyExists`] for a
    /// taken name (a new link never replaces one), [`Errno::NotSupported`]
    /// on a mount whose format stores no links (a permanent limit, not a
    /// transient failure), or a permission or grammar refusal.
    fn symlink(&self, target: &str, link: &str) -> Result<(), Errno>;

    /// Remove the non-directory entry `path` names — the name as typed,
    /// never what a link names.
    ///
    /// Called only for a name `-f` or `-i` said to replace, and only when
    /// [`occupant`](FileSystem::occupant) reported something that is not a
    /// directory (a directory destination receives links rather than being
    /// replaced).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while removing.
    fn remove(&self, path: &str) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// `ln` is silent on success unless `-v` reports each link; this seam also
/// carries the usage banner and the short help.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Asks the interactive confirmation question (`-i`).
///
/// The production implementation writes `ln: <question> ` to standard error
/// and reads one line from standard input, answering `true` only for an
/// affirmative reply (a leading `y`/`Y`), matching the GNU tool. A declined
/// question skips that link; an unanswerable one fails closed — it is never
/// treated as consent.
pub trait Prompt {
    /// Ask `question` and return whether the user consented.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises — the caller fails closed.
    fn confirm(&self, question: &str) -> Result<bool, Errno>;
}

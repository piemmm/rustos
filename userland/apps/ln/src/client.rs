//! The link engine: decide each target's link name, free a taken name only
//! when told to, and create the link.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::RealpathMode;
use tairix_help::{own_short_help, HelpSource};
use tairix_path::{join, leaf_name};

use crate::command::{Clobber, Command, Options, TargetMode};
use crate::error::LnError;
use crate::io::{FileSystem, Occupant, Output, Prompt};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ln`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: ln [-srLPdFfinvT] [-t dir] [--] target... [link_name]

  -s, --symbolic             make symbolic links
  -r, --relative             store the target relative to the link (needs -s)
  -L, --logical              hard-link what a link target names
  -P, --physical             hard-link the target as spelled (default)
  -d, -F, --directory        accept a directory operand (still refused)
  -f, --force                remove an existing link name and retry
  -i, --interactive          ask before removing an existing link name
  -n, --no-dereference       treat a link-to-directory destination as a name
  -v, --verbose              report each link made
  -t dir, --target-directory=dir
                             create every link in dir
  -T, --no-target-directory  treat the destination as a link name
  -h, -?, --help             show this message

Without -s a hard link is made: a second directory entry for the target's
own inode, so both names reach one file and its storage survives until the
last name goes. Both names must be on one volume, and a directory is never
given a second name.

With one operand the link is made in the working directory under the
target's own name. With two, the second is a directory to fill when it is
one and the link's name otherwise. With three or more, the last must
already be a directory. `--` ends option parsing: every later argument is
an operand.
";

/// `ln`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "ln";

/// Run one [`Command`], creating its links through `fs`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// Each target gets one link: at the destination itself for the two-operand
/// name form, inside the destination directory under the target's own leaf
/// name otherwise. A link name that is already taken is refused by default,
/// removed under `-f`, and asked about through `prompt` under `-i` (a
/// declined question skips that link without error). `-v` reports each link
/// on `out`; otherwise `ln` writes nothing on success.
///
/// The first failure stops the run before any later target (fail closed),
/// exactly as `cp` and `mv` do; links already created stay created.
///
/// # Errors
///
/// * [`LnError::Usage`] — more than one target aimed at a single link name.
/// * [`LnError::NotADirectory`] — a destination that must be a directory is
///   not one.
/// * [`LnError::Stat`] — a link name could not be inspected.
/// * [`LnError::Canonicalize`] — `-r` could not canonicalise the target or
///   the link's own directory.
/// * [`LnError::Remove`] — a taken name `-f`/`-i` approved could not be
///   removed.
/// * [`LnError::Create`] — the link could not be created (a taken name
///   without `-f`/`-i` is [`Errno::AlreadyExists`]; a format that stores no
///   link of the asked-for kind is [`Errno::NotSupported`]; a hard link to a
///   directory is [`Errno::IsADirectory`] and one across volumes is
///   [`Errno::CrossVolume`]).
/// * [`LnError::Prompt`] — a confirmation could not be read (never treated
///   as consent).
/// * [`LnError::Output`] — writing the banner or a `-v` report failed.
///
/// [`Errno::AlreadyExists`]: tairix_abi::Errno::AlreadyExists
/// [`Errno::NotSupported`]: tairix_abi::Errno::NotSupported
/// [`Errno::IsADirectory`]: tairix_abi::Errno::IsADirectory
/// [`Errno::CrossVolume`]: tairix_abi::Errno::CrossVolume
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LnError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Link {
            options,
            targets,
            destination,
        } => link_all(&targets, destination.as_deref(), options, fs, prompt, out),
    }
}

/// Render `ln`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LnError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(LnError::Output)
}

/// Create one link per target, deciding once whether the destination is a
/// directory to fill or the single link's own name.
fn link_all(
    targets: &[String],
    destination: Option<&str>,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), LnError> {
    let into_directory = match (destination, options.target_mode) {
        // Neither form fills a directory: the single-operand form links
        // under the target's own leaf name here, and `-T` links at the
        // destination whatever it holds.
        (None, _) | (Some(_), TargetMode::NoDirectory) => None,
        // `-t dir`: the destination must already be a directory.
        (Some(dir), TargetMode::Directory) => {
            if occupant_of(fs, dir)?.receives_links(options.no_dereference) {
                Some(dir)
            } else {
                return Err(LnError::NotADirectory(String::from(dir)));
            }
        }
        // The inferred form: a destination that receives links is a
        // directory to fill; anything else is the link's own name.
        (Some(dest), TargetMode::Inferred) => {
            if occupant_of(fs, dest)?.receives_links(options.no_dereference) {
                Some(dest)
            } else {
                None
            }
        }
    };

    // More than one target needs a directory to land in; a single name
    // cannot hold two links.
    if targets.len() > 1 && into_directory.is_none() {
        return Err(LnError::Usage);
    }

    for target in targets {
        let link = match (into_directory, destination) {
            (Some(dir), _) => join(dir, leaf_name(target)),
            // `-T` and the inferred name form both link at the destination.
            (None, Some(dest)) => String::from(dest),
            // The single-operand form: the target's own leaf name, here.
            (None, None) => String::from(leaf_name(target)),
        };
        // `-r` rewrites what gets *stored*; the operand still names the
        // target, and the link name is untouched.
        let stored = if options.relative {
            relative_target(target, &link, fs)?
        } else {
            String::from(target)
        };
        if !link_one(&stored, &link, options, fs, prompt)? {
            continue;
        }
        if options.verbose {
            out.write_all(format!("'{link}' -> '{stored}'\n").as_bytes())
                .map_err(LnError::Output)?;
        }
    }
    Ok(())
}

/// The `-r` target: `target` spelled relative to the directory `link` sits
/// in.
///
/// Both halves are canonicalised by the **filesystem** first, which is what
/// makes the arithmetic below safe to do lexically: two canonical paths hold
/// no `.`, no `..`, and no symbolic link, so the `..`-and-names spelling
/// between them resolves back to exactly the node the target named. Doing
/// the same arithmetic on the paths as typed would be the lexical-`..`
/// collapse the resolver forbids — it would name a different node the moment
/// a link were involved.
///
/// [`RealpathMode::Missing`] is the reading asked for on both: a symbolic
/// link may legitimately name something that does not exist yet, and the
/// link's own directory may be about to be created.
fn relative_target(target: &str, link: &str, fs: &dyn FileSystem) -> Result<String, LnError> {
    let canonical_target = canonicalize(fs, target)?;
    let link_directory = parent_of(link);
    let canonical_base = canonicalize(fs, link_directory)?;
    Ok(relative_spelling(&canonical_base, &canonical_target))
}

/// Canonicalise `path` through the filesystem, surfacing a refusal as
/// [`LnError::Canonicalize`].
fn canonicalize(fs: &dyn FileSystem, path: &str) -> Result<String, LnError> {
    fs.canonicalize(path, RealpathMode::Missing)
        .map_err(|errno| LnError::Canonicalize(String::from(path), errno))
}

/// The directory part of the link name `link`, as typed.
///
/// A name with no separator sits in the working directory, which the
/// filesystem spells as `.` — the one place a relative spelling is asked for
/// without a directory being named.
fn parent_of(link: &str) -> &str {
    match link.rfind('/') {
        // A leading `/` is the root itself, not an empty directory name.
        Some(0) => "/",
        Some(slash) => &link[..slash],
        None => ".",
    }
}

/// `target` spelled relative to `base`, both already canonical.
///
/// The shared leading components are dropped, one `..` is emitted for each
/// component of `base` that remains, and the rest of `target` follows. An
/// empty result — the two naming the same node — is spelled `.`, as GNU's
/// own `relpath` does.
fn relative_spelling(base: &str, target: &str) -> String {
    fn parts(path: &str) -> Vec<&str> {
        path.split('/').filter(|part| !part.is_empty()).collect()
    }
    let base = parts(base);
    let target = parts(target);
    let shared = base
        .iter()
        .zip(target.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut spelled: Vec<&str> = alloc::vec::Vec::new();
    spelled.resize(base.len() - shared, "..");
    spelled.extend_from_slice(&target[shared..]);
    if spelled.is_empty() {
        return String::from(".");
    }
    spelled.join("/")
}

/// Create the single link `link` naming `target`, freeing a taken name first
/// when `-f`/`-i` approved it.
///
/// Returns whether the link was created, so a `-i` question the user
/// declined skips its `-v` report without being an error.
fn link_one(
    target: &str,
    link: &str,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
) -> Result<bool, LnError> {
    let occupant = occupant_of(fs, link)?;
    if occupant.is_taken() {
        // A directory is never replaced: the destination readings above
        // already routed a link *into* one, so a directory still standing
        // here is a name this link cannot take.
        if occupant == Occupant::Directory {
            return Err(LnError::Create(
                String::from(link),
                tairix_abi::Errno::AlreadyExists,
            ));
        }
        match options.clobber {
            Clobber::Refuse => {
                return Err(LnError::Create(
                    String::from(link),
                    tairix_abi::Errno::AlreadyExists,
                ));
            }
            Clobber::Ask => {
                if !prompt
                    .confirm(&format!("replace '{link}'?"))
                    .map_err(LnError::Prompt)?
                {
                    return Ok(false);
                }
            }
            Clobber::Replace => {}
        }
        // Removing the name is what makes the replacement safe: a new link
        // never replaces an existing one, and writing "through" a link that
        // was already here would name whatever it points at.
        fs.remove(link)
            .map_err(|errno| LnError::Remove(String::from(link), errno))?;
    }
    // `-s` stores the target verbatim; without it the target's own inode
    // gains a second name, and `-L` decides whether a target that is itself
    // a symbolic link is resolved first.
    let created = if options.symbolic {
        fs.symlink(target, link)
    } else {
        fs.link(target, link, options.dereference_target)
    };
    created.map_err(|errno| LnError::Create(String::from(link), errno))?;
    Ok(true)
}

/// What `path` holds, surfacing an inspection failure as
/// [`LnError::Stat`].
fn occupant_of(fs: &dyn FileSystem, path: &str) -> Result<Occupant, LnError> {
    fs.occupant(path)
        .map_err(|errno| LnError::Stat(String::from(path), errno))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::{Errno, RealpathMode};
    use tairix_help::{HelpSource, SourceError};

    use super::{relative_spelling, run, USAGE};
    use crate::command::parse;
    use crate::error::LnError;
    use crate::io::{FileSystem, Occupant, Output, Prompt};

    /// An in-memory tree: a name-to-occupant map, plus the links created and
    /// the names removed, in order.
    #[derive(Default)]
    struct TreeFs {
        nodes: RefCell<Vec<(String, Occupant)>>,
        created: RefCell<Vec<(String, String)>>,
        /// `(link, target, -L asked for)` per hard link made.
        hard_linked: RefCell<Vec<(String, String, bool)>>,
        removed: RefCell<Vec<String>>,
        symlink_errno: Option<Errno>,
        link_errno: Option<Errno>,
        /// Scripted canonical answers, `(path, canonical)`; a path with no
        /// entry canonicalises to itself.
        canonical: RefCell<Vec<(String, String)>>,
        canonicalize_errno: Option<Errno>,
    }

    impl TreeFs {
        /// The fixture with `pairs` as its scripted canonicalisation.
        fn canonicalising(pairs: &[(&str, &str)]) -> Self {
            Self {
                canonical: RefCell::new(
                    pairs
                        .iter()
                        .map(|(p, c)| (String::from(*p), String::from(*c)))
                        .collect::<Vec<_>>(),
                ),
                ..Self::default()
            }
        }

        fn with(nodes: &[(&str, Occupant)]) -> Self {
            Self {
                nodes: RefCell::new(
                    nodes
                        .iter()
                        .map(|(p, o)| (String::from(*p), *o))
                        .collect::<Vec<_>>(),
                ),
                ..Self::default()
            }
        }

        fn refusing(errno: Errno) -> Self {
            Self {
                symlink_errno: Some(errno),
                link_errno: Some(errno),
                ..Self::default()
            }
        }

        fn created(&self) -> Vec<(String, String)> {
            self.created.borrow().clone()
        }

        fn hard_linked(&self) -> Vec<(String, String, bool)> {
            self.hard_linked.borrow().clone()
        }
    }

    impl FileSystem for TreeFs {
        fn occupant(&self, path: &str) -> Result<Occupant, Errno> {
            Ok(self
                .nodes
                .borrow()
                .iter()
                .find(|(p, _)| p == path)
                .map_or(Occupant::Vacant, |(_, o)| *o))
        }

        /// The scripted canonicalisation the kernel would perform: a
        /// mapping, or the path unchanged when the fixture names no
        /// rewrite. `ln` never resolves anything itself, so this is the
        /// only source of a canonical answer.
        fn canonicalize(&self, path: &str, _mode: RealpathMode) -> Result<String, Errno> {
            if let Some(errno) = self.canonicalize_errno {
                return Err(errno);
            }
            self.canonical
                .borrow()
                .iter()
                .find(|(p, _)| p == path)
                .map_or_else(|| Ok(String::from(path)), |(_, c)| Ok(c.clone()))
        }

        fn symlink(&self, target: &str, link: &str) -> Result<(), Errno> {
            if let Some(errno) = self.symlink_errno {
                return Err(errno);
            }
            if self.occupant(link)? != Occupant::Vacant {
                return Err(Errno::AlreadyExists);
            }
            self.nodes
                .borrow_mut()
                .push((String::from(link), Occupant::Link));
            self.created
                .borrow_mut()
                .push((String::from(link), String::from(target)));
            Ok(())
        }

        fn link(&self, target: &str, link: &str, dereference: bool) -> Result<(), Errno> {
            if let Some(errno) = self.link_errno {
                return Err(errno);
            }
            // A second name for a directory is refused whatever `-d` said.
            if self.occupant(target)? == Occupant::Directory {
                return Err(Errno::IsADirectory);
            }
            if self.occupant(link)? != Occupant::Vacant {
                return Err(Errno::AlreadyExists);
            }
            // The fixture records the posture so a test can prove `-L`/`-P`
            // reach the seam rather than being parsed and dropped.
            self.nodes
                .borrow_mut()
                .push((String::from(link), Occupant::File));
            self.hard_linked.borrow_mut().push((
                String::from(link),
                String::from(target),
                dereference,
            ));
            Ok(())
        }

        fn remove(&self, path: &str) -> Result<(), Errno> {
            let mut nodes = self.nodes.borrow_mut();
            let before = nodes.len();
            nodes.retain(|(p, _)| p != path);
            if nodes.len() == before {
                return Err(Errno::NotFound);
            }
            self.removed.borrow_mut().push(String::from(path));
            Ok(())
        }
    }

    /// Records the rendered output, and answers `-i` with a fixed reply.
    #[derive(Default)]
    struct Recorder {
        text: RefCell<String>,
        answer: bool,
        asked: RefCell<Vec<String>>,
    }

    impl Recorder {
        fn answering(answer: bool) -> Self {
            Self {
                answer,
                ..Self::default()
            }
        }

        fn text(&self) -> String {
            self.text.borrow().clone()
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.text
                .borrow_mut()
                .push_str(core::str::from_utf8(bytes).unwrap_or_default());
            Ok(())
        }
    }

    impl Prompt for Recorder {
        fn confirm(&self, question: &str) -> Result<bool, Errno> {
            self.asked.borrow_mut().push(question.to_string());
            Ok(self.answer)
        }
    }

    /// A help source with no documents, so the short help falls back to the
    /// usage banner.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    fn go(args: &[&str], fs: &TreeFs, out: &Recorder) -> Result<(), LnError> {
        let command = parse(args)?;
        run(command, None, fs, out, &NoHelp, out)
    }

    #[test]
    fn a_symbolic_link_is_created_at_the_named_destination() {
        let fs = TreeFs::default();
        let out = Recorder::default();
        assert_eq!(go(&["-s", "/a/target", "/b/link"], &fs, &out), Ok(()));
        assert_eq!(
            fs.created(),
            [(String::from("/b/link"), String::from("/a/target"))]
        );
        // Silent on success without `-v`.
        assert_eq!(out.text(), "");
    }

    #[test]
    fn the_target_is_stored_verbatim_and_may_dangle() {
        let fs = TreeFs::default();
        let out = Recorder::default();
        // A relative target carrying `..`, naming nothing that exists: the
        // tool stores the spelling and never resolves it.
        assert_eq!(go(&["-s", "../elsewhere/x", "/b/link"], &fs, &out), Ok(()));
        assert_eq!(
            fs.created(),
            [(String::from("/b/link"), String::from("../elsewhere/x"))]
        );
    }

    #[test]
    fn one_operand_links_under_the_targets_own_name() {
        let fs = TreeFs::default();
        let out = Recorder::default();
        assert_eq!(go(&["-s", "/a/b/target.txt"], &fs, &out), Ok(()));
        assert_eq!(
            fs.created(),
            [(String::from("target.txt"), String::from("/a/b/target.txt"))]
        );
    }

    #[test]
    fn a_directory_destination_receives_the_links() {
        let fs = TreeFs::with(&[("/dir", Occupant::Directory)]);
        let out = Recorder::default();
        assert_eq!(go(&["-s", "/a/one", "/b/two", "/dir"], &fs, &out), Ok(()));
        assert_eq!(
            fs.created(),
            [
                (String::from("/dir/one"), String::from("/a/one")),
                (String::from("/dir/two"), String::from("/b/two")),
            ]
        );
    }

    #[test]
    fn a_link_to_a_directory_receives_the_links_unless_n_is_given() {
        let fs = TreeFs::with(&[("/dir", Occupant::LinkToDirectory)]);
        let out = Recorder::default();
        assert_eq!(go(&["-s", "/a/one", "/dir"], &fs, &out), Ok(()));
        assert_eq!(
            fs.created(),
            [(String::from("/dir/one"), String::from("/a/one"))]
        );

        // With `-n` the same destination is the plain name it also is, and a
        // taken name still needs `-f`.
        let fs = TreeFs::with(&[("/dir", Occupant::LinkToDirectory)]);
        let out = Recorder::default();
        assert_eq!(
            go(&["-sn", "/a/one", "/dir"], &fs, &out),
            Err(LnError::Create(String::from("/dir"), Errno::AlreadyExists))
        );
        let fs = TreeFs::with(&[("/dir", Occupant::LinkToDirectory)]);
        assert_eq!(go(&["-snf", "/a/one", "/dir"], &fs, &out), Ok(()));
        assert_eq!(
            fs.created(),
            [(String::from("/dir"), String::from("/a/one"))]
        );
        assert_eq!(*fs.removed.borrow(), [String::from("/dir")]);
    }

    #[test]
    fn no_target_directory_links_at_the_destination_itself() {
        let fs = TreeFs::with(&[("/dir", Occupant::Directory)]);
        let out = Recorder::default();
        // `-T` never fills a directory, so the existing directory blocks the
        // name rather than receiving a link.
        assert_eq!(
            go(&["-sT", "/a/one", "/dir"], &fs, &out),
            Err(LnError::Create(String::from("/dir"), Errno::AlreadyExists))
        );
        assert!(fs.created().is_empty());
    }

    #[test]
    fn a_directory_is_never_replaced_even_under_force() {
        let fs = TreeFs::with(&[("/dir", Occupant::Directory)]);
        let out = Recorder::default();
        assert_eq!(
            go(&["-sfT", "/a/one", "/dir"], &fs, &out),
            Err(LnError::Create(String::from("/dir"), Errno::AlreadyExists))
        );
        assert!(fs.removed.borrow().is_empty());
    }

    #[test]
    fn target_directory_must_already_be_a_directory() {
        let fs = TreeFs::with(&[("/file", Occupant::File)]);
        let out = Recorder::default();
        assert_eq!(
            go(&["-s", "-t", "/file", "a"], &fs, &out),
            Err(LnError::NotADirectory(String::from("/file")))
        );
        let fs = TreeFs::default();
        assert_eq!(
            go(&["-s", "-t", "/absent", "a"], &fs, &out),
            Err(LnError::NotADirectory(String::from("/absent")))
        );
    }

    #[test]
    fn two_targets_cannot_share_one_name() {
        let fs = TreeFs::default();
        let out = Recorder::default();
        assert_eq!(
            go(&["-s", "a", "b", "/absent"], &fs, &out),
            Err(LnError::Usage)
        );
        assert!(fs.created().is_empty());
    }

    #[test]
    fn a_taken_name_is_refused_by_default() {
        let fs = TreeFs::with(&[("/link", Occupant::File)]);
        let out = Recorder::default();
        assert_eq!(
            go(&["-s", "/a/target", "/link"], &fs, &out),
            Err(LnError::Create(String::from("/link"), Errno::AlreadyExists))
        );
        assert!(fs.created().is_empty());
        assert!(fs.removed.borrow().is_empty());
    }

    #[test]
    fn force_removes_the_taken_name_before_creating() {
        let fs = TreeFs::with(&[("/link", Occupant::Link)]);
        let out = Recorder::default();
        assert_eq!(go(&["-sf", "/a/target", "/link"], &fs, &out), Ok(()));
        // The existing link is removed, never written through.
        assert_eq!(*fs.removed.borrow(), [String::from("/link")]);
        assert_eq!(
            fs.created(),
            [(String::from("/link"), String::from("/a/target"))]
        );
    }

    #[test]
    fn interactive_asks_and_a_refusal_changes_nothing() {
        let fs = TreeFs::with(&[("/link", Occupant::File)]);
        let out = Recorder::answering(false);
        assert_eq!(go(&["-si", "/a/target", "/link"], &fs, &out), Ok(()));
        assert_eq!(*out.asked.borrow(), [String::from("replace '/link'?")]);
        assert!(fs.created().is_empty());
        assert!(fs.removed.borrow().is_empty());
        // A declined question reports nothing, not even under `-v`.
        assert_eq!(out.text(), "");
    }

    #[test]
    fn interactive_replaces_on_consent() {
        let fs = TreeFs::with(&[("/link", Occupant::File)]);
        let out = Recorder::answering(true);
        assert_eq!(go(&["-siv", "/a/target", "/link"], &fs, &out), Ok(()));
        assert_eq!(*fs.removed.borrow(), [String::from("/link")]);
        assert_eq!(out.text(), "'/link' -> '/a/target'\n");
    }

    #[test]
    fn verbose_reports_each_link() {
        let fs = TreeFs::with(&[("/dir", Occupant::Directory)]);
        let out = Recorder::default();
        assert_eq!(go(&["-sv", "/a/one", "/b/two", "/dir"], &fs, &out), Ok(()));
        assert_eq!(
            out.text(),
            "'/dir/one' -> '/a/one'\n'/dir/two' -> '/b/two'\n"
        );
    }

    #[test]
    fn without_s_a_hard_link_is_made_and_no_symbolic_one() {
        // The default kind: a second directory entry for the target's own
        // inode, made through the hard-link call and never through the
        // symbolic one — they are different objects.
        let fs = TreeFs::with(&[("/a/target", Occupant::File)]);
        let out = Recorder::default();
        assert_eq!(go(&["/a/target", "/link"], &fs, &out), Ok(()));
        assert_eq!(
            fs.hard_linked(),
            [(String::from("/link"), String::from("/a/target"), false)]
        );
        assert!(fs.created().is_empty(), "no symbolic link was made");
    }

    #[test]
    fn the_physical_posture_is_the_default_and_l_asks_to_follow() {
        // `-P` is what POSIX `link()` does and is the default; `-L` is
        // `linkat(AT_SYMLINK_FOLLOW)`, and the later switch wins.
        for (args, expected) in [
            (["-L", "/a/target", "/link"], true),
            (["-P", "/a/target", "/link"], false),
        ] {
            let fs = TreeFs::with(&[("/a/target", Occupant::Link)]);
            let out = Recorder::default();
            assert_eq!(go(&args, &fs, &out), Ok(()));
            assert_eq!(fs.hard_linked()[0].2, expected, "{args:?}");
        }
        let fs = TreeFs::with(&[("/a/target", Occupant::Link)]);
        let out = Recorder::default();
        assert_eq!(go(&["-LP", "/a/target", "/link"], &fs, &out), Ok(()));
        assert!(!fs.hard_linked()[0].2, "the later switch wins");
    }

    #[test]
    fn d_and_f_accept_a_directory_operand_that_the_filesystem_still_refuses() {
        // GNU's `-d`/`-F` only stop `ln` refusing the command line; giving a
        // directory a second name is refused by the system, and here no
        // principal can hold authority for it at all.
        for flag in ["-d", "-F"] {
            let fs = TreeFs::with(&[("/a/dir", Occupant::Directory)]);
            let out = Recorder::default();
            assert_eq!(
                go(&[flag, "/a/dir", "/link"], &fs, &out),
                Err(LnError::Create(String::from("/link"), Errno::IsADirectory)),
                "{flag}"
            );
            assert!(fs.hard_linked().is_empty());
        }
    }

    #[test]
    fn a_format_with_one_name_per_node_reports_the_permanent_limit() {
        let fs = TreeFs::refusing(Errno::NotSupported);
        let out = Recorder::default();
        assert_eq!(
            go(&["/a/target", "/link"], &fs, &out),
            Err(LnError::Create(String::from("/link"), Errno::NotSupported))
        );
        assert!(fs.hard_linked().is_empty());
    }

    #[test]
    fn a_cross_volume_hard_link_reports_that_refusal() {
        // The mover's own signal: a second directory entry cannot address an
        // inode in another backing, so `ln` reports it rather than copying.
        let fs = TreeFs::refusing(Errno::CrossVolume);
        let out = Recorder::default();
        assert_eq!(
            go(&["/a/target", "/other/link"], &fs, &out),
            Err(LnError::Create(
                String::from("/other/link"),
                Errno::CrossVolume
            ))
        );
    }

    #[test]
    fn a_format_without_links_reports_the_permanent_limit() {
        let fs = TreeFs::refusing(Errno::NotSupported);
        let out = Recorder::default();
        assert_eq!(
            go(&["-s", "/a/target", "/link"], &fs, &out),
            Err(LnError::Create(String::from("/link"), Errno::NotSupported))
        );
    }

    #[test]
    fn the_first_failure_stops_the_run() {
        let fs = TreeFs::with(&[("/dir", Occupant::Directory), ("/dir/two", Occupant::File)]);
        let out = Recorder::default();
        assert_eq!(
            go(&["-s", "/a/one", "/b/two", "/c/three", "/dir"], &fs, &out),
            Err(LnError::Create(
                String::from("/dir/two"),
                Errno::AlreadyExists
            ))
        );
        // The first target's link stands; the third was never attempted.
        assert_eq!(
            fs.created(),
            [(String::from("/dir/one"), String::from("/a/one"))]
        );
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_a_tree() {
        let fs = TreeFs::default();
        let out = Recorder::default();
        assert_eq!(go(&["-h"], &fs, &out), Ok(()));
        assert_eq!(out.text(), USAGE);
    }

    #[test]
    fn relative_spelling_walks_up_then_down_between_two_canonical_paths() {
        // Both inputs are canonical, so the arithmetic is exact: shared
        // prefix dropped, one `..` per remaining base component, then the
        // rest of the target.
        for (base, target, want) in [
            ("/a/b", "/a/b/c", "c"),
            ("/a/b/c", "/a/b", ".."),
            ("/a/b/c", "/a/x/y", "../../x/y"),
            ("/a/b", "/a/b", "."),
            ("/", "/a", "a"),
            ("/a", "/", ".."),
            ("/a/b", "/c", "../../c"),
        ] {
            assert_eq!(relative_spelling(base, target), want, "{base} -> {target}");
        }
    }

    #[test]
    fn relative_stores_the_target_relative_to_the_links_own_directory() {
        // The kernel canonicalises both halves; `-r` only spells the
        // difference. Here `/a/b/link` sits in `/a/b`, and the target
        // canonicalises out of a link, so a lexical answer from the operands
        // as typed would have been `../real/file` — a different node.
        let fs = TreeFs::canonicalising(&[("/a/alias/file", "/x/real/file"), ("/a/b", "/a/b")]);
        let out = Recorder::default();
        assert_eq!(
            go(&["-sr", "/a/alias/file", "/a/b/link"], &fs, &out),
            Ok(())
        );
        assert_eq!(
            fs.created(),
            [(String::from("/a/b/link"), String::from("../../x/real/file"))]
        );
    }

    #[test]
    fn relative_reports_the_stored_target_not_the_operand() {
        // `-v` must show what the link actually holds, or the report would
        // describe a link that was never made.
        let fs = TreeFs::canonicalising(&[]);
        let out = Recorder::default();
        assert_eq!(go(&["-srv", "/a/b/target", "/a/b/link"], &fs, &out), Ok(()));
        assert_eq!(out.text(), "'/a/b/link' -> 'target'\n");
    }

    #[test]
    fn relative_without_symbolic_is_refused() {
        // A hard link stores no target, so there is nothing to make
        // relative: refused rather than silently ignored.
        assert_eq!(
            parse(&["-r", "a", "b"]),
            Err(LnError::RelativeNeedsSymbolic)
        );
        assert_eq!(
            parse(&["--relative", "a", "b"]),
            Err(LnError::RelativeNeedsSymbolic)
        );
    }

    #[test]
    fn a_refused_canonicalisation_names_the_path() {
        let fs = TreeFs {
            canonicalize_errno: Some(Errno::PermissionDenied),
            ..TreeFs::default()
        };
        let out = Recorder::default();
        assert_eq!(
            go(&["-sr", "/a/target", "/b/link"], &fs, &out),
            Err(LnError::Canonicalize(
                String::from("/a/target"),
                Errno::PermissionDenied
            ))
        );
        assert!(fs.created().is_empty(), "nothing was created");
    }

    #[test]
    fn an_inspection_failure_names_the_path() {
        struct Refusing;
        impl FileSystem for Refusing {
            fn occupant(&self, _path: &str) -> Result<Occupant, Errno> {
                Err(Errno::PermissionDenied)
            }
            fn symlink(&self, _target: &str, _link: &str) -> Result<(), Errno> {
                Err(Errno::PermissionDenied)
            }
            fn link(&self, _target: &str, _link: &str, _deref: bool) -> Result<(), Errno> {
                Err(Errno::PermissionDenied)
            }
            fn remove(&self, _path: &str) -> Result<(), Errno> {
                Err(Errno::PermissionDenied)
            }
            fn canonicalize(&self, _path: &str, _mode: RealpathMode) -> Result<String, Errno> {
                Err(Errno::PermissionDenied)
            }
        }
        let out = Recorder::default();
        let command = parse(&["-s", "/a/target", "/b/link"]).expect("parse");
        assert_eq!(
            run(command, None, &Refusing, &out, &NoHelp, &out),
            Err(LnError::Stat(
                String::from("/b/link"),
                Errno::PermissionDenied
            ))
        );
    }
}

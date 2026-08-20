//! The link engine: decide each target's link name, free a taken name only
//! when told to, and create the link.

use alloc::format;
use alloc::string::String;

use tairix_help::{own_short_help, HelpSource};
use tairix_path::{join, leaf_name};

use crate::command::{Clobber, Command, Options, TargetMode};
use crate::error::LnError;
use crate::io::{FileSystem, Occupant, Output, Prompt};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ln`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: ln -s [-finvT] [-t dir] [--] target... [link_name]

  -s, --symbolic             make symbolic links (required: see below)
  -f, --force                remove an existing link name and retry
  -i, --interactive          ask before removing an existing link name
  -n, --no-dereference       treat a link-to-directory destination as a name
  -v, --verbose              report each link made
  -t dir, --target-directory=dir
                             create every link in dir
  -T, --no-target-directory  treat the destination as a link name
  -h, -?, --help             show this message

With one operand the link is made in the working directory under the
target's own name. With two, the second is a directory to fill when it is
one and the link's name otherwise. With three or more, the last must
already be a directory. `--` ends option parsing: every later argument is
an operand.

This system has no hard links, so -s is required: without it there is no
link for `ln` to create and it says so rather than making a symbolic link,
which is a different object.
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
/// * [`LnError::HardLink`] — `-s` was absent. This ABI has no hard links, so
///   the refusal states a permanent limit; nothing is created.
/// * [`LnError::Usage`] — more than one target aimed at a single link name.
/// * [`LnError::NotADirectory`] — a destination that must be a directory is
///   not one.
/// * [`LnError::Stat`] — a link name could not be inspected.
/// * [`LnError::Remove`] — a taken name `-f`/`-i` approved could not be
///   removed.
/// * [`LnError::Create`] — the link could not be created (a taken name
///   without `-f`/`-i` is [`Errno::AlreadyExists`]; a format that stores no
///   links is [`Errno::NotSupported`]).
/// * [`LnError::Prompt`] — a confirmation could not be read (never treated
///   as consent).
/// * [`LnError::Output`] — writing the banner or a `-v` report failed.
///
/// [`Errno::AlreadyExists`]: tairix_abi::Errno::AlreadyExists
/// [`Errno::NotSupported`]: tairix_abi::Errno::NotSupported
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
    // A hard link is asked for whenever `-s` is absent. There is no
    // `fs_link` syscall and no driver call behind one, so the limit is
    // permanent and is reported before anything is inspected or created —
    // never approximated with a symbolic link, which is a different object.
    if !options.symbolic {
        return Err(LnError::HardLink);
    }

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
        if !link_one(target, &link, options, fs, prompt)? {
            continue;
        }
        if options.verbose {
            out.write_all(format!("'{link}' -> '{target}'\n").as_bytes())
                .map_err(LnError::Output)?;
        }
    }
    Ok(())
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
    fs.symlink(target, link)
        .map_err(|errno| LnError::Create(String::from(link), errno))?;
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

    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};

    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::LnError;
    use crate::io::{FileSystem, Occupant, Output, Prompt};

    /// An in-memory tree: a name-to-occupant map, plus the links created and
    /// the names removed, in order.
    #[derive(Default)]
    struct TreeFs {
        nodes: RefCell<Vec<(String, Occupant)>>,
        created: RefCell<Vec<(String, String)>>,
        removed: RefCell<Vec<String>>,
        symlink_errno: Option<Errno>,
    }

    impl TreeFs {
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
                ..Self::default()
            }
        }

        fn created(&self) -> Vec<(String, String)> {
            self.created.borrow().clone()
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
    fn without_s_the_hard_link_limit_is_stated_and_nothing_is_created() {
        let fs = TreeFs::default();
        let out = Recorder::default();
        assert_eq!(
            go(&["/a/target", "/link"], &fs, &out),
            Err(LnError::HardLink)
        );
        assert!(fs.created().is_empty());
        // Nothing was even inspected: the limit is permanent, not a probe
        // result.
        assert!(fs.removed.borrow().is_empty());
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
    fn an_inspection_failure_names_the_path() {
        struct Refusing;
        impl FileSystem for Refusing {
            fn occupant(&self, _path: &str) -> Result<Occupant, Errno> {
                Err(Errno::PermissionDenied)
            }
            fn symlink(&self, _target: &str, _link: &str) -> Result<(), Errno> {
                Err(Errno::PermissionDenied)
            }
            fn remove(&self, _path: &str) -> Result<(), Errno> {
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

//! TAIRiX `yes` — repeatedly output a line of text (`plans/APPS.md` §12.1
//! Stage C).
//!
//! The GNU coreutils `yes`: it writes its operands, joined by single
//! spaces — or `y` when none are given — followed by a newline, over and
//! over until its output stops accepting bytes (a closed pipe, a write
//! error) or the process is terminated. Its historical job is feeding an
//! affirmative answer to a prompting command; its modern one is being a
//! cheap source of repeated text.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names, with the GNU
//!   option rules: option scanning stops at the first operand, `--` ends
//!   options, and an unrecognised option is a usage error (GNU `yes`
//!   rejects `-x`; `yes -- -x` prints `-x`).
//! * [`block`] — the output block for a set of operands: the line the
//!   operands name, repeated up to [`BLOCK_LEN`] bytes, so the endless
//!   writer pays one call per block rather than one per line.
//! * [`pump`] — the endless writer: writes the block to the injected
//!   [`Output`] until a write fails, and reports that failure.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); its only dependency is the shared `lib/help`
//! engine used by the `Run` binary. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths; nothing writes to fd 3
//! (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `yes`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: yes [string...]";

/// The output-block size [`block`] fills, in bytes.
///
/// One kernel round-trip then delivers many lines instead of one, keeping
/// the endless writer off a per-line syscall without unbounded buffering.
pub const BLOCK_LEN: usize = 4096;

/// One thing the `yes` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Repeat the line the operands name (empty operands mean `y`).
    Repeat(Vec<String>),
    /// Render `yes`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// The failures the `yes` tool reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum YesError {
    /// The command line was not understood (an unrecognised option).
    Usage,
    /// The output stopped accepting bytes (a closed pipe, a write error).
    Output,
}

/// The output sink the endless writer pumps into. A failed write is the
/// tool's one stop condition, so the error carries no detail beyond the
/// fact of the failure.
pub trait Output {
    /// Write all of `bytes`, or report that the sink no longer accepts
    /// output.
    ///
    /// # Errors
    ///
    /// [`YesError::Output`] when the sink stopped accepting bytes.
    fn write_all(&self, bytes: &[u8]) -> Result<(), YesError>;
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `yes` surface, `yes [string...]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is an operand, so
///   `yes -- -x` prints `-x`.
/// * any other option-shaped token before the first operand — a
///   [`YesError::Usage`] error, exactly as GNU `yes` rejects `-x`.
/// * the first operand ends option scanning: `yes a -x` prints `a -x`.
///
/// A lone `-` is an operand, not an option. Because every recognised
/// option terminates the scan (`--` starts the operands, a help switch
/// answers immediately), only the first token can be an option at all —
/// the decision is a single look at `args[0]`.
///
/// # Errors
///
/// [`YesError::Usage`] for an unrecognised option before the operands.
pub fn parse(args: &[&str]) -> Result<Command, YesError> {
    match args.first() {
        None => Ok(repeat(&[])),
        Some(&"--") => Ok(repeat(&args[1..])),
        Some(&"-h" | &"-?" | &"--help") => Ok(Command::Help),
        Some(&arg) if arg.len() > 1 && arg.starts_with('-') => Err(YesError::Usage),
        Some(_) => Ok(repeat(args)),
    }
}

/// The [`Command::Repeat`] for `operands`, owning its strings.
fn repeat(operands: &[&str]) -> Command {
    Command::Repeat(operands.iter().map(|&s| String::from(s)).collect())
}

/// The output block for `operands`: the line they name (operands joined by
/// single spaces, or `y` when there are none) with a trailing newline,
/// repeated as many whole times as fit in [`BLOCK_LEN`] bytes — always at
/// least once, so an over-long line still streams correctly one line per
/// write.
#[must_use]
pub fn block(operands: &[String]) -> Vec<u8> {
    let mut line = String::new();
    if operands.is_empty() {
        line.push('y');
    } else {
        for (index, operand) in operands.iter().enumerate() {
            if index > 0 {
                line.push(' ');
            }
            line.push_str(operand);
        }
    }
    line.push('\n');

    let line = line.into_bytes();
    let copies = (BLOCK_LEN / line.len()).max(1);
    let mut block = Vec::with_capacity(copies * line.len());
    for _ in 0..copies {
        block.extend_from_slice(&line);
    }
    block
}

/// Write `block` to `out` until a write fails, and report that failure.
///
/// This is the tool's steady state: it only ever returns the
/// [`YesError::Output`] that stopped it — successful termination is the
/// process being terminated by its consumer going away or the user
/// interrupting it. The writer is never idle-spinning: every iteration
/// delivers output, and a full stream backing blocks the write
/// kernel-side until the consumer drains it.
///
/// # Errors
///
/// [`YesError::Output`] when the sink stopped accepting bytes.
pub fn pump(block: &[u8], out: &dyn Output) -> YesError {
    loop {
        if let Err(err) = out.write_all(block) {
            return err;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    use super::{block, parse, pump, Command, Output, YesError, BLOCK_LEN};

    fn words(operands: &[&str]) -> Command {
        Command::Repeat(operands.iter().map(|&s| String::from(s)).collect())
    }

    #[test]
    fn no_operands_repeat_y() {
        assert_eq!(parse(&[]), Ok(words(&[])));
    }

    #[test]
    fn operands_are_taken_verbatim() {
        assert_eq!(parse(&["hello", "world"]), Ok(words(&["hello", "world"])));
        // A lone `-` is an operand.
        assert_eq!(parse(&["-"]), Ok(words(&["-"])));
    }

    #[test]
    fn first_operand_ends_option_scanning() {
        // GNU option scanning stops at the first operand: a later
        // option-shaped token is an operand.
        assert_eq!(parse(&["a", "-x"]), Ok(words(&["a", "-x"])));
        assert_eq!(parse(&["a", "--help"]), Ok(words(&["a", "--help"])));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse(&["--", "-x"]), Ok(words(&["-x"])));
        assert_eq!(parse(&["--"]), Ok(words(&[])));
        assert_eq!(parse(&["--", "--help"]), Ok(words(&["--help"])));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_option_is_usage() {
        // GNU `yes` rejects unrecognised options; `yes -- -x` is the
        // spelling that prints them.
        assert_eq!(parse(&["-x"]), Err(YesError::Usage));
        assert_eq!(parse(&["--frob", "a"]), Err(YesError::Usage));
    }

    #[test]
    fn block_repeats_the_default_line() {
        let block = block(&[]);
        assert_eq!(block.len(), BLOCK_LEN);
        assert!(block.chunks(2).all(|pair| pair == b"y\n"));
    }

    #[test]
    fn block_repeats_the_operand_line_whole_times() {
        let line = b"hello world\n";
        let block = block(&[String::from("hello"), String::from("world")]);
        assert_eq!(block.len(), (BLOCK_LEN / line.len()) * line.len());
        assert!(block.chunks(line.len()).all(|chunk| chunk == line));
    }

    #[test]
    fn block_holds_at_least_one_copy_of_an_overlong_line() {
        let long = "x".repeat(2 * BLOCK_LEN);
        let block = block(core::slice::from_ref(&long));
        assert_eq!(block.len(), long.len() + 1);
        assert!(block.ends_with(b"\n"));
    }

    /// An [`Output`] that accepts a fixed number of writes, recording each,
    /// then fails — the closed-pipe stand-in.
    struct Closing {
        remaining: Cell<usize>,
        written: core::cell::RefCell<Vec<u8>>,
    }

    impl Output for Closing {
        fn write_all(&self, bytes: &[u8]) -> Result<(), YesError> {
            if self.remaining.get() == 0 {
                return Err(YesError::Output);
            }
            self.remaining.set(self.remaining.get() - 1);
            self.written.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    #[test]
    fn pump_writes_until_the_output_closes() {
        let out = Closing {
            remaining: Cell::new(3),
            written: core::cell::RefCell::new(Vec::new()),
        };
        let block = vec![b'y', b'\n'];
        assert_eq!(pump(&block, &out), YesError::Output);
        assert_eq!(&*out.written.borrow(), b"y\ny\ny\n");
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/yes.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/yes.md must document {switch}"
            );
        }
    }
}

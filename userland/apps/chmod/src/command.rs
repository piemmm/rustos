//! The parsed shape of a `chmod` command line, including the mode algebra.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::ChmodError;

/// One thing the `chmod` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Apply `mode` to each of `files`, in operand order.
    Change {
        /// Descend into directories and apply the mode to their contents
        /// (`-R`/`--recursive`).
        recursive: bool,
        /// The mode to apply — either an absolute octal value or a list of
        /// symbolic clauses.
        mode: Mode,
        /// The files to change, in order. Always at least one.
        files: Vec<String>,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// A mode operand, parsed into the form `chmod` applies.
///
/// `chmod` accepts two notations and this captures both: an absolute octal
/// value that replaces the permission bits outright, and a list of symbolic
/// clauses that transform the current bits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// An absolute octal mode (`644`, `0755`, …). The low twelve bits replace
    /// the file's permission bits; the current mode is irrelevant.
    Absolute(u32),
    /// A non-empty list of symbolic clauses (`g+w`, `o-x`, `a=rx`, …), applied
    /// in order to the file's current mode.
    Symbolic(Vec<Clause>),
}

impl Mode {
    /// Resolve this mode against a file's `current` permission bits and
    /// whether it `is_dir`, returning the new permission bits (`& 0o7777`).
    ///
    /// An absolute mode ignores `current`; a symbolic mode folds each clause
    /// over it. `is_dir` resolves the symbolic `X` permission, which grants
    /// execute to a directory or to a file that already carries one.
    #[must_use]
    pub fn resolve(&self, current: u32, is_dir: bool) -> u32 {
        match self {
            Self::Absolute(bits) => *bits & 0o7777,
            Self::Symbolic(clauses) => {
                let mut mode = current & 0o7777;
                for clause in clauses {
                    mode = clause.apply(mode, is_dir);
                }
                mode
            }
        }
    }
}

/// The operator joining a symbolic clause's affected bits to the current mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    /// `+` — turn the clause's bits on, leaving the rest unchanged.
    Add,
    /// `-` — turn the clause's bits off, leaving the rest unchanged.
    Remove,
    /// `=` — set the selected fields to exactly the clause's bits, clearing
    /// the others within those fields.
    Set,
}

/// One symbolic clause: who it affects, the operator, and the permissions.
///
/// `who` and `perms` are bit sets over [`WHO_USER`]/[`WHO_GROUP`]/
/// [`WHO_OTHER`] and the `PERM_*` constants. A `who` of `0` means none was
/// written, which [`Clause::apply`] treats as "all" (the POSIX default).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Clause {
    /// The affected fields (owner/group/other) as a [`WHO_USER`] bit set; `0`
    /// means "all".
    pub who: u8,
    /// The operator.
    pub op: Op,
    /// The permissions as a `PERM_*` bit set.
    pub perms: u8,
}

/// `who` bit: the owning user's permission field.
pub const WHO_USER: u8 = 0b001;
/// `who` bit: the owning group's permission field.
pub const WHO_GROUP: u8 = 0b010;
/// `who` bit: the "other" permission field.
pub const WHO_OTHER: u8 = 0b100;

/// `perms` bit: read (`r`).
pub const PERM_READ: u8 = 0b00_0001;
/// `perms` bit: write (`w`).
pub const PERM_WRITE: u8 = 0b00_0010;
/// `perms` bit: execute (`x`).
pub const PERM_EXEC: u8 = 0b00_0100;
/// `perms` bit: conditional execute (`X`) — execute for a directory or a file
/// that already carries an execute bit.
pub const PERM_COND_EXEC: u8 = 0b00_1000;
/// `perms` bit: the set-user/set-group-ID bit (`s`).
pub const PERM_SETID: u8 = 0b01_0000;
/// `perms` bit: the sticky bit (`t`).
pub const PERM_STICKY: u8 = 0b10_0000;

impl Clause {
    /// Apply this clause to `mode` (the low twelve permission bits), resolving
    /// `X` against `is_dir`, and return the new bits.
    #[must_use]
    pub fn apply(&self, mode: u32, is_dir: bool) -> u32 {
        let who = if self.who == 0 {
            WHO_USER | WHO_GROUP | WHO_OTHER
        } else {
            self.who
        };

        let mut triple = 0u32;
        if self.perms & PERM_READ != 0 {
            triple |= 0o4;
        }
        if self.perms & PERM_WRITE != 0 {
            triple |= 0o2;
        }
        if self.perms & PERM_EXEC != 0 {
            triple |= 0o1;
        }
        if self.perms & PERM_COND_EXEC != 0 && (is_dir || mode & 0o111 != 0) {
            triple |= 0o1;
        }

        let mut bits = 0u32;
        if who & WHO_USER != 0 {
            bits |= triple << 6;
        }
        if who & WHO_GROUP != 0 {
            bits |= triple << 3;
        }
        if who & WHO_OTHER != 0 {
            bits |= triple;
        }
        if self.perms & PERM_SETID != 0 {
            if who & WHO_USER != 0 {
                bits |= 0o4000;
            }
            if who & WHO_GROUP != 0 {
                bits |= 0o2000;
            }
        }
        if self.perms & PERM_STICKY != 0 {
            bits |= 0o1000;
        }

        match self.op {
            Op::Add => mode | bits,
            Op::Remove => mode & !bits,
            Op::Set => {
                let mut clear = 0u32;
                if who & WHO_USER != 0 {
                    clear |= 0o4700;
                }
                if who & WHO_GROUP != 0 {
                    clear |= 0o2070;
                }
                if who & WHO_OTHER != 0 {
                    clear |= 0o1007;
                }
                (mode & !clear) | bits
            }
        }
    }
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `chmod [-R] [--] MODE file...`:
///
/// * `-R` / `--recursive` — descend into directories.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`ChmodError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else — an operand.
///
/// The first operand is the mode; the rest are files. To set a mode that
/// begins with `-` (for example, "remove write for all"), write it without a
/// leading dash (`a-w`) or end option parsing first (`chmod -- -w file`).
///
/// # Errors
///
/// [`ChmodError::Usage`] for any unrecognised option before `--`, or when
/// fewer than two operands (a mode and at least one file) are given.
/// [`ChmodError::BadMode`] when the mode operand parses as neither an octal
/// nor a symbolic mode.
pub fn parse(args: &[&str]) -> Result<Command, ChmodError> {
    let mut recursive = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                match name {
                    "recursive" => recursive = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(ChmodError::Usage),
                }
                continue;
            }
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'R' => recursive = true,
                        'h' => return Ok(Command::Help),
                        _ => return Err(ChmodError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    if operands.is_empty() {
        return Err(ChmodError::Usage);
    }
    let mode_spec = operands.remove(0);
    if operands.is_empty() {
        return Err(ChmodError::Usage);
    }
    let mode = parse_mode(&mode_spec).ok_or(ChmodError::BadMode)?;
    Ok(Command::Change {
        recursive,
        mode,
        files: operands,
    })
}

/// Parse a mode operand as either an octal or a symbolic mode, in that order.
fn parse_mode(spec: &str) -> Option<Mode> {
    if let Some(bits) = parse_octal(spec) {
        return Some(Mode::Absolute(bits));
    }
    parse_symbolic(spec).map(Mode::Symbolic)
}

/// Parse one to four octal digits into the low twelve mode bits. Returns
/// [`None`] for an empty string, more than four digits, or a non-octal digit.
fn parse_octal(spec: &str) -> Option<u32> {
    if spec.is_empty() || spec.len() > 4 {
        return None;
    }
    let mut value = 0u32;
    for c in spec.chars() {
        value = value * 8 + c.to_digit(8)?;
    }
    Some(value)
}

/// Parse a comma-separated list of symbolic clauses. Returns [`None`] for any
/// malformed clause (an empty clause, a clause without an operator, or an
/// unrecognised letter).
fn parse_symbolic(spec: &str) -> Option<Vec<Clause>> {
    let mut clauses = Vec::new();
    for part in spec.split(',') {
        if part.is_empty() {
            return None;
        }
        let mut chars = part.chars().peekable();
        let mut who = 0u8;
        while let Some(&c) = chars.peek() {
            match c {
                'u' => who |= WHO_USER,
                'g' => who |= WHO_GROUP,
                'o' => who |= WHO_OTHER,
                'a' => who |= WHO_USER | WHO_GROUP | WHO_OTHER,
                _ => break,
            }
            chars.next();
        }
        let mut saw_op = false;
        loop {
            let op = match chars.peek() {
                Some('+') => Op::Add,
                Some('-') => Op::Remove,
                Some('=') => Op::Set,
                _ => break,
            };
            chars.next();
            saw_op = true;
            let mut perms = 0u8;
            while let Some(&c) = chars.peek() {
                match c {
                    'r' => perms |= PERM_READ,
                    'w' => perms |= PERM_WRITE,
                    'x' => perms |= PERM_EXEC,
                    'X' => perms |= PERM_COND_EXEC,
                    's' => perms |= PERM_SETID,
                    't' => perms |= PERM_STICKY,
                    '+' | '-' | '=' => break,
                    _ => return None,
                }
                chars.next();
            }
            clauses.push(Clause { who, op, perms });
        }
        if !saw_op || chars.peek().is_some() {
            return None;
        }
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_mode, Command, Mode, Op};
    use super::{
        PERM_COND_EXEC, PERM_EXEC, PERM_READ, PERM_SETID, PERM_STICKY, PERM_WRITE, WHO_GROUP,
        WHO_OTHER, WHO_USER,
    };
    use crate::error::ChmodError;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    fn change(recursive: bool, mode: Mode, files: &[&str]) -> Command {
        Command::Change {
            recursive,
            mode,
            files: files.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    // ----- command-line parsing -------------------------------------------

    #[test]
    fn an_octal_mode_and_one_file_parses() {
        assert_eq!(
            parse(&["644", "a.txt"]),
            Ok(change(false, Mode::Absolute(0o644), &["a.txt"]))
        );
    }

    #[test]
    fn a_leading_zero_octal_mode_parses() {
        assert_eq!(
            parse(&["0755", "f"]),
            Ok(change(false, Mode::Absolute(0o755), &["f"]))
        );
    }

    #[test]
    fn fewer_than_two_operands_is_usage() {
        assert_eq!(parse(&[]), Err(ChmodError::Usage));
        assert_eq!(parse(&["644"]), Err(ChmodError::Usage));
        assert_eq!(parse(&["-R"]), Err(ChmodError::Usage));
        assert_eq!(parse(&["-R", "644"]), Err(ChmodError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-Rh", "644", "f"]), Ok(Command::Help));
    }

    #[test]
    fn recursive_flag_sets_its_field() {
        assert_eq!(
            parse(&["-R", "600", "d"]),
            Ok(change(true, Mode::Absolute(0o600), &["d"]))
        );
        assert_eq!(
            parse(&["--recursive", "600", "d"]),
            Ok(change(true, Mode::Absolute(0o600), &["d"]))
        );
    }

    #[test]
    fn several_files_after_the_mode_are_all_collected() {
        assert_eq!(
            parse(&["640", "a", "b", "c"]),
            Ok(change(false, Mode::Absolute(0o640), &["a", "b", "c"]))
        );
    }

    #[test]
    fn lowercase_r_is_not_recursive_and_is_usage() {
        // POSIX `chmod` spells recursive `-R`; a bare `-r` is not an option.
        assert_eq!(parse(&["-r", "644", "f"]), Err(ChmodError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "644", "f"]), Err(ChmodError::Usage));
        assert_eq!(parse(&["--frob", "644", "f"]), Err(ChmodError::Usage));
        assert_eq!(parse(&["-Rx", "644", "f"]), Err(ChmodError::Usage));
    }

    #[test]
    fn double_dash_ends_options_so_a_dash_mode_is_an_operand() {
        // `chmod -- -w file` applies the symbolic clause `-w` to `file`.
        assert_eq!(
            parse(&["--", "-w", "file"]),
            Ok(change(
                false,
                Mode::Symbolic(vec![super::Clause {
                    who: 0,
                    op: Op::Remove,
                    perms: PERM_WRITE,
                }]),
                &["file"],
            ))
        );
    }

    #[test]
    fn an_unparseable_mode_is_bad_mode() {
        assert_eq!(parse(&["zzz", "f"]), Err(ChmodError::BadMode));
        assert_eq!(parse(&["8", "f"]), Err(ChmodError::BadMode));
        assert_eq!(parse(&["00000", "f"]), Err(ChmodError::BadMode));
        // A `who` with no operator is malformed; an operator with empty perms
        // (`u+`) is a valid no-op, so it is *not* a BadMode.
        assert_eq!(parse(&["g", "f"]), Err(ChmodError::BadMode));
        assert_eq!(parse(&["a+rw,", "f"]), Err(ChmodError::BadMode));
        assert_eq!(parse(&["a+q", "f"]), Err(ChmodError::BadMode));
    }

    // ----- mode parsing ---------------------------------------------------

    #[test]
    fn symbolic_who_op_perms_parse_into_one_clause() {
        let mode = parse_mode("g+w").expect("valid clause");
        assert_eq!(
            mode,
            Mode::Symbolic(vec![super::Clause {
                who: WHO_GROUP,
                op: Op::Add,
                perms: PERM_WRITE,
            }])
        );
    }

    #[test]
    fn multiple_op_sections_share_the_clause_who() {
        // `u+x-w` is two actions on the owner field.
        let mode = parse_mode("u+x-w").expect("valid clause");
        assert_eq!(
            mode,
            Mode::Symbolic(vec![
                super::Clause {
                    who: WHO_USER,
                    op: Op::Add,
                    perms: PERM_EXEC,
                },
                super::Clause {
                    who: WHO_USER,
                    op: Op::Remove,
                    perms: PERM_WRITE,
                },
            ])
        );
    }

    #[test]
    fn comma_separated_clauses_parse_independently() {
        let mode = parse_mode("u=rwx,go=rx").expect("valid clauses");
        assert_eq!(
            mode,
            Mode::Symbolic(vec![
                super::Clause {
                    who: WHO_USER,
                    op: Op::Set,
                    perms: PERM_READ | PERM_WRITE | PERM_EXEC,
                },
                super::Clause {
                    who: WHO_GROUP | WHO_OTHER,
                    op: Op::Set,
                    perms: PERM_READ | PERM_EXEC,
                },
            ])
        );
    }

    #[test]
    fn special_and_conditional_perms_parse() {
        let mode = parse_mode("u+s").expect("valid");
        assert_eq!(
            mode,
            Mode::Symbolic(vec![super::Clause {
                who: WHO_USER,
                op: Op::Add,
                perms: PERM_SETID,
            }])
        );
        let cond = parse_mode("a+X").expect("valid");
        assert_eq!(
            cond,
            Mode::Symbolic(vec![super::Clause {
                who: WHO_USER | WHO_GROUP | WHO_OTHER,
                op: Op::Add,
                perms: PERM_COND_EXEC,
            }])
        );
        let sticky = parse_mode("+t").expect("valid");
        assert_eq!(
            sticky,
            Mode::Symbolic(vec![super::Clause {
                who: 0,
                op: Op::Add,
                perms: PERM_STICKY,
            }])
        );
    }

    // ----- the mode algebra (resolve) -------------------------------------

    #[test]
    fn absolute_mode_replaces_the_bits() {
        assert_eq!(Mode::Absolute(0o644).resolve(0o777, false), 0o644);
        assert_eq!(Mode::Absolute(0o4755).resolve(0o000, true), 0o4755);
        // Only the low twelve bits survive.
        assert_eq!(Mode::Absolute(0o170_644).resolve(0, false), 0o0644);
    }

    fn sym(spec: &str) -> Mode {
        parse_mode(spec).expect("valid symbolic mode")
    }

    #[test]
    fn add_turns_bits_on_leaving_the_rest() {
        assert_eq!(sym("g+w").resolve(0o644, false), 0o664);
        assert_eq!(sym("a+x").resolve(0o644, false), 0o755);
        assert_eq!(sym("o+r").resolve(0o640, false), 0o644);
    }

    #[test]
    fn remove_turns_bits_off() {
        assert_eq!(sym("o-r").resolve(0o644, false), 0o640);
        assert_eq!(sym("a-w").resolve(0o666, false), 0o444);
        assert_eq!(sym("u-x").resolve(0o755, false), 0o655);
    }

    #[test]
    fn set_replaces_only_the_selected_fields() {
        assert_eq!(sym("u=rw").resolve(0o777, false), 0o677);
        assert_eq!(sym("go=").resolve(0o777, false), 0o700);
        assert_eq!(sym("a=rx").resolve(0o777, false), 0o555);
    }

    #[test]
    fn omitted_who_means_all() {
        assert_eq!(sym("+x").resolve(0o644, false), 0o755);
        assert_eq!(sym("=r").resolve(0o777, false), 0o444);
    }

    #[test]
    fn conditional_execute_depends_on_directory_or_existing_execute() {
        // A directory always gets X.
        assert_eq!(sym("a+X").resolve(0o644, true), 0o755);
        // A plain file with no execute bit set does not.
        assert_eq!(sym("a+X").resolve(0o644, false), 0o644);
        // A file that already has one execute bit does.
        assert_eq!(sym("a+X").resolve(0o744, false), 0o755);
    }

    #[test]
    fn setid_and_sticky_bits_apply_per_who() {
        assert_eq!(sym("u+s").resolve(0o755, false), 0o4755);
        assert_eq!(sym("g+s").resolve(0o755, false), 0o2755);
        assert_eq!(sym("+s").resolve(0o755, false), 0o6755);
        assert_eq!(sym("+t").resolve(0o755, true), 0o1755);
        assert_eq!(sym("u-s").resolve(0o4755, false), 0o0755);
    }

    #[test]
    fn an_operator_with_empty_perms_is_a_valid_no_op() {
        // `u+` and `a-` parse and leave the mode unchanged.
        assert_eq!(sym("u+").resolve(0o640, false), 0o640);
        assert_eq!(sym("a-").resolve(0o640, false), 0o640);
    }

    #[test]
    fn clauses_apply_left_to_right() {
        // `a=rwx,go-w` first sets 0o777 then clears group/other write.
        assert_eq!(sym("a=rwx,go-w").resolve(0o000, false), 0o755);
    }
}

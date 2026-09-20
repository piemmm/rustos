//! The per-invocation elevation IPC protocol (`plans/CAPABILITY_USE.md` CU5).
//!
//! Elevation is *starting a new process under a more-privileged account,
//! through the one spawn-as-user holder, after re-authentication* — never a
//! runtime capability raise. The shell forwards an `elevate <user> <program>`
//! request to the **session supervisor serving its own console** (the login
//! process, which already holds `CAP_SPAWN_AS_USER` + `CAP_USERS_READ`); the
//! supervisor re-authenticates the target account exactly as a fresh login
//! would (timing-equalised, secret dropped immediately), spawns the program
//! as that account on the same console, and replies with the program's exit
//! code once it finishes. The requesting shell's own identity and capability
//! set are never touched.
//!
//! A [`ElevateRequest::Run`] carries a bounded argument vector, so a caller
//! can run the tool that already owns a store with the one change a user
//! asked for instead of growing a second writer for that store. The vector
//! widens no authority — the request already named an arbitrary absolute
//! program — and the supervisor performs every check it always did.
//!
//! A graphical caller cannot use that exchange: its reply arrives only
//! once the elevated program has exited, so a desktop session posting it
//! would stop serving windows to the very program it is waiting for. Such
//! a caller posts [`ElevateRequest::Launch`] instead — the identical
//! re-authentication, but the reply carries the started program's pid and
//! the broker never waits for it. The elevated program is interactive and
//! collects its own input, which is why the launch carries no argv.
//!
//! A caller that must *show* what an elevated run printed posts
//! [`ElevateRequest::Capture`]: the identical re-authentication and the
//! identical run, but the child's standard output is bound to a pipe the
//! supervisor owns and drained under [`ELEVATE_MAX_OUTPUT`], and the reply
//! carries those bytes beside the exit code. It is a separate form rather
//! than a flag on [`ElevateRequest::Run`] because relaying a program's
//! output to an unprivileged caller is a new information flow and is meant
//! to be visible as one at the call site. It widens no authority: the
//! caller must still offer the account's password, and an account that can
//! be re-authenticated could already be given a shell through
//! [`ElevateRequest::Launch`] — what the seam bounds is the *volume* of
//! relayed bytes, not their secrecy.
//!
//! The same broker also answers a narrower [`ElevateRequest::Verify`]
//! request that re-authenticates the **caller's own** kernel-attested
//! account and runs nothing — the primitive a graphical session's screen
//! lock needs, without a second authenticator existing anywhere in the
//! tree. It carries no username or uid on the wire: the supervisor reads
//! the caller's attested uid off the same `call_peer_origin` result the
//! console check already uses, so a lock screen can never be tricked into
//! checking a password against someone else's account.
//!
//! # Rendezvous
//!
//! Each console's supervisor binds its own synchronous call endpoint under
//! [`elevate_endpoint`]`(console)` — a reserved per-console id, exactly the
//! [`crate::mailbox_ipc::MAILBOX_ENDPOINT`] pattern — so elevation on one
//! console never queues behind another's. Both ends derive the id from their
//! **kernel-attested** console ([`crate::Origin::console`], via
//! `self_origin`), never from a claimed value, and the serving supervisor
//! additionally cross-checks each caller's attested console against its own
//! (`call_peer_origin`) before touching the request.
//!
//! # Security posture
//!
//! The endpoint is unrestricted-sender: the gate is the re-authentication
//! itself, exactly as the login prompt is reachable by anyone at the
//! keyboard. A wrong password, an unknown account, and a locked account are
//! refused **indistinguishably** ([`ElevateReply::Refused`] with
//! [`Errno::PermissionDenied`]) and every attempt is audited by the
//! supervisor. The request carries the offered password in the clear across
//! the kernel-copied IPC buffer (the same trust boundary as typing it at the
//! login prompt); both ends zeroise their copies as soon as the exchange
//! resolves.

use crate::le::{put_i32, put_i64, put_u32, read_i32, read_i64, read_u32};
use crate::process::CONSOLE_INDEX_MAX;
use crate::Errno;

/// Protocol version carried by every [`ElevateRequest`]; a request with any
/// other version is refused at decode (fail closed, never guessed).
pub const ELEVATE_VERSION: u16 = 1;

/// Hard byte bound on one encoded [`ElevateRequest`] — also the endpoint's
/// maximum request size. A fail-closed memory bound (the strings inside are
/// semantically validated by the supervisor), mirroring
/// [`crate::users_admin::USERS_ADMIN_MAX_REQUEST`].
///
/// Wide enough for the account, the offered secret, an absolute program path,
/// and a full [`ELEVATE_MAX_ARGV_BYTES`] argument vector with its framing.
pub const ELEVATE_MAX_REQUEST: usize = 2048;

/// Most arguments one [`ElevateRequest::Run`] may carry.
///
/// A containment bound, not a capacity: the vector exists so a caller can
/// hand a store-writing tool the one change a user asked for, and every such
/// invocation this system has is a handful of words. A vector a reviewer
/// cannot read at a glance is not one worth running as another account.
pub const ELEVATE_MAX_ARGS: usize = 16;

/// Longest single argument, in bytes.
///
/// Holds every value the configuration registry admits — a key name, a
/// closed-set spelling, an absolute path, or a full list of network time
/// servers — and refuses anything that could only be a paste of something
/// else.
pub const ELEVATE_MAX_ARG_LEN: usize = 512;

/// Most argument *content* bytes one request may carry, across the whole
/// vector and excluding the per-argument framing.
///
/// Bounds the vector as a whole, so [`ELEVATE_MAX_ARGS`] arguments each at
/// [`ELEVATE_MAX_ARG_LEN`] cannot together outgrow the request.
pub const ELEVATE_MAX_ARGV_BYTES: usize = 1024;

/// Most bytes of a captured run's standard output one
/// [`ElevateReply::Captured`] carries.
///
/// A fixed containment bound, not a capacity: it caps how much
/// attacker-influenced text an unprivileged caller can have a privileged
/// program hand back in one exchange. Sized from the widest listing the
/// only consumer asks for — the `configure` tool printing both
/// configuration registries, whose per-interface lines run to roughly a
/// kilobyte for an interface with every key set — so a machine's whole
/// addressing fits and a program that prints an unbounded stream does not.
/// A run that prints more is answered [`ElevateReply::Overran`], never
/// truncated.
pub const ELEVATE_MAX_OUTPUT: usize = 4096;

/// Byte length of the fixed head every encoded [`ElevateReply`] carries: a
/// status word and the value word beside it.
const REPLY_HEAD_LEN: usize = 12;

/// Largest encoded [`ElevateReply`] — also the endpoint's maximum reply
/// size. Every reply but [`ElevateReply::Captured`] is exactly the fixed
/// head; a captured one appends a length-prefixed output region bounded by
/// [`ELEVATE_MAX_OUTPUT`].
pub const ELEVATE_MAX_REPLY: usize = REPLY_HEAD_LEN + 4 + ELEVATE_MAX_OUTPUT;

/// Base of the reserved per-console elevation endpoint-id range; console
/// `n`'s supervisor serves `ELEVATE_ENDPOINT_BASE + n`. (`b"ELV"` spelled in
/// hex, disjoint from [`crate::mailbox_ipc::MAILBOX_ENDPOINT`] and every
/// other reserved id.)
pub const ELEVATE_ENDPOINT_BASE: u64 = 0x454C_5600;

/// The elevation call-endpoint id serving installed console `console`.
///
/// Both ends pass their **kernel-attested** console index
/// ([`crate::Origin::console`]); a value past [`CONSOLE_INDEX_MAX`] —
/// including the [`crate::ORIGIN_CONSOLE_NONE`] "not console-backed"
/// sentinel — names no endpoint and fails closed, so a process with no
/// console can never derive a rendezvous.
///
/// # Errors
///
/// [`Errno::OutOfRange`] when `console` is not a representable installed
/// console index.
pub const fn elevate_endpoint(console: u64) -> Result<u64, Errno> {
    if console > CONSOLE_INDEX_MAX as u64 {
        return Err(Errno::OutOfRange);
    }
    Ok(ELEVATE_ENDPOINT_BASE + console)
}

/// The argument vector an [`ElevateRequest::Run`] hands the program it
/// starts.
///
/// Bounded three ways ([`ELEVATE_MAX_ARGS`], [`ELEVATE_MAX_ARG_LEN`],
/// [`ELEVATE_MAX_ARGV_BYTES`]) and checked by one shared rule at both
/// construction and decode, so an encoder and a decoder can never disagree
/// on what is admissible.
///
/// The arguments are **data**: the broker hands them to the program
/// verbatim and interprets none of them, so an empty argument is a
/// well-formed one and is carried as typed. Every argument's *meaning* is
/// the started program's to judge, and it fails closed on anything it does
/// not understand exactly as it does on a command line.
///
/// It carries either a caller's slice or the packed wire bytes a decode
/// borrowed, because this crate has no allocator to rebuild a slice with;
/// the two compare and iterate identically, so which one a value holds is
/// never observable.
#[derive(Copy, Clone)]
pub struct ElevateArgv<'a> {
    repr: ArgvRepr<'a>,
}

/// How an [`ElevateArgv`] is holding its arguments.
#[derive(Copy, Clone)]
enum ArgvRepr<'a> {
    /// A caller's own slice.
    Slice(&'a [&'a str]),
    /// The `count`-argument packed region a decode borrowed, already
    /// validated: `count` repetitions of a `u16` length and that many UTF-8
    /// bytes, and nothing after them.
    Packed { count: u16, bytes: &'a [u8] },
}

impl<'a> ElevateArgv<'a> {
    /// The empty vector: the program is started with no arguments of its
    /// own, which is what every request carried before the vector existed.
    pub const NONE: Self = Self {
        repr: ArgvRepr::Slice(&[]),
    };

    /// The vector over `args`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] when `args` holds more than
    /// [`ELEVATE_MAX_ARGS`] arguments, one longer than
    /// [`ELEVATE_MAX_ARG_LEN`], or more than [`ELEVATE_MAX_ARGV_BYTES`]
    /// content bytes in total.
    pub fn new(args: &'a [&'a str]) -> Result<Self, Errno> {
        check_argv(args.len(), args.iter().map(|arg| arg.len()))?;
        Ok(Self {
            repr: ArgvRepr::Slice(args),
        })
    }

    /// How many arguments the vector carries.
    #[must_use]
    pub const fn len(&self) -> usize {
        match self.repr {
            ArgvRepr::Slice(args) => args.len(),
            ArgvRepr::Packed { count, .. } => count as usize,
        }
    }

    /// Whether the program is started with no arguments of its own.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The arguments, in order.
    #[must_use]
    pub const fn iter(&self) -> ElevateArgvIter<'a> {
        match self.repr {
            ArgvRepr::Slice(args) => ElevateArgvIter::Slice(args),
            ArgvRepr::Packed { count, bytes } => ElevateArgvIter::Packed { left: count, bytes },
        }
    }

    /// The encoded length of the vector: the count, then each argument's
    /// length prefix and bytes.
    fn encoded_len(&self) -> usize {
        self.iter().fold(2, |total, arg| total + 2 + arg.len())
    }
}

impl<'a> IntoIterator for ElevateArgv<'a> {
    type Item = &'a str;
    type IntoIter = ElevateArgvIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &ElevateArgv<'a> {
    type Item = &'a str;
    type IntoIter = ElevateArgvIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl PartialEq for ElevateArgv<'_> {
    /// By content, so a decoded vector equals the slice it was encoded from.
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for ElevateArgv<'_> {}

impl core::fmt::Debug for ElevateArgv<'_> {
    /// By content, for the same reason equality is.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The arguments of an [`ElevateArgv`], in order.
#[derive(Clone, Debug)]
pub enum ElevateArgvIter<'a> {
    /// Walking a caller's slice.
    Slice(&'a [&'a str]),
    /// Walking a validated packed region.
    Packed {
        /// Arguments still to yield.
        left: u16,
        /// The remaining length-prefixed arguments.
        bytes: &'a [u8],
    },
}

impl<'a> Iterator for ElevateArgvIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::Slice(args) => {
                let (first, rest) = args.split_first()?;
                *args = rest;
                Some(first)
            }
            Self::Packed { left, bytes } => {
                if *left == 0 {
                    return None;
                }
                // The region was validated at decode, so every step here
                // succeeds; ending the walk on a short read keeps that an
                // invariant rather than a panic.
                let mut cur = Cursor::new(bytes);
                let arg = cur.str().ok()?;
                *bytes = bytes.get(cur.at..)?;
                *left -= 1;
                Some(arg)
            }
        }
    }
}

/// The one admissibility rule for an argument vector, applied to a caller's
/// slice and to a decoded region alike.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] when the count, any one argument, or the
/// total content exceeds its bound.
fn check_argv(count: usize, lengths: impl Iterator<Item = usize>) -> Result<(), Errno> {
    if count > ELEVATE_MAX_ARGS {
        return Err(Errno::LengthOutOfRange);
    }
    let mut total = 0usize;
    for len in lengths {
        if len > ELEVATE_MAX_ARG_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        total = total.saturating_add(len);
        if total > ELEVATE_MAX_ARGV_BYTES {
            return Err(Errno::LengthOutOfRange);
        }
    }
    Ok(())
}

/// Wire opcode naming an [`ElevateRequest::Run`] request.
const OPCODE_RUN: u8 = 0;
/// Wire opcode naming an [`ElevateRequest::Verify`] request.
const OPCODE_VERIFY: u8 = 1;
/// Wire opcode naming an [`ElevateRequest::Launch`] request.
const OPCODE_LAUNCH: u8 = 2;
/// Wire opcode naming an [`ElevateRequest::Capture`] request.
const OPCODE_CAPTURE: u8 = 3;

/// One elevation request, posted to the console's supervisor.
///
/// The strings are only *shape*-checked here (UTF-8, within
/// [`ELEVATE_MAX_REQUEST`], non-empty); the supervisor performs the semantic
/// validation (account exists, password verifies, program resolves) and
/// refuses all failures indistinguishably. Every `password` field is a
/// secret: every holder zeroises its buffer as soon as the exchange
/// resolves.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ElevateRequest<'a> {
    /// Re-authenticate `username` and, on success, run `program` as that
    /// account with the argument vector `argv`.
    ///
    /// The vector is what lets a caller run the tool that already owns a
    /// store with the one change a user asked for, rather than growing a
    /// second writer for that store. It widens no authority: the request
    /// already named an arbitrary absolute program, and the broker still
    /// re-authenticates the account, loads through the ordinary signed load
    /// gate, runs as that account, and audits the decision.
    Run {
        /// The target account to re-authenticate and run as.
        username: &'a str,
        /// The offered password for that account.
        password: &'a str,
        /// Absolute path of the program to spawn on success.
        program: &'a str,
        /// The arguments to hand it, [`ElevateArgv::NONE`] for none.
        argv: ElevateArgv<'a>,
    },
    /// Re-authenticate `username` and, on success, run `program` as that
    /// account with the argument vector `argv`, answering **what it
    /// printed** beside its exit code.
    ///
    /// The same authority, re-authentication, signed load gate, run-as-uid
    /// and audit as [`Self::Run`]; the difference is the information flow,
    /// which is why it is its own form. The child's standard output is
    /// bound to a pipe the supervisor owns rather than the caller's
    /// console, drained under [`ELEVATE_MAX_OUTPUT`], and returned as
    /// [`ElevateReply::Captured`]. Its standard input is closed — a run
    /// nobody can see cannot be prompting — and a run that prints more
    /// than the bound is answered [`ElevateReply::Overran`] with no bytes
    /// at all, never a truncation the caller could mistake for the whole.
    Capture {
        /// The target account to re-authenticate and run as.
        username: &'a str,
        /// The offered password for that account.
        password: &'a str,
        /// Absolute path of the program to spawn on success.
        program: &'a str,
        /// The arguments to hand it, [`ElevateArgv::NONE`] for none.
        argv: ElevateArgv<'a>,
    },
    /// Re-authenticate the **calling principal's own** account against
    /// `password`; run nothing.
    ///
    /// Deliberately carries no username or uid: the broker authenticates
    /// against the caller's kernel-attested uid (the same attestation the
    /// console placement check already reads), never a value the request
    /// itself supplies, so a caller can only ever re-verify *itself* — the
    /// primitive a screen lock needs and nothing more.
    Verify {
        /// The offered password for the caller's own account.
        password: &'a str,
    },
    /// Re-authenticate `username` and, on success, start `program` as that
    /// account **without waiting for it**, answering its pid.
    ///
    /// The same authority and the same re-authentication as [`Self::Run`];
    /// only the reply timing differs. It exists for a caller that must keep
    /// serving the elevated program while it runs — a graphical session,
    /// which owns the compositor the program draws through — and would
    /// otherwise deadlock against a reply that waits for that program's
    /// exit. The program is interactive: it takes no argv and collects
    /// whatever it needs itself.
    Launch {
        /// The target account to re-authenticate and run as.
        username: &'a str,
        /// The offered password for that account.
        password: &'a str,
        /// Absolute path of the program to start on success.
        program: &'a str,
    },
}

impl<'a> ElevateRequest<'a> {
    /// Encode the request little-endian into `out`, returning the encoded
    /// length.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] when a field is empty or the encoding
    /// would exceed [`ELEVATE_MAX_REQUEST`]; [`Errno::BufferTooSmall`] when
    /// `out` cannot hold it.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let total = 2
            + 1
            + match *self {
                Self::Run {
                    username,
                    password,
                    program,
                    argv,
                }
                | Self::Capture {
                    username,
                    password,
                    program,
                    argv,
                } => {
                    if username.is_empty() || password.is_empty() || program.is_empty() {
                        return Err(Errno::LengthOutOfRange);
                    }
                    check_argv(argv.len(), argv.iter().map(str::len))?;
                    (2 + username.len())
                        + (2 + password.len())
                        + (2 + program.len())
                        + argv.encoded_len()
                }
                Self::Launch {
                    username,
                    password,
                    program,
                } => {
                    if username.is_empty() || password.is_empty() || program.is_empty() {
                        return Err(Errno::LengthOutOfRange);
                    }
                    (2 + username.len()) + (2 + password.len()) + (2 + program.len())
                }
                Self::Verify { password } => {
                    if password.is_empty() {
                        return Err(Errno::LengthOutOfRange);
                    }
                    2 + password.len()
                }
            };
        if total > ELEVATE_MAX_REQUEST {
            return Err(Errno::LengthOutOfRange);
        }
        let mut w = Writer::new(out);
        w.u16(ELEVATE_VERSION)?;
        w.u8(self.opcode())?;
        match *self {
            Self::Run {
                username,
                password,
                program,
                argv,
            }
            | Self::Capture {
                username,
                password,
                program,
                argv,
            } => {
                w.str(username)?;
                w.str(password)?;
                w.str(program)?;
                w.u16(u16::try_from(argv.len()).map_err(|_| Errno::LengthOutOfRange)?)?;
                for arg in argv {
                    w.str(arg)?;
                }
            }
            Self::Launch {
                username,
                password,
                program,
            } => {
                w.str(username)?;
                w.str(password)?;
                w.str(program)?;
            }
            Self::Verify { password } => w.str(password)?,
        }
        Ok(w.at)
    }

    /// The wire opcode naming this request.
    const fn opcode(&self) -> u8 {
        match *self {
            Self::Run { .. } => OPCODE_RUN,
            Self::Verify { .. } => OPCODE_VERIFY,
            Self::Launch { .. } => OPCODE_LAUNCH,
            Self::Capture { .. } => OPCODE_CAPTURE,
        }
    }

    /// Decode a request from `bytes`, failing closed on any malformation:
    /// wrong version, an unknown opcode, over-long buffer, a field running
    /// past the end, non-UTF-8 bytes, an empty field, or trailing bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] / [`Errno::OutOfRange`] per the rules
    /// above — never a partial decode.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() > ELEVATE_MAX_REQUEST {
            return Err(Errno::LengthOutOfRange);
        }
        let mut cur = Cursor::new(bytes);
        if cur.u16()? != ELEVATE_VERSION {
            return Err(Errno::OutOfRange);
        }
        let request = match cur.u8()? {
            opcode @ (OPCODE_RUN | OPCODE_LAUNCH | OPCODE_CAPTURE) => {
                let username = cur.str()?;
                let password = cur.str()?;
                let program = cur.str()?;
                if username.is_empty() || password.is_empty() || program.is_empty() {
                    return Err(Errno::LengthOutOfRange);
                }
                match opcode {
                    OPCODE_RUN => Self::Run {
                        username,
                        password,
                        program,
                        argv: cur.argv()?,
                    },
                    OPCODE_CAPTURE => Self::Capture {
                        username,
                        password,
                        program,
                        argv: cur.argv()?,
                    },
                    _ => Self::Launch {
                        username,
                        password,
                        program,
                    },
                }
            }
            OPCODE_VERIFY => {
                let password = cur.str()?;
                if password.is_empty() {
                    return Err(Errno::LengthOutOfRange);
                }
                Self::Verify { password }
            }
            _ => return Err(Errno::OutOfRange),
        };
        if !cur.exhausted() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(request)
    }
}

/// The supervisor's answer to one [`ElevateRequest`].
///
/// Borrows the buffer it was decoded from, because
/// [`Self::Captured`] carries the run's output and this crate has no
/// allocator to own it with; every other variant borrows nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ElevateReply<'a> {
    /// A [`ElevateRequest::Run`] re-authenticated, the program ran on the
    /// caller's console as that account, and it exited with this code.
    Completed {
        /// The elevated program's exit status, exactly as `wait` reported it.
        exit_code: i32,
    },
    /// A [`ElevateRequest::Verify`] re-authenticated the caller's own
    /// account; nothing was run.
    Verified,
    /// A [`ElevateRequest::Launch`] re-authenticated and the program was
    /// started as that account; it is still running.
    ///
    /// The pid lets the caller recognise the elevated program among its
    /// peers (a graphical session matches it to the window client that
    /// connects). It is the broker's child, not the caller's, so the caller
    /// cannot wait on it — the broker reaps it.
    Launched {
        /// Process id of the started program, always non-negative.
        pid: i64,
    },
    /// An [`ElevateRequest::Capture`] re-authenticated, the program ran as
    /// that account with its output bound to the supervisor's pipe, and it
    /// exited with this code having printed these bytes.
    ///
    /// The output is the program's *whole* standard output: a run that
    /// printed more than [`ELEVATE_MAX_OUTPUT`] answers [`Self::Overran`]
    /// instead, so a caller never reads a prefix as if it were the lot.
    /// The bytes are the program's, not the supervisor's — arbitrary,
    /// possibly not UTF-8, and to be treated as data by whoever asked for
    /// them.
    Captured {
        /// The elevated program's exit status, exactly as `wait` reported
        /// it.
        exit_code: i32,
        /// Everything it wrote to standard output.
        output: &'a [u8],
    },
    /// An [`ElevateRequest::Capture`] re-authenticated and the program ran,
    /// but it printed more than [`ELEVATE_MAX_OUTPUT`]; no output is
    /// returned.
    ///
    /// Distinct from a refusal because the run genuinely happened and its
    /// exit code is real — whatever it did, it did — and distinct from
    /// [`Self::Captured`] because a truncated prefix is a different answer
    /// from a complete one and must not be mistaken for it.
    Overran {
        /// The elevated program's exit status.
        exit_code: i32,
    },
    /// The request was refused. Authentication failures (wrong password,
    /// unknown account, locked account) are all
    /// [`Errno::PermissionDenied`], indistinguishably; other codes report
    /// mechanical failures (an unresolvable program, a spawn refusal).
    Refused(Errno),
}

/// Wire status word naming a completed [`ElevateReply::Verified`] reply.
const STATUS_VERIFIED: i32 = 1;
/// Wire status word naming an [`ElevateReply::Launched`] reply.
const STATUS_LAUNCHED: i32 = 2;
/// Wire status word naming an [`ElevateReply::Captured`] reply.
const STATUS_CAPTURED: i32 = 3;
/// Wire status word naming an [`ElevateReply::Overran`] reply.
const STATUS_OVERRAN: i32 = 4;

impl<'a> ElevateReply<'a> {
    /// Encode the reply into `out`, returning the encoded length.
    ///
    /// The first word is a result discriminant: `0` for a completed run,
    /// `1` for a verified re-authentication, `2` for a started program,
    /// `3` for a captured one, `4` for one whose output overran the bound,
    /// else the negated [`Errno`] discriminant (the
    /// [`crate::driver_store`] status-word convention). The second word is
    /// the exit code of a completed, captured, or overrun run, the pid of a
    /// started program, and `0` for [`Self::Verified`] and
    /// [`Self::Refused`]. A [`Self::Captured`] reply appends a `u32` output
    /// length and that many bytes; every other reply ends at the head.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` cannot hold the encoding;
    /// [`Errno::OutOfRange`] on a negative pid or an output longer than
    /// [`ELEVATE_MAX_OUTPUT`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < REPLY_HEAD_LEN {
            return Err(Errno::BufferTooSmall);
        }
        // The word is `i64`-wide because it carries a pid, and a pid is a
        // task id drawn over the whole 64-bit space; an exit code merely
        // widens into it.
        let (status, word) = match *self {
            Self::Completed { exit_code } => (0, i64::from(exit_code)),
            Self::Verified => (STATUS_VERIFIED, 0),
            Self::Launched { pid } => {
                if pid < 0 {
                    return Err(Errno::OutOfRange);
                }
                (STATUS_LAUNCHED, pid)
            }
            Self::Captured { exit_code, output } => {
                if output.len() > ELEVATE_MAX_OUTPUT {
                    return Err(Errno::OutOfRange);
                }
                let region = u32::try_from(output.len()).map_err(|_| Errno::OutOfRange)?;
                let end = REPLY_HEAD_LEN + 4 + output.len();
                if out.len() < end {
                    return Err(Errno::BufferTooSmall);
                }
                put_i32(out, 0, STATUS_CAPTURED);
                put_i64(out, 4, i64::from(exit_code));
                put_u32(out, REPLY_HEAD_LEN, region);
                out[REPLY_HEAD_LEN + 4..end].copy_from_slice(output);
                return Ok(end);
            }
            Self::Overran { exit_code } => (STATUS_OVERRAN, i64::from(exit_code)),
            Self::Refused(err) => (-err.as_i32(), 0),
        };
        put_i32(out, 0, status);
        put_i64(out, 4, word);
        Ok(REPLY_HEAD_LEN)
    }

    /// Decode a reply from `bytes`, failing closed on a wrong length, an
    /// unknown errno, an unrecognised status word, a launched pid that is
    /// negative, or a captured output region that is over-long, short, or
    /// followed by trailing bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] on a wrong length;
    /// [`Errno::OutOfRange`] on an unrecognised status word, an
    /// unrepresentable pid, or an over-long output region.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < REPLY_HEAD_LEN || bytes.len() > ELEVATE_MAX_REPLY {
            return Err(Errno::LengthOutOfRange);
        }
        let status = read_i32(bytes, 0);
        let word = read_i64(bytes, 4);
        if status == STATUS_CAPTURED {
            return Self::decode_captured(bytes, word);
        }
        // Every other reply is exactly the head: trailing bytes are a frame
        // this end does not understand, not a longer one to read past.
        if bytes.len() != REPLY_HEAD_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        match status {
            0 => Ok(Self::Completed {
                exit_code: i32::try_from(word).map_err(|_| Errno::OutOfRange)?,
            }),
            STATUS_VERIFIED => Ok(Self::Verified),
            STATUS_LAUNCHED if word >= 0 => Ok(Self::Launched { pid: word }),
            STATUS_OVERRAN => Ok(Self::Overran {
                exit_code: i32::try_from(word).map_err(|_| Errno::OutOfRange)?,
            }),
            s if s < 0 => {
                let errno = Errno::try_from_status(s).ok_or(Errno::OutOfRange)?;
                Ok(Self::Refused(errno))
            }
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Decode the variable tail of a [`Self::Captured`] frame: a `u32`
    /// length and exactly that many bytes, with nothing after them.
    fn decode_captured(bytes: &'a [u8], word: i64) -> Result<Self, Errno> {
        if bytes.len() < REPLY_HEAD_LEN + 4 {
            return Err(Errno::LengthOutOfRange);
        }
        let len = read_u32(bytes, REPLY_HEAD_LEN) as usize;
        if len > ELEVATE_MAX_OUTPUT {
            return Err(Errno::OutOfRange);
        }
        if bytes.len() != REPLY_HEAD_LEN + 4 + len {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self::Captured {
            exit_code: i32::try_from(word).map_err(|_| Errno::OutOfRange)?,
            output: &bytes[REPLY_HEAD_LEN + 4..],
        })
    }
}

/// A fail-closed little-endian reader over a request buffer (the
/// [`crate::users_admin`] cursor shape; small enough that sharing it would
/// couple two independent wire formats).
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Errno> {
        let end = self.at.checked_add(n).ok_or(Errno::LengthOutOfRange)?;
        if end > self.bytes.len() {
            return Err(Errno::LengthOutOfRange);
        }
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Errno> {
        let b = self.take(1)?;
        Ok(b[0])
    }

    fn u16(&mut self) -> Result<u16, Errno> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// A `u16`-length-prefixed UTF-8 string.
    fn str(&mut self) -> Result<&'a str, Errno> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)
    }

    /// A `u16` argument count and that many length-prefixed UTF-8
    /// arguments, validated whole against the same rule an encoder applies.
    ///
    /// Every argument is decoded here — its length read, its bytes taken,
    /// its UTF-8 checked — so the region the borrowed [`ElevateArgv`] keeps
    /// is one that walks cleanly, and the iterator over it needs no error
    /// path of its own.
    fn argv(&mut self) -> Result<ElevateArgv<'a>, Errno> {
        let count = self.u16()?;
        if count as usize > ELEVATE_MAX_ARGS {
            return Err(Errno::LengthOutOfRange);
        }
        let from = self.at;
        let mut total = 0usize;
        for _ in 0..count {
            let arg = self.str()?;
            if arg.len() > ELEVATE_MAX_ARG_LEN {
                return Err(Errno::LengthOutOfRange);
            }
            total = total.saturating_add(arg.len());
            if total > ELEVATE_MAX_ARGV_BYTES {
                return Err(Errno::LengthOutOfRange);
            }
        }
        Ok(ElevateArgv {
            repr: ArgvRepr::Packed {
                count,
                bytes: self.bytes.get(from..self.at).ok_or(Errno::OutOfRange)?,
            },
        })
    }

    const fn exhausted(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// A fail-closed little-endian writer over a caller-supplied buffer.
struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Self { out, at: 0 }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        let end = self
            .at
            .checked_add(bytes.len())
            .ok_or(Errno::BufferTooSmall)?;
        if end > self.out.len() {
            return Err(Errno::BufferTooSmall);
        }
        self.out[self.at..end].copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), Errno> {
        self.bytes(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<(), Errno> {
        self.bytes(&v.to_le_bytes())
    }

    /// A `u16`-length-prefixed UTF-8 string; a string longer than a `u16`
    /// can carry is refused.
    fn str(&mut self, s: &str) -> Result<(), Errno> {
        let len = u16::try_from(s.len()).map_err(|_| Errno::LengthOutOfRange)?;
        self.u16(len)?;
        self.bytes(s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        elevate_endpoint, ElevateArgv, ElevateReply, ElevateRequest, ELEVATE_ENDPOINT_BASE,
        ELEVATE_MAX_ARGS, ELEVATE_MAX_ARGV_BYTES, ELEVATE_MAX_ARG_LEN, ELEVATE_MAX_OUTPUT,
        ELEVATE_MAX_REPLY, ELEVATE_MAX_REQUEST, ELEVATE_VERSION,
    };
    use crate::{Errno, ORIGIN_CONSOLE_NONE};

    #[test]
    fn endpoint_is_per_console_and_refuses_non_consoles() {
        assert_eq!(elevate_endpoint(0), Ok(ELEVATE_ENDPOINT_BASE));
        assert_eq!(elevate_endpoint(1), Ok(ELEVATE_ENDPOINT_BASE + 1));
        assert_eq!(elevate_endpoint(255), Ok(ELEVATE_ENDPOINT_BASE + 255));
        // Past the installed-console index range: no rendezvous.
        assert_eq!(elevate_endpoint(256), Err(Errno::OutOfRange));
        // The "not console-backed" origin sentinel derives nothing.
        assert_eq!(
            elevate_endpoint(ORIGIN_CONSOLE_NONE),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn run_request_round_trips() {
        let req = ElevateRequest::Run {
            username: "root",
            password: "hunter2",
            program: "/System/Commands/users.app/Run",
            argv: ElevateArgv::NONE,
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));
    }

    #[test]
    fn launch_request_round_trips() {
        let req = ElevateRequest::Launch {
            username: "root",
            password: "hunter2",
            program: "/System/Applications/datetime.app/Run",
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));
    }

    #[test]
    fn launch_and_run_are_distinct_on_the_wire() {
        let mut run_buf = [0u8; ELEVATE_MAX_REQUEST];
        let mut launch_buf = [0u8; ELEVATE_MAX_REQUEST];
        let launch_len = ElevateRequest::Launch {
            username: "root",
            password: "hunter2",
            program: "/x",
        }
        .encode(&mut launch_buf)
        .expect("encodes");
        // Even with no arguments to carry, a run spells its (empty) vector,
        // so the two opcodes never encode to the same bytes.
        let bare = ElevateRequest::Run {
            username: "root",
            password: "hunter2",
            program: "/x",
            argv: ElevateArgv::NONE,
        }
        .encode(&mut run_buf)
        .expect("encodes");
        assert_eq!(bare, launch_len + 2);
        assert_ne!(run_buf[..launch_len], launch_buf[..launch_len]);
        // And a launch has nowhere to put arguments: the program it starts
        // is interactive and collects its own input.
        let args = ["os.loginType", "text"];
        let carried = ElevateRequest::Run {
            username: "root",
            password: "hunter2",
            program: "/x",
            argv: ElevateArgv::new(&args).expect("within bounds"),
        }
        .encode(&mut run_buf)
        .expect("encodes");
        assert!(carried > bare);
    }

    #[test]
    fn launch_request_rejects_empty_fields_both_ways() {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        for req in [
            ElevateRequest::Launch {
                username: "",
                password: "p",
                program: "/x",
            },
            ElevateRequest::Launch {
                username: "u",
                password: "",
                program: "/x",
            },
            ElevateRequest::Launch {
                username: "u",
                password: "p",
                program: "",
            },
        ] {
            assert_eq!(req.encode(&mut buf), Err(Errno::LengthOutOfRange));
        }
        // A hand-built launch record with an empty program is refused at
        // decode too (the wire is not trusted to mirror the encoder).
        let mut bytes = [0u8; 11];
        bytes[..2].copy_from_slice(&ELEVATE_VERSION.to_le_bytes());
        bytes[2] = 2; // OPCODE_LAUNCH
        bytes[3..5].copy_from_slice(&1u16.to_le_bytes());
        bytes[5] = b'u';
        bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
        bytes[8] = b'p';
        bytes[9..11].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(ElevateRequest::decode(&bytes), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn verify_request_round_trips() {
        let req = ElevateRequest::Verify {
            password: "hunter2",
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));
    }

    #[test]
    fn run_request_rejects_empty_fields_both_ways() {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        for req in [
            ElevateRequest::Run {
                username: "",
                password: "p",
                program: "/x",
                argv: ElevateArgv::NONE,
            },
            ElevateRequest::Run {
                username: "u",
                password: "",
                program: "/x",
                argv: ElevateArgv::NONE,
            },
            ElevateRequest::Run {
                username: "u",
                password: "p",
                program: "",
                argv: ElevateArgv::NONE,
            },
        ] {
            assert_eq!(req.encode(&mut buf), Err(Errno::LengthOutOfRange));
        }
        // A hand-built record with an empty username is refused at decode
        // too (the wire is not trusted to mirror the encoder).
        let mut bytes = [0u8; 13];
        bytes[..2].copy_from_slice(&ELEVATE_VERSION.to_le_bytes());
        bytes[2] = 0; // OPCODE_RUN
                      // username len 0, password len 1 = "p", program len 1 = "x".
        bytes[5..7].copy_from_slice(&1u16.to_le_bytes());
        bytes[7] = b'p';
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[10] = b'x';
        assert_eq!(
            ElevateRequest::decode(&bytes[..11]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn verify_request_rejects_empty_password() {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        assert_eq!(
            ElevateRequest::Verify { password: "" }.encode(&mut buf),
            Err(Errno::LengthOutOfRange)
        );
        // A hand-built record with an empty password is refused at decode
        // too (the wire is not trusted to mirror the encoder).
        let mut bytes = [0u8; 5];
        bytes[..2].copy_from_slice(&ELEVATE_VERSION.to_le_bytes());
        bytes[2] = 1; // OPCODE_VERIFY
        bytes[3..5].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(ElevateRequest::decode(&bytes), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn request_decode_fails_closed_on_malformations() {
        let req = ElevateRequest::Run {
            username: "root",
            password: "pw",
            program: "/System/Commands/ps.app/Run",
            argv: ElevateArgv::NONE,
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");

        // Wrong version.
        let mut wrong = buf;
        wrong[0] = 9;
        assert_eq!(
            ElevateRequest::decode(&wrong[..len]),
            Err(Errno::OutOfRange)
        );
        // Unknown opcode (one past the four the protocol defines).
        let mut unknown_opcode = buf;
        unknown_opcode[2] = 4;
        assert_eq!(
            ElevateRequest::decode(&unknown_opcode[..len]),
            Err(Errno::OutOfRange)
        );
        // Truncated.
        assert_eq!(
            ElevateRequest::decode(&buf[..len - 1]),
            Err(Errno::LengthOutOfRange)
        );
        // Trailing bytes.
        assert_eq!(
            ElevateRequest::decode(&buf[..=len]),
            Err(Errno::LengthOutOfRange)
        );
        // Non-UTF-8 in a field.
        let mut bad = buf;
        bad[5] = 0xFF;
        assert_eq!(ElevateRequest::decode(&bad[..len]), Err(Errno::OutOfRange));
        // Over-long buffer bound.
        let oversized = [0u8; ELEVATE_MAX_REQUEST + 1];
        assert_eq!(
            ElevateRequest::decode(&oversized),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn oversized_request_is_refused_at_encode() {
        let long = [b'a'; ELEVATE_MAX_REQUEST];
        let long = core::str::from_utf8(&long).expect("ascii");
        let req = ElevateRequest::Run {
            username: long,
            password: "p",
            program: "/x",
            argv: ElevateArgv::NONE,
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST * 2];
        assert_eq!(req.encode(&mut buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn run_request_round_trips_its_argument_vector() {
        let args = ["os.loginType", "text"];
        let req = ElevateRequest::Run {
            username: "root",
            password: "hunter2",
            program: "/System/Commands/configure.app/Run",
            argv: ElevateArgv::new(&args).expect("within bounds"),
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        let decoded = ElevateRequest::decode(&buf[..len]).expect("decodes");
        assert_eq!(decoded, req);
        let ElevateRequest::Run { argv, .. } = decoded else {
            panic!("a run request decodes as one");
        };
        assert_eq!(argv.len(), 2);
        assert!(argv.iter().eq(args));
    }

    #[test]
    fn an_empty_argument_is_carried_as_typed() {
        // The arguments are data the broker hands over verbatim; judging an
        // argument's spelling is the started program's job, not the wire's.
        let args = ["time.servers", ""];
        let req = ElevateRequest::Run {
            username: "root",
            password: "p",
            program: "/x",
            argv: ElevateArgv::new(&args).expect("within bounds"),
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));
    }

    #[test]
    fn an_argument_vector_is_bounded_three_ways() {
        let one = "a";
        let too_many = [one; ELEVATE_MAX_ARGS + 1];
        assert_eq!(ElevateArgv::new(&too_many), Err(Errno::LengthOutOfRange));
        assert!(ElevateArgv::new(&too_many[..ELEVATE_MAX_ARGS]).is_ok());

        let long = [b'a'; ELEVATE_MAX_ARG_LEN + 1];
        let long = core::str::from_utf8(&long).expect("ascii");
        assert_eq!(ElevateArgv::new(&[long]), Err(Errno::LengthOutOfRange));
        assert!(ElevateArgv::new(&[&long[..ELEVATE_MAX_ARG_LEN]]).is_ok());

        // Each argument within its own bound, the vector over the total.
        let chunk = &long[..ELEVATE_MAX_ARG_LEN];
        let spread = [chunk; ELEVATE_MAX_ARGV_BYTES / ELEVATE_MAX_ARG_LEN + 1];
        assert_eq!(ElevateArgv::new(&spread), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn an_over_long_vector_is_refused_at_decode_too() {
        // The wire is not trusted to mirror the encoder: a hand-built record
        // claiming more arguments than the bound admits is refused before a
        // single one of them is read.
        let mut bytes = [0u8; 16];
        bytes[..2].copy_from_slice(&ELEVATE_VERSION.to_le_bytes());
        bytes[2] = 0; // OPCODE_RUN
        let mut at = 3;
        for field in ["u", "p", "/x"] {
            let len = u16::try_from(field.len()).expect("short");
            bytes[at..at + 2].copy_from_slice(&len.to_le_bytes());
            at += 2;
            bytes[at..at + field.len()].copy_from_slice(field.as_bytes());
            at += field.len();
        }
        let count = u16::try_from(ELEVATE_MAX_ARGS + 1).expect("small");
        bytes[at..at + 2].copy_from_slice(&count.to_le_bytes());
        at += 2;
        assert_eq!(
            ElevateRequest::decode(&bytes[..at]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn a_vector_of_non_utf8_bytes_is_refused_at_decode() {
        let args = ["key", "value"];
        let req = ElevateRequest::Run {
            username: "root",
            password: "p",
            program: "/x",
            argv: ElevateArgv::new(&args).expect("within bounds"),
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        // The last argument's final byte, which is inside its own content.
        buf[len - 1] = 0xFF;
        assert_eq!(ElevateRequest::decode(&buf[..len]), Err(Errno::OutOfRange));
    }

    #[test]
    fn the_widest_admissible_vector_still_fits_one_request() {
        // The request bound is not a second, tighter limit on the vector: a
        // vector at every one of its own bounds still encodes, beside a
        // full account, secret and program path.
        let arg = [b'a'; ELEVATE_MAX_ARGV_BYTES / ELEVATE_MAX_ARGS];
        let arg = core::str::from_utf8(&arg).expect("ascii");
        let args = [arg; ELEVATE_MAX_ARGS];
        let account = [b'u'; 64];
        let secret = [b's'; 256];
        let program = [b'/'; 512];
        let req = ElevateRequest::Run {
            username: core::str::from_utf8(&account).expect("ascii"),
            password: core::str::from_utf8(&secret).expect("ascii"),
            program: core::str::from_utf8(&program).expect("ascii"),
            argv: ElevateArgv::new(&args).expect("within bounds"),
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));
    }

    #[test]
    fn reply_round_trips_every_variant() {
        let widest = [b'x'; ELEVATE_MAX_OUTPUT];
        let mut buf = [0u8; ELEVATE_MAX_REPLY];
        for reply in [
            ElevateReply::Completed { exit_code: 0 },
            ElevateReply::Completed { exit_code: 130 },
            ElevateReply::Verified,
            ElevateReply::Launched { pid: 0 },
            ElevateReply::Launched { pid: 4210 },
            ElevateReply::Captured {
                exit_code: 0,
                output: b"",
            },
            ElevateReply::Captured {
                exit_code: 2,
                output: b"os.loginType graphical\n",
            },
            ElevateReply::Captured {
                exit_code: 0,
                output: &widest,
            },
            ElevateReply::Overran { exit_code: 0 },
            ElevateReply::Overran { exit_code: 7 },
            ElevateReply::Refused(Errno::PermissionDenied),
            ElevateReply::Refused(Errno::NotFound),
        ] {
            let len = reply.encode(&mut buf).expect("encodes");
            assert_eq!(ElevateReply::decode(&buf[..len]), Ok(reply));
        }
    }

    #[test]
    fn a_reply_with_no_output_is_exactly_the_head() {
        let mut buf = [0u8; ELEVATE_MAX_REPLY];
        let head = ElevateReply::Verified.encode(&mut buf).expect("encodes");
        // Every non-captured reply is that one length, so a peer that only
        // ever posts the output-free forms needs no larger buffer than the
        // head — and a captured one is exactly the head plus its region.
        for reply in [
            ElevateReply::Completed { exit_code: 3 },
            ElevateReply::Launched { pid: 9 },
            ElevateReply::Overran { exit_code: 3 },
            ElevateReply::Refused(Errno::NotFound),
        ] {
            assert_eq!(reply.encode(&mut buf), Ok(head));
        }
        assert_eq!(
            ElevateReply::Captured {
                exit_code: 0,
                output: b"abcd",
            }
            .encode(&mut buf),
            Ok(head + 4 + 4)
        );
        assert_eq!(
            ElevateReply::Captured {
                exit_code: 0,
                output: &[0u8; ELEVATE_MAX_OUTPUT],
            }
            .encode(&mut buf),
            Ok(ELEVATE_MAX_REPLY)
        );
    }

    #[test]
    fn reply_decode_fails_closed() {
        let head = ELEVATE_MAX_REPLY - 4 - ELEVATE_MAX_OUTPUT;
        // Wrong length.
        assert_eq!(
            ElevateReply::decode(&[0u8; 0]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ElevateReply::decode(&[0u8; 11]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ElevateReply::decode(&[0u8; 13]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ElevateReply::decode(&[0u8; ELEVATE_MAX_REPLY + 1]),
            Err(Errno::LengthOutOfRange)
        );
        // A status word past the known discriminants (`0` completed, `1`
        // verified, `2` launched, `3` captured, `4` overran) is neither a
        // success nor a negated errno.
        let mut buf = [0u8; ELEVATE_MAX_REPLY];
        buf[..4].copy_from_slice(&5i32.to_le_bytes());
        assert_eq!(ElevateReply::decode(&buf[..head]), Err(Errno::OutOfRange));
        // A launched reply whose pid word is negative names no process.
        buf[..4].copy_from_slice(&2i32.to_le_bytes());
        buf[4..head].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(ElevateReply::decode(&buf[..head]), Err(Errno::OutOfRange));
        assert_eq!(
            ElevateReply::Launched { pid: -1 }.encode(&mut buf),
            Err(Errno::OutOfRange)
        );
        buf[4..head].copy_from_slice(&0i64.to_le_bytes());
        // An unknown negated errno is refused, never guessed.
        buf[..4].copy_from_slice(&(-9999i32).to_le_bytes());
        assert_eq!(ElevateReply::decode(&buf[..head]), Err(Errno::OutOfRange));
    }

    #[test]
    fn a_captured_reply_decode_refuses_a_mismatched_region() {
        let head = ELEVATE_MAX_REPLY - 4 - ELEVATE_MAX_OUTPUT;
        let mut buf = [0u8; ELEVATE_MAX_REPLY];
        let len = ElevateReply::Captured {
            exit_code: 0,
            output: b"abcd",
        }
        .encode(&mut buf)
        .expect("encodes");
        // The head alone claims a region that is not there.
        assert_eq!(
            ElevateReply::decode(&buf[..head]),
            Err(Errno::LengthOutOfRange)
        );
        // A region shorter or longer than its length word is a frame this
        // end does not understand, never a prefix to read anyway.
        assert_eq!(
            ElevateReply::decode(&buf[..len - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ElevateReply::decode(&buf[..=len]),
            Err(Errno::LengthOutOfRange)
        );
        // A length word past the bound is refused before any slicing.
        let mut over = [0u8; ELEVATE_MAX_REPLY];
        over[..4].copy_from_slice(&3i32.to_le_bytes());
        over[head..head + 4].copy_from_slice(
            &u32::try_from(ELEVATE_MAX_OUTPUT + 1)
                .expect("fits")
                .to_le_bytes(),
        );
        assert_eq!(ElevateReply::decode(&over), Err(Errno::OutOfRange));
        // And an output longer than the bound cannot be encoded either.
        assert_eq!(
            ElevateReply::Captured {
                exit_code: 0,
                output: &[0u8; ELEVATE_MAX_OUTPUT + 1],
            }
            .encode(&mut [0u8; ELEVATE_MAX_REPLY + 1]),
            Err(Errno::OutOfRange)
        );
        // A buffer that holds the head but not the region refuses rather
        // than writing a frame whose length word lies.
        assert_eq!(
            ElevateReply::Captured {
                exit_code: 0,
                output: b"abcd",
            }
            .encode(&mut buf[..head + 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn a_capture_request_round_trips_and_is_its_own_form() {
        let req = ElevateRequest::Capture {
            username: "root",
            password: "hunter2",
            program: "/System/Commands/configure.app/Run",
            argv: ElevateArgv::new(&["wan.ipv4.method"]).expect("within bounds"),
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));

        // The same fields posted as a `Run` encode to different bytes, so a
        // supervisor can never take one form for the other.
        let mut run_buf = [0u8; ELEVATE_MAX_REQUEST];
        let run_len = ElevateRequest::Run {
            username: "root",
            password: "hunter2",
            program: "/System/Commands/configure.app/Run",
            argv: ElevateArgv::new(&["wan.ipv4.method"]).expect("within bounds"),
        }
        .encode(&mut run_buf)
        .expect("encodes");
        assert_eq!(len, run_len);
        assert_ne!(buf[..len], run_buf[..run_len]);
    }
}

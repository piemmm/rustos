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

use crate::le::{put_i32, read_i32};
use crate::process::CONSOLE_INDEX_MAX;
use crate::Errno;

/// Protocol version carried by every [`ElevateRequest`]; a request with any
/// other version is refused at decode (fail closed, never guessed).
pub const ELEVATE_VERSION: u16 = 1;

/// Hard byte bound on one encoded [`ElevateRequest`] — also the endpoint's
/// maximum request size. A fail-closed memory bound (the strings inside are
/// semantically validated by the supervisor), mirroring
/// [`crate::users_admin::USERS_ADMIN_MAX_REQUEST`].
pub const ELEVATE_MAX_REQUEST: usize = 1024;

/// Exact byte length of an encoded [`ElevateReply`] — also the endpoint's
/// maximum reply size: a status word and an exit code.
pub const ELEVATE_REPLY_LEN: usize = 8;

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

/// Wire opcode naming an [`ElevateRequest::Run`] request.
const OPCODE_RUN: u8 = 0;
/// Wire opcode naming an [`ElevateRequest::Verify`] request.
const OPCODE_VERIFY: u8 = 1;

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
    /// account.
    Run {
        /// The target account to re-authenticate and run as.
        username: &'a str,
        /// The offered password for that account.
        password: &'a str,
        /// Absolute path of the program to spawn on success.
        program: &'a str,
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
        match *self {
            Self::Run {
                username,
                password,
                program,
            } => {
                w.u8(OPCODE_RUN)?;
                w.str(username)?;
                w.str(password)?;
                w.str(program)?;
            }
            Self::Verify { password } => {
                w.u8(OPCODE_VERIFY)?;
                w.str(password)?;
            }
        }
        Ok(w.at)
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
            OPCODE_RUN => {
                let username = cur.str()?;
                let password = cur.str()?;
                let program = cur.str()?;
                if username.is_empty() || password.is_empty() || program.is_empty() {
                    return Err(Errno::LengthOutOfRange);
                }
                Self::Run {
                    username,
                    password,
                    program,
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
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ElevateReply {
    /// A [`ElevateRequest::Run`] re-authenticated, the program ran on the
    /// caller's console as that account, and it exited with this code.
    Completed {
        /// The elevated program's exit status, exactly as `wait` reported it.
        exit_code: i32,
    },
    /// A [`ElevateRequest::Verify`] re-authenticated the caller's own
    /// account; nothing was run.
    Verified,
    /// The request was refused. Authentication failures (wrong password,
    /// unknown account, locked account) are all
    /// [`Errno::PermissionDenied`], indistinguishably; other codes report
    /// mechanical failures (an unresolvable program, a spawn refusal).
    Refused(Errno),
}

/// Wire status word naming a completed [`ElevateReply::Verified`] reply.
const STATUS_VERIFIED: i32 = 1;

impl ElevateReply {
    /// Encode the reply into `out`, returning the encoded length
    /// ([`ELEVATE_REPLY_LEN`]).
    ///
    /// The first word is a result discriminant: `0` for a completed run,
    /// `1` for a verified re-authentication, else the negated [`Errno`]
    /// discriminant (the [`crate::driver_store`] status-word convention);
    /// the second is the exit code (`0` for [`Self::Verified`] and
    /// [`Self::Refused`]).
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` is shorter than
    /// [`ELEVATE_REPLY_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < ELEVATE_REPLY_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let (status, exit_code) = match *self {
            Self::Completed { exit_code } => (0, exit_code),
            Self::Verified => (STATUS_VERIFIED, 0),
            Self::Refused(err) => (-err.as_i32(), 0),
        };
        put_i32(out, 0, status);
        put_i32(out, 4, exit_code);
        Ok(ELEVATE_REPLY_LEN)
    }

    /// Decode a reply from `bytes`, failing closed on a wrong length, an
    /// unknown errno, or a status word that is neither `0`, `1`, nor a
    /// negated known errno.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] on a wrong length;
    /// [`Errno::OutOfRange`] on an unrecognised status word.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != ELEVATE_REPLY_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let status = read_i32(bytes, 0);
        let exit_code = read_i32(bytes, 4);
        match status {
            0 => Ok(Self::Completed { exit_code }),
            STATUS_VERIFIED => Ok(Self::Verified),
            s if s < 0 => {
                let errno = s
                    .checked_neg()
                    .and_then(Errno::from_i32)
                    .ok_or(Errno::OutOfRange)?;
                Ok(Self::Refused(errno))
            }
            _ => Err(Errno::OutOfRange),
        }
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
        elevate_endpoint, ElevateReply, ElevateRequest, ELEVATE_ENDPOINT_BASE, ELEVATE_MAX_REQUEST,
        ELEVATE_REPLY_LEN, ELEVATE_VERSION,
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
            program: "/System/Apps/users.app/Run",
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = req.encode(&mut buf).expect("encodes");
        assert_eq!(ElevateRequest::decode(&buf[..len]), Ok(req));
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
            },
            ElevateRequest::Run {
                username: "u",
                password: "",
                program: "/x",
            },
            ElevateRequest::Run {
                username: "u",
                password: "p",
                program: "",
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
            program: "/System/Apps/ps.app/Run",
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
        // Unknown opcode.
        let mut unknown_opcode = buf;
        unknown_opcode[2] = 2;
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
        };
        let mut buf = [0u8; ELEVATE_MAX_REQUEST * 2];
        assert_eq!(req.encode(&mut buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn reply_round_trips_every_variant() {
        let mut buf = [0u8; ELEVATE_REPLY_LEN];
        for reply in [
            ElevateReply::Completed { exit_code: 0 },
            ElevateReply::Completed { exit_code: 130 },
            ElevateReply::Verified,
            ElevateReply::Refused(Errno::PermissionDenied),
            ElevateReply::Refused(Errno::NotFound),
        ] {
            let len = reply.encode(&mut buf).expect("encodes");
            assert_eq!(len, ELEVATE_REPLY_LEN);
            assert_eq!(ElevateReply::decode(&buf[..len]), Ok(reply));
        }
    }

    #[test]
    fn reply_decode_fails_closed() {
        // Wrong length.
        assert_eq!(
            ElevateReply::decode(&[0u8; ELEVATE_REPLY_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            ElevateReply::decode(&[0u8; ELEVATE_REPLY_LEN + 1]),
            Err(Errno::LengthOutOfRange)
        );
        // A status word past the known discriminants (`0` completed, `1`
        // verified) is neither success nor a negated errno.
        let mut buf = [0u8; ELEVATE_REPLY_LEN];
        buf[..4].copy_from_slice(&2i32.to_le_bytes());
        assert_eq!(ElevateReply::decode(&buf), Err(Errno::OutOfRange));
        // An unknown negated errno is refused, never guessed.
        buf[..4].copy_from_slice(&(-9999i32).to_le_bytes());
        assert_eq!(ElevateReply::decode(&buf), Err(Errno::OutOfRange));
    }
}

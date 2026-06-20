//! The read-only `/System` driver-store file-read IPC protocol (Design D
//! D2b — `.junie/next-pi-prompt.md`).
//!
//! Under Design D the one bootstrap-floor disk is owned for life by the
//! never-returning driver-store kernel service, which keeps the read-only
//! signed-bundle `/System` volume mounted (`AGENTS.md` §18.3 / §18.4). A
//! user-space client — the reactive device manager (`userland/system/devmgr`)
//! — reaches that volume through a single capability-gated synchronous IPC
//! call endpoint ([`SyscallNumber::IPC_CALL`](crate::SyscallNumber::IPC_CALL))
//! served by the kernel service. The two operations the client needs are
//! **list** the `/System/Drivers/` store and **read** a bundle's bytes.
//!
//! This module is the wire contract for that endpoint: the request encoder
//! both sides share and the reply framing, all operating on borrowed buffers
//! (`AGENTS.md` §2.2 — the one definition the kernel server and the user-space
//! client agree on; no allocation, matching the crate's `no_std` contract).
//! Both `list` and `read` replies are length-framed and carry a leading
//! status word so a fail-closed refusal is delivered in-band rather than as a
//! truncated payload (`AGENTS.md` §5.4 / §2.9).

use crate::le::{put_i32, put_u32, put_u64, read_i32, read_u32, read_u64};
use crate::Errno;

/// Well-known kernel-owned call-endpoint id of the read-only `/System`
/// driver-store file-read service (Design D D2b).
///
/// The disk-owning kernel service creates one [`crate::ipc`]-style call
/// endpoint under this reserved id; the device manager names it as the
/// `endpoint` argument to [`SyscallNumber::IPC_CALL`](crate::SyscallNumber::IPC_CALL).
/// A reserved well-known id (rather than a delegated handle) keeps the
/// bootstrap client/server rendezvous from needing a prior name-exchange
/// step; the endpoint's required send capability still gates every call
/// (`AGENTS.md` §5.2 / §5.4).
pub const DRIVER_STORE_ENDPOINT: u64 = 0xD012_5701;

/// Maximum length, in bytes, of a store-relative path in a [`FileRequest`].
///
/// A validation bound on untrusted input, not a scaling capacity
/// (`AGENTS.md` §24.4): a request naming a longer path is refused fail-closed.
pub const DRIVER_STORE_PATH_MAX: usize = 255;

/// Maximum number of bytes a single [`FileRequest::Read`] may ask for.
///
/// The reply is length-framed and must fit the endpoint's reply bound and
/// the client's buffer; a larger run is read in successive chunks. A
/// validation bound, not a capacity (`AGENTS.md` §24.4).
pub const FILE_READ_CHUNK_MAX: u32 = 4096;

/// Request opcode: list the installed `/System/Drivers/` bundle paths.
const OP_LIST: u8 = 1;
/// Request opcode: read a run of a bundle's bytes.
const OP_READ: u8 = 2;

/// Fixed prefix of an encoded [`FileRequest::Read`]: opcode + `offset` (u64)
/// + `len` (u32), before the path bytes.
///
/// Public so the kernel-side server can size its request buffer to the
/// largest valid [`FileRequest::Read`] (`READ_HEADER_LEN +
/// DRIVER_STORE_PATH_MAX`) from the one definition both sides share
/// (`AGENTS.md` §2.2).
pub const READ_HEADER_LEN: usize = 1 + 8 + 4;

/// A request posted to the driver-store file-read endpoint.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FileRequest<'a> {
    /// List every installed bundle path under the store root.
    List,
    /// Read `len` bytes of the bundle at `path` starting at `offset`.
    Read {
        /// Store-relative bundle path (`<= DRIVER_STORE_PATH_MAX` bytes).
        path: &'a str,
        /// Byte offset to start reading at.
        offset: u64,
        /// Number of bytes to read (`<= FILE_READ_CHUNK_MAX`).
        len: u32,
    },
}

impl<'a> FileRequest<'a> {
    /// Encode `self` into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `buf` cannot hold the encoding.
    /// * [`Errno::LengthOutOfRange`] if a `Read` path exceeds
    ///   [`DRIVER_STORE_PATH_MAX`] or `len` exceeds [`FILE_READ_CHUNK_MAX`].
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        match *self {
            FileRequest::List => {
                if buf.is_empty() {
                    return Err(Errno::BufferTooSmall);
                }
                buf[0] = OP_LIST;
                Ok(1)
            }
            FileRequest::Read { path, offset, len } => {
                if path.len() > DRIVER_STORE_PATH_MAX || len > FILE_READ_CHUNK_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
                let total = READ_HEADER_LEN + path.len();
                if buf.len() < total {
                    return Err(Errno::BufferTooSmall);
                }
                buf[0] = OP_READ;
                put_u64(buf, 1, offset);
                put_u32(buf, 9, len);
                buf[READ_HEADER_LEN..total].copy_from_slice(path.as_bytes());
                Ok(total)
            }
        }
    }

    /// Decode a request from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is empty, a `Read` is
    ///   truncated, its path exceeds [`DRIVER_STORE_PATH_MAX`], or `len`
    ///   exceeds [`FILE_READ_CHUNK_MAX`].
    /// * [`Errno::OutOfRange`] if the opcode is unknown.
    /// * [`Errno::BadAddress`] if a `Read` path is not valid UTF-8.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let Some((&op, rest)) = bytes.split_first() else {
            return Err(Errno::LengthOutOfRange);
        };
        match op {
            OP_LIST => Ok(FileRequest::List),
            OP_READ => {
                if rest.len() < READ_HEADER_LEN - 1 {
                    return Err(Errno::LengthOutOfRange);
                }
                let offset = read_u64(bytes, 1);
                let len = read_u32(bytes, 9);
                if len > FILE_READ_CHUNK_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
                let path_bytes = &bytes[READ_HEADER_LEN..];
                if path_bytes.len() > DRIVER_STORE_PATH_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
                let path = core::str::from_utf8(path_bytes).map_err(|_| Errno::BadAddress)?;
                Ok(FileRequest::Read { path, offset, len })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Fixed prefix of every reply frame: a status word (`0` on success, else
/// the negated [`Errno`] discriminant).
const REPLY_STATUS_LEN: usize = 4;
/// Reply prefix once the status word is `0`: the status word plus a `u32`
/// count (entry count for a list reply, byte count for a read reply).
const REPLY_OK_HEADER_LEN: usize = REPLY_STATUS_LEN + 4;

/// Encode a fail-closed error reply (status only) into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` is shorter than the status word.
pub fn encode_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(REPLY_STATUS_LEN)
}

/// Recover the [`Errno`] a reply frame carries, or `Ok` if it is a success
/// frame (returning the success body following the header).
fn split_status(reply: &[u8]) -> Result<i32, Errno> {
    if reply.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    Ok(read_i32(reply, 0))
}

/// Map a reply's status word to the [`Errno`] it encodes, or `Ok(())` when
/// the status is success (`0`).
///
/// # Errors
///
/// The decoded [`Errno`] when the status is negative, or
/// [`Errno::BadMagic`] if the status is neither `0` nor a known negated
/// discriminant (wire corruption — fail closed).
pub fn reply_status(reply: &[u8]) -> Result<(), Errno> {
    match split_status(reply)? {
        0 => Ok(()),
        negative => Errno::from_i32(-negative).map_or(Err(Errno::BadMagic), Err),
    }
}

/// Encode the successful body of a [`FileRequest::Read`] reply (`bytes` read)
/// into `buf`, returning the number of bytes written.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold the framed reply.
pub fn encode_read_reply(buf: &mut [u8], bytes: &[u8]) -> Result<usize, Errno> {
    let total = REPLY_OK_HEADER_LEN + bytes.len();
    if buf.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    put_u32(
        buf,
        REPLY_STATUS_LEN,
        u32::try_from(bytes.len()).map_err(|_| Errno::LengthOutOfRange)?,
    );
    buf[REPLY_OK_HEADER_LEN..total].copy_from_slice(bytes);
    Ok(total)
}

/// Recover the bytes a successful [`FileRequest::Read`] reply carries.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame, or [`Errno::BadMagic`] if the
/// frame is truncated or the declared count overruns it (fail closed).
pub fn decode_read_reply(reply: &[u8]) -> Result<&[u8], Errno> {
    reply_status(reply)?;
    if reply.len() < REPLY_OK_HEADER_LEN {
        return Err(Errno::BadMagic);
    }
    let n = read_u32(reply, REPLY_STATUS_LEN) as usize;
    let body = &reply[REPLY_OK_HEADER_LEN..];
    if body.len() < n {
        return Err(Errno::BadMagic);
    }
    Ok(&body[..n])
}

/// Encode the successful body of a [`FileRequest::List`] reply listing
/// `paths` into `buf`, returning the number of bytes written.
///
/// Each entry is a `u16` length followed by the path bytes; the frame is
/// `status(0) || count || (u16 len || bytes)*`.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] if `buf` cannot hold every entry — the list
///   is never truncated (`AGENTS.md` §2.9); the caller grows its buffer.
/// * [`Errno::LengthOutOfRange`] if a path exceeds [`DRIVER_STORE_PATH_MAX`]
///   or the entry count exceeds `u32`.
pub fn encode_list_reply(buf: &mut [u8], paths: &[&str]) -> Result<usize, Errno> {
    if buf.len() < REPLY_OK_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    put_u32(
        buf,
        REPLY_STATUS_LEN,
        u32::try_from(paths.len()).map_err(|_| Errno::LengthOutOfRange)?,
    );
    let mut pos = REPLY_OK_HEADER_LEN;
    for path in paths {
        if path.len() > DRIVER_STORE_PATH_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let entry = 2 + path.len();
        if buf.len() < pos + entry {
            return Err(Errno::BufferTooSmall);
        }
        crate::le::put_u16(buf, pos, u16::try_from(path.len()).expect("bounded above"));
        buf[pos + 2..pos + entry].copy_from_slice(path.as_bytes());
        pos += entry;
    }
    Ok(pos)
}

/// An iterator over the paths a successful [`FileRequest::List`] reply
/// carries.
///
/// Construct with [`decode_list_reply`]; each [`Iterator::next`] yields one
/// `Result<&str, Errno>`, failing closed on a truncated entry or non-UTF-8
/// path (`AGENTS.md` §2.9).
pub struct ListReplyIter<'a> {
    body: &'a [u8],
    remaining: u32,
}

impl<'a> Iterator for ListReplyIter<'a> {
    type Item = Result<&'a str, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        if self.body.len() < 2 {
            return Some(Err(Errno::BadMagic));
        }
        let len = crate::le::read_u16(self.body, 0) as usize;
        let entry_end = 2 + len;
        if self.body.len() < entry_end {
            return Some(Err(Errno::BadMagic));
        }
        let raw = &self.body[2..entry_end];
        self.body = &self.body[entry_end..];
        Some(core::str::from_utf8(raw).map_err(|_| Errno::BadAddress))
    }
}

/// Decode a successful [`FileRequest::List`] reply into an iterator over its
/// paths.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame, or [`Errno::BadMagic`] if the
/// frame header is truncated (fail closed).
pub fn decode_list_reply(reply: &[u8]) -> Result<ListReplyIter<'_>, Errno> {
    reply_status(reply)?;
    if reply.len() < REPLY_OK_HEADER_LEN {
        return Err(Errno::BadMagic);
    }
    let count = read_u32(reply, REPLY_STATUS_LEN);
    Ok(ListReplyIter {
        body: &reply[REPLY_OK_HEADER_LEN..],
        remaining: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_request_round_trips() {
        let mut buf = [0u8; 8];
        let n = FileRequest::List.encode(&mut buf).expect("encodes");
        assert_eq!(n, 1);
        assert_eq!(FileRequest::decode(&buf[..n]), Ok(FileRequest::List));
    }

    #[test]
    fn read_request_round_trips() {
        let req = FileRequest::Read {
            path: "/System/Drivers/input/kbd",
            offset: 4096,
            len: 512,
        };
        let mut buf = [0u8; 128];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(FileRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn read_request_rejects_oversize_len() {
        let req = FileRequest::Read {
            path: "x",
            offset: 0,
            len: FILE_READ_CHUNK_MAX + 1,
        };
        let mut buf = [0u8; 64];
        assert_eq!(req.encode(&mut buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn request_encode_rejects_small_buffer() {
        let mut empty: [u8; 0] = [];
        assert_eq!(
            FileRequest::List.encode(&mut empty),
            Err(Errno::BufferTooSmall)
        );
        let req = FileRequest::Read {
            path: "abc",
            offset: 0,
            len: 1,
        };
        let mut buf = [0u8; 4];
        assert_eq!(req.encode(&mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn request_decode_rejects_empty_and_unknown_opcode() {
        assert_eq!(FileRequest::decode(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(FileRequest::decode(&[0xFF]), Err(Errno::OutOfRange));
    }

    #[test]
    fn read_reply_round_trips_the_bytes() {
        let payload = b"BUNDLEBYTES";
        let mut buf = [0u8; 64];
        let n = encode_read_reply(&mut buf, payload).expect("encodes");
        assert_eq!(decode_read_reply(&buf[..n]), Ok(&payload[..]));
    }

    #[test]
    fn empty_read_reply_round_trips() {
        let mut buf = [0u8; 16];
        let n = encode_read_reply(&mut buf, &[]).expect("encodes");
        assert_eq!(decode_read_reply(&buf[..n]), Ok(&[][..]));
    }

    #[test]
    fn an_error_reply_surfaces_its_errno() {
        let mut buf = [0u8; 16];
        let n = encode_error_reply(&mut buf, Errno::NotFound).expect("encodes");
        assert_eq!(reply_status(&buf[..n]), Err(Errno::NotFound));
        assert_eq!(decode_read_reply(&buf[..n]), Err(Errno::NotFound));
        assert!(decode_list_reply(&buf[..n]).is_err());
    }

    #[test]
    fn list_reply_round_trips_paths() {
        let paths = ["/System/Drivers/input/kbd", "/System/Drivers/storage/blk"];
        let mut buf = [0u8; 256];
        let n = encode_list_reply(&mut buf, &paths).expect("encodes");
        let mut it = decode_list_reply(&buf[..n]).expect("ok frame");
        assert_eq!(it.next(), Some(Ok(paths[0])));
        assert_eq!(it.next(), Some(Ok(paths[1])));
        assert!(it.next().is_none());
    }

    #[test]
    fn empty_list_reply_round_trips() {
        let mut buf = [0u8; 16];
        let n = encode_list_reply(&mut buf, &[]).expect("encodes");
        let mut it = decode_list_reply(&buf[..n]).expect("ok frame");
        assert!(it.next().is_none());
    }

    #[test]
    fn list_reply_fails_closed_on_small_buffer_never_truncates() {
        let paths = ["aaaaaaaa", "bbbbbbbb"];
        let mut buf = [0u8; 12];
        assert_eq!(
            encode_list_reply(&mut buf, &paths),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn truncated_read_reply_fails_closed() {
        // Status ok + declared count 5 but no body.
        let mut buf = [0u8; REPLY_OK_HEADER_LEN];
        put_i32(&mut buf, 0, 0);
        put_u32(&mut buf, REPLY_STATUS_LEN, 5);
        assert_eq!(decode_read_reply(&buf), Err(Errno::BadMagic));
    }
}

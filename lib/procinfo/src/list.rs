//! The shared paged-list walk for sysinfo queries that return a homogeneous
//! sequence of fixed-size records.
//!
//! Several `sysinfo-v1` queries — the process list, the mount list — answer
//! with a run of fixed-[`WIRE_LEN`] records that the client pages through
//! with an `offset`/`limit` request. The paging loop is identical across
//! them: request a page, reject a structurally invalid reply, decode each
//! record, and stop on a short page. It lives here once rather than being
//! copied per query (`AGENTS.md` §2.2).
//!
//! [`WIRE_LEN`]: rustos_abi::sysinfo::ProcessRecord::WIRE_LEN

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::sysinfo::SysinfoQueryId;
use rustos_abi::Errno;

use crate::request::{call, CallError};
use crate::transport::Transport;

/// Decode an inline byte field for display, substituting `U+FFFD` for any
/// invalid byte rather than failing.
///
/// Shared by the process and mount row renderers so neither re-implements
/// lossy decoding (`AGENTS.md` §2.2); a display routine never panics on
/// hostile bytes (§2.9).
pub(crate) fn field_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Why a paged sysinfo list walk did not complete.
///
/// [`Call`](ListError::Call) carries a transport/capability failure
/// (including a structurally invalid reply, reported as [`Errno::BadMagic`]);
/// [`Sink`](ListError::Sink) carries the [`Errno`] a caller's per-record sink
/// raised (typically a terminal write). Distinguishing them lets a consuming
/// tool map each onto the right line of its own error enum (`AGENTS.md`
/// §2.2). The same type serves every paged walk, so the process and mount
/// tools share one error shape rather than each inventing one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListError {
    /// The query failed, was denied, or returned a structurally invalid
    /// reply.
    Call(CallError),
    /// The per-record sink raised an error (e.g. a failed terminal write).
    Sink(Errno),
}

/// Page through `query` and hand each record's raw bytes to `on_record`.
///
/// `record_len` is the fixed wire size of one record; `page` is the per-page
/// record count baked into each request by `make_request`. The walk **fails
/// closed** (`AGENTS.md` §5.4 / §2.9): a reply whose length is not a whole
/// number of `record_len` records, or one that would overflow the page
/// offset, is rejected rather than partially delivered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `on_record` returned an error; the walk stops at
///   that record.
pub(crate) fn walk_pages(
    transport: &dyn Transport,
    query: SysinfoQueryId,
    record_len: usize,
    page: u16,
    make_request: impl Fn(u32, u16) -> Vec<u8>,
    mut on_record: impl FnMut(&[u8]) -> Result<(), ListError>,
) -> Result<(), ListError> {
    let mut offset: u32 = 0;
    loop {
        let request = make_request(offset, page);
        let reply = call(transport, query, &request).map_err(ListError::Call)?;
        if reply.len() % record_len != 0 {
            return Err(ListError::Call(CallError::Service(Errno::BadMagic)));
        }
        let count = reply.len() / record_len;
        for chunk in reply.chunks_exact(record_len) {
            on_record(chunk)?;
        }
        if count < usize::from(page) {
            return Ok(());
        }
        let advanced = u32::try_from(count)
            .map_err(|_| ListError::Call(CallError::Service(Errno::LengthOutOfRange)))?;
        offset = offset
            .checked_add(advanced)
            .ok_or(ListError::Call(CallError::Service(Errno::LengthOutOfRange)))?;
    }
}

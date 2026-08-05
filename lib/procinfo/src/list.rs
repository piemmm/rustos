//! The shared paged-list walk for sysinfo queries that return a homogeneous
//! sequence of fixed-size records.
//!
//! Several `sysinfo-v1` queries — the process list, the mount list — answer
//! with a run of fixed-[`WIRE_LEN`] records that the client pages through
//! with an `offset`/`limit` request. The paging loop is identical across
//! them: request a page, reject a structurally invalid reply, decode each
//! record, and stop on a short page. It lives here once rather than being
//! copied per query.
//!
//! [`WIRE_LEN`]: tairix_abi::sysinfo::ProcessRecord::WIRE_LEN

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::sysinfo::SysinfoQueryId;
use tairix_abi::Errno;

use crate::request::{call, CallError};
use crate::transport::Transport;

/// Decode an inline byte field for display, substituting `U+FFFD` for any
/// invalid byte rather than failing.
///
/// Shared by the process and mount row renderers (and consumers such as
/// `top`'s own row layout) so none re-implements lossy decoding; a display
/// routine never panics on hostile bytes.
#[must_use]
pub fn field_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Why a paged sysinfo list walk did not complete.
///
/// [`Call`](ListError::Call) carries a transport/capability failure
/// (including a structurally invalid reply, reported as [`Errno::BadMagic`]);
/// [`Sink`](ListError::Sink) carries the [`Errno`] a caller's per-record sink
/// raised (typically a terminal write). Distinguishing them lets a consuming
/// tool map each onto the right line of its own error enum. The same type serves every paged walk, so the process and mount
/// tools share one error shape rather than each inventing one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListError {
    /// The query failed, was denied, or returned a structurally invalid
    /// reply.
    Call(CallError),
    /// The per-record sink raised an error (e.g. a failed terminal write).
    Sink(Errno),
}

/// Whether a paged walk should carry on past the record just delivered.
///
/// A sink that has taken all the records its caller can hold — a periodic
/// sampler bounding how much of a huge or hostile reply it will
/// accumulate — answers [`Stop`](WalkStep::Stop) and the walk ends
/// successfully, without asking for a further page. Ending early is
/// therefore an ordinary `Ok` outcome the caller chose, while a genuine
/// failure is the only thing that comes back as [`Err`]: a deliberate
/// truncation can never be mistaken for a broken service, and no caller
/// has to smuggle "I have enough" through an error it then has to catch
/// back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkStep {
    /// Deliver the next record.
    Continue,
    /// End the walk here; no further record is delivered and no further
    /// page is requested.
    Stop,
}

/// Page through `query` and hand each record's raw bytes to `on_record`,
/// until the list is exhausted or `on_record` answers
/// [`WalkStep::Stop`].
///
/// `record_len` is the fixed wire size of one record; `page` is the per-page
/// record count baked into each request by `make_request`. The walk **fails
/// closed**: a reply whose length is not a whole
/// number of `record_len` records, one that would overflow the page
/// offset, or a `record_len` of zero, is rejected rather than partially
/// delivered.
///
/// Public so a consumer with its own bounding or cadence policy (for
/// example a periodic sampler that must cap how many records a single walk
/// may accumulate) can drive the same paging loop directly instead of
/// re-implementing it; every query-specific `for_each_*` walk in this crate
/// is built on it. Such a consumer bounds itself by answering
/// [`WalkStep::Stop`], which ends the walk as a success, so its own
/// truncation stays distinguishable from a service that failed.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `on_record` returned an error; the walk stops at
///   that record.
pub fn walk_pages(
    transport: &dyn Transport,
    query: SysinfoQueryId,
    record_len: usize,
    page: u16,
    make_request: impl Fn(u32, u16) -> Vec<u8>,
    mut on_record: impl FnMut(&[u8]) -> Result<WalkStep, ListError>,
) -> Result<(), ListError> {
    if record_len == 0 {
        return Err(ListError::Call(CallError::Service(Errno::LengthOutOfRange)));
    }
    let mut offset: u32 = 0;
    loop {
        let request = make_request(offset, page);
        let reply = call(transport, query, &request).map_err(ListError::Call)?;
        if reply.len() % record_len != 0 {
            return Err(ListError::Call(CallError::Service(Errno::BadMagic)));
        }
        let count = reply.len() / record_len;
        for chunk in reply.chunks_exact(record_len) {
            if on_record(chunk)? == WalkStep::Stop {
                return Ok(());
            }
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

#[cfg(test)]
mod tests {
    use super::{walk_pages, ListError, WalkStep};
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{SysinfoQueryId, SysinfoRequestHeader};
    use tairix_abi::Errno;

    /// One record per byte value, so a page's worth of records is trivially
    /// countable and the paging arithmetic is what is under test.
    const RECORD_LEN: usize = 1;
    const PAGE: u16 = 4;

    /// A stand-in that always answers a full page, so the walk only ever
    /// ends because the sink says so — never because the list ran out.
    struct Endless {
        pages: RefCell<usize>,
    }

    impl Transport for Endless {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            SysinfoRequestHeader::from_bytes(request)?;
            *self.pages.borrow_mut() += 1;
            Ok(alloc::vec![7u8; usize::from(PAGE) * RECORD_LEN])
        }
    }

    fn walk(
        transport: &dyn Transport,
        record_len: usize,
        on_record: impl FnMut(&[u8]) -> Result<WalkStep, ListError>,
    ) -> Result<(), ListError> {
        walk_pages(
            transport,
            SysinfoQueryId::MOUNT_LIST,
            record_len,
            PAGE,
            |_, _| Vec::new(),
            on_record,
        )
    }

    #[test]
    fn a_stopping_sink_ends_the_walk_successfully_mid_page() {
        let transport = Endless {
            pages: RefCell::new(0),
        };
        let seen = RefCell::new(0usize);
        let result = walk(&transport, RECORD_LEN, |_| {
            *seen.borrow_mut() += 1;
            if *seen.borrow() == 2 {
                Ok(WalkStep::Stop)
            } else {
                Ok(WalkStep::Continue)
            }
        });
        // Ending early is the caller's own decision, so it is `Ok`: only a
        // real failure is an `Err`.
        assert_eq!(result, Ok(()));
        assert_eq!(*seen.borrow(), 2);
        // The rest of the page is not delivered and no further page is
        // requested.
        assert_eq!(*transport.pages.borrow(), 1);
    }

    #[test]
    fn a_stopping_sink_and_a_failing_sink_are_different_outcomes() {
        let transport = Endless {
            pages: RefCell::new(0),
        };
        assert_eq!(walk(&transport, RECORD_LEN, |_| Ok(WalkStep::Stop)), Ok(()));
        assert_eq!(
            walk(&transport, RECORD_LEN, |_| Err(ListError::Sink(
                Errno::NotFound
            ))),
            Err(ListError::Sink(Errno::NotFound))
        );
    }

    #[test]
    fn a_continuing_sink_keeps_paging_a_full_page() {
        let transport = Endless {
            pages: RefCell::new(0),
        };
        let seen = RefCell::new(0usize);
        let result = walk(&transport, RECORD_LEN, |_| {
            *seen.borrow_mut() += 1;
            if *seen.borrow() > usize::from(PAGE) {
                Ok(WalkStep::Stop)
            } else {
                Ok(WalkStep::Continue)
            }
        });
        assert_eq!(result, Ok(()));
        // The first record of the second page is what stopped it, so the
        // walk did request that second page.
        assert_eq!(*transport.pages.borrow(), 2);
    }

    #[test]
    fn a_zero_record_length_is_refused_rather_than_dividing_by_zero() {
        let transport = Endless {
            pages: RefCell::new(0),
        };
        assert_eq!(
            walk(&transport, 0, |_| Ok(WalkStep::Continue)),
            Err(ListError::Call(CallError::Service(Errno::LengthOutOfRange)))
        );
        // Refused before any query was issued.
        assert_eq!(*transport.pages.borrow(), 0);
    }
}

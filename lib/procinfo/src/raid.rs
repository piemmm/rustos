//! The shared RAID array and member paging walks.
//!
//! The array composer is a user-space service, so its reports reach a tool
//! the same way every other live figure does: through the System Information
//! API, which fronts the composer's own control endpoint and gates the read
//! on the caller's hardware-read authority. A tool therefore never calls the
//! composer directly and never reads a `/proc`-style file.
//!
//! Both walks are the generic [`walk_pages`](crate::list) loop the process
//! and mount lists use, so only the per-record decode lives here.

use alloc::vec::Vec;

use tairix_abi::raid_admin::{RaidArrayRecord, RaidMemberRecord, RAID_LIST_LIMIT_MAX};
use tairix_abi::sysinfo::{RaidListRequest, SysinfoQueryId};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError};
use crate::request::CallError;
use crate::transport::Transport;

/// Records requested per RAID list page.
///
/// The composer's own reply-frame bound caps how many records one call may
/// carry, so a page is exactly that bound and a longer list is walked by
/// paging rather than by asking for a frame the composer would refuse.
pub const RAID_PAGE: u16 = RAID_LIST_LIMIT_MAX;

/// Page through the composed arrays and hand each decoded
/// [`RaidArrayRecord`] to `sink`.
///
/// The query is [`SysinfoQueryId::RAID_ARRAYS`], which the service serves
/// only to a holder of the hardware-read capability — an array report says
/// which storage devices exist and how they are composed, so it is read under
/// the same authority as the hardware tree itself.
///
/// The walk **fails closed**: a reply whose length is not a whole number of
/// [`RaidArrayRecord::WIRE_LEN`] records, or a record carrying an unknown
/// level, health, or reserved bit, is rejected rather than partially
/// rendered. A machine with no running composer surfaces the transport's own
/// error, never an empty list — "no arrays" and "nothing answered" are
/// different answers and a caller must be able to tell them apart.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, no composer answered, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_raid_array(
    transport: &dyn Transport,
    mut sink: impl FnMut(&RaidArrayRecord) -> Result<(), Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::RAID_ARRAYS,
        RaidArrayRecord::WIRE_LEN,
        RAID_PAGE,
        page_request,
        |chunk| {
            let record = RaidArrayRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Page through the devices the composer holds and hand each decoded
/// [`RaidMemberRecord`] to `sink`.
///
/// The query is [`SysinfoQueryId::RAID_MEMBERS`], gated on the same
/// hardware-read authority as [`for_each_raid_array`]. The list covers every
/// device the composer holds — the members of live arrays, devices held for
/// an array that is not assembled, and unaffiliated candidates — so a caller
/// sees both what is composed and what could be.
///
/// The walk **fails closed** exactly as [`for_each_raid_array`] does: a
/// mis-framed reply or a record with an unknown disposition is rejected, and
/// a machine with no running composer surfaces the transport's own error
/// rather than an empty list.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, no composer answered, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_raid_member(
    transport: &dyn Transport,
    mut sink: impl FnMut(&RaidMemberRecord) -> Result<(), Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::RAID_MEMBERS,
        RaidMemberRecord::WIRE_LEN,
        RAID_PAGE,
        page_request,
        |chunk| {
            let record = RaidMemberRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Encode one page request; both RAID lists take the identical payload, so
/// the two walks share this encoder.
fn page_request(offset: u32, limit: u16) -> Vec<u8> {
    RaidListRequest {
        offset,
        limit,
        flags: 0,
    }
    .to_le_bytes()
    .to_vec()
}

/// Flatten a walk failure onto the frozen [`Errno`] vocabulary.
///
/// The `Vec`-returning fetches below answer with an [`Errno`] because their
/// callers are whole command apps that report a single diagnosis; the
/// distinction [`CallError`] draws is only useful to a caller that renders a
/// different line per case, which the paging walks above still expose.
#[cfg(all(freestanding, feature = "program"))]
fn flatten(error: ListError) -> Errno {
    match error {
        ListError::Call(CallError::PermissionDenied) => Errno::PermissionDenied,
        ListError::Call(CallError::Service(errno)) | ListError::Sink(errno) => errno,
    }
}

/// Read every composed array through the production transport.
///
/// The whole list is collected by paging [`for_each_raid_array`] until a
/// short page ends it, so a caller that wants the table rather than a
/// streaming walk does not re-implement the paging loop.
///
/// # Errors
///
/// The flattened [`Errno`]: [`Errno::PermissionDenied`] when the caller does
/// not hold the hardware-read capability the query requires, or the
/// transport's own error when no composer answered or the reply was
/// structurally invalid. It is never an empty list standing in for a
/// failure.
#[cfg(all(freestanding, feature = "program"))]
pub fn raid_arrays() -> Result<Vec<RaidArrayRecord>, Errno> {
    let mut records = Vec::new();
    for_each_raid_array(&crate::client::IpcTransport, |record| {
        records.push(*record);
        Ok(())
    })
    .map_err(flatten)?;
    Ok(records)
}

/// Read every device the composer holds through the production transport.
///
/// The whole list is collected by paging [`for_each_raid_member`] until a
/// short page ends it, the sibling of [`raid_arrays`].
///
/// # Errors
///
/// The flattened [`Errno`]: [`Errno::PermissionDenied`] when the caller does
/// not hold the hardware-read capability the query requires, or the
/// transport's own error when no composer answered or the reply was
/// structurally invalid. It is never an empty list standing in for a
/// failure.
#[cfg(all(freestanding, feature = "program"))]
pub fn raid_members() -> Result<Vec<RaidMemberRecord>, Errno> {
    let mut records = Vec::new();
    for_each_raid_member(&crate::client::IpcTransport, |record| {
        records.push(*record);
        Ok(())
    })
    .map_err(flatten)?;
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::{for_each_raid_array, for_each_raid_member, RAID_PAGE};
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::raid::{ArrayHealth, RaidLevel};
    use tairix_abi::raid_admin::{
        RaidArrayRecord, RaidMemberDisposition, RaidMemberRecord, RAID_SLOT_NONE,
    };
    use tairix_abi::sysinfo::{RaidListRequest, SysinfoQueryId, SysinfoRequestHeader};
    use tairix_abi::Errno;

    /// An in-memory `sysinfod` stand-in answering both RAID list queries from
    /// fixed record sets, decoding each request exactly as the real service.
    struct Fixture {
        arrays: Vec<RaidArrayRecord>,
        members: Vec<RaidMemberRecord>,
        malformed: bool,
        denied: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(arrays: Vec<RaidArrayRecord>, members: Vec<RaidMemberRecord>) -> Self {
            Self {
                arrays,
                members,
                malformed: false,
                denied: false,
                seen: RefCell::new(Vec::new()),
            }
        }

        /// Serve the window `payload` selects out of `records`, each encoded
        /// with `encode`.
        fn page<T: Copy>(
            payload: &[u8],
            records: &[T],
            encode: impl Fn(&T) -> Vec<u8>,
        ) -> Result<Vec<u8>, Errno> {
            let request = RaidListRequest::from_bytes(payload)?;
            let offset = request.offset as usize;
            if offset >= records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(records.len() - offset, request.limit as usize);
            let mut out = Vec::new();
            for record in &records[offset..offset + take] {
                out.extend_from_slice(&encode(record));
            }
            Ok(out)
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.denied {
                return Err(Errno::PermissionDenied);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            if header.query == SysinfoQueryId::RAID_ARRAYS {
                if self.malformed {
                    return Ok(alloc::vec![0u8; RaidArrayRecord::WIRE_LEN + 1]);
                }
                Self::page(payload, &self.arrays, |r| r.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::RAID_MEMBERS {
                if self.malformed {
                    return Ok(alloc::vec![0u8; RaidMemberRecord::WIRE_LEN + 1]);
                }
                Self::page(payload, &self.members, |r| r.to_le_bytes().to_vec())
            } else {
                Err(Errno::NotImplemented)
            }
        }
    }

    fn array(tag: u8) -> RaidArrayRecord {
        RaidArrayRecord::new(
            [tag; 16],
            RaidLevel::Parity,
            ArrayHealth::Optimal,
            0,
            3,
            3,
            4096,
            128,
            2_000_000,
            0x5241_2000 + u64::from(tag),
            u32::from(tag),
            2_000_000,
            2_000_000,
            4,
        )
    }

    fn member(slot: u16, disposition: RaidMemberDisposition) -> RaidMemberRecord {
        let affiliation = if disposition == RaidMemberDisposition::Candidate {
            [0u8; 16]
        } else {
            [0x22; 16]
        };
        RaidMemberRecord::new(
            affiliation,
            disposition,
            slot,
            u32::from(slot) + 50,
            0x5241_3000 + u64::from(slot),
            1_000_000,
            4096,
            4,
        )
    }

    fn collect_arrays(fixture: &Fixture) -> Result<Vec<RaidArrayRecord>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_raid_array(fixture, |record| {
            seen.borrow_mut().push(*record);
            Ok(())
        })?;
        Ok(seen.into_inner())
    }

    fn collect_members(fixture: &Fixture) -> Result<Vec<RaidMemberRecord>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_raid_member(fixture, |record| {
            seen.borrow_mut().push(*record);
            Ok(())
        })?;
        Ok(seen.into_inner())
    }

    #[test]
    fn the_array_walk_routes_its_query_and_yields_records() {
        let fixture = Fixture::new(alloc::vec![array(1), array(2)], Vec::new());
        let got = collect_arrays(&fixture).expect("walk");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].array(), [1u8; 16]);
        assert_eq!(got[1].array(), [2u8; 16]);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::RAID_ARRAYS]
        );
    }

    #[test]
    fn the_member_walk_routes_its_query_and_preserves_disposition() {
        let fixture = Fixture::new(
            Vec::new(),
            alloc::vec![
                member(0, RaidMemberDisposition::InSync),
                member(RAID_SLOT_NONE, RaidMemberDisposition::Candidate),
            ],
        );
        let got = collect_members(&fixture).expect("walk");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].disposition(), RaidMemberDisposition::InSync);
        assert!(!got[0].is_unaffiliated());
        assert_eq!(got[1].slot(), RAID_SLOT_NONE);
        assert!(got[1].is_unaffiliated());
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::RAID_MEMBERS]
        );
    }

    #[test]
    fn both_walks_page_until_a_short_page() {
        let arrays = (0..=RAID_PAGE)
            .map(|i| array(u8::try_from(i).expect("a page bound fits a byte tag")))
            .collect();
        let members = (0..=RAID_PAGE)
            .map(|i| member(i, RaidMemberDisposition::InSync))
            .collect();
        let fixture = Fixture::new(arrays, members);

        let got = collect_arrays(&fixture).expect("walk arrays");
        assert_eq!(got.len(), usize::from(RAID_PAGE) + 1);
        assert_eq!(fixture.seen.borrow().len(), 2);

        let got = collect_members(&fixture).expect("walk members");
        assert_eq!(got.len(), usize::from(RAID_PAGE) + 1);
        assert_eq!(fixture.seen.borrow().len(), 4);
    }

    #[test]
    fn an_empty_table_yields_nothing() {
        let fixture = Fixture::new(Vec::new(), Vec::new());
        assert!(collect_arrays(&fixture).expect("walk").is_empty());
        assert!(collect_members(&fixture).expect("walk").is_empty());
    }

    #[test]
    fn a_malformed_page_fails_closed() {
        let mut fixture = Fixture::new(
            alloc::vec![array(1)],
            alloc::vec![member(0, RaidMemberDisposition::InSync)],
        );
        fixture.malformed = true;
        assert_eq!(
            collect_arrays(&fixture),
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
        assert_eq!(
            collect_members(&fixture),
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
    }

    #[test]
    fn an_unknown_discriminant_in_a_page_fails_closed() {
        // A whole-record-length page whose bytes are not a record the frozen
        // vocabulary defines: a zero level is no level, so it is refused
        // rather than rendered as some default array.
        struct Bogus(usize);
        impl Transport for Bogus {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; self.0])
            }
        }
        assert_eq!(
            for_each_raid_array(&Bogus(RaidArrayRecord::WIRE_LEN), |_| Ok(())),
            Err(ListError::Call(CallError::Service(Errno::OutOfRange)))
        );
    }

    #[test]
    fn a_denied_query_surfaces_the_refusal_not_an_empty_list() {
        let mut fixture = Fixture::new(alloc::vec![array(1)], Vec::new());
        fixture.denied = true;
        assert_eq!(
            collect_arrays(&fixture),
            Err(ListError::Call(CallError::PermissionDenied))
        );
        assert_eq!(
            collect_members(&fixture),
            Err(ListError::Call(CallError::PermissionDenied))
        );
    }

    #[test]
    fn a_sink_error_stops_the_walk() {
        let fixture = Fixture::new(
            alloc::vec![array(1), array(2)],
            alloc::vec![
                member(0, RaidMemberDisposition::InSync),
                member(1, RaidMemberDisposition::InSync),
            ],
        );
        let count = RefCell::new(0usize);
        let result = for_each_raid_array(&fixture, |_| {
            *count.borrow_mut() += 1;
            Err(Errno::NotFound)
        });
        assert_eq!(result, Err(ListError::Sink(Errno::NotFound)));
        assert_eq!(*count.borrow(), 1);

        let count = RefCell::new(0usize);
        let result = for_each_raid_member(&fixture, |_| {
            *count.borrow_mut() += 1;
            Err(Errno::NotFound)
        });
        assert_eq!(result, Err(ListError::Sink(Errno::NotFound)));
        assert_eq!(*count.borrow(), 1);
    }
}

//! The shared account-directory walk.
//!
//! The `USER_DIRECTORY` query serves the uid + username pairing (and
//! nothing else — no credential material) so a `top`/`ls -l`-style display
//! can render account names. The paged walk mirrors
//! [`crate::for_each_process`] and lives here once rather than being
//! copied into each consumer.

use alloc::vec::Vec;

use tairix_abi::sysinfo::{SysinfoQueryId, UserDirectoryRecord, UserDirectoryRequest};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError};
use crate::request::CallError;
use crate::transport::Transport;

/// Number of [`UserDirectoryRecord`]s requested per directory page.
///
/// A page bounds the reply size so the transport never has to carry every
/// account at once; [`for_each_user`] walks pages until a short page ends
/// the directory.
pub const USER_DIRECTORY_PAGE: u16 = 64;

/// Page through the account directory and hand each decoded
/// [`UserDirectoryRecord`] to `sink`.
///
/// The query is ungated (the directory is the `/etc/passwd`-class public
/// uid + username pairing); a system whose user database is not loaded
/// yields an empty directory. Records are delivered in the order the
/// service returns them. The walk **fails closed**: a reply whose length
/// is not a whole number of [`UserDirectoryRecord::WIRE_LEN`] records is
/// rejected rather than partially decoded.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed or the reply was
///   structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_user(
    transport: &dyn Transport,
    mut sink: impl FnMut(&UserDirectoryRecord) -> Result<(), Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::USER_DIRECTORY,
        UserDirectoryRecord::WIRE_LEN,
        USER_DIRECTORY_PAGE,
        |offset, limit| {
            UserDirectoryRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = UserDirectoryRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Collect the whole account directory into an owned `(uid, name)` list,
/// decoding each name lossily for display.
///
/// The convenience form both `top`'s `USER` column and any future
/// `ls -l`-style owner column consume: a transport failure yields an empty
/// map (the callers degrade to numeric uids — the honest answer), never a
/// fabricated name.
#[must_use]
pub fn user_names(transport: &dyn Transport) -> Vec<(u32, alloc::string::String)> {
    let mut names = Vec::new();
    let walked = for_each_user(transport, |record| {
        names.push((record.uid, crate::list::field_lossy(record.name_bytes())));
        Ok(())
    });
    if walked.is_err() {
        return Vec::new();
    }
    names
}

#[cfg(test)]
mod tests {
    use super::{for_each_user, user_names, USER_DIRECTORY_PAGE};
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{
        SysinfoQueryId, SysinfoRequestHeader, UserDirectoryRecord, UserDirectoryRequest,
    };
    use tairix_abi::Errno;

    /// An in-memory `sysinfod` stand-in answering directory queries from a
    /// fixed record set, decoding the request exactly as the real service.
    struct Fixture {
        records: Vec<UserDirectoryRecord>,
        malformed: bool,
        seen: RefCell<usize>,
    }

    impl Fixture {
        fn new(records: Vec<UserDirectoryRecord>) -> Self {
            Self {
                records,
                malformed: false,
                seen: RefCell::new(0),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            assert_eq!(header.query, SysinfoQueryId::USER_DIRECTORY);
            *self.seen.borrow_mut() += 1;
            if self.malformed {
                return Ok(alloc::vec![0u8; UserDirectoryRecord::WIRE_LEN + 1]);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = UserDirectoryRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * UserDirectoryRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    fn record(uid: u32, name: &[u8]) -> UserDirectoryRecord {
        UserDirectoryRecord::new(uid, name).expect("record")
    }

    #[test]
    fn walk_yields_every_record_and_pages_until_short() {
        let mut records = Vec::new();
        for uid in 0..=u32::from(USER_DIRECTORY_PAGE) {
            records.push(record(uid, b"u"));
        }
        let fixture = Fixture::new(records);
        let seen = RefCell::new(0usize);
        for_each_user(&fixture, |_| {
            *seen.borrow_mut() += 1;
            Ok(())
        })
        .expect("ok");
        assert_eq!(*seen.borrow(), usize::from(USER_DIRECTORY_PAGE) + 1);
        // A full page plus a short page: two requests.
        assert_eq!(*fixture.seen.borrow(), 2);
    }

    #[test]
    fn malformed_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(0, b"root")]);
        fixture.malformed = true;
        let outcome = for_each_user(&fixture, |_| Ok(()));
        assert_eq!(
            outcome,
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
    }

    /// A transport that always fails, standing in for a broken service.
    struct Failing;

    impl Transport for Failing {
        fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
            Err(Errno::NotFound)
        }
    }

    #[test]
    fn user_names_collects_pairs_and_degrades_to_empty_on_failure() {
        let fixture = Fixture::new(alloc::vec![record(0, b"root"), record(1000, b"alice")]);
        let names = user_names(&fixture);
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0, 0);
        assert_eq!(names[0].1, "root");
        assert_eq!(names[1].1, "alice");

        assert!(user_names(&Failing).is_empty());
    }
}

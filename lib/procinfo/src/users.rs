//! The shared account- and group-directory walks, and the caller's own
//! account record.
//!
//! The `USER_DIRECTORY` and `GROUP_DIRECTORY` queries serve the uid +
//! username and gid + group-name pairings (and nothing else — no
//! credential material, no membership, no grant) so a `top`/`ls -l`-style
//! display can render an id as a name. The paged walks mirror
//! [`crate::for_each_process`] and live here once rather than being copied
//! into each consumer.
//!
//! [`self_account`] is the third read: the caller's own record, resolved
//! by the service against the uid the kernel attested, so it names no
//! account and can reach no other principal's.

use alloc::vec::Vec;

use tairix_abi::sysinfo::{
    GroupDirectoryRecord, GroupDirectoryRequest, SelfAccountRecord, SysinfoQueryId,
    UserDirectoryRecord, UserDirectoryRequest,
};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError, WalkStep};
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
/// service returns them.
///
/// `sink` answers [`WalkStep::Continue`] to be given the next record or
/// [`WalkStep::Stop`] to end the walk there, which is how a caller bounds
/// how much of a long or hostile directory it will accept. Stopping is an
/// ordinary success, so it stays distinguishable from a failure.
///
/// The walk **fails closed**: a reply whose length
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
    mut sink: impl FnMut(&UserDirectoryRecord) -> Result<WalkStep, Errno>,
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

/// The account name `uid` holds in the directory, or `None` when the
/// directory holds no entry for it.
///
/// The one definition of "which account is this uid?", so a tool resolving a
/// single identity re-derives neither the directory walk, the duplicate-uid
/// rule, nor the lossy name decode. The walk ends at the first matching
/// record rather than paging the rest of a directory whose answer is
/// already known, so a uid the directory lists twice keeps its first name
/// and the answer is deterministic whatever order the service returns
/// records in.
///
/// A failure is reported rather than turned into a missing name: a caller
/// that cannot reach the directory must not conclude the account does not
/// exist.
///
/// # Errors
///
/// [`ListError`] when the transport failed or a reply did not decode.
pub fn user_name(
    transport: &dyn Transport,
    uid: u32,
) -> Result<Option<alloc::string::String>, ListError> {
    let mut name = None;
    for_each_user(transport, |record| {
        if record.uid != uid {
            return Ok(WalkStep::Continue);
        }
        name = Some(crate::list::field_lossy(record.name_bytes()));
        Ok(WalkStep::Stop)
    })?;
    Ok(name)
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
        Ok(WalkStep::Continue)
    });
    if walked.is_err() {
        return Vec::new();
    }
    names
}

/// Number of [`GroupDirectoryRecord`]s requested per directory page.
///
/// The group sibling of [`USER_DIRECTORY_PAGE`], and the same figure: the
/// two records are the same size and the two directories are walked the
/// same way, so one page size serves both.
pub const GROUP_DIRECTORY_PAGE: u16 = USER_DIRECTORY_PAGE;

/// Page through the group directory and hand each decoded
/// [`GroupDirectoryRecord`] to `sink`.
///
/// The group counterpart of [`for_each_user`], on identical terms: the
/// query is ungated (a gid-to-name pairing is public), a system whose
/// group registry is not loaded yields the compiled-in system groups
/// alone, and the walk fails closed on a reply that is not a whole number
/// of records.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed or the reply was
///   structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_group(
    transport: &dyn Transport,
    mut sink: impl FnMut(&GroupDirectoryRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::GROUP_DIRECTORY,
        GroupDirectoryRecord::WIRE_LEN,
        GROUP_DIRECTORY_PAGE,
        |offset, limit| {
            GroupDirectoryRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = GroupDirectoryRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Collect the whole group directory into an owned `(gid, name)` list,
/// decoding each name lossily for display.
///
/// The group form of [`user_names`], and degrades the same way: a
/// transport failure yields an empty map rather than a fabricated name, so
/// a caller renders numeric gids — the honest answer.
#[must_use]
pub fn group_names(transport: &dyn Transport) -> Vec<(u32, alloc::string::String)> {
    let mut names = Vec::new();
    let walked = for_each_group(transport, |record| {
        names.push((record.gid, crate::list::field_lossy(record.name_bytes())));
        Ok(WalkStep::Continue)
    });
    if walked.is_err() {
        return Vec::new();
    }
    names
}

/// Read the caller's **own** account record: its name, display name, home,
/// shell, primary group, and memberships.
///
/// Ungated and self-scoped — the service resolves the record against the
/// uid the kernel attested on the request, never one this call names, so
/// there is no parameter for whose account to read. `Ok(None)` is an
/// account no database holds, which a caller renders as unknown rather
/// than as a failure.
///
/// The record carries no capability grant ceiling, no account state, and
/// no password material: reading another account's fields is the
/// `CAP_USER_ADMIN` listing's job and has no path here.
///
/// # Errors
///
/// [`CallError`] when the transport failed, and
/// [`CallError::Service`] carrying [`Errno::BadMagic`] when the reply was
/// not a whole record (fail closed — never a partial decode).
pub fn self_account(transport: &dyn Transport) -> Result<Option<SelfAccountRecord>, CallError> {
    let reply = crate::request::call(transport, SysinfoQueryId::SELF_ACCOUNT, &[])?;
    if reply.is_empty() {
        return Ok(None);
    }
    SelfAccountRecord::from_bytes(&reply)
        .map(Some)
        .map_err(|_| CallError::Service(Errno::BadMagic))
}

#[cfg(test)]
mod tests {
    use super::{
        for_each_group, for_each_user, group_names, self_account, user_name, user_names, WalkStep,
        GROUP_DIRECTORY_PAGE, USER_DIRECTORY_PAGE,
    };
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{
        GroupDirectoryRecord, GroupDirectoryRequest, SelfAccountRecord, SysinfoQueryId,
        SysinfoRequestHeader, UserDirectoryRecord, UserDirectoryRequest,
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
            Ok(WalkStep::Continue)
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
        let outcome = for_each_user(&fixture, |_| Ok(WalkStep::Continue));
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

    #[test]
    fn user_name_resolves_listed_uid_and_reports_absent_and_failure() {
        let fixture = Fixture::new(alloc::vec![record(0, b"root"), record(1000, b"alice")]);
        assert_eq!(
            user_name(&fixture, 1000).expect("ok").as_deref(),
            Some("alice")
        );
        assert_eq!(user_name(&fixture, 0).expect("ok").as_deref(), Some("root"));
        assert_eq!(user_name(&fixture, 4242).expect("ok"), None);
        assert_eq!(
            user_name(&Failing, 0),
            Err(ListError::Call(CallError::Service(Errno::NotFound)))
        );
    }

    /// An in-memory `sysinfod` stand-in answering the group directory and
    /// the self-account read from fixed state.
    struct GroupFixture {
        groups: Vec<GroupDirectoryRecord>,
        account: Option<SelfAccountRecord>,
    }

    impl Transport for GroupFixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            if header.query == SysinfoQueryId::SELF_ACCOUNT {
                return Ok(self
                    .account
                    .map(|record| record.to_le_bytes().to_vec())
                    .unwrap_or_default());
            }
            assert_eq!(header.query, SysinfoQueryId::GROUP_DIRECTORY);
            let req = GroupDirectoryRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.groups.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.groups.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * GroupDirectoryRecord::WIRE_LEN);
            for record in &self.groups[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    fn group(gid: u32, name: &[u8]) -> GroupDirectoryRecord {
        GroupDirectoryRecord::new(gid, name).expect("record")
    }

    fn fixture_account() -> SelfAccountRecord {
        SelfAccountRecord::new(
            1000,
            1000,
            &[100],
            tairix_abi::sysinfo::SelfAccountText {
                name: b"alice",
                display_name: b"Alice Liddell",
                home: b"/Users/alice",
                shell: b"/System/Commands/elsh.app/Run",
            },
        )
        .expect("record")
    }

    #[test]
    fn group_walk_pages_until_short_and_collects_pairs() {
        let mut groups = Vec::new();
        for gid in 0..=u32::from(GROUP_DIRECTORY_PAGE) {
            groups.push(group(gid, b"g"));
        }
        let fixture = GroupFixture {
            groups,
            account: None,
        };
        let seen = RefCell::new(0usize);
        for_each_group(&fixture, |_| {
            *seen.borrow_mut() += 1;
            Ok(WalkStep::Continue)
        })
        .expect("ok");
        assert_eq!(*seen.borrow(), usize::from(GROUP_DIRECTORY_PAGE) + 1);

        let fixture = GroupFixture {
            groups: alloc::vec![group(0, b"system"), group(100, b"storage")],
            account: None,
        };
        let names = group_names(&fixture);
        assert_eq!(names.len(), 2);
        assert_eq!(names[1], (100, alloc::string::String::from("storage")));
        // A transport failure degrades to numeric gids, never a fabricated
        // name.
        assert!(group_names(&Failing).is_empty());
    }

    #[test]
    fn the_self_account_read_answers_a_record_an_absence_and_a_failure_apart() {
        let fixture = GroupFixture {
            groups: Vec::new(),
            account: Some(fixture_account()),
        };
        let record = self_account(&fixture).expect("ok").expect("a record");
        assert_eq!(record.name_bytes(), b"alice");
        assert_eq!(record.supplementary_gids(), &[100]);

        // No record is `None`, distinguishable from a failure.
        let empty = GroupFixture {
            groups: Vec::new(),
            account: None,
        };
        assert_eq!(self_account(&empty).expect("ok"), None);
        assert_eq!(
            self_account(&Failing),
            Err(CallError::Service(Errno::NotFound))
        );
    }

    #[test]
    fn a_truncated_self_account_reply_fails_closed() {
        /// A service whose reply is one byte short of a whole record.
        struct Truncating;
        impl Transport for Truncating {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; SelfAccountRecord::WIRE_LEN - 1])
            }
        }
        assert_eq!(
            self_account(&Truncating),
            Err(CallError::Service(Errno::BadMagic))
        );
    }

    #[test]
    fn user_name_keeps_the_first_of_a_duplicated_uid() {
        let fixture = Fixture::new(alloc::vec![record(7, b"first"), record(7, b"second")]);
        assert_eq!(
            user_name(&fixture, 7).expect("ok").as_deref(),
            Some("first")
        );
    }
}

//! The production [`GroupDb`]: the `users_admin` client that lists the
//! group registry and submits the new group record.
//!
//! [`GroupsAdminDb`] holds the whole client policy — the request encoding,
//! the reply decoding, and the gid auto-allocation — behind one injected
//! transport seam, so every decision is host-tested and the freestanding
//! `Run` binary adds only the raw syscall. The auto-allocated gid comes
//! from the one `lib/users` policy definition
//! ([`tairix_users::next_id`], interactive-user range), never a private
//! copy.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::users_admin::{decode_group_list, UsersAdminRequest, USERS_ADMIN_MAX_REQUEST};
use tairix_abi::Errno;
use tairix_users::{next_id, IdRange, MAX_GROUPS_DB_LEN};

use crate::io::{GroupDb, GroupSpec};

/// Byte capacity for a `ListGroups` reply: comfortably above the largest
/// registry the kernel serialises (the same headroom the `users` tool's
/// session uses for its list replies).
const RESPONSE_CAPACITY: usize = 2 * MAX_GROUPS_DB_LEN;

/// The transport that carries one encoded `users_admin` request and
/// returns the response bytes written. On a running system this is the
/// `users_admin` syscall; in tests an in-memory registry. Every
/// authorisation decision stays on the far side of this seam.
pub trait AdminChannel {
    /// Submit `req`, writing any response into `out`.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the registry raises — e.g.
    /// [`Errno::PermissionDenied`] for a caller without `CAP_USER_ADMIN`.
    fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno>;
}

/// The production [`GroupDb`] over an [`AdminChannel`].
pub struct GroupsAdminDb<'a> {
    channel: &'a dyn AdminChannel,
}

impl<'a> GroupsAdminDb<'a> {
    /// A client over `channel`.
    #[must_use]
    pub fn new(channel: &'a dyn AdminChannel) -> Self {
        Self { channel }
    }

    /// Every group's `(name, gid)`, from a `ListGroups` round trip.
    fn list(&self) -> Result<Vec<(String, u32)>, Errno> {
        let mut req = [0u8; USERS_ADMIN_MAX_REQUEST];
        let len = UsersAdminRequest::ListGroups.encode_into(&mut req)?;
        let mut out = alloc::vec![0u8; RESPONSE_CAPACITY];
        let used = self.channel.call(&req[..len], &mut out)?;
        let bytes = out.get(..used).ok_or(Errno::LengthOutOfRange)?;
        let mut groups = Vec::new();
        for entry in decode_group_list(bytes)? {
            let entry = entry?;
            groups.push((String::from(entry.name), entry.gid));
        }
        Ok(groups)
    }
}

impl GroupDb for GroupsAdminDb<'_> {
    fn name_in_use(&self, name: &str) -> Result<bool, Errno> {
        Ok(self.list()?.iter().any(|(taken, _)| taken == name))
    }

    fn create(&self, spec: &GroupSpec<'_>) -> Result<(), Errno> {
        let gid = match spec.gid {
            Some(gid) => gid,
            None => next_id(IdRange::User, self.list()?.into_iter().map(|(_, gid)| gid))
                .ok_or(Errno::OutOfRange)?,
        };
        let request = UsersAdminRequest::CreateGroup {
            name: spec.name,
            gid,
        };
        let mut req = [0u8; USERS_ADMIN_MAX_REQUEST];
        let len = request.encode_into(&mut req)?;
        self.channel.call(&req[..len], &mut [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AdminChannel, GroupsAdminDb, RESPONSE_CAPACITY};
    use crate::io::{GroupDb, GroupSpec};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::users_admin::{
        GroupEntry, ListResponseBuilder, UsersAdminRequest, USERS_ADMIN_VERSION,
    };
    use tairix_abi::Errno;

    /// An in-memory `users_admin` endpoint: serves `ListGroups` from a
    /// `(name, gid)` table and records every decoded `CreateGroup`.
    struct MemChannel {
        existing: Vec<(String, u32)>,
        fail: Option<Errno>,
        created: RefCell<Vec<(String, u32)>>,
    }

    impl MemChannel {
        fn new(existing: &[(&str, u32)]) -> Self {
            Self {
                existing: existing
                    .iter()
                    .map(|(n, gid)| ((*n).to_string(), *gid))
                    .collect(),
                fail: None,
                created: RefCell::new(Vec::new()),
            }
        }

        fn failing(mut self, errno: Errno) -> Self {
            self.fail = Some(errno);
            self
        }

        fn created(&self) -> Vec<(String, u32)> {
            self.created.borrow().clone()
        }
    }

    impl AdminChannel for MemChannel {
        fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
            if let Some(errno) = self.fail {
                return Err(errno);
            }
            match UsersAdminRequest::decode(req)? {
                UsersAdminRequest::ListGroups => {
                    let mut builder = ListResponseBuilder::new(out)?;
                    for (name, gid) in &self.existing {
                        builder.push_group(&GroupEntry { name, gid: *gid })?;
                    }
                    Ok(builder.finish())
                }
                UsersAdminRequest::CreateGroup { name, gid } => {
                    self.created.borrow_mut().push((name.to_string(), gid));
                    Ok(0)
                }
                _ => Err(Errno::NotImplemented),
            }
        }
    }

    fn spec(name: &str, gid: Option<u32>) -> GroupSpec<'_> {
        GroupSpec { name, gid }
    }

    #[test]
    fn name_in_use_reflects_the_listing() {
        let channel = MemChannel::new(&[("wheel", 0), ("staff", 100)]);
        let db = GroupsAdminDb::new(&channel);
        assert_eq!(db.name_in_use("staff"), Ok(true));
        assert_eq!(db.name_in_use("audio"), Ok(false));
    }

    #[test]
    fn a_channel_failure_surfaces_from_the_lookup() {
        let channel = MemChannel::new(&[]).failing(Errno::PermissionDenied);
        let db = GroupsAdminDb::new(&channel);
        assert_eq!(db.name_in_use("staff"), Err(Errno::PermissionDenied));
    }

    #[test]
    fn an_omitted_gid_is_allocated_in_the_user_band() {
        // System-band gids never steer the allocation: a registry holding
        // only reserved gids yields the band's first id…
        let channel = MemChannel::new(&[("wheel", 0), ("staff", 100)]);
        let db = GroupsAdminDb::new(&channel);
        assert_eq!(db.create(&spec("audio", None)), Ok(()));
        assert_eq!(channel.created(), [("audio".to_string(), 1000)]);

        // …and an existing user-band gid is allocated above.
        let channel = MemChannel::new(&[("wheel", 0), ("ada", 1004)]);
        let db = GroupsAdminDb::new(&channel);
        assert_eq!(db.create(&spec("audio", None)), Ok(()));
        assert_eq!(channel.created(), [("audio".to_string(), 1005)]);
    }

    #[test]
    fn a_requested_gid_is_passed_through_verbatim() {
        let channel = MemChannel::new(&[("wheel", 0)]);
        let db = GroupsAdminDb::new(&channel);
        assert_eq!(db.create(&spec("audio", Some(4321))), Ok(()));
        assert_eq!(channel.created(), [("audio".to_string(), 4321)]);
    }

    #[test]
    fn an_exhausted_gid_space_fails_closed() {
        let channel = MemChannel::new(&[("wheel", 0), ("max", u32::MAX)]);
        let db = GroupsAdminDb::new(&channel);
        assert_eq!(db.create(&spec("audio", None)), Err(Errno::OutOfRange));
        assert!(channel.created().is_empty());
    }

    #[test]
    fn a_hostile_reply_fails_closed() {
        /// A channel answering with a version the client does not speak.
        struct BadVersion;
        impl AdminChannel for BadVersion {
            fn call(&self, _req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
                let bad = (USERS_ADMIN_VERSION + 1).to_le_bytes();
                out[..2].copy_from_slice(&bad);
                out[2..4].copy_from_slice(&0u16.to_le_bytes());
                Ok(4)
            }
        }
        let db = GroupsAdminDb::new(&BadVersion);
        assert_eq!(db.name_in_use("staff"), Err(Errno::AbiVersionUnsupported));
    }

    #[test]
    fn a_reply_longer_than_the_buffer_fails_closed() {
        /// A channel claiming to have written more than the buffer holds.
        struct Overlong;
        impl AdminChannel for Overlong {
            fn call(&self, _req: &[u8], _out: &mut [u8]) -> Result<usize, Errno> {
                Ok(RESPONSE_CAPACITY + 1)
            }
        }
        let db = GroupsAdminDb::new(&Overlong);
        assert_eq!(db.name_in_use("staff"), Err(Errno::LengthOutOfRange));
    }
}

//! The `/System/Security/Groups` database: the first-class group registry.
//!
//! Groups are first-class objects: every group a user may belong to is
//! declared here once, by name and numeric [`Gid`], and nothing else owns
//! that registry. **Membership is not stored here** — a user's primary and
//! supplementary groups live in that user's [`UserRecord`](crate::UserRecord)
//! ([`/System/Security/Users`](crate::UsersDb)), so a membership fact has a
//! single home and the two files can never disagree about who is in a group.
//! This file answers the orthogonal question *"which groups exist, and what
//! are they called?"*, the authoritative set every user reference is checked
//! against when the kernel builds its identity table (a user naming a group
//! with no record here is refused, fail closed).
//!
//! The on-disk text is **untrusted input**: the parser bounds the whole
//! file, every line, and the record count before reading anything, validates
//! every field through [`GroupRecord`], enforces group-name and gid
//! uniqueness, and fails closed on the first defect — a database the parser
//! cannot fully understand yields **no** [`GroupsDb`].
//!
//! # Format (`rustos-groups-v1`)
//!
//! Line one is exactly [`GROUPS_FORMAT_HEADER`]. Every other line is blank, a
//! `#` comment, or one [`GroupRecord`] line `groupname:gid`:
//!
//! ```text
//! rustos-groups-v1
//! # groupname:gid
//! wheel:0
//! ada:1000
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use crate::record::{name_charset_ok, parse_canonical_u32, Gid};
use crate::ParseError;

/// The exact first line of every `groups-v1` database.
pub const GROUPS_FORMAT_HEADER: &str = "rustos-groups-v1";

/// Longest group name, in bytes (the same bound as a username, since both
/// obey the one identifier grammar).
pub const MAX_GROUPNAME_LEN: usize = 32;

/// Largest database file, in bytes, the parser will consider (a validation
/// bound — a defence, not a capacity).
pub const MAX_GROUPS_DB_LEN: usize = 64 * 1024;

/// Longest single line, in bytes.
pub const MAX_GROUP_LINE_LEN: usize = 128;

/// Most records one group database may hold.
pub const MAX_GROUPS: usize = 1024;

/// Name of the well-known removable-storage access group.
///
/// A hotplug volume whose filesystem stores no owner model (FAT32) is
/// mounted with a kernel-side identity map: every node appears owned by
/// the system user and this group, group read/write, so any logged-in
/// member can use the medium without ambient authority. The kernel
/// resolves the group **by this name** from the loaded registry at boot;
/// a registry without it simply leaves foreign volumes system-owned
/// (fail closed, never an invented gid).
pub const STORAGE_GROUP: &str = "storage";

/// The [`Gid`] provisioning seeds [`STORAGE_GROUP`] with (the image
/// builder's debug profile and the test fixtures; the installer mints the
/// production registry from the same constant). The kernel never assumes
/// this value — it resolves the group by name — so an administrator
/// renumbering the group only has to keep the registry consistent.
pub const STORAGE_GID: Gid = Gid(100);

/// One validated group: a name and its numeric [`Gid`].
///
/// A group carries no member list (membership lives in the user records,
/// see the module docs). It is the registry entry the kernel resolves a
/// user's primary and supplementary gids against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupRecord {
    name: String,
    gid: Gid,
}

impl GroupRecord {
    /// Build a record from a validated name and gid.
    ///
    /// # Errors
    ///
    /// [`ParseError::GroupName`] if `name` violates the identifier grammar
    /// or the [`MAX_GROUPNAME_LEN`] bound.
    pub fn new(name: &str, gid: Gid) -> Result<Self, ParseError> {
        if !name_charset_ok(name, MAX_GROUPNAME_LEN) {
            return Err(ParseError::GroupName);
        }
        Ok(Self {
            name: String::from(name),
            gid,
        })
    }

    /// Decode one database line `groupname:gid`.
    ///
    /// # Errors
    ///
    /// [`ParseError::FieldCount`] for the wrong number of `:`-separated
    /// fields, [`ParseError::GroupName`] for an invalid name, or
    /// [`ParseError::GroupId`] for a non-canonically-spelled gid.
    pub fn decode_line(line: &str) -> Result<Self, ParseError> {
        let mut fields = line.split(':');
        let name = fields.next().ok_or(ParseError::FieldCount)?;
        let gid = fields.next().ok_or(ParseError::FieldCount)?;
        if fields.next().is_some() {
            return Err(ParseError::FieldCount);
        }
        let gid = parse_canonical_u32(gid).ok_or(ParseError::GroupId)?;
        Self::new(name, Gid(gid))
    }

    /// Encode the record into the line form [`Self::decode_line`] accepts.
    #[must_use]
    pub fn encode_line(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.name);
        out.push(':');
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{}", self.gid.0));
        out
    }

    /// The group name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The numeric group id.
    #[must_use]
    pub fn gid(&self) -> Gid {
        self.gid
    }
}

/// A parsed, validated group database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupsDb {
    records: Vec<GroupRecord>,
}

impl GroupsDb {
    /// Build a database from validated records, enforcing the
    /// whole-database invariants.
    ///
    /// # Errors
    ///
    /// [`ParseError::TooManyGroups`] past [`MAX_GROUPS`];
    /// [`ParseError::DuplicateGroupName`] / [`ParseError::DuplicateGroupId`]
    /// when two records collide.
    pub fn new(records: Vec<GroupRecord>) -> Result<Self, ParseError> {
        if records.len() > MAX_GROUPS {
            return Err(ParseError::TooManyGroups);
        }
        for (index, record) in records.iter().enumerate() {
            for earlier in &records[..index] {
                if earlier.name() == record.name() {
                    return Err(ParseError::DuplicateGroupName);
                }
                if earlier.gid() == record.gid() {
                    return Err(ParseError::DuplicateGroupId);
                }
            }
        }
        Ok(Self { records })
    }

    /// Parse and validate a whole database text.
    ///
    /// # Errors
    ///
    /// The matching [`ParseError`], failing closed on the first defect.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        if text.len() > MAX_GROUPS_DB_LEN {
            return Err(ParseError::TooLong);
        }
        let mut lines = text.lines();
        if lines.next() != Some(GROUPS_FORMAT_HEADER) {
            return Err(ParseError::Header);
        }

        let mut records = Vec::new();
        for line in lines {
            if line.len() > MAX_GROUP_LINE_LEN {
                return Err(ParseError::LineTooLong);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if records.len() == MAX_GROUPS {
                return Err(ParseError::TooManyGroups);
            }
            records.push(GroupRecord::decode_line(trimmed)?);
        }
        Self::new(records)
    }

    /// Serialise the database into the text form [`Self::parse`] accepts.
    #[must_use]
    pub fn serialise(&self) -> String {
        let mut out = String::from(GROUPS_FORMAT_HEADER);
        out.push('\n');
        for record in &self.records {
            out.push_str(&record.encode_line());
            out.push('\n');
        }
        out
    }

    /// Every record, in file order.
    #[must_use]
    pub fn records(&self) -> &[GroupRecord] {
        &self.records
    }

    /// The record named `name`, if any.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<&GroupRecord> {
        self.records.iter().find(|record| record.name() == name)
    }

    /// The record owning `gid`, if any.
    #[must_use]
    pub fn lookup_gid(&self, gid: Gid) -> Option<&GroupRecord> {
        self.records.iter().find(|record| record.gid() == gid)
    }
}

#[cfg(test)]
mod tests {
    use super::{GroupRecord, GroupsDb, GROUPS_FORMAT_HEADER, MAX_GROUPS, MAX_GROUP_LINE_LEN};
    use crate::record::Gid;
    use crate::ParseError;

    use alloc::string::String;
    use alloc::vec::Vec;

    fn db() -> GroupsDb {
        GroupsDb::new(alloc::vec![
            GroupRecord::new("wheel", Gid(0)).expect("valid"),
            GroupRecord::new("ada", Gid(1000)).expect("valid"),
            GroupRecord::new("staff", Gid(50)).expect("valid"),
        ])
        .expect("valid db")
    }

    #[test]
    fn serialise_parse_round_trips() {
        let original = db();
        let text = original.serialise();
        assert!(text.starts_with("rustos-groups-v1\n"));
        assert_eq!(GroupsDb::parse(&text), Ok(original));
    }

    #[test]
    fn record_line_round_trips() {
        let record = GroupRecord::new("wheel", Gid(0)).expect("valid");
        assert_eq!(record.encode_line(), "wheel:0");
        assert_eq!(GroupRecord::decode_line("wheel:0"), Ok(record));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let mut text = String::from(GROUPS_FORMAT_HEADER);
        text.push_str("\n# a comment\n\n   \nada:1000\n");
        let parsed = GroupsDb::parse(&text).expect("parses");
        assert_eq!(parsed.records().len(), 1);
        assert_eq!(parsed.lookup("ada").map(GroupRecord::gid), Some(Gid(1000)));
    }

    #[test]
    fn missing_or_wrong_header_is_rejected() {
        assert_eq!(GroupsDb::parse(""), Err(ParseError::Header));
        assert_eq!(
            GroupsDb::parse("rustos-groups-v2\n"),
            Err(ParseError::Header)
        );
        assert_eq!(GroupsDb::parse("wheel:0\n"), Err(ParseError::Header));
    }

    #[test]
    fn bad_names_are_rejected() {
        for name in ["", "Wheel", "1grp", "-grp", "grp space", "grp:x"] {
            assert_eq!(
                GroupRecord::new(name, Gid(1)),
                Err(ParseError::GroupName),
                "accepted name {name:?}"
            );
        }
        let long = "g".repeat(33);
        assert_eq!(GroupRecord::new(&long, Gid(1)), Err(ParseError::GroupName));
    }

    #[test]
    fn malformed_gids_are_rejected() {
        assert_eq!(
            GroupRecord::decode_line("wheel:-1"),
            Err(ParseError::GroupId)
        );
        assert_eq!(
            GroupRecord::decode_line("wheel:01"),
            Err(ParseError::GroupId)
        );
        assert_eq!(
            GroupRecord::decode_line("wheel:+1"),
            Err(ParseError::GroupId)
        );
        assert_eq!(
            GroupRecord::decode_line("wheel"),
            Err(ParseError::FieldCount)
        );
        assert_eq!(
            GroupRecord::decode_line("wheel:0:extra"),
            Err(ParseError::FieldCount)
        );
    }

    #[test]
    fn duplicates_are_rejected() {
        assert_eq!(
            GroupsDb::new(alloc::vec![
                GroupRecord::new("wheel", Gid(0)).expect("valid"),
                GroupRecord::new("wheel", Gid(1)).expect("valid"),
            ]),
            Err(ParseError::DuplicateGroupName)
        );
        assert_eq!(
            GroupsDb::new(alloc::vec![
                GroupRecord::new("wheel", Gid(0)).expect("valid"),
                GroupRecord::new("root", Gid(0)).expect("valid"),
            ]),
            Err(ParseError::DuplicateGroupId)
        );
    }

    #[test]
    fn oversized_inputs_are_rejected_before_scanning() {
        let mut text = String::from(GROUPS_FORMAT_HEADER);
        text.push('\n');
        while text.len() <= super::MAX_GROUPS_DB_LEN {
            text.push_str("# padding\n");
        }
        assert_eq!(GroupsDb::parse(&text), Err(ParseError::TooLong));

        let mut long_line = String::from(GROUPS_FORMAT_HEADER);
        long_line.push('\n');
        long_line.push('#');
        for _ in 0..=MAX_GROUP_LINE_LEN {
            long_line.push('x');
        }
        long_line.push('\n');
        assert_eq!(GroupsDb::parse(&long_line), Err(ParseError::LineTooLong));
    }

    #[test]
    fn the_record_budget_is_enforced() {
        let records: Vec<GroupRecord> = (0..=MAX_GROUPS)
            .map(|i| {
                let mut name = String::from("g");
                let _ = core::fmt::Write::write_fmt(&mut name, format_args!("{i}"));
                GroupRecord::new(&name, Gid(u32::try_from(i).expect("fits"))).expect("valid")
            })
            .collect();
        assert_eq!(GroupsDb::new(records), Err(ParseError::TooManyGroups));
    }

    #[test]
    fn lookups_find_records_by_name_and_gid() {
        let db = db();
        assert_eq!(db.lookup("wheel").map(GroupRecord::gid), Some(Gid(0)));
        assert!(db.lookup("missing").is_none());
        assert_eq!(db.lookup_gid(Gid(50)).map(GroupRecord::name), Some("staff"));
        assert!(db.lookup_gid(Gid(42)).is_none());
    }
}

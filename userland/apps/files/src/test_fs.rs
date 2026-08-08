//! The in-memory directory source the crate's host tests browse.
//!
//! A path either lists (as an empty directory) or is refused, which is all the
//! routing decisions under test need: what is exercised is where the browser
//! ends up and what happens when it cannot get there. Shared by every host
//! test module so the tree — including the deliberately unreadable places the
//! refusal paths need — is described once.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_browse::{Browser, DirectorySource, Entry};

/// An in-memory directory tree: a path either lists (empty) or is refused.
pub struct FakeFs {
    listable: BTreeSet<String>,
}

impl FakeFs {
    /// The user's home tree and the machine roots, with the user's `Desktop`
    /// and the `Storage` catalog deliberately unreadable, so both a refused
    /// place and a refused climb to a parent can be driven.
    pub fn fixture() -> Self {
        let mut listable = BTreeSet::new();
        for path in [
            "/",
            "/Users",
            "/Users/ann",
            "/Users/ann/Documents",
            "/Apps",
            "/System",
            "/Storage/Backup",
        ] {
            listable.insert(path.to_string());
        }
        Self { listable }
    }
}

impl DirectorySource for FakeFs {
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno> {
        let mut path = String::new();
        for component in components {
            path.push('/');
            path.push_str(component);
        }
        if path.is_empty() {
            path.push('/');
        }
        if self.listable.contains(&path) {
            Ok(Vec::new())
        } else {
            Err(Errno::PermissionDenied)
        }
    }
}

/// A browser opened at the storage root of [`FakeFs::fixture`].
pub fn browser() -> Browser<FakeFs> {
    Browser::open_root(FakeFs::fixture()).expect("the fixture root lists")
}

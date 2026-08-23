//! The caller's own catalog overlay, in this application's **published**
//! app-data scope (`plans/APPDATA.md` §1.1, §3.11).
//!
//! The overlay is per-user, per-application data — the entries an account
//! added for itself and its verdicts on the machine store's — and it used to
//! be an ordinary file under the user's home that *every* application they
//! launched could read and rewrite. A hostile program could file a launcher
//! row named "Terminal" against a bundle of its choosing, and the desktop
//! would draw it. It now lives in the app-data store, where:
//!
//! - `applib` is the only principal that can write it, by construction: an
//!   application publishes only its **own** scope, so no request shape
//!   exists by which another program reaches this one; and
//! - the desktop session reads it by naming the publisher on a request that
//!   carries **no scope field**, so what it can obtain is what `applib`
//!   chose to publish and nothing else `applib` keeps.
//!
//! Nothing here spells a path, a user, or a bundle identifier: the service
//! derives the store from the identity the kernel attested for this task.
//!
//! # Why the machine layer did not move with it
//!
//! `tairix_proglib::LIBRARY_PATH` stays an ordinary `/System/Settings`
//! administrator document. It is *machine* policy rather than any one
//! application's data — every account reads it, only a principal that tree's
//! policy admits may rewrite it — so it is not the per-user, per-app data the
//! app-data store exists to isolate, and putting it in one application's
//! scope would have made a machine's library the property of a command.

use core::cell::RefCell;

use tairix_abi::Errno;
use tairix_appconf::Document;
use tairix_appdata::{AppDataHost, Settings as SettingsStore};

use crate::Store;

/// The caller's own catalog overlay, reached through the app-data service.
///
/// The handle is opened for the one round trip a read or a publish costs and
/// never held between them, so nothing here can go stale against what the
/// service holds. The host sits behind a [`RefCell`] because the [`Store`]
/// seam is shared read-only by the editing engine while the transport
/// underneath is inherently mutating; the borrow never escapes a method, so
/// no two are ever live at once.
pub struct AppDataStore<H: AppDataHost> {
    host: RefCell<H>,
}

impl<H: AppDataHost> AppDataStore<H> {
    /// The overlay reached over `host`.
    pub const fn new(host: H) -> Self {
        Self {
            host: RefCell::new(host),
        }
    }
}

impl<H: AppDataHost> Store for AppDataStore<H> {
    /// The published overlay, or `None` when it holds nothing.
    ///
    /// A scope that publishes nothing and one that was never written are the
    /// same answer, exactly as they are to any other reader of it.
    fn read(&self) -> Result<Option<Document>, Errno> {
        let mut host = self.host.borrow_mut();
        let store = SettingsStore::open_published(&mut *host);
        if let Some(err) = store.store_refusal() {
            return Err(err);
        }
        let mut document = Document::new();
        for setting in store.settings() {
            // Fail closed rather than drop: a key this rebuild lost would be
            // a catalog entry the tool then republished without, silently
            // unregistering an application. The service cannot answer with a
            // document it could not itself have accepted, so this is
            // unreachable — and it costs one `?` to not have to rely on that.
            document
                .set(setting.key, setting.value)
                .map_err(|_| Errno::OutOfRange)?;
        }
        let empty = document.settings().next().is_none();
        Ok((!empty).then_some(document))
    }

    /// Publish `document` as the whole of this scope.
    ///
    /// Every key the scope carries that `document` does not is unset and
    /// every setting it carries is set, so what the scope says afterwards is
    /// exactly what was handed in — a removed entry leaves nothing behind.
    /// The commit publishes the lot as one atomic document replacement, so a
    /// reader never sees half an edit.
    fn write(&self, document: &Document) -> Result<(), Errno> {
        let mut host = self.host.borrow_mut();
        let mut store = SettingsStore::open_published(&mut *host);
        let stale: alloc::vec::Vec<alloc::string::String> = store
            .settings()
            .iter()
            .filter(|setting| document.get(setting.key).is_none())
            .map(|setting| alloc::string::String::from(setting.key))
            .collect();
        for key in stale {
            store.unset(&key);
        }
        for setting in document.settings() {
            // The registry only ever renders keys and values the format
            // engine accepts (`lib/proglib` pins that against the engine's own
            // definitions), so a refusal here is a defect in this program
            // rather than the user's doing; it is reported as a refused write
            // either way.
            store
                .set(setting.key, setting.value)
                .map_err(|_| Errno::OutOfRange)?;
        }
        store.commit()
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

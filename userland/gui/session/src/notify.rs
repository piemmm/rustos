//! The desktop's one notification intake: who posted a notice, and who may
//! learn which programs have.
//!
//! Every notice is attributed to the bundle the kernel attests its producer
//! runs, because that is the one name a policy can be keyed on that the
//! sender cannot choose. A producer running no verified bundle has no such
//! name, so the policy could neither list nor quieten it, and it is refused.

use alloc::vec::Vec;

use tairix_abi::notify_ipc::NOTIFY_SOURCES_MAX;
use tairix_abi::{AppIdentity, BundleId, Errno, Origin};
use tairix_taskbar::system::SETTINGS_BUNDLE;
use tairix_taskbar::Producer;

/// The producer the kernel attests in `origin`.
///
/// # Errors
///
/// [`Errno::PermissionDenied`] for a producer running no verified bundle.
pub fn producer_of(origin: &Origin) -> Result<Producer, Errno> {
    let app = origin.app().ok_or(Errno::PermissionDenied)?;
    Ok(Producer {
        instance: origin.proc_id(),
        pid: origin.pid(),
        source: BundleId::new(app.bundle_id())?,
    })
}

/// Whether `caller` is the desktop's own Settings application: the bundle the
/// session knows it by, signed by the publisher the session itself runs under,
/// so a bundle claiming the identifier under another key is not it.
#[must_use]
pub fn is_settings_surface(caller: Option<&AppIdentity>, own: Option<&AppIdentity>) -> bool {
    match (caller, own) {
        (Some(caller), Some(own)) => {
            caller.bundle_id() == SETTINGS_BUNDLE && caller.publisher() == own.publisher()
        }
        _ => false,
    }
}

/// The sources that have posted a notice since the desktop started, in the
/// order they first did, at most [`NOTIFY_SOURCES_MAX`] of them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotifySources {
    seen: Vec<BundleId>,
}

impl NotifySources {
    /// No source has posted.
    #[must_use]
    pub const fn new() -> Self {
        Self { seen: Vec::new() }
    }

    /// Remember that `source` has posted, unless it already is or the list is
    /// full.
    pub fn note(&mut self, source: BundleId) {
        if self.seen.len() < NOTIFY_SOURCES_MAX
            && !self
                .seen
                .iter()
                .any(|held| held.as_str() == source.as_str())
        {
            self.seen.push(source);
        }
    }

    /// Every remembered source.
    #[must_use]
    pub fn as_slice(&self) -> &[BundleId] {
        &self.seen
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use tairix_abi::notify_ipc::NOTIFY_SOURCES_MAX;
    use tairix_abi::origin::{CapabilitySummary, TrustDomain, ORIGIN_CONSOLE_NONE};
    use tairix_abi::{AppIdentity, BundleId, Errno, Origin, ProcId, PublisherId};

    use super::{is_settings_surface, producer_of, NotifySources};
    use tairix_taskbar::system::SETTINGS_BUNDLE;

    fn app(id: &str, key: u8) -> AppIdentity {
        AppIdentity::new(id, PublisherId::from_raw([key; 32])).expect("a well-formed identity")
    }

    fn origin() -> Origin {
        Origin::new(
            TrustDomain::User,
            1000,
            1000,
            42,
            ProcId::from_raw([9; 16]),
            CapabilitySummary::EMPTY,
            ORIGIN_CONSOLE_NONE,
        )
    }

    #[test]
    fn a_producer_is_its_attested_instance_and_bundle() {
        let producer = producer_of(&origin().with_app(app("os.tairix.netstack", 1)))
            .expect("a verified bundle");
        assert_eq!(producer.instance, ProcId::from_raw([9; 16]));
        assert_eq!(producer.pid, 42);
        assert_eq!(producer.source.as_str(), "os.tairix.netstack");
    }

    #[test]
    fn a_producer_running_no_bundle_is_refused() {
        assert_eq!(producer_of(&origin()), Err(Errno::PermissionDenied));
    }

    #[test]
    fn only_the_settings_bundle_under_the_desktops_own_publisher_is_the_surface() {
        let own = app("os.tairix.desktop", 1);
        let settings = app(SETTINGS_BUNDLE, 1);
        assert!(is_settings_surface(Some(&settings), Some(&own)));
        let impostor = app(SETTINGS_BUNDLE, 2);
        assert!(!is_settings_surface(Some(&impostor), Some(&own)));
        assert!(!is_settings_surface(
            Some(&app("os.tairix.files", 1)),
            Some(&own)
        ));
        assert!(!is_settings_surface(None, Some(&own)));
        // A session that knows no identity of its own trusts no caller.
        assert!(!is_settings_surface(Some(&settings), None));
    }

    #[test]
    fn sources_are_remembered_once_in_the_order_they_first_posted() {
        let mut sources = NotifySources::new();
        for id in ["b.two", "a.one", "b.two"] {
            sources.note(BundleId::new(id).expect("a bounded identity"));
        }
        let held: alloc::vec::Vec<&str> = sources.as_slice().iter().map(BundleId::as_str).collect();
        assert_eq!(held, ["b.two", "a.one"]);
    }

    #[test]
    fn the_source_list_stops_at_its_bound() {
        let mut sources = NotifySources::new();
        for n in 0..=NOTIFY_SOURCES_MAX {
            sources.note(BundleId::new(&format!("s.n{n}")).expect("a bounded identity"));
        }
        assert_eq!(sources.as_slice().len(), NOTIFY_SOURCES_MAX);
    }

    #[test]
    fn the_settings_bundle_is_the_one_its_manifest_declares() {
        let manifest = include_str!("../../../apps/settings/AppInfo.toml");
        let declared = manifest
            .lines()
            .find_map(|line| line.strip_prefix("id = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("the manifest declares an id");
        assert_eq!(declared, SETTINGS_BUNDLE);
    }
}

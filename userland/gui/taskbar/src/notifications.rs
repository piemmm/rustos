//! The notification / status-icon area, immediately before the clock.
//!
//! The area holds an ordered list of status icons (network, volume,
//! battery, and transient notifications). Each [`NotificationIcon`] names a
//! theme asset id; the actual artwork is resolved from `/System/Graphics`
//! by the renderer in a later increment.

use alloc::string::String;
use alloc::vec::Vec;

/// A stable identifier for a notification icon.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IconId(pub u64);

/// One status/notification icon: a stable id and the asset it displays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationIcon {
    /// The icon's stable id.
    pub id: IconId,
    /// The theme asset id resolved to artwork under `/System/Graphics`.
    pub asset: String,
}

/// The ordered set of notification/status icons.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotificationArea {
    icons: Vec<NotificationIcon>,
}

impl NotificationArea {
    /// An empty notification area.
    #[must_use]
    pub const fn new() -> Self {
        Self { icons: Vec::new() }
    }

    /// The icons in display order (leading to trailing, i.e. left-to-right
    /// for a horizontal bar).
    #[must_use]
    pub fn icons(&self) -> &[NotificationIcon] {
        &self.icons
    }

    /// The number of icons.
    #[must_use]
    pub fn len(&self) -> usize {
        self.icons.len()
    }

    /// `true` when there are no icons.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.icons.is_empty()
    }

    /// Add an icon. A duplicate id changes nothing and returns `false`
    /// (fail closed).
    pub fn add(&mut self, id: IconId, asset: impl Into<String>) -> bool {
        if self.icons.iter().any(|i| i.id == id) {
            return false;
        }
        self.icons.push(NotificationIcon {
            id,
            asset: asset.into(),
        });
        true
    }

    /// Remove an icon. Returns `false` for an unknown id.
    pub fn remove(&mut self, id: IconId) -> bool {
        let Some(index) = self.icons.iter().position(|i| i.id == id) else {
            return false;
        };
        self.icons.remove(index);
        true
    }
}

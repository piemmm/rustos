//! The live cache-admission control: the one switchboard every SMARTRAM
//! cache consults before it admits or retains an entry.
//!
//! TAIRiX exposes administrator-settable caching switches through the
//! boot-time configuration store (`tairix_sysconfig`: the master `cache.all`
//! ceiling plus the per-class `cache.filesystem` / `cache.block` /
//! `cache.transform` / `cache.semantic` switches). This module is the kernel
//! side of those switches: a process-global [`CacheControl`] holding one
//! admission flag per live cache class, applied once from the parsed
//! [`SystemConfig`] as the encrypted root is unlocked (`crate::syscfg`).
//!
//! # Why a live control, not a construction-time flag
//!
//! The store lives on the encrypted root, so it is only readable *after* the
//! unlock — by which point some caches (the whole-disk block cache) are
//! already constructed. Each cache therefore consults the live control at
//! **admission** and at its per-operation enforcement, exactly as it already
//! samples the memory-pressure gauge, rather than freezing a mode at
//! construction. Disabling a class then takes effect on that cache's next
//! operation: it admits nothing further and purges (zeroing) what it holds,
//! a real bypass rather than a flag that is read and ignored.
//!
//! # Safety of the switch
//!
//! Every SMARTRAM cache is a reclaimable accelerator that is never the source
//! of truth (`plans/SMARTRAM.md` section 6). Turning any or all of them off is
//! therefore *degrade-gracefully* — slower, still correct — never a
//! behavioural change. The default (an absent or unreadable store) leaves
//! every class enabled, reproducing today's behaviour exactly.
//!
//! # Hot path
//!
//! [`CacheControl::admits`] is a single relaxed atomic load on the cache
//! admission path, so the switch adds no lock and no allocation to the hot
//! path. `Relaxed` ordering is correct here: the flag is a policy gate, not a
//! guard publishing other memory, and a cache that admits one entry either
//! side of a flip simply purges it on its next operation.

use core::sync::atomic::{AtomicBool, Ordering};

pub use tairix_sysconfig::{CacheClass, CacheMode, SystemConfig};

/// One admission flag per live SMARTRAM cache class.
///
/// Construct with [`CacheControl::new`] (every class enabled); apply an
/// operator's configuration with [`CacheControl::apply`]. Consult it on the
/// admission path with [`CacheControl::admits`]. The production kernel shares
/// the one [`CACHE_CONTROL`] static; host tests build their own instance.
#[derive(Debug)]
pub struct CacheControl {
    filesystem: AtomicBool,
    block: AtomicBool,
    transform: AtomicBool,
    semantic: AtomicBool,
}

impl CacheControl {
    /// A control with every cache class enabled — the default an absent or
    /// unreadable configuration store implies (today's behaviour).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            filesystem: AtomicBool::new(true),
            block: AtomicBool::new(true),
            transform: AtomicBool::new(true),
            semantic: AtomicBool::new(true),
        }
    }

    /// The admission flag backing `class`.
    const fn slot(&self, class: CacheClass) -> &AtomicBool {
        match class {
            CacheClass::Filesystem => &self.filesystem,
            CacheClass::Block => &self.block,
            CacheClass::Transform => &self.transform,
            CacheClass::Semantic => &self.semantic,
        }
    }

    /// Whether the cache of `class` may currently admit and retain entries.
    /// `false` means the class is hard-disabled: the cache admits nothing and
    /// purges what it holds on its next operation.
    #[must_use]
    pub fn admits(&self, class: CacheClass) -> bool {
        self.slot(class).load(Ordering::Relaxed)
    }

    /// Set the admission flag for `class` from an effective [`CacheMode`].
    pub fn set(&self, class: CacheClass, mode: CacheMode) {
        self.slot(class).store(mode.admits(), Ordering::Relaxed);
    }

    /// Apply an operator's parsed configuration: set every class's admission
    /// flag from its **effective** mode, so the master `cache.all` ceiling is
    /// honoured (a `cache.all off` disables every class regardless of its
    /// per-class value).
    pub fn apply(&self, config: &SystemConfig) {
        for class in CacheClass::ALL {
            self.set(*class, config.effective_cache(*class));
        }
    }
}

impl Default for CacheControl {
    fn default() -> Self {
        Self::new()
    }
}

/// The one process-global cache-admission control the production caches
/// consult and the unlock path applies the operator's configuration to.
pub static CACHE_CONTROL: CacheControl = CacheControl::new();

#[cfg(test)]
mod tests {
    use super::{CacheClass, CacheControl, CacheMode};
    use tairix_sysconfig::{CacheSwitch, SystemConfig};

    #[test]
    fn a_fresh_control_admits_every_class() {
        let control = CacheControl::new();
        for class in CacheClass::ALL {
            assert!(control.admits(*class), "{class:?} should admit by default");
        }
    }

    #[test]
    fn applying_the_default_config_leaves_every_class_enabled() {
        let control = CacheControl::new();
        control.apply(&SystemConfig::default());
        for class in CacheClass::ALL {
            assert!(control.admits(*class));
        }
    }

    #[test]
    fn a_per_class_off_disables_only_that_class() {
        let control = CacheControl::new();
        let config = SystemConfig {
            cache_filesystem: CacheMode::Off,
            ..SystemConfig::default()
        };
        control.apply(&config);
        assert!(!control.admits(CacheClass::Filesystem));
        assert!(control.admits(CacheClass::Block));
        assert!(control.admits(CacheClass::Transform));
        assert!(control.admits(CacheClass::Semantic));
    }

    #[test]
    fn the_master_switch_off_disables_every_class() {
        let control = CacheControl::new();
        let config = SystemConfig {
            cache_all: CacheSwitch::Off,
            // A per-class `auto` must not survive the master ceiling.
            cache_block: CacheMode::Auto,
            ..SystemConfig::default()
        };
        control.apply(&config);
        for class in CacheClass::ALL {
            assert!(!control.admits(*class), "{class:?} must obey cache.all off");
        }
    }

    #[test]
    fn apply_is_idempotent_and_re_enables() {
        let control = CacheControl::new();
        control.apply(&SystemConfig {
            cache_all: CacheSwitch::Off,
            ..SystemConfig::default()
        });
        assert!(!control.admits(CacheClass::Semantic));
        // A later apply of an enabling config restores admission.
        control.apply(&SystemConfig::default());
        assert!(control.admits(CacheClass::Semantic));
    }
}

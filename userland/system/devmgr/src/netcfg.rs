//! Delivering the stack-wide `net.*` policy to the network stack.
//!
//! `netstack` is the network-parsing sandbox and holds no filesystem
//! capability, so it cannot read `/System/Settings/Configuration/system.conf`
//! itself. The device manager already holds `CAP_NET_ADMIN` and drives the
//! network stack's admin endpoint (see [`crate::netbind`]), so it is the
//! component that reads the stack-wide `net.*` settings from the
//! configuration store post-unlock and delivers them to `netstack` over the
//! capability-gated `ApplyNetworkSettings` admin op (`plans/NETWORK.md`
//! N9b-2).
//!
//! This module is the pure, host-testable policy for that delivery: read the
//! settings through the [`NetworkConfigSource`] seam and, until they have
//! been delivered, push them through the [`crate::netbind::NetstackBind`]
//! seam. Delivery is fail-soft — the store may not be mounted yet (before the
//! root unlock) and the stack may not be up yet, so a failed attempt is
//! logged and retried on the next hardware-tree generation bump, exactly like
//! an unavailable driver store. Until the real policy lands, `netstack`'s own
//! safe defaults (both families enabled, SYN cookies `auto`) hold.

use tairix_abi::net_ipc::NetworkSettings;
use tairix_log::{log as log_event, Event, Level, Sink};

use crate::events;
use crate::netbind::NetstackBind;

/// The device manager's read of the stack-wide `net.*` policy from the
/// system-configuration store.
///
/// The production implementation reads
/// `/System/Settings/Configuration/system.conf` and maps it through the one
/// shared `lib/sysconfig` engine ([`settings_from_config`]); it is a seam so
/// the delivery policy is host-testable against a scripted double.
pub trait NetworkConfigSource {
    /// Load the current stack-wide network settings.
    ///
    /// Returns [`Some`] when the store was read and parsed (the real policy,
    /// ready to deliver), and [`None`] when it could not be read — the store
    /// is not mounted yet (before the root unlock) or the read failed. A
    /// [`None`] is not an error: the caller keeps the network stack on its
    /// safe defaults and retries on the next generation bump.
    fn load(&mut self) -> Option<NetworkSettings>;
}

/// Map a parsed [`system.conf`](tairix_sysconfig::SystemConfig) onto the
/// stack-wide [`NetworkSettings`] the network stack enforces.
///
/// The mapping is exact and the single definition both the service binary and
/// its tests use (`AGENTS.md` §2.2): `net.ipv4.enabled` / `net.ipv6.enabled`
/// gate the families, and `net.tcp.syncookies always` selects unconditional
/// SYN cookies (`auto` leaves the bounded backlog). `net.ipv6.privacy` has no
/// enforcement consumer yet, so it is deliberately not carried.
#[cfg(feature = "program")]
#[must_use]
pub fn settings_from_config(config: &tairix_sysconfig::SystemConfig) -> NetworkSettings {
    NetworkSettings {
        ipv4_enabled: config.net_ipv4_enabled.is_enabled(),
        ipv6_enabled: config.net_ipv6_enabled.is_enabled(),
        syncookies_always: matches!(
            config.net_tcp_syncookies,
            tairix_sysconfig::SynCookies::Always
        ),
    }
}

/// The device manager's memory of whether it has delivered the stack-wide
/// `net.*` policy to the network stack.
///
/// Delivery happens exactly once: the configuration store is static after the
/// root unlock (runtime reload is a later increment), so once the policy has
/// been read and the stack accepted it, no further read or push is made.
#[derive(Default)]
pub struct NetConfigState {
    delivered: bool,
}

impl NetConfigState {
    /// A fresh state with nothing delivered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the policy has already been delivered and accepted.
    #[must_use]
    pub fn is_delivered(&self) -> bool {
        self.delivered
    }
}

/// Deliver the stack-wide `net.*` policy to the network stack, once.
///
/// A no-op after a successful delivery. Otherwise it reads the policy through
/// `source`; if the store is not yet readable ([`None`]) it leaves the stack
/// on its safe defaults and returns (retried on the next bump). If a policy
/// is read, it is pushed through `netstack`: success is recorded (no further
/// attempts), and a refusal is logged fail-soft and retried next bump — the
/// stack may not have bound its admin endpoint yet.
pub fn deliver_network_settings(
    source: &mut dyn NetworkConfigSource,
    state: &mut NetConfigState,
    netstack: &mut dyn NetstackBind,
    sink: &dyn Sink,
) {
    if state.delivered {
        return;
    }
    let Some(settings) = source.load() else {
        // The store is not readable yet (pre-unlock, or a failed read): the
        // stack keeps its safe defaults and this is retried on the next
        // generation bump. Not logged — an absent store before the unlock is
        // the expected early-boot state, not an anomaly.
        return;
    };
    match netstack.apply_settings(settings) {
        Ok(()) => {
            state.delivered = true;
            log_event(
                sink,
                &Event {
                    level: Level::Info,
                    id: events::NETWORK_SETTINGS_DELIVERED,
                    message: "network settings delivered to the network stack",
                    fields: &[],
                },
            );
        }
        Err(_) => {
            log_event(
                sink,
                &Event {
                    level: Level::Warn,
                    id: events::NETWORK_SETTINGS_DELIVERY_FAILED,
                    message: "network settings delivery to the network stack failed; will retry",
                    fields: &[],
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;
    use tairix_abi::net_ipc::IF_NAME_LEN;
    use tairix_abi::Errno;
    use tairix_log::Event;

    /// A scripted config source: hands out a queued `load` result per call.
    struct ScriptedSource {
        results: RefCell<Vec<Option<NetworkSettings>>>,
    }

    impl ScriptedSource {
        fn new(results: Vec<Option<NetworkSettings>>) -> Self {
            Self {
                results: RefCell::new(results),
            }
        }
    }

    impl NetworkConfigSource for ScriptedSource {
        fn load(&mut self) -> Option<NetworkSettings> {
            self.results.borrow_mut().pop().flatten()
        }
    }

    /// A recording netstack seam: captures each delivered policy and answers
    /// each `apply_settings` with a scripted result.
    struct RecordingNetstack {
        applied: RefCell<Vec<NetworkSettings>>,
        results: RefCell<Vec<Result<(), Errno>>>,
    }

    impl RecordingNetstack {
        fn new(results: Vec<Result<(), Errno>>) -> Self {
            Self {
                applied: RefCell::new(Vec::new()),
                results: RefCell::new(results),
            }
        }
    }

    impl NetstackBind for RecordingNetstack {
        fn bind_driver(&mut self, _e: u64, _i: &[u8; IF_NAME_LEN]) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_settings(&mut self, settings: NetworkSettings) -> Result<(), Errno> {
            self.applied.borrow_mut().push(settings);
            self.results.borrow_mut().pop().unwrap_or(Ok(()))
        }
    }

    struct RecordingSink {
        ids: RefCell<Vec<u32>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                ids: RefCell::new(Vec::new()),
            }
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    fn settings(v4: bool, v6: bool, cookies: bool) -> NetworkSettings {
        NetworkSettings {
            ipv4_enabled: v4,
            ipv6_enabled: v6,
            syncookies_always: cookies,
        }
    }

    #[cfg(feature = "program")]
    #[test]
    fn settings_map_from_the_config_registry() {
        let mut config = tairix_sysconfig::SystemConfig::default();
        assert_eq!(
            settings_from_config(&config),
            settings(true, true, false),
            "the registry defaults map to families-on, cookies-auto"
        );
        config.net_ipv6_enabled = tairix_sysconfig::NetToggle::Disabled;
        config.net_tcp_syncookies = tairix_sysconfig::SynCookies::Always;
        assert_eq!(settings_from_config(&config), settings(true, false, true));
    }

    #[test]
    fn absent_store_keeps_defaults_and_retries() {
        let mut source = ScriptedSource::new(alloc::vec![None]);
        let mut state = NetConfigState::new();
        let mut netstack = RecordingNetstack::new(Vec::new());
        let sink = RecordingSink::new();
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(!state.is_delivered(), "an unreadable store defers delivery");
        assert!(netstack.applied.borrow().is_empty(), "nothing pushed");
        assert!(
            sink.ids.borrow().is_empty(),
            "the expected early state is quiet"
        );
    }

    #[test]
    fn a_read_policy_is_delivered_once() {
        let policy = settings(true, false, true);
        let mut source = ScriptedSource::new(alloc::vec![Some(policy), Some(policy)]);
        let mut state = NetConfigState::new();
        let mut netstack = RecordingNetstack::new(alloc::vec![Ok(()), Ok(())]);
        let sink = RecordingSink::new();
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(state.is_delivered());
        assert_eq!(*netstack.applied.borrow(), alloc::vec![policy]);
        assert_eq!(
            sink.ids.borrow().as_slice(),
            &[events::NETWORK_SETTINGS_DELIVERED.0]
        );
        // A second pass delivers nothing more (the store is static).
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert_eq!(netstack.applied.borrow().len(), 1, "delivered exactly once");
    }

    #[test]
    fn a_refused_delivery_is_retried() {
        let policy = settings(false, true, false);
        let mut source = ScriptedSource::new(alloc::vec![Some(policy), Some(policy)]);
        let mut state = NetConfigState::new();
        // First apply refused (stack not up yet), second accepted.
        let mut netstack = RecordingNetstack::new(alloc::vec![Ok(()), Err(Errno::NotConnected)]);
        let sink = RecordingSink::new();
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(!state.is_delivered(), "a refused delivery is not recorded");
        deliver_network_settings(&mut source, &mut state, &mut netstack, &sink);
        assert!(state.is_delivered(), "retried and delivered");
        assert_eq!(*netstack.applied.borrow(), alloc::vec![policy, policy]);
        assert_eq!(
            sink.ids.borrow().as_slice(),
            &[
                events::NETWORK_SETTINGS_DELIVERY_FAILED.0,
                events::NETWORK_SETTINGS_DELIVERED.0
            ]
        );
    }
}

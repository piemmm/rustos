//! Unit tests for the interface address engine.

use super::*;
use crate::test_support::{temp_source, SeqTempSource};

const IID: [u8; 8] = [0x02, 0xCA, 0xFE, 0xFF, 0xFE, 0xBA, 0xBE, 0x01];

fn t(secs: i64) -> Duration64 {
    Duration64::from_secs(secs)
}

fn link_local_addr() -> Ipv6Addr {
    Ipv6Addr::from([
        0xFE, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0xCA, 0xFE, 0xFF, 0xFE, 0xBA, 0xBE, 0x01,
    ])
}

fn slaac_prefix() -> Ipv6Addr {
    Ipv6Addr::from([0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
}

fn slaac_addr() -> Ipv6Addr {
    Ipv6Addr::from([
        0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0x02, 0xCA, 0xFE, 0xFF, 0xFE, 0xBA, 0xBE, 0x01,
    ])
}

fn prefix_info(valid: u32, preferred: u32) -> PrefixInformation {
    PrefixInformation {
        prefix: slaac_prefix(),
        prefix_len: 64,
        on_link: true,
        autonomous: true,
        valid_lifetime: valid,
        preferred_lifetime: preferred,
    }
}

/// Bring an interface's link-local up: one DAD transmit at t0, then
/// DAD completion one retransmission interval later — which, with no
/// start jitter, also emits the first Router Solicitation.
fn ready_iface() -> Iface {
    let mut iface = Iface::new(&IfaceConfig::new(IID), temp_source(), t(0));
    let actions = iface.advance(t(0));
    assert_eq!(
        actions,
        [IfaceAction::SendDadSolicit {
            target: link_local_addr()
        }]
    );
    let actions = iface.advance(t(1));
    assert_eq!(
        actions,
        [
            IfaceAction::AddressPreferred {
                addr: link_local_addr()
            },
            IfaceAction::SendRouterSolicitation {
                source: Some(link_local_addr())
            },
        ]
    );
    iface
}

#[test]
fn link_local_completes_dad_and_solicits_routers() {
    let mut iface = ready_iface();
    assert_eq!(iface.link_local(), Some(link_local_addr()));
    // The remaining two solicitations, four seconds apart, then
    // silence.
    let actions = iface.advance(t(5));
    assert_eq!(
        actions,
        [IfaceAction::SendRouterSolicitation {
            source: Some(link_local_addr())
        }]
    );
    let actions = iface.advance(t(9));
    assert_eq!(actions.len(), 1);
    let actions = iface.advance(t(13));
    assert!(actions.is_empty());
    assert_eq!(iface.next_deadline(), None);
}

#[test]
fn start_delay_defers_first_dad_transmit() {
    let config = IfaceConfig {
        start_delay: Duration64::from_secs(1),
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, temp_source(), t(0));
    assert!(iface.advance(t(0)).is_empty());
    assert_eq!(iface.next_deadline(), Some(t(1)));
    let actions = iface.advance(t(1));
    assert_eq!(
        actions,
        [IfaceAction::SendDadSolicit {
            target: link_local_addr()
        }]
    );
}

#[test]
fn tentative_address_is_not_a_candidate() {
    let mut iface = Iface::new(&IfaceConfig::new(IID), temp_source(), t(0));
    assert!(iface.candidates().is_empty());
    assert!(!iface.is_assigned(link_local_addr()));
    assert!(iface.is_tentative(link_local_addr()));
    iface.advance(t(0));
    iface.advance(t(1));
    assert_eq!(iface.candidates().len(), 1);
    assert!(iface.is_assigned(link_local_addr()));
}

#[test]
fn dad_conflict_on_link_local_disables_ipv6() {
    let mut iface = Iface::new(&IfaceConfig::new(IID), temp_source(), t(0));
    iface.advance(t(0));
    let action = iface.on_dad_evidence(link_local_addr());
    assert_eq!(
        action,
        Some(IfaceAction::DadFailed {
            addr: link_local_addr()
        })
    );
    assert!(iface.v6_disabled());
    assert!(iface.ipv6_addresses().is_empty());
    assert_eq!(
        iface.add_ipv6_static(slaac_addr(), 64, t(2)),
        Err(AddrError::V6Disabled)
    );
    // No further ND activity is scheduled.
    assert_eq!(iface.next_deadline(), None);
}

#[test]
fn dad_evidence_for_a_usable_address_is_ignored() {
    let mut iface = ready_iface();
    assert_eq!(iface.on_dad_evidence(link_local_addr()), None);
    assert!(iface.is_assigned(link_local_addr()));
}

#[test]
fn slaac_address_forms_from_ra_prefix_and_runs_dad() {
    let mut iface = ready_iface();
    iface.on_router_advertisement(&[prefix_info(3600, 1800)], t(2));
    assert!(iface.is_tentative(slaac_addr()));
    let actions = iface.advance(t(2));
    assert!(actions.contains(&IfaceAction::SendDadSolicit {
        target: slaac_addr()
    }));
    let actions = iface.advance(t(3));
    assert!(actions.contains(&IfaceAction::AddressPreferred { addr: slaac_addr() }));
    let info = iface
        .ipv6_addresses()
        .into_iter()
        .find(|info| info.addr == slaac_addr())
        .expect("slaac address exists");
    assert_eq!(info.origin, AddrOrigin::Slaac);
    assert_eq!(info.prefix_len, 64);
}

#[test]
fn ra_stops_router_solicitations() {
    let mut iface = ready_iface(); // first RS already sent
    iface.on_router_advertisement(&[], t(2));
    assert!(iface.advance(t(5)).is_empty());
    assert_eq!(iface.next_deadline(), None);
}

#[test]
fn slaac_shape_rules_reject_bad_prefixes() {
    let mut iface = ready_iface();
    // Non-autonomous.
    let mut p = prefix_info(3600, 1800);
    p.autonomous = false;
    iface.on_router_advertisement(&[p], t(2));
    // Wrong length.
    let mut p = prefix_info(3600, 1800);
    p.prefix_len = 48;
    iface.on_router_advertisement(&[p], t(2));
    // Link-local prefix.
    let mut p = prefix_info(3600, 1800);
    p.prefix = link_local_addr();
    iface.on_router_advertisement(&[p], t(2));
    // Preferred beyond valid.
    iface.on_router_advertisement(&[prefix_info(100, 200)], t(2));
    // Zero valid lifetime never creates.
    iface.on_router_advertisement(&[prefix_info(0, 0)], t(2));
    assert_eq!(iface.ipv6_addresses().len(), 1); // link-local only
}

#[test]
fn preferred_lifetime_lapse_deprecates_then_valid_lapse_invalidates() {
    let mut iface = ready_iface();
    iface.on_router_advertisement(&[prefix_info(20, 10)], t(2));
    iface.advance(t(2));
    iface.advance(t(3)); // DAD complete
    assert!(
        !iface
            .candidates()
            .iter()
            .find(|c| c.addr == slaac_addr())
            .expect("candidate")
            .deprecated
    );
    iface.advance(t(12));
    assert!(
        iface
            .candidates()
            .iter()
            .find(|c| c.addr == slaac_addr())
            .expect("candidate")
            .deprecated
    );
    let actions = iface.advance(t(22));
    assert!(actions.contains(&IfaceAction::AddressInvalidated { addr: slaac_addr() }));
    assert!(!iface.is_assigned(slaac_addr()));
}

#[test]
fn two_hour_rule_caps_a_shrinking_valid_lifetime() {
    let mut iface = ready_iface();
    // Establish with a ten-hour valid lifetime.
    iface.on_router_advertisement(&[prefix_info(36_000, 36_000)], t(2));
    iface.advance(t(2));
    iface.advance(t(3));
    // A spoofed RA trying to expire the address in ten seconds is
    // capped at two hours remaining.
    iface.on_router_advertisement(&[prefix_info(10, 10)], t(4));
    assert!(iface.is_assigned(slaac_addr()));
    iface.advance(t(20));
    assert!(iface.is_assigned(slaac_addr()), "still valid before 2h");
    let actions = iface.advance(t(4 + 7_200));
    assert!(actions.contains(&IfaceAction::AddressInvalidated { addr: slaac_addr() }));
}

#[test]
fn growing_valid_lifetime_is_always_accepted() {
    let mut iface = ready_iface();
    iface.on_router_advertisement(&[prefix_info(60, 60)], t(2));
    iface.advance(t(2));
    iface.advance(t(3));
    iface.on_router_advertisement(&[prefix_info(36_000, 36_000)], t(4));
    iface.advance(t(120));
    assert!(iface.is_assigned(slaac_addr()), "lifetime was extended");
}

#[test]
fn ra_update_never_rebinds_a_static_address() {
    let mut iface = ready_iface();
    iface
        .add_ipv6_static(slaac_addr(), 64, t(2))
        .expect("static add");
    iface.advance(t(2));
    iface.advance(t(3));
    // An RA for the same prefix must not convert or expire the
    // static assignment.
    iface.on_router_advertisement(&[prefix_info(10, 10)], t(4));
    iface.advance(t(7_300));
    assert!(iface.is_assigned(slaac_addr()));
}

#[test]
fn static_ipv6_add_validates_and_bounds() {
    let mut iface = ready_iface();
    assert_eq!(
        iface.add_ipv6_static(Ipv6Addr::from([0u8; 16]), 64, t(2)),
        Err(AddrError::NotUnicast)
    );
    assert_eq!(
        iface.add_ipv6_static(slaac_addr(), 0, t(2)),
        Err(AddrError::BadPrefixLen)
    );
    assert_eq!(iface.add_ipv6_static(slaac_addr(), 64, t(2)), Ok(()));
    assert_eq!(
        iface.add_ipv6_static(slaac_addr(), 64, t(2)),
        Err(AddrError::Duplicate)
    );
    for index in 0..MAX_IPV6_ADDRS {
        let mut octets = [0x20u8; 16];
        octets[15] = u8::try_from(index).expect("small table");
        let _ = iface.add_ipv6_static(Ipv6Addr::from(octets), 64, t(2));
    }
    let mut octets = [0x20u8; 16];
    octets[14] = 0xFF;
    assert_eq!(
        iface.add_ipv6_static(Ipv6Addr::from(octets), 64, t(2)),
        Err(AddrError::TableFull)
    );
}

#[test]
fn remove_ipv6_refuses_the_link_local() {
    let mut iface = ready_iface();
    assert!(!iface.remove_ipv6(link_local_addr()));
    iface
        .add_ipv6_static(slaac_addr(), 64, t(2))
        .expect("static add");
    assert!(iface.remove_ipv6(slaac_addr()));
    assert!(!iface.remove_ipv6(slaac_addr()));
}

#[test]
fn slaac_table_bound_holds_under_hostile_ra() {
    let mut iface = ready_iface();
    let mut prefixes = Vec::new();
    for index in 0..64u8 {
        let mut p = prefix_info(3600, 1800);
        let mut octets = p.prefix.octets();
        octets[7] = index;
        p.prefix = Ipv6Addr::from(octets);
        prefixes.push(p);
    }
    iface.on_router_advertisement(&prefixes, t(2));
    assert!(iface.ipv6_addresses().len() <= MAX_IPV6_ADDRS);
}

#[test]
fn static_ipv4_assignment_validates() {
    let mut iface = ready_iface();
    assert_eq!(
        iface.set_ipv4(Ipv4Addr::UNSPECIFIED, 24),
        Err(AddrError::NotUnicast)
    );
    assert_eq!(
        iface.set_ipv4(Ipv4Addr::BROADCAST, 24),
        Err(AddrError::NotUnicast)
    );
    assert_eq!(
        iface.set_ipv4(Ipv4Addr::new(224, 0, 0, 1), 24),
        Err(AddrError::NotUnicast)
    );
    assert_eq!(
        iface.set_ipv4(Ipv4Addr::new(10, 0, 2, 15), 33),
        Err(AddrError::BadPrefixLen)
    );
    assert_eq!(iface.set_ipv4(Ipv4Addr::new(10, 0, 2, 15), 24), Ok(()));
    assert_eq!(iface.ipv4(), Some((Ipv4Addr::new(10, 0, 2, 15), 24)));
    assert!(iface.clear_ipv4());
    assert!(!iface.clear_ipv4());
}

#[test]
fn dad_disabled_makes_addresses_immediately_usable() {
    let config = IfaceConfig {
        dad_transmits: 0,
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, temp_source(), t(0));
    assert!(iface.is_assigned(link_local_addr()));
    // Router solicitation is scheduled straight away.
    let actions = iface.advance(t(0));
    assert_eq!(
        actions,
        [IfaceAction::SendRouterSolicitation {
            source: Some(link_local_addr())
        }]
    );
}

#[test]
fn next_deadline_tracks_earliest_pending_work() {
    let mut iface = Iface::new(&IfaceConfig::new(IID), temp_source(), t(0));
    assert_eq!(iface.next_deadline(), Some(t(0)));
    iface.advance(t(0));
    assert_eq!(iface.next_deadline(), Some(t(1)));
}

#[test]
fn ipv6_disabled_by_policy_forms_no_link_local() {
    let config = IfaceConfig {
        ipv6_enabled: false,
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, temp_source(), t(0));
    assert!(iface.v6_admin_disabled());
    assert!(iface.ipv6_addresses().is_empty());
    // No DAD/RS activity is ever scheduled for a disabled family.
    assert!(iface.advance(t(0)).is_empty());
    assert_eq!(iface.next_deadline(), None);
    // A static assignment is refused while disabled.
    assert_eq!(
        iface.add_ipv6_static(slaac_addr(), 64, t(0)),
        Err(AddrError::V6Disabled)
    );
}

#[test]
fn re_enabling_ipv6_reforms_the_link_local() {
    let config = IfaceConfig {
        ipv6_enabled: false,
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, temp_source(), t(0));
    iface.set_ipv6_enabled(true, t(0));
    assert!(!iface.v6_admin_disabled());
    // Bring-up proceeds exactly as a fresh interface would.
    assert_eq!(
        iface.advance(t(0)),
        [IfaceAction::SendDadSolicit {
            target: link_local_addr()
        }]
    );
    iface.advance(t(1));
    assert_eq!(iface.link_local(), Some(link_local_addr()));
}

#[test]
fn disabling_ipv6_flushes_every_address_and_halts_solicitation() {
    let mut iface = ready_iface();
    iface
        .add_ipv6_static(slaac_addr(), 64, t(1))
        .expect("static add");
    assert!(!iface.ipv6_addresses().is_empty());
    iface.set_ipv6_enabled(false, t(2));
    assert!(iface.v6_admin_disabled());
    assert!(iface.ipv6_addresses().is_empty());
    assert!(iface.candidates().is_empty());
    assert_eq!(iface.next_deadline(), None);
    assert!(iface.advance(t(3)).is_empty());
}

#[test]
fn set_ipv6_enabled_is_idempotent() {
    let mut iface = ready_iface();
    // Enabling an already-enabled interface changes nothing.
    iface.set_ipv6_enabled(true, t(2));
    assert_eq!(iface.link_local(), Some(link_local_addr()));
    iface.set_ipv6_enabled(false, t(2));
    iface.set_ipv6_enabled(false, t(2));
    assert!(iface.ipv6_addresses().is_empty());
}

#[test]
fn re_enabling_after_dad_failure_does_not_reform_link_local() {
    let mut iface = Iface::new(&IfaceConfig::new(IID), temp_source(), t(0));
    iface.advance(t(0));
    iface.on_dad_evidence(link_local_addr());
    assert!(iface.v6_disabled());
    // A policy toggle cannot override the RFC 4862 DAD-failure disable.
    iface.set_ipv6_enabled(false, t(1));
    iface.set_ipv6_enabled(true, t(1));
    assert!(iface.ipv6_addresses().is_empty());
}

// ---- RFC 8981 temporary (privacy) addresses -------------------------

/// A privacy configuration with DAD disabled (addresses immediately
/// usable) and short temporary lifetimes, so regeneration is reachable
/// in a few seconds of simulated time.
fn privacy_config() -> IfaceConfig {
    IfaceConfig {
        privacy: true,
        dad_transmits: 0,
        temp_preferred_lifetime: t(20),
        temp_valid_lifetime: t(40),
        ..IfaceConfig::new(IID)
    }
}

/// A prefix advertising infinite lifetimes, so the temporary address's
/// own (short) lifetimes govern regeneration, not the prefix's.
fn infinite_prefix() -> PrefixInformation {
    prefix_info(u32::MAX, u32::MAX)
}

fn temp_addrs(iface: &Iface) -> alloc::vec::Vec<Ipv6AddrInfo> {
    iface
        .ipv6_addresses()
        .into_iter()
        .filter(|info| info.origin == AddrOrigin::Temporary)
        .collect()
}

/// A scripted [`TempAddrSource`] yielding queued 8-byte words in order
/// (then a fixed non-reserved fallback), so a test can force a reserved
/// draw or a specific identifier.
#[derive(Debug)]
struct ScriptedTempSource {
    words: alloc::collections::VecDeque<[u8; 8]>,
}

impl ScriptedTempSource {
    fn new(words: &[[u8; 8]]) -> Self {
        Self {
            words: words.iter().copied().collect(),
        }
    }
}

impl TempAddrSource for ScriptedTempSource {
    fn fill_random(&mut self, out: &mut [u8]) {
        let word = self.words.pop_front().unwrap_or([0xAB; 8]);
        for chunk in out.chunks_mut(8) {
            let len = chunk.len();
            chunk.copy_from_slice(&word[..len]);
        }
    }
}

#[test]
fn privacy_off_forms_no_temporary_address() {
    let config = IfaceConfig {
        dad_transmits: 0,
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, temp_source(), t(0));
    iface.on_router_advertisement(&[infinite_prefix()], t(0));
    iface.advance(t(0));
    // The stable SLAAC address is present; no temporary one is.
    assert!(iface.is_assigned(slaac_addr()));
    assert!(temp_addrs(&iface).is_empty());
}

#[test]
fn privacy_on_forms_one_distinct_nonreserved_temporary_address() {
    let mut iface = Iface::new(&privacy_config(), Box::new(SeqTempSource::new()), t(0));
    iface.on_router_advertisement(&[infinite_prefix()], t(0));
    let actions = iface.advance(t(0));
    let temps = temp_addrs(&iface);
    assert_eq!(temps.len(), 1, "exactly one temporary address forms");
    let temp = temps[0];
    // DAD disabled: it is immediately usable and announced.
    assert!(!temp.tentative);
    assert!(actions
        .iter()
        .any(|a| matches!(a, IfaceAction::AddressPreferred { addr } if *addr == temp.addr)));
    // It shares the /64 prefix but uses a distinct, non-reserved
    // interface identifier (not the stable EUI-64 one).
    assert_eq!(temp.prefix_len, 64);
    assert_eq!(&temp.addr.octets()[..8], &slaac_addr().octets()[..8]);
    assert_ne!(temp.addr, slaac_addr());
    assert_ne!(&temp.addr.octets()[8..], &IID);
    assert_ne!(temp.addr.octets()[8..], [0u8; 8]);
    // The stable address remains alongside the temporary one.
    assert!(iface.is_assigned(slaac_addr()));
}

#[test]
fn a_reserved_temporary_identifier_is_skipped_and_redrawn() {
    // The desync draw (first 8 bytes) is any value; the first identifier
    // draw is the all-zero subnet-router anycast (reserved) and must be
    // rejected; the second is a good identifier.
    let good = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    let source = ScriptedTempSource::new(&[[0; 8], [0; 8], good]);
    let mut iface = Iface::new(&privacy_config(), Box::new(source), t(0));
    iface.on_router_advertisement(&[infinite_prefix()], t(0));
    iface.advance(t(0));
    let temps = temp_addrs(&iface);
    assert_eq!(temps.len(), 1);
    assert_eq!(&temps[0].addr.octets()[8..], &good);
}

#[test]
fn a_temporary_address_regenerates_before_it_deprecates() {
    let mut iface = Iface::new(&privacy_config(), Box::new(SeqTempSource::new()), t(0));
    iface.on_router_advertisement(&[infinite_prefix()], t(0));
    // Drive the engine forward at each deadline; the successor must be
    // formed while the predecessor is still preferred (two coexist), and
    // the table must never exceed its bound.
    let mut saw_two_preferred = false;
    let mut now = t(0);
    for _ in 0..200 {
        let out = iface.advance(now);
        let _ = out;
        let temps = temp_addrs(&iface);
        assert!(temps.len() <= MAX_IPV6_ADDRS);
        let preferred = temps
            .iter()
            .filter(|t| !t.tentative && !t.deprecated)
            .count();
        if temps.len() >= 2 && preferred >= 1 {
            saw_two_preferred = true;
        }
        match iface.next_deadline() {
            Some(d) => now = d,
            None => break,
        }
        if now.secs() > 300 {
            break;
        }
    }
    assert!(
        saw_two_preferred,
        "a fresh temporary address overlaps its predecessor at regeneration"
    );
}

#[test]
fn runtime_privacy_enable_forms_temporaries_and_disable_removes_them() {
    let config = IfaceConfig {
        dad_transmits: 0,
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, Box::new(SeqTempSource::new()), t(0));
    iface.on_router_advertisement(&[infinite_prefix()], t(0));
    iface.advance(t(0));
    assert!(temp_addrs(&iface).is_empty(), "privacy off: no temporary");

    // Enabling schedules an immediate maintenance pass.
    iface.set_privacy(true, t(1));
    assert_eq!(iface.next_deadline(), Some(t(1)));
    iface.advance(t(1));
    assert_eq!(temp_addrs(&iface).len(), 1, "enable forms a temporary");

    // Disabling removes every temporary but keeps the stable address.
    iface.set_privacy(false, t(2));
    assert!(temp_addrs(&iface).is_empty());
    assert!(iface.is_assigned(slaac_addr()));
}

#[test]
fn temporary_dad_failures_retry_a_bounded_number_of_times() {
    // Long temporary lifetimes so regeneration never interferes; DAD on
    // (so each temporary is tentative and can be failed).
    let config = IfaceConfig {
        privacy: true,
        dad_transmits: 1,
        temp_preferred_lifetime: t(10_000),
        temp_valid_lifetime: t(20_000),
        ..IfaceConfig::new(IID)
    };
    let mut iface = Iface::new(&config, Box::new(SeqTempSource::new()), t(0));
    iface.advance(t(0)); // link-local DAD solicit
    iface.advance(t(1)); // link-local preferred + RS
    iface.on_router_advertisement(&[infinite_prefix()], t(1));

    // Each maintenance pass forms one tentative temporary; fail its DAD.
    // After TEMP_IDGEN_RETRIES consecutive failures no more are formed.
    let mut now = t(1);
    let mut failures = 0u8;
    for _ in 0..(TEMP_IDGEN_RETRIES + 3) {
        iface.advance(now);
        let tentative_temp = temp_addrs(&iface).into_iter().find(|t| t.tentative);
        if let Some(temp) = tentative_temp {
            assert!(failures < TEMP_IDGEN_RETRIES, "no temporary past the cap");
            iface.on_dad_evidence(temp.addr);
            failures += 1;
        }
        now = Duration64::from_secs(now.secs() + 1);
    }
    assert_eq!(failures, TEMP_IDGEN_RETRIES, "retried exactly the cap");
    // No temporary address survives, and none is being formed anymore.
    assert!(temp_addrs(&iface).is_empty());
    iface.advance(Duration64::from_secs(now.secs() + 10));
    assert!(temp_addrs(&iface).is_empty(), "generation stays disabled");
}

#[test]
fn eui64_splits_the_oui_and_inverts_the_universal_local_bit() {
    // A locally-administered example MAC: the u/l bit (0x02) of the first
    // octet flips, FF:FE fills the middle, and the low 24 bits pass
    // through — the RFC 4291 Appendix A construction.
    assert_eq!(
        eui64_interface_id([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
        [0x50, 0x54, 0x00, 0xFF, 0xFE, 0x12, 0x34, 0x56],
    );
    // A globally-unique MAC (u/l bit clear) gains the bit.
    assert_eq!(
        eui64_interface_id([0x00, 0x0C, 0x29, 0xAB, 0xCD, 0xEF]),
        [0x02, 0x0C, 0x29, 0xFF, 0xFE, 0xAB, 0xCD, 0xEF],
    );
}

//! Unit tests for the interface address engine.

use super::*;

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
    let mut iface = Iface::new(&IfaceConfig::new(IID), t(0));
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
    let mut iface = Iface::new(&config, t(0));
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
    let mut iface = Iface::new(&IfaceConfig::new(IID), t(0));
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
    let mut iface = Iface::new(&IfaceConfig::new(IID), t(0));
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
    let mut iface = Iface::new(&config, t(0));
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
    let mut iface = Iface::new(&IfaceConfig::new(IID), t(0));
    assert_eq!(iface.next_deadline(), Some(t(0)));
    iface.advance(t(0));
    assert_eq!(iface.next_deadline(), Some(t(1)));
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

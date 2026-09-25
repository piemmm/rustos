//! Host tests for vetting an offer against what the kernel attests.
//!
//! [`vet_offer`] takes no device and no window: an offer it refuses is refused
//! before the composer maps anything the offer names or reaches any device.

use alloc::vec::Vec;

use super::{granted_resource, vet_offer, OfferRefusal};

use tairix_abi::hwtree::{GrantedResource, HwResource};
use tairix_abi::raid_ipc::{MemberOffer, RAID_CANDIDATE_COMPATIBLE, RAID_MEMBER_COMPATIBLE};
use tairix_abi::{HwDeviceClass, HwMatchKey, HwNode};

/// The member device's block-service endpoint.
const ENDPOINT: u64 = 0x0056_424B_0000_0003;
/// The region id of the member device's data window.
const REGION: u64 = 0x2A;
/// The composer's own grant handle for that window.
const WINDOW_GRANT: u64 = 7;

/// A node bearing `compatible` and declaring `resources`.
fn node(compatible: &[u8], resources: &[HwResource]) -> HwNode {
    let mut node = HwNode::new(31, 30, HwDeviceClass::Storage);
    node.push_match_key(HwMatchKey::compatible(compatible).expect("fits"))
        .expect("room for the key");
    for resource in resources {
        node.push_resource(*resource)
            .expect("room for the resource");
    }
    node
}

/// The node the volume manager emits for a member device.
fn member_node() -> HwNode {
    node(
        RAID_MEMBER_COMPATIBLE,
        &[HwResource::endpoint(ENDPOINT), HwResource::shared(REGION)],
    )
}

/// The offer that device's agent posts.
fn genuine_offer() -> MemberOffer {
    MemberOffer {
        endpoint: ENDPOINT,
        window_grant: WINDOW_GRANT,
        node: 31,
    }
}

#[test]
fn a_genuine_offer_is_admitted() {
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            Some(&member_node()),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Ok(())
    );
    let candidate = node(
        RAID_CANDIDATE_COMPATIBLE,
        &[HwResource::endpoint(ENDPOINT), HwResource::shared(REGION)],
    );
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            Some(&candidate),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Ok(()),
        "a blank candidate reaches the composer the same way"
    );
}

#[test]
fn an_offer_from_a_task_no_member_node_admitted_is_refused() {
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            None,
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Err(OfferRefusal::NotAMemberNode),
        "a sender that is no driver loaded for a node"
    );
    let disk = node(
        b"tairix,block-device",
        &[HwResource::endpoint(ENDPOINT), HwResource::shared(REGION)],
    );
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            Some(&disk),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Err(OfferRefusal::NotAMemberNode),
        "the driver of any other node, even one holding the same transport"
    );
}

#[test]
fn an_offer_claiming_a_node_other_than_its_senders_is_refused() {
    let forged = MemberOffer {
        node: 32,
        ..genuine_offer()
    };
    assert_eq!(
        vet_offer(
            &forged,
            Some(&member_node()),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Err(OfferRefusal::NodeMismatch),
        "an administrator names a device by its node, so that name must be the kernel's"
    );
}

#[test]
fn an_offer_naming_an_endpoint_its_node_does_not_declare_is_refused() {
    let forged = MemberOffer {
        endpoint: ENDPOINT + 1,
        ..genuine_offer()
    };
    assert_eq!(
        vet_offer(
            &forged,
            Some(&member_node()),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Err(OfferRefusal::EndpointMismatch)
    );
    let two_endpoints = node(
        RAID_MEMBER_COMPATIBLE,
        &[
            HwResource::endpoint(ENDPOINT),
            HwResource::endpoint(ENDPOINT + 1),
            HwResource::shared(REGION),
        ],
    );
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            Some(&two_endpoints),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Err(OfferRefusal::EndpointMismatch),
        "a node declaring more than one endpoint names no one transport"
    );
}

#[test]
fn an_offer_naming_the_composers_own_endpoint_is_refused() {
    // Driving an endpoint the composer serves as a member would post to it
    // and wait for a reply only the composer can give.
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            Some(&member_node()),
            Some(HwResource::shared(REGION)),
            |endpoint| endpoint == ENDPOINT
        ),
        Err(OfferRefusal::OwnEndpoint)
    );
}

#[test]
fn an_offer_whose_window_grant_names_another_region_is_refused() {
    for window in [
        Some(HwResource::shared(REGION + 1)),
        Some(HwResource::endpoint(REGION)),
        None,
    ] {
        assert_eq!(
            vet_offer(&genuine_offer(), Some(&member_node()), window, |_| false),
            Err(OfferRefusal::WindowMismatch),
            "{window:?}"
        );
    }
    let two_regions = node(
        RAID_MEMBER_COMPATIBLE,
        &[
            HwResource::endpoint(ENDPOINT),
            HwResource::shared(REGION),
            HwResource::shared(REGION + 1),
        ],
    );
    assert_eq!(
        vet_offer(
            &genuine_offer(),
            Some(&two_regions),
            Some(HwResource::shared(REGION)),
            |_| false
        ),
        Err(OfferRefusal::WindowMismatch)
    );
}

#[test]
fn a_grant_handle_is_resolved_from_the_callers_own_grant_records() {
    let mut table = Vec::new();
    for (handle, resource) in [
        (1, HwResource::endpoint(ENDPOINT)),
        (WINDOW_GRANT, HwResource::shared(REGION)),
    ] {
        table.extend_from_slice(&GrantedResource::new(handle, resource).to_le_bytes());
    }
    assert_eq!(
        granted_resource(&table, WINDOW_GRANT),
        Some(HwResource::shared(REGION))
    );
    assert_eq!(
        granted_resource(&table, 2),
        None,
        "a handle with no record names nothing"
    );
    assert_eq!(
        granted_resource(&table[..table.len() - 1], WINDOW_GRANT),
        None,
        "a torn record is never read"
    );
}

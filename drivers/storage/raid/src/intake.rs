//! Whether an offer comes from the driver of a member node and names exactly
//! that node's transport.
//!
//! Any holder of `CAP_SHM` may post to the rendezvous, so an offer proves
//! nothing by itself. What the composer believes is what the kernel attests:
//! the node the sender was admitted for (`call_peer_node`) and what the
//! composer's own grant for the offered window names (`resource_grants`). An
//! offer is admitted only when that node is a member or candidate node and the
//! one the offer names, its one declared endpoint is the offered one and not
//! one the composer serves itself, and the composer's window grant names the
//! node's one declared region. Every verdict here is reached before the offered window is mapped
//! or any device is reached.

use tairix_abi::hwtree::{GrantedResource, HwResource, HwResourceKind};
use tairix_abi::raid_ipc::{MemberOffer, RAID_CANDIDATE_COMPATIBLE, RAID_MEMBER_COMPATIBLE};
use tairix_abi::HwNode;

/// Why an offer was refused before anything it names was touched.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OfferRefusal {
    /// The kernel names no node for the sender, or one that is neither a
    /// member nor a candidate node.
    NotAMemberNode,
    /// The offer claims a node other than the one the kernel attests, and an
    /// administrator names a device by its node.
    NodeMismatch,
    /// The node does not declare exactly one endpoint, or declares one other
    /// than the offered endpoint.
    EndpointMismatch,
    /// The offered endpoint is one the composer serves itself, so driving it
    /// as a member would wait on the composer's own reply.
    OwnEndpoint,
    /// The node does not declare exactly one shared region, or the composer's
    /// grant for the offered window names another resource.
    WindowMismatch,
}

/// Vet `offer` against the kernel-attested `node` its sender was admitted
/// for and the resource `window` the composer's own grant for the offered
/// window names. `serves` answers whether the composer serves an endpoint.
///
/// # Errors
///
/// The first [`OfferRefusal`] the offer earns.
pub fn vet_offer(
    offer: &MemberOffer,
    node: Option<&HwNode>,
    window: Option<HwResource>,
    serves: impl FnOnce(u64) -> bool,
) -> Result<(), OfferRefusal> {
    let node = node
        .filter(|node| is_member_node(node))
        .ok_or(OfferRefusal::NotAMemberNode)?;
    if node.id() != offer.node {
        return Err(OfferRefusal::NodeMismatch);
    }
    let endpoint = sole(node.resources(), HwResourceKind::Endpoint)
        .filter(|endpoint| endpoint.base() == offer.endpoint)
        .ok_or(OfferRefusal::EndpointMismatch)?;
    if serves(endpoint.base()) {
        return Err(OfferRefusal::OwnEndpoint);
    }
    sole(node.resources(), HwResourceKind::Shared)
        .filter(|region| window == Some(*region))
        .map(|_| ())
        .ok_or(OfferRefusal::WindowMismatch)
}

/// The resource the grant `handle` names in `table`, a caller's own grant
/// records as `resource_grants` delivers them, or [`None`] when no record
/// there is that handle's.
#[must_use]
pub fn granted_resource(table: &[u8], handle: u64) -> Option<HwResource> {
    table
        .as_chunks::<{ GrantedResource::WIRE_LEN }>()
        .0
        .iter()
        .filter_map(|record| GrantedResource::from_bytes(record.as_slice()).ok())
        .find(|record| record.handle == handle)
        .map(|record| record.resource)
}

fn is_member_node(node: &HwNode) -> bool {
    node.match_keys().iter().any(|key| {
        let compatible = key.compatible_bytes();
        compatible == RAID_MEMBER_COMPATIBLE || compatible == RAID_CANDIDATE_COMPATIBLE
    })
}

/// The one resource of `kind` among `resources`, or [`None`] when there is
/// none or more than one.
fn sole(resources: &[HwResource], kind: HwResourceKind) -> Option<HwResource> {
    let mut of_kind = resources
        .iter()
        .filter(|resource| resource.kind() == Some(kind));
    let only = of_kind.next().copied()?;
    of_kind.next().is_none().then_some(only)
}

#[cfg(test)]
mod tests;

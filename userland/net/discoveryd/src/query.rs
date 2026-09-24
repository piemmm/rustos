//! What a typed request asks the segment, and the authority it needs.
//!
//! Every name the service asks is derived here from the request's fields, so
//! the type a browse is scoped to is the field the grant is checked against,
//! never something inferred from a name the caller spelled.

use tairix_abi::discovery_ipc::Query;
use tairix_abi::Errno;
use tairix_inline::ArrayVec;
use tairix_net::dns::Name;
use tairix_net::dnssd::{InstanceName, ServiceInstance, ServiceType};
use tairix_net::mdns::{is_link_local_address, is_link_local_name, LOCAL_LABEL};

use crate::wire::Form;

/// The name the RFC 6763 §9 type enumeration asks about.
const SERVICES_NAME: &str = "_services._dns-sd._udp.local";

/// The authority a query needs, beyond the `CAP_NET` every session needs.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Needs {
    /// Nothing more: it names one host or address, as unicast resolution
    /// does.
    Nothing,
    /// A grant for this type, or `CAP_NET_DISCOVER_ALL`.
    Type(ServiceType),
    /// `CAP_NET_DISCOVER_ALL` alone.
    Everything,
}

/// One question a query asks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Asking {
    /// How its answers are read.
    pub form: Form,
    /// The name asked about.
    pub name: Name,
}

/// A query's questions and the authority they need.
#[derive(Debug)]
pub struct Plan {
    /// The authority.
    pub needs: Needs,
    /// The questions, one or two.
    pub asks: ArrayVec<Asking, 2>,
}

/// Plan `query`.
///
/// # Errors
///
/// [`Errno::OutOfRange`] for a service name outside RFC 6335, an instance
/// label outside RFC 6763, a host name not under `local`, or a reverse lookup
/// of an address that is not link-local — none of which multicast DNS
/// answers for.
pub fn plan(query: &Query<'_>) -> Result<Plan, Errno> {
    let local = Name::from_labels(&[LOCAL_LABEL]).map_err(|_| Errno::OutOfRange)?;
    let mut asks = ArrayVec::new();
    let mut ask = |form, name| {
        asks.try_push(Asking { form, name })
            .map_err(|_| Errno::OutOfRange)
    };
    let needs = match *query {
        Query::Browse { service } => {
            let service =
                ServiceType::new(service.name, service.transport).map_err(|_| Errno::OutOfRange)?;
            ask(
                Form::Instance,
                service.to_name(&local).map_err(|_| Errno::OutOfRange)?,
            )?;
            Needs::Type(service)
        }
        Query::Resolve { instance, service } => {
            let service =
                ServiceType::new(service.name, service.transport).map_err(|_| Errno::OutOfRange)?;
            let instance = InstanceName::new(instance).map_err(|_| Errno::OutOfRange)?;
            let name = ServiceInstance::new(instance, service, local)
                .and_then(|instance| instance.to_name())
                .map_err(|_| Errno::OutOfRange)?;
            ask(Form::Service, name)?;
            ask(Form::Text, name)?;
            Needs::Type(service)
        }
        Query::Host { name, families } => {
            let name = Name::from_wire(name).ok_or(Errno::OutOfRange)?;
            if !is_link_local_name(&name) {
                return Err(Errno::OutOfRange);
            }
            if families.v4 {
                ask(Form::AddressV4, name)?;
            }
            if families.v6 {
                ask(Form::AddressV6, name)?;
            }
            Needs::Nothing
        }
        Query::Reverse { address } => {
            if !is_link_local_address(address) {
                return Err(Errno::OutOfRange);
            }
            ask(Form::Pointer, Name::reverse(address))?;
            Needs::Nothing
        }
        Query::Types => {
            ask(
                Form::Type,
                Name::encode(SERVICES_NAME).map_err(|_| Errno::OutOfRange)?,
            )?;
            Needs::Everything
        }
    };
    Ok(Plan { needs, asks })
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;

//! The browse grants a request is admitted against: the store at
//! [`DISCOVERY_POLICY_PATH`](tairix_abi::discovery_policy::DISCOVERY_POLICY_PATH),
//! read once at start and held as the attested identities it names.

use alloc::vec::Vec;

use tairix_abi::discovery_policy::read_grants;
use tairix_abi::{AppIdentity, Errno};
use tairix_net::dnssd::ServiceType;

/// Which application may browse for which service type.
#[derive(Default)]
pub struct Grants {
    entries: Vec<(AppIdentity, ServiceType)>,
}

impl Grants {
    /// The grants `store` records, taken whole or not at all: a line the
    /// codec refuses, a service name outside RFC 6335, or an identity the
    /// kernel could never attest refuses the store, so the service grants
    /// nothing rather than whatever of it parsed.
    ///
    /// # Errors
    ///
    /// The codec's refusal, [`Errno::OutOfRange`] for a grant naming no
    /// attestable identity or no valid service type, and
    /// [`Errno::OutOfMemory`] when the table cannot grow.
    pub fn load(store: &[u8]) -> Result<Self, Errno> {
        let mut entries = Vec::new();
        read_grants(store, &mut |grant| {
            let app = AppIdentity::new(grant.bundle_id, grant.publisher)?;
            let service = ServiceType::new(grant.service.name, grant.service.transport)
                .map_err(|_| Errno::OutOfRange)?;
            entries.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
            entries.push((app, service));
            Ok(())
        })?;
        Ok(Self { entries })
    }

    /// Whether `app` may browse for `service`.
    #[must_use]
    pub fn allows(&self, app: &AppIdentity, service: &ServiceType) -> bool {
        self.entries
            .iter()
            .any(|(granted, to)| granted == app && to == service)
    }
}

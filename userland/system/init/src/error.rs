//! Error and failure types surfaced by the [`Init`](crate::Init) manager.
//!
//! Two distinct vocabularies are deliberately kept apart:
//!
//! * [`InitError`] is a **graph-level, fail-closed** error returned by
//!   [`Init::register`](crate::Init::register) and
//!   [`Init::start_all`](crate::Init::start_all). It signals a structural
//!   defect in the registered service set — a duplicate name, a dependency
//!   on an unregistered service, or a cycle — that prevents *any* service
//!   from coming up. The system does not boot a
//!   partial, surprising configuration.
//! * [`StartFailure`] is a **per-service** outcome recorded in the
//!   [`StartReport`](crate::StartReport). When the graph is sound, init
//!   brings up every service it can; a single service that fails (and the
//!   dependents it blocks) is reported here without aborting the services
//!   that are independent of it.

use core::fmt;

use rustos_abi::Errno;

/// A structural defect in the registered service set that prevents bring-up.
///
/// Every variant is a fail-closed refusal: [`Init`](crate::Init) reports it
/// and starts nothing, rather than launch an incomplete system.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InitError {
    /// Two services were registered under the same name.
    DuplicateService,
    /// A service declares a dependency on a name that is not registered.
    DependencyMissing,
    /// The dependency graph contains a cycle, so no total start order exists.
    DependencyCycle,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::DuplicateService => "a service with this name is already registered",
            Self::DependencyMissing => "a service depends on an unregistered service",
            Self::DependencyCycle => "the service dependency graph contains a cycle",
        };
        f.write_str(message)
    }
}

/// Why a single service was not started during an otherwise-valid bring-up.
///
/// Recorded in [`StartReport::failed`](crate::StartReport); never aborts the
/// services that do not depend on the failed one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StartFailure {
    /// The service's signed manifest could not be decoded into a requested
    /// capability set; the wrapped [`Errno`] is the decode error verbatim.
    ManifestInvalid(Errno),
    /// The manifest requests a capability the system authority does not
    /// hold. Granting it would widen authority, so the service is refused.
    CapabilityEscalation,
    /// The [`Spawner`](crate::Spawner) refused to launch the service; the
    /// wrapped [`Errno`] is the spawner's error verbatim.
    SpawnFailed(Errno),
    /// A dependency of this service failed to start, so it was skipped.
    DependencyFailed,
}

impl fmt::Display for StartFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestInvalid(e) => write!(f, "manifest invalid: {e}"),
            Self::CapabilityEscalation => {
                f.write_str("manifest requests capabilities the system authority does not hold")
            }
            Self::SpawnFailed(e) => write!(f, "spawn failed: {e}"),
            Self::DependencyFailed => f.write_str("a dependency failed to start"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InitError, StartFailure};
    use rustos_abi::Errno;

    extern crate alloc;
    use alloc::format;

    #[test]
    fn init_error_display_is_stable() {
        assert_eq!(
            format!("{}", InitError::DependencyCycle),
            "the service dependency graph contains a cycle",
        );
    }

    #[test]
    fn start_failure_display_wraps_errno() {
        assert_eq!(
            format!("{}", StartFailure::SpawnFailed(Errno::NotFound)),
            "spawn failed: not found",
        );
        assert_eq!(
            format!("{}", StartFailure::CapabilityEscalation),
            "manifest requests capabilities the system authority does not hold",
        );
    }
}

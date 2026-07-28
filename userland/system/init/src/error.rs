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

use tairix_abi::Errno;

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

/// Why a readiness notification was refused.
///
/// A notice is only ever *attributed* to a service the manager already
/// spawned; these are the fail-closed reasons a well-formed notice is still
/// not acted on. The notice is ignored (never trusted), the reason audited.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NotifyError {
    /// The notice was attributed to a name the manager does not know. In
    /// production the manager maps the kernel-attested sender to a service,
    /// so this is wire corruption or a stale sender, never a normal path.
    UnknownService,
    /// The named service is not in the `starting` state, so it has no
    /// pending readiness edge to resolve — a service cannot become ready
    /// before it is spawned, nor announce readiness twice. The notice is a
    /// protocol violation and is dropped.
    NotStarting,
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnknownService => "readiness notice names an unknown service",
            Self::NotStarting => "readiness notice for a service that is not starting",
        };
        f.write_str(message)
    }
}

/// Why an on-demand endpoint-activation connect request was refused.
///
/// Every variant is a fail-closed refusal audited by the manager: a connect
/// that does not succeed grants the client nothing (no partial connection,
/// no ambient authority). The capability check runs before any state is
/// touched, so [`Denied`](Self::Denied) is reported without the service
/// being started.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActivateError {
    /// The connect named a service the manager does not have registered.
    /// Presence on disk never grants activation, and an unregistered name
    /// is never activated (fail closed).
    UnknownService,
    /// The client does not hold the capability the service's endpoint
    /// requires. Refused before the service is touched (capability check
    /// before state).
    Denied,
    /// The service cannot be activated right now: a readiness condition it
    /// requires is unsatisfied (for example a GUI-only service on a headless
    /// system), or it is mid-teardown or terminally failed. The client fails
    /// closed and may retry once the condition holds.
    Unavailable,
    /// The service's pending-connection queue is full. Bounded and
    /// fail-closed against a connect flood: the request is refused rather
    /// than growing the queue without limit (never dropped silently, never
    /// spun on).
    QueueFull,
    /// The service could not be launched: its manifest failed to decode,
    /// requested authority the manager lacks, or the spawn was refused. The
    /// underlying [`StartFailure`] is recorded in the audit log.
    NotActivatable,
}

impl fmt::Display for ActivateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnknownService => "connect names an unregistered service",
            Self::Denied => "client lacks the capability the endpoint requires",
            Self::Unavailable => "the service cannot be activated in its current state",
            Self::QueueFull => "the service's pending-connection queue is full",
            Self::NotActivatable => "the service could not be launched",
        };
        f.write_str(message)
    }
}

#[cfg(test)]
mod tests {
    use super::{InitError, StartFailure};
    use tairix_abi::Errno;

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

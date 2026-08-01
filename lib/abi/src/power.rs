//! Machine power-state vocabulary: the argument of the `system_power`
//! syscall ([`crate::SyscallNumber::SYSTEM_POWER`]).
//!
//! Ending the machine's current power state is the most consequential act a
//! program can ask the kernel to perform: it terminates every task of every
//! principal at once. It is therefore a single, closed, capability-gated
//! vocabulary rather than a free-form request — the kernel decodes the
//! caller's register into exactly one [`PowerAction`] and refuses anything
//! it does not recognise, so a zeroed or garbage register can never resolve
//! to "power the machine off".
//!
//! The authority is [`crate::CapabilityId::SYSTEM_POWER`], checked by the
//! dispatcher before the handler touches any state, and every call is
//! audited. There is no ambient path: holding a seat, owning the console, or
//! running as the system user grants nothing here.

use crate::Errno;

/// The power state the caller asks the machine to move to.
///
/// A closed set: these are the two transitions the Arch HAL's platform
/// primitives can actually perform (firmware power-off and platform reset).
/// The discriminant is the `u32` carried in the syscall's `action` register;
/// `0` is reserved and never valid, so a zeroed register fails closed rather
/// than naming a transition the caller did not ask for.
///
/// A platform whose firmware exposes no primitive for the requested
/// transition answers [`Errno::NotSupported`] and leaves the machine
/// running — an honest refusal, never a silent halt or a guessed
/// chipset-specific poke.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PowerAction {
    /// Flush every mounted volume, then power the machine off.
    PowerOff = 1,
    /// Flush every mounted volume, then reset the machine.
    Restart = 2,
}

impl PowerAction {
    /// The discriminant carried on the wire.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a [`PowerAction`] from its wire discriminant.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] for any value that is not a defined
    /// action (including the reserved `0`), so an unknown or zeroed register
    /// fails closed rather than being interpreted as a transition the caller
    /// did not name.
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::PowerOff),
            2 => Ok(Self::Restart),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// A short, stable, lower-case name for logs and diagnostics.
    ///
    /// One definition shared by the kernel's audit record and every
    /// first-party caller that reports the transition it asked for, so the
    /// trail and the message can never disagree on the spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PowerOff => "power-off",
            Self::Restart => "restart",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_are_frozen() {
        assert_eq!(PowerAction::PowerOff.as_u32(), 1);
        assert_eq!(PowerAction::Restart.as_u32(), 2);
    }

    #[test]
    fn round_trips_every_defined_action() {
        for action in [PowerAction::PowerOff, PowerAction::Restart] {
            assert_eq!(PowerAction::from_u32(action.as_u32()), Ok(action));
        }
    }

    #[test]
    fn reserved_zero_and_unknown_values_fail_closed() {
        assert_eq!(PowerAction::from_u32(0), Err(Errno::OutOfRange));
        assert_eq!(PowerAction::from_u32(3), Err(Errno::OutOfRange));
        assert_eq!(PowerAction::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn names_are_distinct_and_stable() {
        assert_eq!(PowerAction::PowerOff.name(), "power-off");
        assert_eq!(PowerAction::Restart.name(), "restart");
    }
}

//! Telnet option codes and the RFC 1143 option-negotiation state machine.
//!
//! RFC 855 negotiation is symmetric and unsequenced, so a naive "reply to
//! every request" client loops forever against a peer that does the same. The
//! Q Method of RFC 1143 is the fix and is what this module implements: each
//! option carries two six-valued states — one for our side (`WILL`/`WONT`) and
//! one for the peer's (`DO`/`DONT`) — with a queued-request bit, so a reply is
//! sent only for a genuine *change* of state and no exchange can cycle.
//!
//! The option set is closed. Anything outside [`SUPPORTED`] is refused, which
//! is the complete and correct RFC 855 behaviour for an option a client does
//! not implement — never silent acceptance of a surface it cannot honour.

use alloc::vec::Vec;

use crate::nvt::{self, DO, DONT, WILL, WONT};

/// Transmit and receive 8-bit data with no NVT line convention (RFC 856).
pub const BINARY: u8 = 0;
/// The peer echoes what we send (RFC 857).
pub const ECHO: u8 = 1;
/// Suppress the half-duplex Go Ahead, i.e. run full duplex (RFC 858).
pub const SUPPRESS_GO_AHEAD: u8 = 3;
/// Exchange the current option status (RFC 859).
pub const STATUS: u8 = 5;
/// The timing mark (RFC 860).
pub const TIMING_MARK: u8 = 6;
/// Forcibly log the session out (RFC 727).
pub const LOGOUT: u8 = 18;
/// Report the terminal type (RFC 1091).
pub const TERMINAL_TYPE: u8 = 24;
/// Negotiate About Window Size (RFC 1073).
pub const NAWS: u8 = 31;
/// Report the terminal's line speed (RFC 1079).
pub const TERMINAL_SPEED: u8 = 32;
/// Remote flow-control toggling (RFC 1080).
pub const TOGGLE_FLOW_CONTROL: u8 = 33;
/// Client-side line editing under server control (RFC 1184).
pub const LINEMODE: u8 = 34;
/// Exchange environment variables (RFC 1572).
pub const NEW_ENVIRON: u8 = 39;

/// The closed set of options this client implements, in ascending code order.
///
/// `LOGOUT` is deliberately absent: the `logout` command *sends* `DO LOGOUT`
/// as a one-shot request, and a server asking *us* to log out has nothing to
/// act on, so the client neither offers nor accepts it.
pub const SUPPORTED: &[u8] = &[
    BINARY,
    ECHO,
    SUPPRESS_GO_AHEAD,
    STATUS,
    TIMING_MARK,
    TERMINAL_TYPE,
    NAWS,
    TERMINAL_SPEED,
    TOGGLE_FLOW_CONTROL,
    LINEMODE,
    NEW_ENVIRON,
];

/// The human name of an option code, for `display`, `status`, and the
/// `send do`/`toggle options` surfaces. Returns [`None`] for a code this
/// client has no name for, so a trace prints the number honestly rather than
/// inventing a label.
#[must_use]
pub const fn option_name(option: u8) -> Option<&'static str> {
    Some(match option {
        BINARY => "BINARY",
        ECHO => "ECHO",
        SUPPRESS_GO_AHEAD => "SUPPRESS GO AHEAD",
        STATUS => "STATUS",
        TIMING_MARK => "TIMING MARK",
        LOGOUT => "LOGOUT",
        TERMINAL_TYPE => "TERMINAL TYPE",
        NAWS => "NAWS",
        TERMINAL_SPEED => "TERMINAL SPEED",
        TOGGLE_FLOW_CONTROL => "TOGGLE FLOW CONTROL",
        LINEMODE => "LINEMODE",
        NEW_ENVIRON => "NEW-ENVIRON",
        _ => return None,
    })
}

/// One side's RFC 1143 option state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Q {
    /// Disabled and not being negotiated.
    #[default]
    No,
    /// Enabled.
    Yes,
    /// A disable request is outstanding.
    WantNoEmpty,
    /// A disable request is outstanding, with a re-enable queued behind it.
    WantNoOpposite,
    /// An enable request is outstanding.
    WantYesEmpty,
    /// An enable request is outstanding, with a disable queued behind it.
    WantYesOpposite,
}

impl Q {
    /// Whether the option is enabled *now* (a request in flight does not count).
    #[must_use]
    pub const fn enabled(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Why a negotiation exchange could not be carried out as the peer or the
/// caller asked. Every variant leaves the state machine consistent; none is
/// fatal to the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NegotiationFault {
    /// The peer answered a `DONT`/`WONT` with a `WILL`/`DO` (RFC 1143's
    /// "error: DONT answered by WILL"). The state machine resynchronises.
    AnsweredWrongWay,
    /// A local request asked for a state the option is already in.
    AlreadyThere,
    /// A local request duplicated one already queued.
    AlreadyQueued,
}

/// What folding one received negotiation produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    /// The reply to transmit, if the exchange calls for one.
    pub reply: Option<(u8, u8)>,
    /// Whether this exchange left the option newly enabled or newly disabled
    /// (so the caller can act — send the initial NAWS, tear LINEMODE down).
    pub changed: Option<bool>,
    /// A protocol irregularity worth reporting under `toggle options`.
    pub fault: Option<NegotiationFault>,
}

impl Outcome {
    /// Nothing to send, nothing changed.
    const fn quiet() -> Self {
        Self {
            reply: None,
            changed: None,
            fault: None,
        }
    }

    const fn send(verb: u8, option: u8) -> Self {
        Self {
            reply: Some((verb, option)),
            changed: None,
            fault: None,
        }
    }

    const fn with_change(mut self, enabled: bool) -> Self {
        self.changed = Some(enabled);
        self
    }

    const fn with_fault(mut self, fault: NegotiationFault) -> Self {
        self.fault = Some(fault);
        self
    }
}

/// The per-option negotiation state for one connection.
///
/// Indexed by option code, so lookup is a bounded array access with no
/// allocation and no hash of attacker-chosen keys.
#[derive(Debug)]
pub struct Options {
    /// Our own side: whether *we* have the option enabled (`WILL`/`WONT`).
    us: [Q; 256],
    /// The peer's side: whether *it* has the option enabled (`DO`/`DONT`).
    him: [Q; 256],
    /// Options we will agree to enable on our side when asked.
    offer: [bool; 256],
    /// Options we will agree to see enabled on the peer's side when offered.
    accept: [bool; 256],
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

impl Options {
    /// A fresh table with every option disabled, and the supported set marked
    /// agreeable in both directions.
    #[must_use]
    pub fn new() -> Self {
        let mut table = Self {
            us: [Q::No; 256],
            him: [Q::No; 256],
            offer: [false; 256],
            accept: [false; 256],
        };
        for &option in SUPPORTED {
            table.offer[usize::from(option)] = true;
            table.accept[usize::from(option)] = true;
        }
        // `ECHO` is the server's to perform, not ours: a client that echoed
        // for the server would double every character. We accept `WILL ECHO`
        // and never offer it.
        table.offer[usize::from(ECHO)] = false;
        table
    }

    /// Our side's state for `option`.
    #[must_use]
    pub const fn us(&self, option: u8) -> Q {
        self.us[option as usize]
    }

    /// The peer's side's state for `option`.
    #[must_use]
    pub const fn him(&self, option: u8) -> Q {
        self.him[option as usize]
    }

    /// Whether `option` is enabled on our side right now.
    #[must_use]
    pub const fn local(&self, option: u8) -> bool {
        self.us[option as usize].enabled()
    }

    /// Whether `option` is enabled on the peer's side right now.
    #[must_use]
    pub const fn remote(&self, option: u8) -> bool {
        self.him[option as usize].enabled()
    }

    /// Stop agreeing to enable `option` on our side, and stop accepting it on
    /// the peer's. Used by `-E`-class policy switches and by the `toggle`
    /// commands that turn an option off for the rest of the session.
    pub fn refuse(&mut self, option: u8) {
        self.offer[usize::from(option)] = false;
        self.accept[usize::from(option)] = false;
    }

    /// Resume agreeing to `option` in both directions.
    pub fn permit(&mut self, option: u8) {
        self.offer[usize::from(option)] = true;
        self.accept[usize::from(option)] = true;
    }

    /// Fold a received `WILL <option>` (RFC 1143's `him` side).
    pub fn on_will(&mut self, option: u8) -> Outcome {
        let index = usize::from(option);
        match self.him[index] {
            Q::No => {
                if self.accept[index] {
                    self.him[index] = Q::Yes;
                    Outcome::send(DO, option).with_change(true)
                } else {
                    Outcome::send(DONT, option)
                }
            }
            Q::Yes => Outcome::quiet(),
            Q::WantNoEmpty => {
                self.him[index] = Q::No;
                Outcome::quiet().with_fault(NegotiationFault::AnsweredWrongWay)
            }
            Q::WantNoOpposite => {
                self.him[index] = Q::Yes;
                Outcome::quiet()
                    .with_change(true)
                    .with_fault(NegotiationFault::AnsweredWrongWay)
            }
            Q::WantYesEmpty => {
                self.him[index] = Q::Yes;
                Outcome::quiet().with_change(true)
            }
            Q::WantYesOpposite => {
                self.him[index] = Q::WantNoEmpty;
                Outcome::send(DONT, option)
            }
        }
    }

    /// Fold a received `WONT <option>`.
    pub fn on_wont(&mut self, option: u8) -> Outcome {
        let index = usize::from(option);
        match self.him[index] {
            Q::No => Outcome::quiet(),
            Q::Yes => {
                self.him[index] = Q::No;
                Outcome::send(DONT, option).with_change(false)
            }
            Q::WantNoEmpty | Q::WantYesEmpty | Q::WantYesOpposite => {
                self.him[index] = Q::No;
                Outcome::quiet()
            }
            Q::WantNoOpposite => {
                self.him[index] = Q::WantYesEmpty;
                Outcome::send(DO, option)
            }
        }
    }

    /// Fold a received `DO <option>` (RFC 1143's `us` side).
    pub fn on_do(&mut self, option: u8) -> Outcome {
        let index = usize::from(option);
        match self.us[index] {
            Q::No => {
                if self.offer[index] {
                    self.us[index] = Q::Yes;
                    Outcome::send(WILL, option).with_change(true)
                } else {
                    Outcome::send(WONT, option)
                }
            }
            Q::Yes => Outcome::quiet(),
            Q::WantNoEmpty => {
                self.us[index] = Q::No;
                Outcome::quiet().with_fault(NegotiationFault::AnsweredWrongWay)
            }
            Q::WantNoOpposite => {
                self.us[index] = Q::Yes;
                Outcome::quiet()
                    .with_change(true)
                    .with_fault(NegotiationFault::AnsweredWrongWay)
            }
            Q::WantYesEmpty => {
                self.us[index] = Q::Yes;
                Outcome::quiet().with_change(true)
            }
            Q::WantYesOpposite => {
                self.us[index] = Q::WantNoEmpty;
                Outcome::send(WONT, option)
            }
        }
    }

    /// Fold a received `DONT <option>`.
    pub fn on_dont(&mut self, option: u8) -> Outcome {
        let index = usize::from(option);
        match self.us[index] {
            Q::No => Outcome::quiet(),
            Q::Yes => {
                self.us[index] = Q::No;
                Outcome::send(WONT, option).with_change(false)
            }
            Q::WantNoEmpty | Q::WantYesEmpty | Q::WantYesOpposite => {
                self.us[index] = Q::No;
                Outcome::quiet()
            }
            Q::WantNoOpposite => {
                self.us[index] = Q::WantYesEmpty;
                Outcome::send(WILL, option)
            }
        }
    }

    /// Ask the peer to enable `option` on its side, appending the `DO` to
    /// `out` when the request is new.
    ///
    /// # Errors
    ///
    /// A [`NegotiationFault`] when the option is already in the asked-for
    /// state or the request is already queued; nothing is transmitted.
    pub fn ask_remote_enable(
        &mut self,
        option: u8,
        out: &mut Vec<u8>,
    ) -> Result<(), NegotiationFault> {
        let index = usize::from(option);
        match self.him[index] {
            Q::No => {
                self.him[index] = Q::WantYesEmpty;
                nvt::push_negotiate(DO, option, out);
                Ok(())
            }
            Q::Yes => Err(NegotiationFault::AlreadyThere),
            Q::WantNoEmpty => {
                self.him[index] = Q::WantNoOpposite;
                Ok(())
            }
            Q::WantNoOpposite | Q::WantYesEmpty => Err(NegotiationFault::AlreadyQueued),
            Q::WantYesOpposite => {
                self.him[index] = Q::WantYesEmpty;
                Ok(())
            }
        }
    }

    /// Ask the peer to disable `option` on its side.
    ///
    /// # Errors
    ///
    /// As [`ask_remote_enable`](Self::ask_remote_enable).
    pub fn ask_remote_disable(
        &mut self,
        option: u8,
        out: &mut Vec<u8>,
    ) -> Result<(), NegotiationFault> {
        let index = usize::from(option);
        match self.him[index] {
            Q::No => Err(NegotiationFault::AlreadyThere),
            Q::Yes => {
                self.him[index] = Q::WantNoEmpty;
                nvt::push_negotiate(DONT, option, out);
                Ok(())
            }
            Q::WantNoEmpty | Q::WantYesOpposite => Err(NegotiationFault::AlreadyQueued),
            Q::WantNoOpposite => {
                self.him[index] = Q::WantNoEmpty;
                Ok(())
            }
            Q::WantYesEmpty => {
                self.him[index] = Q::WantYesOpposite;
                Ok(())
            }
        }
    }

    /// Offer `option` on our side, appending the `WILL` when the request is new.
    ///
    /// # Errors
    ///
    /// As [`ask_remote_enable`](Self::ask_remote_enable).
    pub fn ask_local_enable(
        &mut self,
        option: u8,
        out: &mut Vec<u8>,
    ) -> Result<(), NegotiationFault> {
        let index = usize::from(option);
        match self.us[index] {
            Q::No => {
                self.us[index] = Q::WantYesEmpty;
                nvt::push_negotiate(WILL, option, out);
                Ok(())
            }
            Q::Yes => Err(NegotiationFault::AlreadyThere),
            Q::WantNoEmpty => {
                self.us[index] = Q::WantNoOpposite;
                Ok(())
            }
            Q::WantNoOpposite | Q::WantYesEmpty => Err(NegotiationFault::AlreadyQueued),
            Q::WantYesOpposite => {
                self.us[index] = Q::WantYesEmpty;
                Ok(())
            }
        }
    }

    /// Withdraw `option` on our side.
    ///
    /// # Errors
    ///
    /// As [`ask_remote_enable`](Self::ask_remote_enable).
    pub fn ask_local_disable(
        &mut self,
        option: u8,
        out: &mut Vec<u8>,
    ) -> Result<(), NegotiationFault> {
        let index = usize::from(option);
        match self.us[index] {
            Q::No => Err(NegotiationFault::AlreadyThere),
            Q::Yes => {
                self.us[index] = Q::WantNoEmpty;
                nvt::push_negotiate(WONT, option, out);
                Ok(())
            }
            Q::WantNoEmpty | Q::WantYesOpposite => Err(NegotiationFault::AlreadyQueued),
            Q::WantNoOpposite => {
                self.us[index] = Q::WantNoEmpty;
                Ok(())
            }
            Q::WantYesEmpty => {
                self.us[index] = Q::WantYesOpposite;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests;

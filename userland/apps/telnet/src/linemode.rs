//! RFC 1184 LINEMODE: the `MODE` mask, the Set Local Characters table, and
//! `FORWARDMASK`.
//!
//! LINEMODE moves line editing to the *client* under the server's direction.
//! Three sub-negotiations carry that direction, and all three are stateful, so
//! this module is a state machine rather than a field codec:
//!
//! * `MODE` (RFC 1184 §2) — a bit mask (`EDIT`, `TRAPSIG`, `SOFT_TAB`,
//!   `LIT_ECHO`) the two ends agree on. Whoever receives a mask acknowledges it
//!   by echoing it back with `MODE_ACK` set, and an acknowledgement is never
//!   itself acknowledged: that is what terminates the exchange.
//! * `SLC` (RFC 1184 §3) — which local characters mean `interrupt`, `erase`,
//!   `kill` and the rest, negotiated per function with a support *level* and an
//!   acknowledgement bit. The level rules are the whole subtlety of RFC 1184
//!   and are implemented in [`SlcTable::fold`].
//! * `FORWARDMASK` (RFC 1184 §2.3) — a 32-byte bit mask naming the characters
//!   that must force a partial line to the server immediately.
//!
//! The server is hostile like every other peer: a malformed sub-negotiation is
//! discarded whole, an SLC triplet naming an unknown function is answered
//! `NOSUPPORT` rather than stored, and the reply an exchange produces is
//! bounded by the fixed function count — a peer cannot make the client emit an
//! unbounded reply.

use alloc::vec::Vec;

use crate::nvt;
use crate::option;

/// RFC 1184 §2 LINEMODE sub-option codes.
pub mod sub {
    /// The `MODE` mask follows.
    pub const MODE: u8 = 1;
    /// A `FORWARDMASK` negotiation follows (`DO`/`DONT`/`WILL`/`WONT`).
    pub const FORWARDMASK: u8 = 2;
    /// Set Local Characters triplets follow.
    pub const SLC: u8 = 3;
}

/// RFC 1184 §2.1 `MODE` mask bits.
pub mod mode {
    /// The client performs the line editing and forwards complete lines.
    pub const EDIT: u8 = 0x01;
    /// The client maps its local signal characters to the telnet commands
    /// (`IP`, `BRK`, `ABORT`, `SUSP`, `EOF`) rather than sending them as data.
    pub const TRAPSIG: u8 = 0x02;
    /// This mask is an acknowledgement of one just received.
    pub const MODE_ACK: u8 = 0x04;
    /// The client expands tabs locally.
    pub const SOFT_TAB: u8 = 0x08;
    /// The client echoes control characters literally rather than as `^X`.
    pub const LIT_ECHO: u8 = 0x10;

    /// The bits the two ends actually negotiate — `MODE_ACK` is a framing bit,
    /// not a mode, so it is excluded from every comparison.
    pub const NEGOTIATED: u8 = EDIT | TRAPSIG | SOFT_TAB | LIT_ECHO;
}

/// RFC 1184 §3 SLC function codes. `SLC_SYNCH` is 1 and the set is dense up to
/// [`SLC_MAX`], which is what lets the table be a fixed array indexed by
/// function.
pub mod slc {
    /// Synch.
    pub const SYNCH: u8 = 1;
    /// Break.
    pub const BRK: u8 = 2;
    /// Interrupt process.
    pub const IP: u8 = 3;
    /// Abort output.
    pub const AO: u8 = 4;
    /// Are you there.
    pub const AYT: u8 = 5;
    /// End of record.
    pub const EOR: u8 = 6;
    /// Abort.
    pub const ABORT: u8 = 7;
    /// End of file.
    pub const EOF: u8 = 8;
    /// Suspend.
    pub const SUSP: u8 = 9;
    /// Erase character.
    pub const EC: u8 = 10;
    /// Erase line.
    pub const EL: u8 = 11;
    /// Erase word.
    pub const EW: u8 = 12;
    /// Reprint line.
    pub const RP: u8 = 13;
    /// Literal next.
    pub const LNEXT: u8 = 14;
    /// Resume output.
    pub const XON: u8 = 15;
    /// Suspend output.
    pub const XOFF: u8 = 16;
    /// Forwarding character 1.
    pub const FORW1: u8 = 17;
    /// Forwarding character 2.
    pub const FORW2: u8 = 18;
    /// Move cursor one character left.
    pub const MCL: u8 = 19;
    /// Move cursor one character right.
    pub const MCR: u8 = 20;
    /// Move cursor one word left.
    pub const MCWL: u8 = 21;
    /// Move cursor one word right.
    pub const MCWR: u8 = 22;
    /// Move cursor to the beginning of the line.
    pub const MCBOL: u8 = 23;
    /// Move cursor to the end of the line.
    pub const MCEOL: u8 = 24;
    /// Enter insert mode.
    pub const INSRT: u8 = 25;
    /// Enter overstrike mode.
    pub const OVER: u8 = 26;
    /// Erase to the end of the line.
    pub const ECR: u8 = 27;
    /// Erase the word to the right.
    pub const EWR: u8 = 28;
    /// Erase to the beginning of the line.
    pub const EBOL: u8 = 29;
    /// Erase to the end of the line.
    pub const EEOL: u8 = 30;
}

/// The highest SLC function code RFC 1184 defines.
pub const SLC_MAX: u8 = slc::EEOL;

/// RFC 1184 §3 SLC flag bits and support levels.
pub mod slc_flag {
    /// The mask selecting the two-bit support level.
    pub const LEVELBITS: u8 = 0x03;
    /// The function is not supported and has no character.
    pub const NOSUPPORT: u8 = 0;
    /// The function's character is fixed and cannot be changed.
    pub const CANTCHANGE: u8 = 1;
    /// The function's character is the one in the triplet.
    pub const VALUE: u8 = 2;
    /// "Use your own default for this function."
    pub const DEFAULT: u8 = 3;
    /// This triplet acknowledges one just received.
    pub const ACK: u8 = 0x80;
    /// Flush the input queue when this character is recognised.
    pub const FLUSHIN: u8 = 0x40;
    /// Flush the output queue when this character is recognised.
    pub const FLUSHOUT: u8 = 0x20;
}

/// The byte RFC 1184 §3 reserves to mean "this function has no character",
/// used whenever the level is `NOSUPPORT`.
pub const SLC_NOVALUE: u8 = 0;

/// One SLC function's negotiated state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlcEntry {
    /// The level and flush bits currently in force (never carries `ACK`).
    pub flags: u8,
    /// The character bound to the function, or [`SLC_NOVALUE`].
    pub value: u8,
}

impl SlcEntry {
    /// The support level, with the flush and acknowledgement bits masked off.
    #[must_use]
    pub const fn level(self) -> u8 {
        self.flags & slc_flag::LEVELBITS
    }

    /// Whether the function is usable — supported and bound to a character.
    #[must_use]
    pub const fn active(self) -> bool {
        self.level() != slc_flag::NOSUPPORT && self.value != SLC_NOVALUE
    }
}

/// The client's own defaults: which characters it would choose for each
/// function, and at what level, before the server says anything.
///
/// The values are the familiar Unix terminal bindings so a session behaves as
/// an operator expects; the *levels* say how firmly the client holds them. A
/// function the client cannot perform is `NOSUPPORT`, which is the honest
/// answer and stops the server relying on it.
const DEFAULTS: &[(u8, u8, u8)] = &[
    // (function, flags, value)
    (slc::SYNCH, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::BRK, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (
        slc::IP,
        slc_flag::VALUE | slc_flag::FLUSHIN | slc_flag::FLUSHOUT,
        0x03,
    ),
    (slc::AO, slc_flag::VALUE | slc_flag::FLUSHOUT, 0x0F),
    (slc::AYT, slc_flag::VALUE, 0x14),
    (slc::EOR, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (
        slc::ABORT,
        slc_flag::VALUE | slc_flag::FLUSHIN | slc_flag::FLUSHOUT,
        0x1C,
    ),
    (slc::EOF, slc_flag::VALUE, 0x04),
    (slc::SUSP, slc_flag::VALUE | slc_flag::FLUSHIN, 0x1A),
    (slc::EC, slc_flag::VALUE, 0x7F),
    (slc::EL, slc_flag::VALUE, 0x15),
    (slc::EW, slc_flag::VALUE, 0x17),
    (slc::RP, slc_flag::VALUE, 0x12),
    (slc::LNEXT, slc_flag::VALUE, 0x16),
    (slc::XON, slc_flag::VALUE, 0x11),
    (slc::XOFF, slc_flag::VALUE, 0x13),
    (slc::FORW1, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::FORW2, slc_flag::NOSUPPORT, SLC_NOVALUE),
    // The cursor-motion and partial-erase functions belong to a full-screen
    // line editor the client does not implement; claiming them would promise
    // the server editing it would then rely on.
    (slc::MCL, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::MCR, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::MCWL, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::MCWR, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::MCBOL, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::MCEOL, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::INSRT, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::OVER, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::ECR, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::EWR, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::EBOL, slc_flag::NOSUPPORT, SLC_NOVALUE),
    (slc::EEOL, slc_flag::NOSUPPORT, SLC_NOVALUE),
];

/// The human name of an SLC function, for `slc export` and `display`. Returns
/// [`None`] for a code RFC 1184 does not define.
#[must_use]
pub const fn slc_name(function: u8) -> Option<&'static str> {
    Some(match function {
        slc::SYNCH => "synch",
        slc::BRK => "brk",
        slc::IP => "ip",
        slc::AO => "ao",
        slc::AYT => "ayt",
        slc::EOR => "eor",
        slc::ABORT => "abort",
        slc::EOF => "eof",
        slc::SUSP => "susp",
        slc::EC => "erase",
        slc::EL => "kill",
        slc::EW => "worderase",
        slc::RP => "reprint",
        slc::LNEXT => "lnext",
        slc::XON => "start",
        slc::XOFF => "stop",
        slc::FORW1 => "forw1",
        slc::FORW2 => "forw2",
        slc::MCL => "mcl",
        slc::MCR => "mcr",
        slc::MCWL => "mcwl",
        slc::MCWR => "mcwr",
        slc::MCBOL => "mcbol",
        slc::MCEOL => "mceol",
        slc::INSRT => "insrt",
        slc::OVER => "over",
        slc::ECR => "ecr",
        slc::EWR => "ewr",
        slc::EBOL => "ebol",
        slc::EEOL => "eeol",
        _ => return None,
    })
}

/// The SLC function a `set`/`unset` variable name selects. Returns [`None`] for
/// a name that is not an SLC function (the interpreter handles `escape` and
/// `echo` itself).
#[must_use]
pub fn slc_function(name: &str) -> Option<u8> {
    (1..=SLC_MAX).find(|&function| slc_name(function) == Some(name))
}

/// The negotiated Set Local Characters table.
///
/// A fixed array indexed by function code, so a hostile server cannot grow it
/// and a lookup needs no search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlcTable {
    entries: [SlcEntry; SLC_MAX as usize + 1],
}

impl Default for SlcTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SlcTable {
    /// A table holding the client's own defaults.
    #[must_use]
    pub fn new() -> Self {
        let mut table = Self {
            entries: [SlcEntry {
                flags: slc_flag::NOSUPPORT,
                value: SLC_NOVALUE,
            }; SLC_MAX as usize + 1],
        };
        for &(function, flags, value) in DEFAULTS {
            table.entries[usize::from(function)] = SlcEntry { flags, value };
        }
        table
    }

    /// The entry for `function`, or [`None`] when the code is out of range.
    #[must_use]
    pub fn get(&self, function: u8) -> Option<SlcEntry> {
        if function == 0 || function > SLC_MAX {
            return None;
        }
        Some(self.entries[usize::from(function)])
    }

    /// The character bound to `function` if it is active.
    #[must_use]
    pub fn char_for(&self, function: u8) -> Option<u8> {
        self.get(function).filter(|e| e.active()).map(|e| e.value)
    }

    /// The function bound to `byte`, searched in function order so the mapping
    /// is deterministic when a server binds one character twice.
    #[must_use]
    pub fn function_for(&self, byte: u8) -> Option<u8> {
        if byte == SLC_NOVALUE {
            return None;
        }
        (1..=SLC_MAX).find(|&function| {
            let entry = self.entries[usize::from(function)];
            entry.active() && entry.value == byte
        })
    }

    /// Locally rebind `function` to `value` (the `set` command), leaving its
    /// flush bits alone and raising the level to `VALUE`.
    ///
    /// Returns `false` for an out-of-range function or one the server pinned
    /// with `CANTCHANGE` — the operator is told rather than the change being
    /// applied locally and silently disagreeing with the server.
    pub fn set_local(&mut self, function: u8, value: u8) -> bool {
        if function == 0 || function > SLC_MAX {
            return false;
        }
        let entry = &mut self.entries[usize::from(function)];
        if entry.level() == slc_flag::CANTCHANGE {
            return false;
        }
        entry.flags = (entry.flags & !slc_flag::LEVELBITS) | slc_flag::VALUE;
        entry.value = value;
        true
    }

    /// Locally unbind `function` (the `unset` command).
    ///
    /// Returns `false` as [`set_local`](Self::set_local) does.
    pub fn unset_local(&mut self, function: u8) -> bool {
        if function == 0 || function > SLC_MAX {
            return false;
        }
        let entry = &mut self.entries[usize::from(function)];
        if entry.level() == slc_flag::CANTCHANGE {
            return false;
        }
        entry.flags = (entry.flags & !slc_flag::LEVELBITS) | slc_flag::NOSUPPORT;
        entry.value = SLC_NOVALUE;
        true
    }

    /// Fold a received SLC parameter region, returning the reply triplets the
    /// exchange calls for (empty when the peer's message was purely
    /// acknowledgements, which is what ends the exchange).
    ///
    /// This is RFC 1184 §3's negotiation, function by function:
    ///
    /// * a triplet carrying `ACK` is an answer to something we sent — it is
    ///   recorded if it matches what we asked for and otherwise ignored, and
    ///   is **never** replied to, so the exchange cannot cycle;
    /// * `DEFAULT` asks for our own default, which we state at our own level;
    /// * `NOSUPPORT` from the peer disables the function, since a function only
    ///   one end performs is no function at all;
    /// * `VALUE` is accepted when we can change the function, acknowledged as
    ///   accepted, and otherwise answered with our own value at the level we
    ///   actually hold it.
    ///
    /// A parameter region whose length is not a multiple of three is malformed;
    /// the trailing partial triplet is ignored and the complete ones are still
    /// honoured, so one truncated byte cannot discard a whole valid table.
    pub fn fold(&mut self, params: &[u8]) -> Vec<u8> {
        let mut reply = Vec::new();
        for triplet in params.as_chunks::<3>().0 {
            let (function, flags, value) = (triplet[0], triplet[1], triplet[2]);
            let Some(current) = self.get(function) else {
                // An undefined function cannot be stored, and answering
                // `NOSUPPORT` tells the server not to rely on it.
                push_triplet(function, slc_flag::NOSUPPORT, SLC_NOVALUE, &mut reply);
                continue;
            };
            if flags & slc_flag::ACK != 0 {
                if value == current.value {
                    self.entries[usize::from(function)].flags = flags & !slc_flag::ACK;
                }
                continue;
            }
            let level = flags & slc_flag::LEVELBITS;
            let index = usize::from(function);
            match level {
                slc_flag::DEFAULT => {
                    let default = default_entry(function);
                    self.entries[index] = default;
                    push_triplet(function, default.flags, default.value, &mut reply);
                }
                slc_flag::NOSUPPORT => {
                    self.entries[index] = SlcEntry {
                        flags: slc_flag::NOSUPPORT,
                        value: SLC_NOVALUE,
                    };
                    push_triplet(
                        function,
                        slc_flag::NOSUPPORT | slc_flag::ACK,
                        SLC_NOVALUE,
                        &mut reply,
                    );
                }
                // The peer pins the character: we must hold it, and we
                // acknowledge at the level the peer stated.
                slc_flag::CANTCHANGE => {
                    self.entries[index] = SlcEntry {
                        flags: flags & !slc_flag::ACK,
                        value,
                    };
                    push_triplet(function, flags | slc_flag::ACK, value, &mut reply);
                }
                // `VALUE`: accept unless our own level forbids a change, in
                // which case we restate what we hold rather than pretend.
                _ => {
                    if current.level() == slc_flag::CANTCHANGE {
                        push_triplet(function, current.flags, current.value, &mut reply);
                    } else {
                        self.entries[index] = SlcEntry {
                            flags: flags & !slc_flag::ACK,
                            value,
                        };
                        push_triplet(function, flags | slc_flag::ACK, value, &mut reply);
                    }
                }
            }
        }
        reply
    }

    /// Encode the whole table as an SLC subnegotiation the client volunteers
    /// (`slc export`, and the initial statement once LINEMODE is agreed).
    pub fn push_export(&self, out: &mut Vec<u8>) {
        let mut params = alloc::vec![sub::SLC];
        for function in 1..=SLC_MAX {
            let entry = self.entries[usize::from(function)];
            push_triplet(function, entry.flags, entry.value, &mut params);
        }
        nvt::push_subnegotiation(option::LINEMODE, &params, out);
    }
}

/// The client's default entry for `function`.
fn default_entry(function: u8) -> SlcEntry {
    DEFAULTS
        .iter()
        .find(|&&(code, _, _)| code == function)
        .map_or(
            SlcEntry {
                flags: slc_flag::NOSUPPORT,
                value: SLC_NOVALUE,
            },
            |&(_, flags, value)| SlcEntry { flags, value },
        )
}

/// Append one SLC triplet to a parameter region.
fn push_triplet(function: u8, flags: u8, value: u8, params: &mut Vec<u8>) {
    params.extend_from_slice(&[function, flags, value]);
}

/// The RFC 1184 §2.3 `FORWARDMASK`: a 256-bit set naming the characters that
/// must force a partial line to the server at once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForwardMask {
    bits: [u8; Self::LEN],
}

impl Default for ForwardMask {
    fn default() -> Self {
        Self::empty()
    }
}

impl ForwardMask {
    /// Wire length of the mask a peer sends: 32 octets covering codes 0..=255.
    pub const LEN: usize = 32;

    /// A mask naming no character.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bits: [0; Self::LEN],
        }
    }

    /// Decode a mask from a `DO FORWARDMASK` payload.
    ///
    /// RFC 1184 permits a short mask, which names only the low codes; the
    /// remaining octets are zero. A payload longer than [`LEN`](Self::LEN) is
    /// malformed and refused whole.
    #[must_use]
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() > Self::LEN {
            return None;
        }
        let mut mask = Self::empty();
        mask.bits[..payload.len()].copy_from_slice(payload);
        Some(mask)
    }

    /// Whether `byte` must force the line to the server.
    #[must_use]
    pub const fn contains(&self, byte: u8) -> bool {
        // The mask is big-endian within each octet: bit 7 of octet 0 is code 0.
        let octet = (byte >> 3) as usize;
        let bit = 7 - (byte & 0x07);
        self.bits[octet] & (1 << bit) != 0
    }

    /// Whether the mask names no character at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&octet| octet == 0)
    }
}

/// What folding one LINEMODE subnegotiation asks the session to do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LinemodeOutcome {
    /// Bytes to transmit (a `MODE` acknowledgement, an SLC reply, or a
    /// `FORWARDMASK` answer).
    pub reply: Vec<u8>,
    /// The mode mask now in force, when this exchange changed it.
    pub mode_changed: Option<u8>,
    /// Whether the subnegotiation was malformed and discarded.
    pub refused: bool,
}

/// The LINEMODE state of one connection.
#[derive(Debug, Default)]
pub struct Linemode {
    mask: u8,
    /// The mask we last sent unacknowledged, so an incoming `MODE_ACK` can be
    /// matched against what we actually asked for.
    pending: Option<u8>,
    slc: SlcTable,
    forward: ForwardMask,
    forwarding_agreed: bool,
}

impl Linemode {
    /// A fresh state: no mode bits, the client's default SLC table, no
    /// forwarding mask.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mask: 0,
            pending: None,
            slc: SlcTable::new(),
            forward: ForwardMask::empty(),
            forwarding_agreed: false,
        }
    }

    /// The mode mask in force.
    #[must_use]
    pub const fn mask(&self) -> u8 {
        self.mask
    }

    /// Whether the client is doing the line editing.
    #[must_use]
    pub const fn edit(&self) -> bool {
        self.mask & mode::EDIT != 0
    }

    /// Whether the client maps its signal characters to telnet commands.
    #[must_use]
    pub const fn trapsig(&self) -> bool {
        self.mask & mode::TRAPSIG != 0
    }

    /// Whether the client expands tabs locally.
    #[must_use]
    pub const fn soft_tab(&self) -> bool {
        self.mask & mode::SOFT_TAB != 0
    }

    /// Whether the client echoes control characters literally.
    #[must_use]
    pub const fn lit_echo(&self) -> bool {
        self.mask & mode::LIT_ECHO != 0
    }

    /// The negotiated SLC table.
    #[must_use]
    pub const fn slc(&self) -> &SlcTable {
        &self.slc
    }

    /// The negotiated SLC table, mutably (the `set`/`unset` commands).
    pub const fn slc_mut(&mut self) -> &mut SlcTable {
        &mut self.slc
    }

    /// Whether `byte` must force a partial line to the server.
    #[must_use]
    pub fn forwards(&self, byte: u8) -> bool {
        self.forwarding_agreed && self.forward.contains(byte)
    }

    /// Reset to the un-negotiated state, for a fresh connection.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Ask the server for `mask`, appending the `MODE` subnegotiation to `out`.
    ///
    /// The request is remembered so the server's acknowledgement can be matched
    /// against it. The client's own state changes only when the server
    /// acknowledges (or restates) the mask — never optimistically, which would
    /// leave the two ends editing on different rules.
    pub fn request_mode(&mut self, mask: u8, out: &mut Vec<u8>) {
        let wanted = mask & mode::NEGOTIATED;
        self.pending = Some(wanted);
        nvt::push_subnegotiation(option::LINEMODE, &[sub::MODE, wanted], out);
    }

    /// Fold one LINEMODE subnegotiation payload.
    pub fn fold(&mut self, params: &[u8]) -> LinemodeOutcome {
        let mut outcome = LinemodeOutcome::default();
        let Some((&head, rest)) = params.split_first() else {
            outcome.refused = true;
            return outcome;
        };
        match head {
            sub::MODE => self.fold_mode(rest, &mut outcome),
            sub::SLC => {
                let reply = self.slc.fold(rest);
                if !reply.is_empty() {
                    let mut payload = alloc::vec![sub::SLC];
                    payload.extend_from_slice(&reply);
                    nvt::push_subnegotiation(option::LINEMODE, &payload, &mut outcome.reply);
                }
            }
            nvt::DO if rest.first() == Some(&sub::FORWARDMASK) => {
                self.fold_forwardmask_request(&rest[1..], &mut outcome);
            }
            nvt::DONT if rest.first() == Some(&sub::FORWARDMASK) => {
                self.forwarding_agreed = false;
                self.forward = ForwardMask::empty();
                nvt::push_subnegotiation(
                    option::LINEMODE,
                    &[nvt::WONT, sub::FORWARDMASK],
                    &mut outcome.reply,
                );
            }
            // A server offering to *do* the forwarding itself has nothing for
            // a client to act on (the client is the forwarder), so it is
            // refused rather than silently accepted.
            nvt::WILL | nvt::WONT if rest.first() == Some(&sub::FORWARDMASK) => {
                nvt::push_subnegotiation(
                    option::LINEMODE,
                    &[nvt::DONT, sub::FORWARDMASK],
                    &mut outcome.reply,
                );
            }
            _ => outcome.refused = true,
        }
        outcome
    }

    /// Fold a `MODE` payload: exactly one mask octet.
    fn fold_mode(&mut self, rest: &[u8], outcome: &mut LinemodeOutcome) {
        let [received] = *rest else {
            outcome.refused = true;
            return;
        };
        let wanted = received & mode::NEGOTIATED;
        if received & mode::MODE_ACK != 0 {
            // An acknowledgement: adopt it only if it answers what we asked
            // for, and never acknowledge an acknowledgement.
            if self.pending.take() == Some(wanted) && self.mask != wanted {
                self.mask = wanted;
                outcome.mode_changed = Some(wanted);
            }
            return;
        }
        // The server states a mask. Adopt it and acknowledge exactly once; a
        // mask we are already in needs no second acknowledgement, which is
        // what stops a server that restates it from cycling.
        self.pending = None;
        if self.mask == wanted {
            return;
        }
        self.mask = wanted;
        outcome.mode_changed = Some(wanted);
        nvt::push_subnegotiation(
            option::LINEMODE,
            &[sub::MODE, wanted | mode::MODE_ACK],
            &mut outcome.reply,
        );
    }

    /// Fold a `DO FORWARDMASK <mask>` request.
    fn fold_forwardmask_request(&mut self, payload: &[u8], outcome: &mut LinemodeOutcome) {
        if let Some(mask) = ForwardMask::parse(payload) {
            self.forward = mask;
            self.forwarding_agreed = true;
            nvt::push_subnegotiation(
                option::LINEMODE,
                &[nvt::WILL, sub::FORWARDMASK],
                &mut outcome.reply,
            );
        } else {
            outcome.refused = true;
            nvt::push_subnegotiation(
                option::LINEMODE,
                &[nvt::WONT, sub::FORWARDMASK],
                &mut outcome.reply,
            );
        }
    }
}

#[cfg(test)]
mod tests;

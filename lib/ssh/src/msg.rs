//! Message numbers (RFC 4250 §4.1) and disconnect reason codes (§4.2.2).
//!
//! The numbers here are the transport layer's own; the layers above define
//! theirs beside the code that speaks them.

/// `SSH_MSG_DISCONNECT`.
pub const DISCONNECT: u8 = 1;
/// `SSH_MSG_IGNORE`.
pub const IGNORE: u8 = 2;
/// `SSH_MSG_UNIMPLEMENTED`.
pub const UNIMPLEMENTED: u8 = 3;
/// `SSH_MSG_DEBUG`.
pub const DEBUG: u8 = 4;
/// `SSH_MSG_SERVICE_REQUEST`.
pub const SERVICE_REQUEST: u8 = 5;
/// `SSH_MSG_SERVICE_ACCEPT`.
pub const SERVICE_ACCEPT: u8 = 6;
/// `SSH_MSG_KEXINIT`.
pub const KEXINIT: u8 = 20;
/// `SSH_MSG_NEWKEYS`.
pub const NEWKEYS: u8 = 21;

/// The RFC 4250 §4.1.1 block a message number is assigned from.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Block {
    /// 1–19: transport-layer generic messages.
    TransportGeneric,
    /// 20–29: algorithm negotiation.
    AlgorithmNegotiation,
    /// 30–49: specific to the negotiated key-exchange method.
    KeyExchangeMethod,
    /// 50–255: the layers above the transport.
    Upper,
    /// 0, which no block assigns.
    Unassigned,
}

impl Block {
    /// The block `msg` belongs to.
    #[must_use]
    pub const fn of(msg: u8) -> Self {
        match msg {
            0 => Self::Unassigned,
            1..=19 => Self::TransportGeneric,
            20..=29 => Self::AlgorithmNegotiation,
            30..=49 => Self::KeyExchangeMethod,
            _ => Self::Upper,
        }
    }
}

/// Whether RFC 4253 §7.1 lets `msg` be sent between a party's
/// `SSH_MSG_KEXINIT` and its `SSH_MSG_NEWKEYS`: the transport-generic
/// messages other than the service request and accept, the negotiation
/// messages other than a further `SSH_MSG_KEXINIT`, and the method's own.
#[must_use]
pub const fn permitted_during_kex(msg: u8) -> bool {
    match Block::of(msg) {
        Block::TransportGeneric => msg != SERVICE_REQUEST && msg != SERVICE_ACCEPT,
        Block::AlgorithmNegotiation => msg != KEXINIT,
        Block::KeyExchangeMethod => true,
        Block::Upper | Block::Unassigned => false,
    }
}

/// An `SSH_MSG_DISCONNECT` reason code (RFC 4250 §4.2.2).
///
/// Any `uint32` a peer sends decodes; the associated constants are the
/// registered codes, which are the ones this engine sends.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct DisconnectReason(u32);

impl DisconnectReason {
    /// `SSH_DISCONNECT_HOST_NOT_ALLOWED_TO_CONNECT`.
    pub const HOST_NOT_ALLOWED_TO_CONNECT: Self = Self(1);
    /// `SSH_DISCONNECT_PROTOCOL_ERROR`.
    pub const PROTOCOL_ERROR: Self = Self(2);
    /// `SSH_DISCONNECT_KEY_EXCHANGE_FAILED`.
    pub const KEY_EXCHANGE_FAILED: Self = Self(3);
    /// `SSH_DISCONNECT_MAC_ERROR`.
    pub const MAC_ERROR: Self = Self(5);
    /// `SSH_DISCONNECT_COMPRESSION_ERROR`.
    pub const COMPRESSION_ERROR: Self = Self(6);
    /// `SSH_DISCONNECT_SERVICE_NOT_AVAILABLE`.
    pub const SERVICE_NOT_AVAILABLE: Self = Self(7);
    /// `SSH_DISCONNECT_PROTOCOL_VERSION_NOT_SUPPORTED`.
    pub const PROTOCOL_VERSION_NOT_SUPPORTED: Self = Self(8);
    /// `SSH_DISCONNECT_HOST_KEY_NOT_VERIFIABLE`.
    pub const HOST_KEY_NOT_VERIFIABLE: Self = Self(9);
    /// `SSH_DISCONNECT_CONNECTION_LOST`.
    pub const CONNECTION_LOST: Self = Self(10);
    /// `SSH_DISCONNECT_BY_APPLICATION`.
    pub const BY_APPLICATION: Self = Self(11);
    /// `SSH_DISCONNECT_TOO_MANY_CONNECTIONS`.
    pub const TOO_MANY_CONNECTIONS: Self = Self(12);
    /// `SSH_DISCONNECT_AUTH_CANCELLED_BY_USER`.
    pub const AUTH_CANCELLED_BY_USER: Self = Self(13);
    /// `SSH_DISCONNECT_NO_MORE_AUTH_METHODS_AVAILABLE`.
    pub const NO_MORE_AUTH_METHODS_AVAILABLE: Self = Self(14);
    /// `SSH_DISCONNECT_ILLEGAL_USER_NAME`.
    pub const ILLEGAL_USER_NAME: Self = Self(15);

    /// The reason a peer's code names.
    #[must_use]
    pub const fn from_code(code: u32) -> Self {
        Self(code)
    }

    /// The code on the wire.
    #[must_use]
    pub const fn code(self) -> u32 {
        self.0
    }
}

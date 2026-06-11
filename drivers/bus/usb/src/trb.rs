//! Transfer Request Block (TRB) vocabulary (xHCI 1.2 §4.11 / §6.4).
//!
//! A TRB is the 16-byte unit every xHCI ring is built from: commands to
//! the controller, transfer descriptors to an endpoint, and events back
//! from the controller all travel as TRBs. Only the TRB types the
//! bring-up and HID-interrupt paths use are defined (`AGENTS.md` §2.3);
//! the set grows with the consumers.

use rustos_abi::DriverError;

/// Byte length of one TRB (§4.11).
pub const TRB_LEN: usize = 16;

/// Control-word bit 0: the cycle bit, the producer/consumer ownership
/// handshake (§4.9.2). Set and read by the rings, never by TRB
/// builders.
pub const CONTROL_CYCLE: u32 = 1 << 0;

/// Control-word bit 1 on a Link TRB: Toggle Cycle (§6.4.4.1).
pub const CONTROL_LINK_TOGGLE: u32 = 1 << 1;

/// Shift of the TRB Type field (control-word bits 15:10, §6.4.1).
const TYPE_SHIFT: u32 = 10;

/// Mask of the TRB Type field after shifting.
const TYPE_MASK: u32 = 0x3F;

/// One 16-byte Transfer Request Block.
///
/// Field names follow §6.4.1: a 64-bit parameter (pointer or immediate
/// data), a 32-bit status word, and a 32-bit control word carrying the
/// type, the cycle bit, and type-specific flags. The in-memory layout
/// matches the on-ring little-endian layout on every Tier-1 target the
/// rings run on; the rings hand TRBs to hardware only through the DMA
/// seam that owns the byte order.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Trb {
    /// Parameter dwords 0..2 (pointer or type-specific immediate).
    pub parameter: u64,
    /// Status dword 2 (transfer length, completion code on events).
    pub status: u32,
    /// Control dword 3 (cycle bit, TRB type, type-specific flags).
    pub control: u32,
}

impl Trb {
    /// The all-zero TRB rings are initialised with (cycle bit clear,
    /// type 0 = reserved, owned by the producer).
    pub const ZERO: Self = Self {
        parameter: 0,
        status: 0,
        control: 0,
    };

    /// Build a TRB of `trb_type` with type-specific `flags` or-ed into
    /// the control word. The cycle bit is owned by the ring and must
    /// not be part of `flags`; a `flags` value carrying it is rejected
    /// by the producer ring at enqueue time.
    #[must_use]
    pub const fn new(trb_type: TrbType, parameter: u64, status: u32, flags: u32) -> Self {
        Self {
            parameter,
            status,
            control: flags | ((trb_type.as_u8() as u32) << TYPE_SHIFT),
        }
    }

    /// The cycle bit (§4.9.2).
    #[must_use]
    pub const fn cycle(&self) -> bool {
        self.control & CONTROL_CYCLE != 0
    }

    /// Decode the TRB Type field.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if the field does not name a type
    /// this driver models (failing closed on a forged or future type,
    /// `AGENTS.md` §5.4).
    pub const fn trb_type(&self) -> Result<TrbType, DriverError> {
        TrbType::from_raw((self.control >> TYPE_SHIFT) & TYPE_MASK)
    }

    /// Completion code of an event TRB (status bits 31:24, §6.4.2).
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if the code is not one this driver
    /// models.
    pub const fn completion_code(&self) -> Result<CompletionCode, DriverError> {
        CompletionCode::from_raw(self.status >> 24)
    }

    /// Slot ID of an event TRB (control bits 31:24, §6.4.2).
    #[must_use]
    pub const fn slot_id(&self) -> u8 {
        self.control.to_le_bytes()[3]
    }
}

/// TRB types this driver models (§6.4.6, table 6-91).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum TrbType {
    /// Normal transfer TRB (bulk/interrupt data stages).
    Normal = 1,
    /// Setup Stage TRB (control transfers).
    SetupStage = 2,
    /// Data Stage TRB (control transfers).
    DataStage = 3,
    /// Status Stage TRB (control transfers).
    StatusStage = 4,
    /// Link TRB: chains ring segments / wraps a ring (§6.4.4.1).
    Link = 6,
    /// No Op transfer TRB (transfer-ring diagnostics).
    NoOp = 8,
    /// Enable Slot command (§6.4.3.2).
    EnableSlot = 9,
    /// Address Device command (§6.4.3.4).
    AddressDevice = 11,
    /// Configure Endpoint command (§6.4.3.5).
    ConfigureEndpoint = 12,
    /// No Op command (command-ring diagnostics, §6.4.3.1).
    NoOpCommand = 23,
    /// Transfer event (§6.4.2.1).
    TransferEvent = 32,
    /// Command completion event (§6.4.2.2).
    CommandCompletion = 33,
    /// Port status change event (§6.4.2.3).
    PortStatusChange = 34,
}

impl TrbType {
    /// Raw type-field value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a raw type-field value.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `raw` is not a modelled type.
    pub const fn from_raw(raw: u32) -> Result<Self, DriverError> {
        match raw {
            1 => Ok(Self::Normal),
            2 => Ok(Self::SetupStage),
            3 => Ok(Self::DataStage),
            4 => Ok(Self::StatusStage),
            6 => Ok(Self::Link),
            8 => Ok(Self::NoOp),
            9 => Ok(Self::EnableSlot),
            11 => Ok(Self::AddressDevice),
            12 => Ok(Self::ConfigureEndpoint),
            23 => Ok(Self::NoOpCommand),
            32 => Ok(Self::TransferEvent),
            33 => Ok(Self::CommandCompletion),
            34 => Ok(Self::PortStatusChange),
            _ => Err(DriverError::OutOfRange),
        }
    }
}

/// Event completion codes this driver models (§6.4.5, table 6-90).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum CompletionCode {
    /// The command or transfer completed successfully.
    Success = 1,
    /// The controller could not access a data buffer.
    DataBufferError = 2,
    /// Babble: the device returned more data than expected.
    BabbleDetected = 3,
    /// A USB transaction error (CRC, timeout, bad PID).
    UsbTransactionError = 4,
    /// A malformed TRB reached the controller.
    TrbError = 5,
    /// The endpoint returned STALL.
    StallError = 6,
    /// The transfer completed short of the requested length —
    /// expected for variable-length HID interrupt reports.
    ShortPacket = 13,
}

impl CompletionCode {
    /// Raw completion-code value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a raw completion-code value.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `raw` is not a modelled code —
    /// the caller treats the event as a device fault rather than
    /// guessing at its meaning (`AGENTS.md` §2.9).
    pub const fn from_raw(raw: u32) -> Result<Self, DriverError> {
        match raw {
            1 => Ok(Self::Success),
            2 => Ok(Self::DataBufferError),
            3 => Ok(Self::BabbleDetected),
            4 => Ok(Self::UsbTransactionError),
            5 => Ok(Self::TrbError),
            6 => Ok(Self::StallError),
            13 => Ok(Self::ShortPacket),
            _ => Err(DriverError::OutOfRange),
        }
    }
}

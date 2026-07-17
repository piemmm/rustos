//! Transfer Request Block (TRB) vocabulary (xHCI 1.2 §4.11 / §6.4).
//!
//! A TRB is the 16-byte unit every xHCI ring is built from: commands to
//! the controller, transfer descriptors to an endpoint, and events back
//! from the controller all travel as TRBs. Only the TRB types the
//! bring-up and HID-interrupt paths use are defined;
//! the set grows with the consumers.

use tairix_abi::DriverError;

/// Byte length of one TRB.
pub const TRB_LEN: usize = 16;

/// Control-word bit 0: the cycle bit, the producer/consumer ownership
/// handshake (§4.9.2). Set and read by the rings, never by TRB
/// builders.
pub const CONTROL_CYCLE: u32 = 1 << 0;

/// Control-word bit 1 on a Link TRB: Toggle Cycle (§6.4.4.1).
pub const CONTROL_LINK_TOGGLE: u32 = 1 << 1;

/// Control-word bit 2 on a transfer TRB: Interrupt-on-Short-Packet
/// (§6.4.1.1) — a short transfer posts a [`CompletionCode::ShortPacket`]
/// event for this TRB.
pub const CONTROL_ISP: u32 = 1 << 2;

/// Control-word bit 5 on a transfer TRB: Interrupt On Completion
/// (§6.4.1.1).
pub const CONTROL_IOC: u32 = 1 << 5;

/// Control-word bit 6 on a Setup Stage TRB: Immediate Data — the
/// parameter dwords carry the 8 setup bytes themselves (§6.4.1.2.1).
pub const CONTROL_IDT: u32 = 1 << 6;

/// Control-word bit 16 on Data/Status Stage TRBs: transfer direction
/// is IN (device to host, §6.4.1.2.2/§6.4.1.2.3).
pub const CONTROL_DIR_IN: u32 = 1 << 16;

/// Setup Stage TRB Transfer Type field (bits 17:16): no data stage
/// (§6.4.1.2.1, table 6-26).
pub const SETUP_TRT_NO_DATA: u32 = 0;

/// Setup Stage TRB Transfer Type field: OUT data stage.
pub const SETUP_TRT_OUT: u32 = 2 << 16;

/// Setup Stage TRB Transfer Type field: IN data stage.
pub const SETUP_TRT_IN: u32 = 3 << 16;

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
    /// this driver models (failing closed on a forged or future type).
    pub const fn trb_type(&self) -> Result<TrbType, DriverError> {
        TrbType::from_raw((self.control >> TYPE_SHIFT) & TYPE_MASK)
    }

    /// Raw TRB Type field (control bits 15:10), **undecoded**.
    ///
    /// Unlike [`Self::trb_type`], this never fails closed on a type the
    /// driver does not model — so a diagnostic can report the verbatim
    /// type of an unexpected event (e.g. an asynchronous controller
    /// event) the consumer rejected, rather than collapsing it to an
    /// error (measure, don't guess). `0` doubles as
    /// "no event observed".
    #[must_use]
    pub const fn trb_type_raw(&self) -> u8 {
        ((self.control >> TYPE_SHIFT) & TYPE_MASK) as u8
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

    /// Raw completion-code byte of an event TRB (status bits 31:24),
    /// **undecoded**.
    ///
    /// Unlike [`Self::completion_code`], this never fails closed on a
    /// code the driver does not model — so a diagnostic can report a
    /// controller-specific or reserved fault code verbatim instead of
    /// collapsing it to an error (measure, don't
    /// guess). `0` (xHCI "Invalid") doubles as "no event observed".
    #[must_use]
    pub const fn completion_code_raw(&self) -> u8 {
        (self.status >> 24) as u8
    }

    /// Slot ID of an event TRB (control bits 31:24, §6.4.2).
    #[must_use]
    pub const fn slot_id(&self) -> u8 {
        self.control.to_le_bytes()[3]
    }

    /// Endpoint ID (DCI) of a Transfer Event TRB (control bits 20:16,
    /// §6.4.2.1).
    #[must_use]
    pub const fn endpoint_id(&self) -> u8 {
        ((self.control >> 16) & 0x1F).to_le_bytes()[0]
    }

    /// Bytes the transfer left undelivered — the Transfer Event's
    /// transfer-length residual (status bits 23:0, §6.4.2.1).
    #[must_use]
    pub const fn transfer_residual(&self) -> u32 {
        self.status & 0x00FF_FFFF
    }

    /// The on-ring little-endian byte image of this TRB, for the
    /// owner of the device-shared memory to publish.
    #[must_use]
    pub const fn to_bytes(&self) -> [u8; TRB_LEN] {
        let p = self.parameter.to_le_bytes();
        let s = self.status.to_le_bytes();
        let c = self.control.to_le_bytes();
        [
            p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7], s[0], s[1], s[2], s[3], c[0], c[1],
            c[2], c[3],
        ]
    }

    /// Decode a TRB from its on-ring little-endian byte image.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; TRB_LEN]) -> Self {
        Self {
            parameter: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            status: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            control: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        }
    }
}

/// Encode a slot ID into the control-word Slot ID field (bits 31:24)
/// of a command or transfer-event TRB (§6.4.3).
#[must_use]
pub const fn control_slot(slot: u8) -> u32 {
    (slot as u32) << 24
}

/// Encode an Endpoint ID (DCI) into the control-word Endpoint ID field
/// (bits 20:16) of an endpoint-targeted command TRB — Reset Endpoint and
/// Set TR Dequeue Pointer (§6.4.3.8 / §6.4.3.9).
#[must_use]
pub const fn control_endpoint(dci: u8) -> u32 {
    ((dci as u32) & 0x1F) << 16
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
    /// Disable Slot command (§6.4.3.3): release a device slot when its
    /// device has disconnected, returning the slot to the controller's pool.
    DisableSlot = 10,
    /// Address Device command (§6.4.3.4).
    AddressDevice = 11,
    /// Configure Endpoint command (§6.4.3.5).
    ConfigureEndpoint = 12,
    /// Evaluate Context command (§6.4.3.6): re-evaluate fields of an
    /// addressed device's contexts — here the default control endpoint's
    /// Max Packet Size once the device descriptor reports the real
    /// `bMaxPacketSize0` (§4.6.7).
    EvaluateContext = 13,
    /// Reset Endpoint command (§6.4.3.8): clear a halted endpoint's state
    /// after a STALL so it can be repositioned and resumed.
    ResetEndpoint = 14,
    /// Set TR Dequeue Pointer command (§6.4.3.9): reposition a stopped
    /// endpoint's transfer-ring dequeue pointer, dropping the TRBs the halt
    /// abandoned.
    SetTrDequeuePointer = 16,
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
            10 => Ok(Self::DisableSlot),
            11 => Ok(Self::AddressDevice),
            12 => Ok(Self::ConfigureEndpoint),
            13 => Ok(Self::EvaluateContext),
            14 => Ok(Self::ResetEndpoint),
            16 => Ok(Self::SetTrDequeuePointer),
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
    /// A split transaction to a low/full-speed device behind a high-speed
    /// hub's transaction translator failed: the hub could not complete the
    /// start-/complete-split handshake to the device. On a hot-removal of a
    /// low/full-speed device (e.g. a keyboard plugged into a USB-A port behind
    /// the controller's internal hub) the controller surfaces the unplug here,
    /// on the device's *own* endpoint, before the hub posts a port change.
    SplitTransactionError = 36,
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
    /// guessing at its meaning.
    pub const fn from_raw(raw: u32) -> Result<Self, DriverError> {
        match raw {
            1 => Ok(Self::Success),
            2 => Ok(Self::DataBufferError),
            3 => Ok(Self::BabbleDetected),
            4 => Ok(Self::UsbTransactionError),
            5 => Ok(Self::TrbError),
            6 => Ok(Self::StallError),
            13 => Ok(Self::ShortPacket),
            36 => Ok(Self::SplitTransactionError),
            _ => Err(DriverError::OutOfRange),
        }
    }

    /// Whether this completion code means the device failed to respond to a
    /// transaction — it is unreachable, as at a hot-removal — rather than
    /// actively responding with an error.
    ///
    /// A removed device cannot answer: the controller reports either a
    /// [`Self::UsbTransactionError`] (no handshake/timeout/bad PID on a
    /// directly-attached or high-speed device) or a
    /// [`Self::SplitTransactionError`] (the hub's transaction translator could
    /// not reach a low/full-speed device behind it). A [`Self::StallError`] or
    /// [`Self::BabbleDetected`], by contrast, is the device *responding*, so it
    /// is deliberately excluded — those must not be read as a disconnect.
    #[must_use]
    pub const fn indicates_device_unreachable(self) -> bool {
        matches!(
            self,
            Self::UsbTransactionError | Self::SplitTransactionError
        )
    }
}

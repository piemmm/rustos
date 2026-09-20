//! The virtio sound device's wire vocabulary (virtio 1.2 §5.14).
//!
//! Fixed little-endian record layouts and the request/status/event code
//! space, kept apart from the engine so the protocol reads as the
//! specification does and the engine reads as device logic.

/// Control virtqueue: the driver's request/response channel.
pub const CONTROL_QUEUE: u16 = 0;
/// Event virtqueue: the device's unsolicited notifications.
pub const EVENT_QUEUE: u16 = 1;
/// Transmit virtqueue: frames the driver sends to an output stream.
pub const TX_QUEUE: u16 = 2;
/// Receive virtqueue: frames the device delivers from an input stream.
pub const RX_QUEUE: u16 = 3;
/// Virtqueues a conforming virtio sound device presents.
pub const QUEUE_COUNT: u16 = 4;

/// `VIRTIO_F_VERSION_1` (feature bit 32): the modern virtio 1.x
/// split-virtqueue layout, required of a non-transitional device. No
/// device-specific feature is negotiated — `VIRTIO_SND_F_CTLS` exposes
/// mixer control elements this driver does not model, and the charter's
/// one volume model lives in the engine rather than in a device's own
/// control graph.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Byte offsets within `struct virtio_snd_config`.
pub mod config {
    /// `le32 jacks`.
    pub const JACKS: usize = 0;
    /// `le32 streams`.
    pub const STREAMS: usize = 4;
    /// `le32 chmaps`.
    pub const CHMAPS: usize = 8;
    /// Bytes of device configuration this driver reads.
    pub const LEN: usize = 12;
}

/// Request codes carried in a `struct virtio_snd_hdr`.
pub mod request {
    /// `VIRTIO_SND_R_JACK_INFO`.
    pub const JACK_INFO: u32 = 1;
    /// `VIRTIO_SND_R_PCM_INFO`.
    pub const PCM_INFO: u32 = 0x0100;
    /// `VIRTIO_SND_R_PCM_SET_PARAMS`.
    pub const PCM_SET_PARAMS: u32 = 0x0101;
    /// `VIRTIO_SND_R_PCM_PREPARE`.
    pub const PCM_PREPARE: u32 = 0x0102;
    /// `VIRTIO_SND_R_PCM_RELEASE`.
    pub const PCM_RELEASE: u32 = 0x0103;
    /// `VIRTIO_SND_R_PCM_START`.
    pub const PCM_START: u32 = 0x0104;
    /// `VIRTIO_SND_R_PCM_STOP`.
    pub const PCM_STOP: u32 = 0x0105;
    /// `VIRTIO_SND_R_CHMAP_INFO`.
    pub const CHMAP_INFO: u32 = 0x0200;
}

/// Status codes a device answers a control request or a transfer with.
pub mod status {
    /// `VIRTIO_SND_S_OK`.
    pub const OK: u32 = 0x8000;
    /// `VIRTIO_SND_S_BAD_MSG`.
    pub const BAD_MSG: u32 = 0x8001;
    /// `VIRTIO_SND_S_NOT_SUPP`.
    pub const NOT_SUPP: u32 = 0x8002;
    /// `VIRTIO_SND_S_IO_ERR`.
    pub const IO_ERR: u32 = 0x8003;
}

/// Event codes the device posts on the event queue.
pub mod event {
    /// `VIRTIO_SND_EVT_JACK_CONNECTED`.
    pub const JACK_CONNECTED: u32 = 0x1000;
    /// `VIRTIO_SND_EVT_JACK_DISCONNECTED`.
    pub const JACK_DISCONNECTED: u32 = 0x1001;
    /// `VIRTIO_SND_EVT_PCM_PERIOD_ELAPSED`.
    pub const PCM_PERIOD_ELAPSED: u32 = 0x1100;
    /// `VIRTIO_SND_EVT_PCM_XRUN`.
    pub const PCM_XRUN: u32 = 0x1101;
    /// Byte length of one `struct virtio_snd_event`: the code then the
    /// jack or stream id it concerns.
    pub const LEN: usize = 8;
}

/// Stream direction as the device spells it.
pub mod direction {
    /// `VIRTIO_SND_D_OUTPUT`.
    pub const OUTPUT: u8 = 0;
    /// `VIRTIO_SND_D_INPUT`.
    pub const INPUT: u8 = 1;
}

/// Byte length of `struct virtio_snd_query_info`: the header code, the
/// first item, the count, and the per-item size.
pub const QUERY_INFO_LEN: usize = 16;

/// Bytes the largest information record occupies, so one staging buffer
/// serves every `*_INFO` query rather than each caller sizing its own — and
/// so a record can never be read past the end of a buffer sized for a
/// different one.
pub const MAX_INFO_RECORD_LEN: usize = max3(pcm_info::LEN, chmap_info::LEN, jack_info::LEN);

/// The largest of three lengths, in a form a `const` can use.
const fn max3(a: usize, b: usize, c: usize) -> usize {
    let ab = if a > b { a } else { b };
    if ab > c {
        ab
    } else {
        c
    }
}

/// Byte length of `struct virtio_snd_hdr`.
pub const HDR_LEN: usize = 4;

/// Byte length of `struct virtio_snd_pcm_hdr`: the code then the stream id.
pub const PCM_HDR_LEN: usize = 8;

/// Byte length of `struct virtio_snd_pcm_set_params`.
pub const SET_PARAMS_LEN: usize = 24;

/// Byte offsets within `struct virtio_snd_pcm_info`, past the four-byte
/// `virtio_snd_info` node id every item carries.
pub mod pcm_info {
    /// `le32 features`.
    pub const FEATURES: usize = 4;
    /// `le64 formats`.
    pub const FORMATS: usize = 8;
    /// `le64 rates`.
    pub const RATES: usize = 16;
    /// `u8 direction`.
    pub const DIRECTION: usize = 24;
    /// `u8 channels_min`.
    pub const CHANNELS_MIN: usize = 25;
    /// `u8 channels_max`.
    pub const CHANNELS_MAX: usize = 26;
    /// Byte length of the record, padding included.
    pub const LEN: usize = 32;
}

/// Byte offsets within `struct virtio_snd_jack_info`.
pub mod jack_info {
    /// `u8 connected`.
    pub const CONNECTED: usize = 16;
    /// Byte length of the record, padding included.
    pub const LEN: usize = 24;
}

/// Byte offsets within `struct virtio_snd_chmap_info`.
pub mod chmap_info {
    /// `u8 direction`.
    pub const DIRECTION: usize = 4;
    /// `u8 channels`.
    pub const CHANNELS: usize = 5;
    /// `u8 positions[VIRTIO_SND_CHMAP_MAX_SIZE]`.
    pub const POSITIONS: usize = 6;
    /// Positions one record may carry (`VIRTIO_SND_CHMAP_MAX_SIZE`).
    pub const MAX_POSITIONS: usize = 18;
    /// Byte length of the record.
    pub const LEN: usize = POSITIONS + MAX_POSITIONS;
}

/// Channel positions as the device spells them, in the order
/// `VIRTIO_SND_CHMAP_*` defines.
pub mod chmap {
    /// `VIRTIO_SND_CHMAP_MONO`.
    pub const MONO: u8 = 2;
    /// `VIRTIO_SND_CHMAP_FL`.
    pub const FL: u8 = 3;
    /// `VIRTIO_SND_CHMAP_FR`.
    pub const FR: u8 = 4;
    /// `VIRTIO_SND_CHMAP_RL`.
    pub const RL: u8 = 5;
    /// `VIRTIO_SND_CHMAP_RR`.
    pub const RR: u8 = 6;
    /// `VIRTIO_SND_CHMAP_FC`.
    pub const FC: u8 = 7;
    /// `VIRTIO_SND_CHMAP_LFE`.
    pub const LFE: u8 = 8;
    /// `VIRTIO_SND_CHMAP_SL`.
    pub const SL: u8 = 9;
    /// `VIRTIO_SND_CHMAP_SR`.
    pub const SR: u8 = 10;
}

/// Sample-format bit positions within a PCM info record's `formats` mask.
///
/// Byte-wide because `SET_PARAMS` carries the chosen one in a `u8`: one
/// definition serves both the mask test and the programming.
pub mod format {
    /// `VIRTIO_SND_PCM_FMT_U8`.
    pub const U8: u8 = 4;
    /// `VIRTIO_SND_PCM_FMT_S16`.
    pub const S16: u8 = 5;
    /// `VIRTIO_SND_PCM_FMT_S24_3` — three packed bytes.
    pub const S24_3: u8 = 11;
    /// `VIRTIO_SND_PCM_FMT_S24` — sign-extended into 32 bits.
    pub const S24: u8 = 15;
    /// `VIRTIO_SND_PCM_FMT_S32`.
    pub const S32: u8 = 17;
    /// `VIRTIO_SND_PCM_FMT_FLOAT`.
    pub const FLOAT: u8 = 19;
}

/// Rate bit positions within a PCM info record's `rates` mask, paired with
/// the hertz each denotes.
///
/// The device names rates by index rather than by value, so this is the one
/// table that turns one into the other — read by the facts path (which rates
/// to advertise) and by the configure path (which index to program).
pub const RATES: &[(u8, u32)] = &[
    (0, 5_512),
    (1, 8_000),
    (2, 11_025),
    (3, 16_000),
    (4, 22_050),
    (5, 32_000),
    (6, 44_100),
    (7, 48_000),
    (8, 64_000),
    (9, 88_200),
    (10, 96_000),
    (11, 176_400),
    (12, 192_000),
    (13, 384_000),
];

/// Byte length of `struct virtio_snd_pcm_xfer`: the stream id a transfer
/// buffer belongs to.
pub const XFER_HDR_LEN: usize = 4;

/// Byte length of `struct virtio_snd_pcm_status`: the transfer's status and
/// the device's current latency in bytes.
pub const XFER_STATUS_LEN: usize = 8;

/// Read a little-endian `u32` at `offset`, or zero past the end.
///
/// Past-the-end reads zero rather than refusing because every caller has
/// already length-checked the record; a total accessor keeps the field reads
/// free of indexing that could panic.
#[must_use]
pub fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let Some(field) = bytes.get(offset..offset + 4) else {
        return 0;
    };
    u32::from_le_bytes([field[0], field[1], field[2], field[3]])
}

/// Read a little-endian `u64` at `offset`, or zero past the end.
#[must_use]
pub fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let Some(field) = bytes.get(offset..offset + 8) else {
        return 0;
    };
    let mut raw = [0u8; 8];
    raw.copy_from_slice(field);
    u64::from_le_bytes(raw)
}

/// Write a little-endian `u32` at `offset`, ignoring a short buffer.
pub fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    if let Some(field) = bytes.get_mut(offset..offset + 4) {
        field.copy_from_slice(&value.to_le_bytes());
    }
}

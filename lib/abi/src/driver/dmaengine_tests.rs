use core::num::NonZeroU32;

use super::*;
use crate::hwtree::{HwResource, BUS_CHILD_ENDPOINTS};
use crate::le::{put_i32, put_u16, put_u32};
use crate::time::Duration64;
use crate::Errno;

fn endpoint() -> u64 {
    DMA_CONTROLLER_ENDPOINTS.endpoint(12)
}

fn line() -> DmaRequestLine {
    DmaRequestLine::new(endpoint(), 0, &[2], b"tx").expect("valid line")
}

fn params() -> CyclicParams {
    CyclicParams {
        fifo: 0xFE20_3004,
        direction: DmaDirection::MemoryToDevice,
        period_bytes: 1920,
        periods: 4,
    }
}

fn every_request() -> [DmaEngineRequest; 7] {
    [
        DmaEngineRequest::Open(line()),
        DmaEngineRequest::Prepare {
            channel: 2,
            params: params(),
        },
        DmaEngineRequest::Start { channel: 2 },
        DmaEngineRequest::Stop { channel: 2 },
        DmaEngineRequest::Position { channel: 63 },
        DmaEngineRequest::Close { channel: 0 },
        DmaEngineRequest::Wait {
            channel: 2,
            after: u64::MAX - 1,
        },
    ]
}

fn frame(request: &DmaEngineRequest) -> ([u8; DMA_ENGINE_MAX_REQUEST], usize) {
    let mut out = [0u8; DMA_ENGINE_MAX_REQUEST];
    let len = request.encode(&mut out).expect("fits the largest frame");
    (out, len)
}

#[test]
fn every_request_round_trips_at_its_exact_length() {
    for request in every_request() {
        let (out, len) = frame(&request);
        assert_eq!(DmaEngineRequest::decode(&out[..len]), Ok(request));
        assert_eq!(out[6], request.op() as u8);
    }
}

#[test]
fn the_largest_frames_are_the_quoted_record_and_the_wait_report() {
    assert_eq!(DMA_ENGINE_MAX_REQUEST, 8 + HwResource::WIRE_LEN);
    assert_eq!(DMA_ENGINE_MAX_REPLY, 8 + 16 + Duration64::WIRE_LEN);
    for request in every_request() {
        assert!(frame(&request).1 <= DMA_ENGINE_MAX_REQUEST);
    }
}

#[test]
fn a_frame_of_the_wrong_length_is_refused_either_way() {
    for request in every_request() {
        let (out, len) = frame(&request);
        assert_eq!(
            DmaEngineRequest::decode(&out[..len - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut longer = [0u8; DMA_ENGINE_MAX_REQUEST + 1];
        longer[..len].copy_from_slice(&out[..len]);
        assert_eq!(
            DmaEngineRequest::decode(&longer[..=len]),
            Err(Errno::LengthOutOfRange),
            "{request:?}"
        );
    }
    assert_eq!(DmaEngineRequest::decode(&[]), Err(Errno::BufferTooSmall));
    let mut short = [0u8; 4];
    assert_eq!(
        DmaEngineRequest::Open(line()).encode(&mut short),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn the_header_is_checked_whole() {
    let (good, len) = frame(&DmaEngineRequest::Start { channel: 1 });
    let mut magic = good;
    magic[0] ^= 1;
    assert_eq!(
        DmaEngineRequest::decode(&magic[..len]),
        Err(Errno::BadMagic)
    );
    let mut version = good;
    put_u16(&mut version, 4, 2);
    assert_eq!(
        DmaEngineRequest::decode(&version[..len]),
        Err(Errno::AbiVersionUnsupported)
    );
    let mut reserved = good;
    reserved[7] = 1;
    assert_eq!(
        DmaEngineRequest::decode(&reserved[..len]),
        Err(Errno::BadMagic)
    );
    for op in [0u8, 8, 0xFF] {
        let mut unknown = good;
        unknown[6] = op;
        assert_eq!(
            DmaEngineRequest::decode(&unknown[..len]),
            Err(Errno::OutOfRange)
        );
    }
}

#[test]
fn every_reserved_body_byte_must_be_zero() {
    for request in every_request() {
        let (good, len) = frame(&request);
        let reserved: &[usize] = match request {
            DmaEngineRequest::Open(_) => &[],
            DmaEngineRequest::Prepare { .. } => &[2, 3, 12, 13, 14, 15],
            _ => &[1, 2, 3, 4, 5, 6, 7],
        };
        for &offset in reserved {
            let mut dirty = good;
            dirty[HEADER_LEN + offset] = 0x80;
            assert_eq!(
                DmaEngineRequest::decode(&dirty[..len]),
                Err(Errno::BadMagic),
                "{request:?} byte {offset}"
            );
        }
    }
}

#[test]
fn a_channel_past_the_mask_width_is_refused() {
    for request in every_request() {
        if matches!(request, DmaEngineRequest::Open(_)) {
            continue;
        }
        let (mut out, len) = frame(&request);
        out[HEADER_LEN] = DMA_MAX_CHANNELS;
        assert_eq!(
            DmaEngineRequest::decode(&out[..len]),
            Err(Errno::OutOfRange),
            "{request:?}"
        );
    }
}

#[test]
fn a_prepare_refuses_an_unknown_direction_and_an_impossible_buffer() {
    let prepare = |params| DmaEngineRequest::Prepare { channel: 0, params };
    let (good, len) = frame(&prepare(params()));
    for direction in [0u8, 3, 0xFF] {
        let mut out = good;
        out[HEADER_LEN + 1] = direction;
        assert_eq!(
            DmaEngineRequest::decode(&out[..len]),
            Err(Errno::OutOfRange)
        );
    }
    for (period_bytes, periods) in [(0, 4), (1920, 0), (u32::MAX, 2), (0x1_0000, 0x1_0000)] {
        let mut out = good;
        put_u32(&mut out, HEADER_LEN + 4, period_bytes);
        put_u32(&mut out, HEADER_LEN + 8, periods);
        assert_eq!(
            DmaEngineRequest::decode(&out[..len]),
            Err(Errno::LengthOutOfRange),
            "{period_bytes} x {periods}"
        );
    }
    assert_eq!(params().buffer_bytes(), Ok(7680));
    let largest = CyclicParams {
        period_bytes: 0x8000_0000,
        periods: 1,
        ..params()
    };
    assert_eq!(largest.buffer_bytes(), Ok(0x8000_0000));
}

#[test]
fn an_open_quotes_only_a_canonical_request_line() {
    let (good, len) = frame(&DmaEngineRequest::Open(line()));
    let mut not_a_request = good;
    not_a_request[HEADER_LEN..len].copy_from_slice(&HwResource::endpoint(endpoint()).to_le_bytes());
    assert_eq!(
        DmaEngineRequest::decode(&not_a_request[..len]),
        Err(Errno::OutOfRange)
    );
    let mut unknown_kind = good;
    put_u16(&mut unknown_kind, HEADER_LEN, 0xFFFF);
    assert_eq!(
        DmaEngineRequest::decode(&unknown_kind[..len]),
        Err(Errno::BadMagic)
    );
    // A cell count past two, in the record's flags.
    let mut three_cells = good;
    three_cells[HEADER_LEN + 5] = 3;
    assert_eq!(
        DmaEngineRequest::decode(&three_cells[..len]),
        Err(Errno::BadMagic)
    );
}

#[test]
fn a_request_line_refuses_what_its_record_could_not_carry() {
    assert_eq!(
        DmaRequestLine::new(BUS_CHILD_ENDPOINTS.endpoint(12), 0, &[2], b"tx"),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        DmaRequestLine::new(endpoint(), 0, &[2], b"t\0x"),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        DmaRequestLine::new(endpoint(), 0, &[1, 2, 3], b"tx"),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        DmaRequestLine::new(endpoint(), 0, &[2], b"audio-out"),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        DmaControllerDuty::new(BUS_CHILD_ENDPOINTS.endpoint(12), None),
        Err(Errno::OutOfRange)
    );
    let unnamed = DmaRequestLine::new(endpoint(), 4, &[], &[]).expect("valid");
    assert!(unnamed.name().is_empty());
    assert!(unnamed.specifier().is_empty());
}

#[test]
fn every_reply_round_trips() {
    let mut out = [0u8; DMA_ENGINE_MAX_REPLY];
    let len = encode_open_reply(&mut out, 5).expect("encodes");
    assert_eq!(decode_open_reply(&out[..len]), Ok(5));

    let len = encode_prepare_reply(&mut out, 0x77).expect("encodes");
    assert_eq!(decode_prepare_reply(&out[..len]), Ok(0x77));

    let len = encode_position_reply(&mut out, 0x1234).expect("encodes");
    assert_eq!(decode_position_reply(&out[..len]), Ok(0x1234));

    for op in [DmaEngineOp::Start, DmaEngineOp::Stop, DmaEngineOp::Close] {
        let len = encode_done_reply(&mut out, op).expect("encodes");
        assert_eq!(decode_done_reply(&out[..len], op), Ok(()));
    }

    let serviced = Duration64::new(12, 999_999_999).expect("canonical");
    for end in [
        WaitEnd::Boundary,
        WaitEnd::Stopped,
        WaitEnd::Faulted(NonZeroU32::new(0b110).expect("non-zero")),
    ] {
        let report = WaitReport {
            end,
            position: 7680,
            serviced,
        };
        let len = encode_wait_reply(&mut out, &report).expect("encodes");
        assert_eq!(len, DMA_ENGINE_MAX_REPLY);
        assert_eq!(decode_wait_reply(&out[..len]), Ok(report));
    }
}

#[test]
fn a_reply_names_the_operation_it_answers() {
    let mut out = [0u8; DMA_ENGINE_MAX_REPLY];
    // An open and a position reply are the same length; the operation byte
    // is what keeps one from being read as the other.
    let len = encode_position_reply(&mut out, 5).expect("encodes");
    assert_eq!(decode_open_reply(&out[..len]), Err(Errno::BadMagic));
    let len = encode_done_reply(&mut out, DmaEngineOp::Stop).expect("encodes");
    assert_eq!(
        decode_done_reply(&out[..len], DmaEngineOp::Start),
        Err(Errno::BadMagic)
    );
    assert_eq!(
        encode_done_reply(&mut out, DmaEngineOp::Wait),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        decode_done_reply(&out[..len], DmaEngineOp::Position),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_refusal_reaches_every_decoder_as_its_own_errno() {
    let mut out = [0u8; DMA_ENGINE_MAX_REPLY];
    for err in [Errno::PermissionDenied, Errno::Busy, Errno::DeviceFault] {
        let len = encode_error_reply(&mut out, err).expect("encodes");
        assert_eq!(len, REPLY_HEADER_LEN);
        assert_eq!(decode_open_reply(&out[..len]), Err(err));
        assert_eq!(decode_prepare_reply(&out[..len]), Err(err));
        assert_eq!(decode_position_reply(&out[..len]), Err(err));
        assert_eq!(decode_wait_reply(&out[..len]), Err(err));
        assert_eq!(decode_done_reply(&out[..len], DmaEngineOp::Stop), Err(err));
    }
    assert_eq!(
        encode_error_reply(&mut [0u8; 4], Errno::Busy),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_malformed_reply_is_refused_rather_than_half_read() {
    let mut out = [0u8; DMA_ENGINE_MAX_REPLY];
    let len = encode_open_reply(&mut out, 5).expect("encodes");
    let mut positive = out;
    put_i32(&mut positive, 0, 3);
    assert_eq!(decode_open_reply(&positive[..len]), Err(Errno::BadMagic));
    let mut unknown = out;
    put_i32(&mut unknown, 0, -9999);
    assert_eq!(decode_open_reply(&unknown[..len]), Err(Errno::BadMagic));
    let mut dirty = out;
    dirty[6] = 1;
    assert_eq!(decode_open_reply(&dirty[..len]), Err(Errno::BadMagic));
    let mut dirty_body = out;
    dirty_body[REPLY_HEADER_LEN + 3] = 1;
    assert_eq!(decode_open_reply(&dirty_body[..len]), Err(Errno::BadMagic));
    let mut wide_channel = out;
    wide_channel[REPLY_HEADER_LEN] = DMA_MAX_CHANNELS;
    assert_eq!(
        decode_open_reply(&wide_channel[..len]),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        decode_open_reply(&out[..len - 1]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(
        decode_open_reply(&out[..=len]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(decode_open_reply(&out[..3]), Err(Errno::BufferTooSmall));

    assert_eq!(encode_prepare_reply(&mut out, 0), Err(Errno::OutOfRange));
    let len = encode_prepare_reply(&mut out, 1).expect("encodes");
    out[REPLY_HEADER_LEN] = 0;
    assert_eq!(decode_prepare_reply(&out[..len]), Err(Errno::OutOfRange));
    assert_eq!(encode_open_reply(&mut out, 64), Err(Errno::OutOfRange));
}

#[test]
fn a_wait_report_carries_error_bits_exactly_when_it_faulted() {
    let mut out = [0u8; DMA_ENGINE_MAX_REPLY];
    let report = WaitReport {
        end: WaitEnd::Boundary,
        position: 1,
        serviced: Duration64::ZERO,
    };
    let len = encode_wait_reply(&mut out, &report).expect("encodes");
    let end = REPLY_HEADER_LEN;
    let errors = REPLY_HEADER_LEN + 4;

    let mut boundary_with_errors = out;
    put_u32(&mut boundary_with_errors, errors, 1);
    let mut stopped_with_errors = boundary_with_errors;
    stopped_with_errors[end] = 2;
    let mut fault_without_errors = out;
    fault_without_errors[end] = 3;
    let mut dirty = out;
    dirty[end + 1] = 1;
    let mut clock = out;
    put_u32(&mut clock, REPLY_HEADER_LEN + 16 + 8, 1_000_000_000);
    for bad in [
        boundary_with_errors,
        stopped_with_errors,
        fault_without_errors,
        dirty,
        clock,
    ] {
        assert_eq!(decode_wait_reply(&bad[..len]), Err(Errno::BadMagic));
    }
    for unknown in [0u8, 4, 0xFF] {
        let mut out = out;
        out[end] = unknown;
        assert_eq!(decode_wait_reply(&out[..len]), Err(Errno::OutOfRange));
    }
}

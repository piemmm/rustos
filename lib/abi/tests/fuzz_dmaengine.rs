//! Deterministic fuzz harness for the `dmaengine-v1` wire surface and the two
//! hardware-tree records it rests on.
//!
//! A DMA controller's driver is the one process allowed to write control
//! blocks, so every frame it decodes arrives from a less trusted consumer
//! driver, and every reply a consumer decodes arrives from a process it did
//! not write. The invariants driven here:
//!
//! * decoding any byte image as a request, any reply, or any record never
//!   panics and never reads out of bounds;
//! * anything accepted re-encodes to exactly the bytes it came from, so a
//!   controller asking the kernel whether its caller holds a quoted request
//!   line asks about precisely the record the caller sent;
//! * an accepted request never names a channel past the mask width or a
//!   buffer the size arithmetic could not represent.
//!
//! A plain `cargo test` runs the fixed smoke sweep; `cargo xtask fuzz`
//! extends the loop to a wall-clock budget.

use core::num::NonZeroU32;

use tairix_abi::driver::dmaengine::{
    decode_done_reply, decode_open_reply, decode_position_reply, decode_prepare_reply,
    decode_wait_reply, encode_done_reply, encode_error_reply, encode_open_reply,
    encode_position_reply, encode_prepare_reply, encode_wait_reply, CyclicParams,
    DmaControllerDuty, DmaDirection, DmaEngineOp, DmaEngineRequest, DmaRequestLine, WaitEnd,
    WaitReport, DMA_CONTROLLER_ENDPOINTS, DMA_ENGINE_MAX_REPLY, DMA_ENGINE_MAX_REQUEST,
    DMA_MAX_CHANNELS,
};
use tairix_abi::hwtree::HwResource;
use tairix_abi::time::Duration64;
use tairix_abi::Errno;
use tairix_fuzzseed::Prng;

const SMOKE_ITERATIONS: u64 = 8_000;

fn scramble(frame: &mut [u8], most: usize, rng: &mut Prng) {
    if frame.is_empty() {
        return;
    }
    for _ in 0..rng.at_most(most) {
        let pos = rng.below(frame.len());
        frame[pos] ^= rng.next_u8();
    }
}

fn line(index: u8, specifier: &[u32], name: &[u8]) -> DmaRequestLine {
    DmaRequestLine::new(DMA_CONTROLLER_ENDPOINTS.endpoint(7), index, specifier, name)
        .expect("a valid seed line")
}

fn request_seeds() -> Vec<Vec<u8>> {
    let params = CyclicParams {
        fifo: 0xFE20_C018,
        direction: DmaDirection::MemoryToDevice,
        period_bytes: 4096,
        periods: 3,
    };
    [
        DmaEngineRequest::Open(line(0, &[0x2000_000D], b"rx-tx")),
        DmaEngineRequest::Open(line(1, &[1, 2], b"audio-rx")),
        DmaEngineRequest::Open(line(2, &[], b"")),
        DmaEngineRequest::Prepare { channel: 3, params },
        DmaEngineRequest::Prepare {
            channel: 63,
            params: CyclicParams {
                direction: DmaDirection::DeviceToMemory,
                ..params
            },
        },
        DmaEngineRequest::Start { channel: 1 },
        DmaEngineRequest::Stop { channel: 2 },
        DmaEngineRequest::Position { channel: 4 },
        DmaEngineRequest::Close { channel: 5 },
        DmaEngineRequest::Wait {
            channel: 6,
            after: 0x7FFF_FFFF_FFFF,
        },
    ]
    .iter()
    .map(|request| {
        let mut out = vec![0u8; DMA_ENGINE_MAX_REQUEST];
        let len = request.encode(&mut out).expect("the seed fits");
        out.truncate(len);
        out
    })
    .collect()
}

fn reply_seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    let mut push = |encode: &dyn Fn(&mut [u8]) -> Result<usize, Errno>| {
        let mut out = vec![0u8; DMA_ENGINE_MAX_REPLY];
        let len = encode(&mut out).expect("the seed fits");
        out.truncate(len);
        seeds.push(out);
    };
    push(&|out| encode_open_reply(out, 9));
    push(&|out| encode_prepare_reply(out, 0x42));
    push(&|out| encode_position_reply(out, 0x8000));
    push(&|out| encode_done_reply(out, DmaEngineOp::Stop));
    push(&|out| encode_error_reply(out, Errno::PermissionDenied));
    for end in [
        WaitEnd::Boundary,
        WaitEnd::Stopped,
        WaitEnd::Faulted(NonZeroU32::new(4).expect("non-zero")),
    ] {
        push(&move |out| {
            encode_wait_reply(
                out,
                &WaitReport {
                    end,
                    position: 12_288,
                    serviced: Duration64::new(3, 5).expect("canonical"),
                },
            )
        });
    }
    seeds
}

fn exercise_request(bytes: &[u8]) {
    let Ok(request) = DmaEngineRequest::decode(bytes) else {
        return;
    };
    let mut out = vec![0u8; DMA_ENGINE_MAX_REQUEST];
    let len = request
        .encode(&mut out)
        .expect("an accepted request re-encodes");
    assert_eq!(&out[..len], bytes, "a request was normalised on decode");
    let channel = match request {
        DmaEngineRequest::Open(_) => None,
        DmaEngineRequest::Prepare { channel, params } => {
            assert!(
                params.buffer_bytes().is_ok(),
                "an impossible buffer was accepted"
            );
            Some(channel)
        }
        DmaEngineRequest::Start { channel }
        | DmaEngineRequest::Stop { channel }
        | DmaEngineRequest::Position { channel }
        | DmaEngineRequest::Close { channel }
        | DmaEngineRequest::Wait { channel, .. } => Some(channel),
    };
    if let Some(channel) = channel {
        assert!(channel < DMA_MAX_CHANNELS, "a channel past the mask width");
    }
}

fn exercise_replies(bytes: &[u8]) {
    let mut out = vec![0u8; DMA_ENGINE_MAX_REPLY];
    if let Ok(channel) = decode_open_reply(bytes) {
        let len = encode_open_reply(&mut out, channel).expect("re-encodes");
        assert_eq!(&out[..len], bytes);
    }
    if let Ok(grant) = decode_prepare_reply(bytes) {
        let len = encode_prepare_reply(&mut out, grant).expect("re-encodes");
        assert_eq!(&out[..len], bytes);
    }
    if let Ok(offset) = decode_position_reply(bytes) {
        let len = encode_position_reply(&mut out, offset).expect("re-encodes");
        assert_eq!(&out[..len], bytes);
    }
    for op in [DmaEngineOp::Start, DmaEngineOp::Stop, DmaEngineOp::Close] {
        if decode_done_reply(bytes, op).is_ok() {
            let len = encode_done_reply(&mut out, op).expect("re-encodes");
            assert_eq!(&out[..len], bytes);
        }
    }
    if let Ok(report) = decode_wait_reply(bytes) {
        let len = encode_wait_reply(&mut out, &report).expect("re-encodes");
        assert_eq!(&out[..len], bytes);
    }
}

fn exercise_record(bytes: &[u8]) {
    let Ok(record) = HwResource::from_bytes(bytes) else {
        return;
    };
    if let Ok(line) = record.dma_request_line() {
        assert_eq!(HwResource::dma_request(&line), record);
        assert!(DMA_CONTROLLER_ENDPOINTS.contains(line.endpoint()));
    }
    if let Ok(duty) = record.dma_controller_duty() {
        assert_eq!(HwResource::dma_controller(&duty), record);
        assert!(DMA_CONTROLLER_ENDPOINTS.contains(duty.endpoint()));
    }
}

fn record_seeds() -> Vec<[u8; HwResource::WIRE_LEN]> {
    let endpoint = DMA_CONTROLLER_ENDPOINTS.endpoint(7);
    vec![
        HwResource::dma_request(&line(0, &[0x2000_000D], b"rx-tx")).to_le_bytes(),
        HwResource::dma_request(&line(3, &[1, 2], b"audio-rx")).to_le_bytes(),
        HwResource::dma_controller(&DmaControllerDuty::new(endpoint, Some(0x7F5)).expect("valid"))
            .to_le_bytes(),
        HwResource::dma_controller(&DmaControllerDuty::new(endpoint, None).expect("valid"))
            .to_le_bytes(),
    ]
}

#[test]
fn decoding_any_dmaengine_frame_never_panics() {
    let requests = request_seeds();
    let replies = reply_seeds();
    let records = record_seeds();
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "decoding_any_dmaengine_frame_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        let seed = rng.pick(&requests);
        let mut mutated = seed.clone();
        scramble(&mut mutated, 8, &mut rng);
        exercise_request(&mutated);
        exercise_request(&seed[..rng.at_most(seed.len())]);
        let mut longer = seed.clone();
        longer.push(rng.next_u8());
        exercise_request(&longer);
        let mut noise = vec![0u8; rng.at_most(DMA_ENGINE_MAX_REQUEST + 8)];
        rng.fill(&mut noise);
        exercise_request(&noise);

        let seed = rng.pick(&replies);
        let mut mutated = seed.clone();
        scramble(&mut mutated, 8, &mut rng);
        exercise_replies(&mutated);
        exercise_replies(&seed[..rng.at_most(seed.len())]);
        let mut noise = vec![0u8; rng.at_most(DMA_ENGINE_MAX_REPLY + 8)];
        rng.fill(&mut noise);
        exercise_replies(&noise);

        let mut record = *rng.pick(&records);
        scramble(&mut record, 6, &mut rng);
        exercise_record(&record);
        let mut noise = [0u8; HwResource::WIRE_LEN];
        rng.fill(&mut noise);
        exercise_record(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

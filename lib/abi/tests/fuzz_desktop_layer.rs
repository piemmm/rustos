//! Deterministic fuzz harness for the desktop-layer surface's wire surface:
//! the three capability-gated request frames, the two feed events, and the
//! terrain reply.
//!
//! These frames cross a trust boundary in both directions. A request arrives
//! from an application the desktop does not trust — one that holds
//! `CAP_DESKTOP_LAYER` is still ordinary user code, and one that does not may
//! still post bytes — and the terrain reply arrives at an application from a
//! session it authenticates but whose frame may still be truncated or corrupt.
//! A malformed extent, a depth outside the closed set, a dirty reserved tail, a
//! plate naming no area, or a count beyond the bound must all be **rejected**
//! fail-closed, never trusted and never a panic. The invariants driven here:
//!
//! * feeding any byte image to `WindowRequest::from_bytes`,
//!   `WindowEvent::from_bytes`, and `decode_terrain_reply` never panics and
//!   never reads out of bounds — each returns a validated value or an `Errno`;
//! * anything a decoder *accepts* re-encodes and re-decodes identically
//!   (round-trip stability), so nothing is silently normalised on the way in;
//! * an accepted `OpenLayer` never carries an extent past
//!   `DESKTOP_LAYER_MAX_SIDE_LOGICAL` — the bound that keeps a layer surface
//!   from reproducing a surface the user is meant to trust.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates valid
//! seed frames and feeds pure noise. A plain `cargo test` runs the fixed smoke
//! sweep; `cargo xtask fuzz` extends the loop to a wall-clock budget.

use tairix_abi::driver::display::DisplayFormat;
use tairix_abi::window_ipc::{
    decode_terrain_reply, encode_terrain_reply, LayerDepth, TerrainPlate, WindowEvent,
    WindowRequest, DESKTOP_LAYER_MAX_PLATES, DESKTOP_LAYER_MAX_SIDE_LOGICAL,
    WINDOW_TERRAIN_REPLY_MAX,
};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// A well-formed `OpenLayer` as a mutation seed.
fn seed_open() -> Vec<u8> {
    encode(&WindowRequest::OpenLayer {
        shm_handle: 0x4242,
        event_endpoint: 0x99,
        frame_count: 2,
        width_px: 176,
        height_px: 176,
        stride_bytes: 176 * 4,
        format: DisplayFormat::Bgra8888,
        x: -40,
        y: 900,
        depth: LayerDepth::Above,
    })
}

/// A well-formed `PlaceLayer` as a mutation seed.
fn seed_place() -> Vec<u8> {
    encode(&WindowRequest::PlaceLayer {
        window_id: 7,
        x: 12,
        y: -34,
        depth: LayerDepth::Below,
    })
}

/// A well-formed `TakeTerrain` as a mutation seed.
fn seed_terrain_request() -> Vec<u8> {
    encode(&WindowRequest::TakeTerrain { window_id: 7 })
}

/// A well-formed terrain reply carrying a handful of plates.
fn seed_terrain_reply() -> Vec<u8> {
    let plates = [
        TerrainPlate {
            x: 0,
            y: 0,
            width_px: 640,
            height_px: 480,
        },
        TerrainPlate {
            x: -20,
            y: 300,
            width_px: 100,
            height_px: 40,
        },
    ];
    let mut out = vec![0u8; WINDOW_TERRAIN_REPLY_MAX];
    let len = encode_terrain_reply(&plates, &mut out).expect("a two-plate answer");
    out.truncate(len);
    out
}

/// Encode `request` into a frame of exactly its own length.
fn encode(request: &WindowRequest) -> Vec<u8> {
    let mut out = vec![0u8; request.wire_len()];
    let len = request.encode(&mut out).expect("the seed frame fits");
    out.truncate(len);
    out
}

/// Decode `bytes` as a request; anything accepted must round-trip, and an
/// accepted layer open must be inside the bound the surface's containment
/// rests on.
fn exercise_request(bytes: &[u8]) {
    let Ok(request) = WindowRequest::from_bytes(bytes) else {
        return;
    };
    if let WindowRequest::OpenLayer {
        width_px,
        height_px,
        ..
    } = request
    {
        assert!(
            width_px <= DESKTOP_LAYER_MAX_SIDE_LOGICAL
                && height_px <= DESKTOP_LAYER_MAX_SIDE_LOGICAL,
            "an accepted layer surface must never exceed the bound that keeps it \
             from reproducing a trusted surface ({width_px}x{height_px})"
        );
    }
    let round = WindowRequest::from_bytes(&encode(&request)).expect("re-decode of accepted frame");
    assert_eq!(round, request, "a layer request is not round-trip stable");
}

/// Decode `bytes` as an event; anything accepted must round-trip.
fn exercise_event(bytes: &[u8]) {
    if bytes.len() < WindowEvent::WIRE_LEN {
        // The decoder still has to refuse it rather than read short.
        assert!(WindowEvent::from_bytes(bytes).is_err());
        return;
    }
    let Ok(event) = WindowEvent::from_bytes(bytes) else {
        return;
    };
    let round = WindowEvent::from_bytes(&event.to_le_bytes()).expect("re-decode of accepted event");
    assert_eq!(round, event, "a layer event is not round-trip stable");
}

/// Decode `bytes` as a terrain reply; anything accepted must round-trip, and
/// every plate it yields must name a real area.
fn exercise_terrain(bytes: &[u8]) {
    let mut out = [TerrainPlate {
        x: 0,
        y: 0,
        width_px: 1,
        height_px: 1,
    }; DESKTOP_LAYER_MAX_PLATES as usize];
    let Ok(plates) = decode_terrain_reply(bytes, &mut out) else {
        return;
    };
    for plate in plates {
        assert!(
            plate.width_px != 0 && plate.height_px != 0,
            "a plate naming no area must have refused the whole frame"
        );
    }
    let accepted: Vec<TerrainPlate> = plates.to_vec();
    let mut again = vec![0u8; WINDOW_TERRAIN_REPLY_MAX];
    let len = encode_terrain_reply(&accepted, &mut again).expect("re-encode of accepted page");
    let mut round = [TerrainPlate {
        x: 0,
        y: 0,
        width_px: 1,
        height_px: 1,
    }; DESKTOP_LAYER_MAX_PLATES as usize];
    let reread = decode_terrain_reply(&again[..len], &mut round).expect("re-decode");
    assert_eq!(
        reread,
        &accepted[..],
        "a terrain page is not round-trip stable"
    );
}

#[test]
fn decoding_any_desktop_layer_frame_never_panics() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let seeds = [seed_open(), seed_place(), seed_terrain_request()];
    let reply_seed = seed_terrain_reply();
    let event_seeds = [
        WindowEvent::TerrainChanged {
            window_id: 7,
            generation: 12,
        }
        .to_le_bytes(),
        WindowEvent::LayerPointer {
            window_id: 7,
            x: -5,
            y: 900,
        }
        .to_le_bytes(),
    ];

    let mut rng = Prng::new(tairix_fuzzseed::start(
        "decoding_any_desktop_layer_frame_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. A valid request with a handful of bytes flipped.
        let seed = rng.pick(&seeds);
        let mut mutated = seed.clone();
        let flips = rng.at_most(16);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = rng.below(mutated.len());
            mutated[pos] ^= rng.next_u8();
        }
        exercise_request(&mutated);

        // 2. A truncation of a valid request, driving the exact-length checks.
        let keep = rng.at_most(seed.len());
        exercise_request(&seed[..keep]);

        // 3. An over-long request: a byte past the operation's own end is a
        //    smuggled field, however innocuous its value.
        let mut longer = seed.clone();
        longer.push(rng.next_u8());
        exercise_request(&longer);

        // 4. Pure noise of an arbitrary length through the request decoder.
        let nlen = rng.at_most(512);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        exercise_request(&noise);

        // 5. A valid event with bytes flipped, driving the reserved-tail and
        //    closed-set checks.
        let mut event = *rng.pick(&event_seeds);
        let eflips = rng.at_most(6);
        for _ in 0..eflips {
            let pos = rng.below(event.len());
            event[pos] ^= rng.next_u8();
        }
        exercise_event(&event);

        // 6. Pure noise as an event frame.
        let elen = rng.at_most(WindowEvent::WIRE_LEN + 8);
        let mut enoise = vec![0u8; elen];
        rng.fill(&mut enoise);
        exercise_event(&enoise);

        // 7. A valid terrain reply with bytes flipped: a corrupt count, a
        //    dirty reserved pair, or a plate with no area must all refuse.
        let mut reply = reply_seed.clone();
        let rflips = rng.at_most(12);
        for _ in 0..rflips {
            if reply.is_empty() {
                break;
            }
            let pos = rng.below(reply.len());
            reply[pos] ^= rng.next_u8();
        }
        exercise_terrain(&reply);

        // 8. A truncated terrain reply, and pure noise as one.
        let rkeep = rng.at_most(reply_seed.len());
        exercise_terrain(&reply_seed[..rkeep]);
        let rlen = rng.at_most(WINDOW_TERRAIN_REPLY_MAX);
        let mut rnoise = vec![0u8; rlen];
        rng.fill(&mut rnoise);
        exercise_terrain(&rnoise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

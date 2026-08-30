//! Deterministic fuzz harness for the sandbox seam's decode, helpdoc,
//! imagerender, and timesync surfaces.
//!
//! Two hostile directions, both driven through the public client path so
//! the request encoder, the service's request decoder, the decoders
//! themselves, and the caller-side reply validation are all exercised
//! together:
//!
//! * **Hostile input files** — mutated container/help-document/icon
//!   templates and pure noise fed to
//!   [`tairix_sandbox::decode::container_summary`] / [`manifest_summary`] /
//!   [`disassemble`] / [`render_help`] / [`rasterise_icon`] over the
//!   in-process loopback worker — and mutated NTP server replies fed to
//!   [`tairix_sandbox::timesync::evaluate_datagram`]: every outcome must be
//!   a typed result, never a panic.
//! * **Hostile workers** — a launcher whose "worker" frames pure noise as
//!   its reply: the caller-side fail-closed reply decoders must refuse or
//!   accept typed, never panic, and the seam must survive.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG drives
//! the mutations through the shared `tairix_fuzzseed` seam. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to
//! extend the loop to a wall-clock budget.

use std::cell::RefCell;
use std::rc::Rc;

use tairix_sandbox::decode::{
    container_summary, disassemble, manifest_summary, DecodeService, Isa,
};
use tairix_sandbox::helpdoc::{render_help, HelpService, RenderMode, Styling};
use tairix_sandbox::host::{Launcher, ParserSandbox};
use tairix_sandbox::imagerender::{
    rasterise_icon, render_wallpaper, ImageRenderService, MAX_ICON_SIDE, MAX_WALLPAPER_WIDTH,
};
use tairix_sandbox::loopback::LoopbackLauncher;
use tairix_sandbox::proto::Channel;
use tairix_sandbox::timesync::{evaluate_datagram, TimeSyncService};
use tairix_wallpaper::WallpaperFit;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 2_000;

/// Largest arbitrary byte string fed as an input file or a hostile reply.
const MAX_NOISE: usize = 2048;

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// A well-formed stratum-2 NTP server reply echoing `nonce`, reporting an
/// instant inside the plausibility window — the template mutations start from.
fn ntp_reply_template(nonce: u64) -> Vec<u8> {
    /// Seconds from the NTP epoch (1900) to the Unix epoch (1970).
    const NTP_UNIX_DELTA: i64 = 2_208_988_800;
    let unix_secs = tairix_abi::RELEASE_EPOCH_SECS + 86_400;
    let field = u32::try_from((unix_secs + NTP_UNIX_DELTA).rem_euclid(1 << 32))
        .expect("reduced modulo 2^32");
    let ts = (u64::from(field) << 32).to_be_bytes();
    let mut p = vec![0u8; tairix_net::ntp::PACKET_LEN];
    p[0] = (4 << 3) | 4; // version 4, mode 4 (server)
    p[1] = 2; // stratum
    p[24..32].copy_from_slice(&nonce.to_be_bytes());
    p[32..40].copy_from_slice(&ts);
    p[40..48].copy_from_slice(&ts);
    p
}

/// One help-render fuzz iteration: a help document with a handful of bytes
/// flipped, a random truncation, and pure noise, rendered through the honest
/// worker under every surface, styling level, and served-locale tag (including
/// malformed spellings, so the request grammar is fuzzed too).
///
/// Returns the render mode used, so the caller can reuse it against the
/// hostile worker.
fn fuzz_help_iteration(
    sandbox: &mut ParserSandbox<LoopbackLauncher<fn() -> HelpService>, SilentSink>,
    noise: &[u8],
    next: &mut impl FnMut() -> u64,
) -> RenderMode {
    let mut help = HELP_TEMPLATE.to_vec();
    for _ in 0..bounded(next(), 6) {
        let pos = bounded(next(), help.len() - 1);
        help[pos] ^= low_byte(next() >> 17);
    }
    let mode = if next() & 1 == 0 {
        RenderMode::Short
    } else {
        RenderMode::Full
    };
    let styling = match bounded(next(), 2) {
        0 => Styling::Plain,
        1 => Styling::Monochrome,
        _ => Styling::Colour,
    };
    let locales = ["en-US", "fr-FR", "zh-CN", "", "not a tag", "xx-XX"];
    let locale = locales[bounded(next(), locales.len() - 1)];
    let _ = render_help(sandbox, mode, styling, locale, &help);
    let cut = bounded(next(), help.len());
    let _ = render_help(sandbox, mode, styling, locale, &help[..cut]);
    let _ = render_help(sandbox, mode, styling, locale, noise);
    mode
}

/// One NTP-evaluation fuzz iteration: a mutated well-formed reply, a
/// truncated one, and pure noise, each evaluated through the honest worker
/// under both a matching and a mismatched nonce. A sample that survives must
/// be one the engine's own rules admit — the caller-side re-validation the
/// authority split rests on.
///
/// Returns the nonce and receive instant used, so the caller can reuse them
/// against the hostile worker.
fn fuzz_ntp_iteration(
    sandbox: &mut ParserSandbox<LoopbackLauncher<fn() -> TimeSyncService>, SilentSink>,
    noise: &[u8],
    next: &mut impl FnMut() -> u64,
) -> (u64, tairix_abi::time::Duration64) {
    let nonce = next();
    let mut packet = ntp_reply_template(nonce);
    for _ in 0..bounded(next(), 8) {
        let pos = bounded(next(), packet.len() - 1);
        packet[pos] ^= low_byte(next() >> 17);
    }
    let received = tairix_abi::time::Duration64::from_nanos(next() >> 24);
    let cut = bounded(next(), packet.len());
    for (label_nonce, bytes) in [
        (nonce, &packet[..]),
        (nonce ^ 1, &packet[..]),
        (nonce, &packet[..cut]),
        (nonce, noise),
    ] {
        let txn = tairix_net::ntp::Transaction {
            server: 0,
            nonce: tairix_net::ntp::NtpTimestamp::from_raw(label_nonce),
            sent_at: tairix_abi::time::Duration64::ZERO,
        };
        if let Ok(tairix_net::ntp::Reply::Sample(sample)) =
            evaluate_datagram(sandbox, &txn, received, bytes)
        {
            assert!(
                tairix_abi::is_plausible_wall_time(sample.true_time),
                "an implausible instant escaped the evaluation"
            );
            assert!(
                sample.round_trip <= tairix_net::ntp::MAX_ROUND_TRIP,
                "an over-long round trip escaped the evaluation"
            );
            assert!(
                (1..16).contains(&sample.stratum),
                "an unusable stratum escaped the evaluation"
            );
        }
    }
    (nonce, received)
}

/// Discards every logged event.
struct SilentSink;

impl tairix_log::Sink for SilentSink {
    fn write_event(&self, _event: &tairix_log::Event<'_>) {}
}

/// A minimal valid wasm module with two function bodies.
fn wasm_template() -> Vec<u8> {
    let mut bytes = b"\0asm\x01\0\0\0".to_vec();
    // code section: id 10, payload = count 2, two 3-byte bodies.
    let payload = [2u8, 3, 0, 1, 0x0B, 3, 0, 1, 0x0B];
    bytes.push(10);
    bytes.push(u8::try_from(payload.len()).expect("small"));
    bytes.extend_from_slice(&payload);
    bytes
}

/// A minimal valid rxe image (one RX segment, PIE, current ABI).
fn rxe_template() -> Vec<u8> {
    let segment = tairix_abi::Segment {
        vaddr: tairix_abi::RXE_PAGE_SIZE,
        file_offset: 0,
        file_size: 32,
        mem_size: tairix_abi::RXE_PAGE_SIZE,
        permission: tairix_abi::RxePermission::ReadExecute,
    };
    let header = tairix_abi::LoadHeader {
        magic: tairix_abi::LOAD_MAGIC,
        abi_version: tairix_abi::ABI_VERSION_CURRENT,
        flags: tairix_abi::LOAD_FLAG_PIE,
        segment_count: 1,
        needed_count: 0,
        entry: tairix_abi::RXE_PAGE_SIZE,
        cfi_tag: [0x5A; 32],
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&header.to_le_bytes());
    bytes.extend_from_slice(&segment.to_le_bytes());
    bytes
}

/// The four request ISAs, cycled by the driver.
const ISAS: [Isa; 4] = [Isa::X86_64, Isa::Aarch64, Isa::Riscv64, Isa::Wasm];

/// A minimal valid help document, as the helpdoc mutation template.
const HELP_TEMPLATE: &[u8] =
    b"## NAME\n\ntop \xe2\x80\x94 display tasks\n\n## SYNOPSIS\n\n`top [-d seconds]`\n\n## DESCRIPTION\n\nShows tasks.\n";

/// A minimal valid SVG icon, as the imagerender mutation template.
const SVG_TEMPLATE: &[u8] =
    br##"<svg viewBox="0 0 10 10"><polygon points="0,0 10,0 10,10 0,10" fill="#3070f0"/></svg>"##;

/// Standard CRC-32 (the polynomial PNG chunks use), computed over the
/// concatenation of `parts`.
fn crc32_of(parts: &[&[u8]]) -> u32 {
    fn update(mut crc: u32, byte: u8) -> u32 {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
        crc
    }
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            crc = update(crc, byte);
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn chunk(chunk_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u32::try_from(payload.len()).expect("test payload fits a u32 length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&chunk_type);
    out.extend_from_slice(payload);
    let crc = crc32_of(&[&chunk_type, payload]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// The Adler-32 checksum RFC 1950 requires as the zlib stream trailer.
fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Wrap `data` in a well-formed zlib stream built from a single STORED
/// deflate block, so no compressor is needed to produce a stream
/// `tairix_image`'s decoder accepts.
fn zlib_wrap(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78u8, 0x9C, 0x01];
    let len = u16::try_from(data.len()).expect("template fits a u16 length");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// The five wallpaper fits, cycled by the driver.
const FITS: [WallpaperFit; 5] = [
    WallpaperFit::Fill,
    WallpaperFit::Fit,
    WallpaperFit::Stretch,
    WallpaperFit::Centre,
    WallpaperFit::Tile,
];

/// A minimal valid 2x2 RGBA8 PNG icon, as both the imagerender and the
/// wallpaper mutation template.
fn png_template() -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
    let raw = [
        0, 10, 20, 30, 255, 40, 50, 60, 255, // row 0: filter None, 2 pixels
        0, 70, 80, 90, 255, 100, 110, 120, 255, // row 1
    ];
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend(chunk(*b"IHDR", &ihdr));
    bytes.extend(chunk(*b"IDAT", &zlib_wrap(&raw)));
    bytes.extend(chunk(*b"IEND", &[]));
    bytes
}

/// A "worker" that answers every request with seeded noise framed as a
/// reply — the compromised-worker model.
struct HostileChannel {
    pending: Vec<u8>,
    at: usize,
}

impl Channel for HostileChannel {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, tairix_abi::Errno> {
        if self.at == self.pending.len() || buf.is_empty() {
            return Ok(0);
        }
        let take = buf.len().min(self.pending.len() - self.at);
        buf[..take].copy_from_slice(&self.pending[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, tairix_abi::Errno> {
        Ok(buf.len())
    }
}

/// The honest icon-rasterisation sandbox the fuzz loop drives.
type HonestIconSandbox = ParserSandbox<LoopbackLauncher<fn() -> ImageRenderService>, SilentSink>;

/// Fuzz one iteration's icon coverage: an SVG icon and a PNG icon, each
/// with a handful of bytes flipped, a random truncation, and pure `noise`,
/// rasterised through the honest worker at a random side. Returns the side
/// used, so the caller can reuse it against the hostile worker too.
fn fuzz_icon_iteration(
    honest_icon: &mut HonestIconSandbox,
    noise: &[u8],
    next: &mut impl FnMut() -> u64,
) -> u32 {
    let side = u32::try_from(bounded(
        next(),
        usize::try_from(MAX_ICON_SIDE - 1).unwrap_or(0),
    ))
    .unwrap_or(0)
        + 1;
    let mut svg = SVG_TEMPLATE.to_vec();
    for _ in 0..bounded(next(), 6) {
        let pos = bounded(next(), svg.len() - 1);
        svg[pos] ^= low_byte(next() >> 17);
    }
    let _ = rasterise_icon(honest_icon, side, &svg);
    let cut = bounded(next(), svg.len());
    let _ = rasterise_icon(honest_icon, side, &svg[..cut]);
    let mut png = png_template();
    for _ in 0..bounded(next(), 6) {
        let pos = bounded(next(), png.len() - 1);
        png[pos] ^= low_byte(next() >> 17);
    }
    let _ = rasterise_icon(honest_icon, side, &png);
    let cut = bounded(next(), png.len());
    let _ = rasterise_icon(honest_icon, side, &png[..cut]);
    let _ = rasterise_icon(honest_icon, side, noise);
    side
}

/// Fuzz one iteration's wallpaper coverage: a PNG source, mutated, a
/// random truncation, and pure `noise`, rendered through the honest worker
/// (the same one [`fuzz_icon_iteration`] drives, since one worker serves
/// both surfaces) at a random small destination and fit. Returns the
/// destination and fit used, so the caller can reuse them against the
/// hostile worker too.
fn fuzz_wallpaper_iteration(
    honest: &mut HonestIconSandbox,
    noise: &[u8],
    next: &mut impl FnMut() -> u64,
) -> (u32, u32, WallpaperFit) {
    let width = u32::try_from(bounded(next(), 15)).unwrap_or(0) + 1;
    let height = u32::try_from(bounded(next(), 15)).unwrap_or(0) + 1;
    let fit = FITS[bounded(next(), FITS.len() - 1)];
    let mut png = png_template();
    for _ in 0..bounded(next(), 6) {
        let pos = bounded(next(), png.len() - 1);
        png[pos] ^= low_byte(next() >> 17);
    }
    let _ = render_wallpaper(honest, width, height, fit, &png);
    let cut = bounded(next(), png.len());
    let _ = render_wallpaper(honest, width, height, fit, &png[..cut]);
    let _ = render_wallpaper(honest, width, height, fit, noise);
    // Also exercise a destination one past the ceiling: always refused
    // locally, before any request is even sent, so this is cheap to run
    // every iteration unlike a genuine ceiling-sized render.
    let _ = render_wallpaper(honest, MAX_WALLPAPER_WIDTH + 1, height, fit, &png);
    (width, height, fit)
}

/// Launches [`HostileChannel`] workers with fresh noise per launch.
struct HostileLauncher {
    state: Rc<RefCell<u64>>,
}

impl Launcher for HostileLauncher {
    type Channel = HostileChannel;

    fn launch(&mut self) -> Result<HostileChannel, tairix_abi::Errno> {
        let mut state = self.state.borrow_mut();
        let mut next = || {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *state
        };
        let noise: Vec<u8> = (0..bounded(next(), MAX_NOISE))
            .map(|_| low_byte(next() >> 21))
            .collect();
        let mut pending = u32::try_from(noise.len())
            .expect("bounded noise")
            .to_le_bytes()
            .to_vec();
        pending.extend_from_slice(&noise);
        Ok(HostileChannel { pending, at: 0 })
    }

    fn dispose(&mut self, _channel: HostileChannel) -> Option<i32> {
        None
    }
}

#[test]
fn decode_surface_never_panics_for_any_input_or_reply() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "decode_surface_never_panics_for_any_input_or_reply",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut honest = ParserSandbox::new(
        LoopbackLauncher::new(DecodeService::default as fn() -> DecodeService),
        SilentSink,
    );
    let mut honest_help = ParserSandbox::new(
        LoopbackLauncher::new(HelpService::default as fn() -> HelpService),
        SilentSink,
    );
    let mut honest_icon = ParserSandbox::new(
        LoopbackLauncher::new(ImageRenderService::default as fn() -> ImageRenderService),
        SilentSink,
    );
    let mut honest_time = ParserSandbox::new(
        LoopbackLauncher::new(TimeSyncService::default as fn() -> TimeSyncService),
        SilentSink,
    );
    let hostile_state = Rc::new(RefCell::new(next()));
    let mut hostile = ParserSandbox::new(
        HostileLauncher {
            state: hostile_state,
        },
        SilentSink,
    );
    let wasm = wasm_template();
    let rxe = rxe_template();

    let mut iteration: u64 = 0;
    loop {
        // 1. A container template with a handful of bytes flipped, plus a
        //    random truncation, summarised through the honest worker.
        let template = if next() & 1 == 0 { &wasm } else { &rxe };
        let mut mutated = template.clone();
        for _ in 0..bounded(next(), 6) {
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        let _ = container_summary(&mut honest, &mutated);
        let cut = bounded(next(), mutated.len());
        let _ = container_summary(&mut honest, &mutated[..cut]);

        // 2. Pure noise as an input file and as a manifest.
        let noise: Vec<u8> = (0..bounded(next(), MAX_NOISE))
            .map(|_| low_byte(next() >> 29))
            .collect();
        let _ = container_summary(&mut honest, &noise);
        let _ = manifest_summary(&mut honest, &noise);

        // 3. Noise disassembled under every ISA at a random address,
        //    depth, and window size.
        let isa = ISAS[bounded(next(), ISAS.len() - 1)];
        let _ = disassemble(
            &mut honest,
            isa,
            next(),
            u32::from(low_byte(next())),
            u32::from(low_byte(next())),
            &noise,
        );

        // 4. Help documents through the honest worker, in their own helper
        //    to keep this loop's body a readable, bounded size. Returns the
        //    render mode used, so the caller can reuse it against the
        //    hostile worker.
        let mode = fuzz_help_iteration(&mut honest_help, &noise, &mut next);

        // 6. The icon-rasterisation and wallpaper surfaces, fuzzed in
        //    their own helpers to keep this loop's body a readable,
        //    bounded size. Both run over the same worker instance,
        //    exercising the two surfaces interleaved.
        let side = fuzz_icon_iteration(&mut honest_icon, &noise, &mut next);
        let (wallpaper_w, wallpaper_h, fit) =
            fuzz_wallpaper_iteration(&mut honest_icon, &noise, &mut next);

        // 7. NTP server replies through the honest worker, in its own helper
        //    to keep this loop's body a readable, bounded size. Returns the
        //    nonce used, so the caller can reuse it against the hostile
        //    worker too.
        let (nonce, received) = fuzz_ntp_iteration(&mut honest_time, &noise, &mut next);

        // 8. The hostile worker: framed noise replies into every client
        //    decoder. Each request crashes and replaces the worker, so
        //    every iteration sees fresh noise.
        let _ = container_summary(&mut hostile, &rxe);
        let _ = manifest_summary(&mut hostile, &noise[..bounded(next(), noise.len())]);
        let _ = disassemble(&mut hostile, isa, 0, 0, 8, b"\x90\x90");
        let _ = render_help(&mut hostile, mode, Styling::Colour, "en-US", HELP_TEMPLATE);
        let _ = rasterise_icon(&mut hostile, side, SVG_TEMPLATE);
        let _ = render_wallpaper(&mut hostile, wallpaper_w, wallpaper_h, fit, &png_template());
        let hostile_txn = tairix_net::ntp::Transaction {
            server: 0,
            nonce: tairix_net::ntp::NtpTimestamp::from_raw(nonce),
            sent_at: tairix_abi::time::Duration64::ZERO,
        };
        let _ = evaluate_datagram(
            &mut hostile,
            &hostile_txn,
            received,
            &ntp_reply_template(nonce),
        );

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

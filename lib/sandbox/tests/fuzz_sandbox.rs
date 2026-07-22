//! Deterministic fuzz harness for the sandbox seam's decode and helpdoc
//! surfaces.
//!
//! Two hostile directions, both driven through the public client path so
//! the request encoder, the service's request decoder, the decoders
//! themselves, and the caller-side reply validation are all exercised
//! together:
//!
//! * **Hostile input files** — mutated container/help-document templates
//!   and pure noise fed to [`tairix_sandbox::decode::container_summary`] /
//!   [`manifest_summary`] / [`disassemble`] / [`render_help`] over the
//!   in-process loopback worker: every outcome must be a typed result,
//!   never a panic.
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
use tairix_sandbox::loopback::LoopbackLauncher;
use tairix_sandbox::proto::Channel;

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

        // 4. A help document with a handful of bytes flipped, a random
        //    truncation, and pure noise, rendered through the honest
        //    worker under both surfaces.
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
        // Vary the styling level and the served-locale tag (including
        // malformed spellings) so the render request grammar is fuzzed too.
        let styling = match bounded(next(), 2) {
            0 => Styling::Plain,
            1 => Styling::Monochrome,
            _ => Styling::Colour,
        };
        let locales = ["en-US", "fr-FR", "zh-CN", "", "not a tag", "xx-XX"];
        let locale = locales[bounded(next(), locales.len() - 1)];
        let _ = render_help(&mut honest_help, mode, styling, locale, &help);
        let cut = bounded(next(), help.len());
        let _ = render_help(&mut honest_help, mode, styling, locale, &help[..cut]);
        let _ = render_help(&mut honest_help, mode, styling, locale, &noise);

        // 5. The hostile worker: framed noise replies into every client
        //    decoder. Each request crashes and replaces the worker, so
        //    every iteration sees fresh noise.
        let _ = container_summary(&mut hostile, &rxe);
        let _ = manifest_summary(&mut hostile, &noise[..bounded(next(), noise.len())]);
        let _ = disassemble(&mut hostile, isa, 0, 0, 8, b"\x90\x90");
        let _ = render_help(&mut hostile, mode, Styling::Colour, "en-US", HELP_TEMPLATE);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

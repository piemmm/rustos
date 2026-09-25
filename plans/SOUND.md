# SOUND — the audio stack

Binding under `AGENTS.md`. What the audio subsystem is, the one path every
sample takes, where each piece lives, the seams that keep decoding out of the
mixer and the mixer out of the kernel, and the two applications that play
audio.

TAIRiX has no audio today: no driver class, no mixer, no stream ABI, no
capability. `plans/NEW-DESKTOP-SETTINGS.md` §3 states that absence and names
this plan as the prerequisite; `plans/ALIAS.md` §6.11 already reserves the
`audio:` resource scheme, and `lib/icon`'s `Volume` glyph is already a speaker.
This plan is the whole subsystem, from the device register to the pixel of a
seek slider.

## Ledger

| # | Item | Status |
|---|---|---|
| SND1 | `plans/SOUND.md`, the jump-sheet row, the corrected Settings reference, and the `plans/USB.md` scope change | done |
| SND2 | `lib/abi`: `HwDeviceClass::Audio`, the PCM vocabulary, `audio_ring`, `audiochan-v1`, `audio-v1` | done |
| SND3 | `lib/audio`: conversion, mixer, resampler, channel mapping, clock model, routing policy, volume model, the stream client — all host-tested, plus the ring's loom model | done |
| SND4 | `lib/audiochan` serve loop; `drivers/audio/virtio_snd`; `userland/system/audiod`; `CAP_AUDIO_DEVICE` and `CAP_AUDIO_CAPTURE`; the end-to-end QEMU vertical asserting a sample-exact host WAV | done |
| SND5a | The DMA seam's ABI and discovery: `HwDeviceClass::Dma`, the `DmaController` duty and `DmaRequest` resources, the sixteen-resource node, the endpoint block and its wire protocol; the shared walk's `dmas` binding, per-entry `dma-ranges` and `interrupt-parent`; the Broadcom channel mask | done |
| SND5b | The three kernel prerequisites — `shm_create_dma` (quarantined with its creator), `shm_grant_peer`, `call_peer_holds` — and the duty-gated controller endpoint | done |
| SND5c | The `DmaEngine`/`DmaChannel` class trait and `drivers/dma/bcm2835`, host-tested against a register-level model that fetches control blocks | planned |
| SND6 | Isochronous transfer support: the endpoint kind and service-interval scheduling in `lib/usb`, and periodic bandwidth reservation, frame-indexed rings and feedback endpoints in `drivers/bus/usb/xhci` | planned |
| SND7 | `drivers/audio/usb_uac`: UAC1 and UAC2, clock and feature units, explicit and implicit feedback | planned |
| SND8 | `drivers/audio/bcm2711_pwm` with noise shaping; `drivers/audio/bcm2711_i2s` with a separately-bound codec | planned |
| SND9 | `lib/sound`: the registry, AU and WAV complete, the sandboxed decode seam, the fuzz target | planned |
| SND10 | `userland/apps/play`, with and without the curses interface, backgroundable | planned |
| SND11 | `drivers/audio/hda`: controller, CORB/RIRB, stream descriptors, the pure-graph codec walk; the QEMU `intel-hda` vertical | planned |
| SND12 | `lib/sound`: FLAC, decoder and feature-gated encoder, verified by round-trip and against each stream's own STREAMINFO digest | planned |
| SND13 | Seat integration: leases, pause-and-resume across a fast user switch, the capture indicator, the notice topic; the two-session vertical | planned |
| SND14 | `userland/apps/music` | planned |
| SND15 | Desktop integration: the Settings pane, the taskbar volume control and recording indicator, Switchboard, sysinfo, the `audio:` resolver, media types and icons, `audioctl` | planned |
| SND16 | `lib/sound`: MPEG audio Layers I/II/III, verified against the ISO compliance limits | planned |
| SND17 | `lib/sound`: Ogg container and Vorbis I | planned |
| SND18 | `lib/sound`: Opus, verified against the RFC 6716 vectors | planned |
| SND19 | `drivers/audio/rpi_hdmi` | blocked: needs a native VC6 HDMI encoder — the open decision below |
| SND20 | `lib/soundtheme`: the `SoundEvent` vocabulary, the shipped theme catalog, the settings document and the cue client; the shipped masters and their build-time family contract in `tools/syshelp` | planned |
| SND21 | `userland/system/soundd`: the cue authority, its two authority classes, resolution to silence, and the bounds; the sample-exact QEMU cue vertical | planned |
| SND22 | The cue sites: session login/logout, machine startup/shutdown, hotplug attach/detach, the terminal's `Op::Bell`, and the notification area's own cues | planned |

The ledger is worked in dependency order: an item is complete before anything
that depends on it begins, and each carries its own tests and documentation.
Every item through SND19 depends on the one before it. SND20–SND22 depend on
SND9's sandboxed decode seam and on SND12's FLAC codec, which is the shipped
sound set's format, and on nothing later.

**What SND4 guarantees.** The vertical passes on all three Tier-1 QEMU
targets: boot, discovery, signed-bundle autoload of the driver into its own
user process, the published channel, `audiod` adopting the device as the
default sink, and `audiotone` playing through a scripted root shell. A frame
crosses two real process boundaries and two shared PCM rings before it
reaches the card. PASS needs both witnesses — the guest's `AUDIO PASS` and
the host-side check that QEMU's `wav` capture holds every one of the 12 000
signal frames byte for byte — because a mixer that substituted, resampled or
dropped frames would still print the witness. The three ports' captures are
byte-identical over the signal region.

The capture legitimately loses the backend's last buffered tick, so the guest
plays a silence pad and the comparison trims it. The comparison itself is
exact and must stay so: relaxing it to a frame-count tolerance would excuse
genuinely dropped frames, which is the one thing this vertical exists to
catch.

**What SND5a guarantees.** Every FDT port publishes a DMA controller as a
`Dma` node carrying its `DmaController` duty — its endpoint from the reserved
`DMA_CONTROLLER_ENDPOINTS` block, its channel mask in node-relative numbering,
and whether the tree stated one — and one translated `Dma` window per entry of
its bus's `dma-ranges`, flagged `DMA_TRANSLATED` so a window starting at bus
`0` is never read as a plain limit (`plans/OPEN-DEFECTS.md` D178). Each consumer `dmas` entry becomes a `DmaRequest`
naming its controller's endpoint, including a consumer met before its
controller. Both records decode only from their canonical encoding and
`dmaengine-v1`'s frames only at their exact length; `fuzz_dmaengine` holds
that every accepted record and frame re-encodes to its own bytes.

**What SND5b guarantees.** An endpoint in `DMA_CONTROLLER_ENDPOINTS` binds
only for the holder of the `DmaController` duty naming it. `shm_create_dma`
carves one contiguous block below a `Dma` grant's ceiling — the highest free
one, found by the frame allocator's search below the ceiling rather than by
the order of its lists (`plans/OPEN-DEFECTS.md` D173) — maps it
`DMA_COHERENT` in every process that maps it, and reports its bus address
through the grant's window. The region binds its creator's node quarantine:
the creator's own unmap is its word that the device is done, and a creator
that ends still mapping it orphans it, so its frames join the quarantine when
the last mapping goes. `shm_grant_peer` mints a region to the caller an
endpoint's server is serving, and `call_peer_holds` answers the controller
whether that caller holds one of its request lines or a register window; both
answer only about a caller being served, by process instance, and every
delegated mint refuses a recipient that has ended.

**Why the two capabilities sit in SND4 rather than beside the ABI.** A
capability is added with the subsystem that enforces it, never ahead of it: it
needs a live holder and a live enforcement point in the same change, and
`CAP_AUDIO_DEVICE`'s holder (`audiod`) and enforcement point (the kernel, at
the driver's restricted-sender endpoint) both arrive with SND4, as do
`CAP_AUDIO_CAPTURE`'s. SND2's wire surface therefore names the authority in
prose and the constants land with the code that checks them.

**Why the two seams come before the decoders.** SND5 and SND6 are the plan's
priority and sit immediately after the working base, ahead of everything that
makes a file play. Both are **cross-cutting**: the DMA-engine seam is what SPI,
SD and UART will want next and the only reason any Pi audio path is currently
unreachable, and isochronous transfers are the gate on every USB device class
that streams — audio first, cameras later. Both are also independent of the
audio engine, so neither is waiting on SND3. Landing them next, with their
first consumers immediately behind them (SND7, SND8), means the stack proves
itself on the two most widely-owned pieces of audio hardware — a USB headset or
DAC, and a Pi's own outputs — before it grows a sixth file format. A file
format is worth nothing on a machine with no sink.

## What it is

**One path, one clock, one mixer, one authority.** Every sample any program
plays reaches the hardware by exactly the same route, and there is no second
route to reach for. That single sentence is the whole design, and everything
below is what it costs to mean it.

It is worth being precise about what is being avoided, because "avoid Linux's
mess" is not a design.

- **Linux** has OSS, then ALSA (a kernel PCM layer *and* a userspace library
  *and* a plugin chain — `dmix`, `dsnoop`, `plug`, `softvol`), then
  PulseAudio, then PipeWire, with JACK beside them. Four client APIs, three
  independent mixing-and-resampling implementations, two routing policies, and
  a per-application configuration file deciding which one a program gets. The
  kernel API leaks the DMA ring's geometry into every application as
  "period size" and "buffer size", so every program re-derives latency from
  numbers it should never have seen. Codec support is a per-board quirk table
  (`patch_realtek.c` is twelve thousand lines of "this laptop wired pin 0x1b
  to the speaker"). Device access is a group-membership check, not a
  capability.
- **Windows** is cleaner — WASAPI, one engine, endpoint objects — but exclusive
  mode is literally a second path that bypasses the mixer, and it exists
  *because* the shared path resamples everything to a "mix format" the user
  sets in a control panel. Effects processors load into the audio service.
- **macOS** is cleanest — one HAL, one clock per device, `coreaudiod` — and is
  the model worth beating. It is beaten on three points: audio is not bound to
  the seat (a fast-user-switch does not arbitrate it), capture is gated by
  consent prompts rather than by an unforgeable capability, and decoding
  untrusted media happens with far more authority than a decoder needs.

TAIRiX's answers, stated as binding invariants:

1. **One transport, no bypass.** One client surface (`lib/audio`'s stream
   client over `audio-v1`), one mixer (`audiod` running `lib/audio`'s engine),
   one device contract (`audiochan-v1`). No exclusive mode, no raw device node,
   no "pro" path. The single path is low-latency enough that nothing wants to
   bypass it: the mixer adds **exactly one device period** of latency and
   nothing else.
2. **Bit-exactness is a property, not a mode.** The reason Windows needs
   exclusive mode is that its shared path resamples and re-quantises
   unconditionally. Remove the cause and the mode is unnecessary: TAIRiX's
   device runs at a rate *chosen from the streams present*, and a single
   stream at unity gain whose rate and format the device accepts reaches the
   hardware unaltered. That is a testable claim and it is this plan's headline
   test, not a marketing line (§Verification).
3. **The device is a clock, and the clock is exported.** Every period, the
   driver reports the pair (device frame position, `Time64` it was sampled at).
   The mixer maintains the linear map per device and hands it to clients. A
   client therefore writes *at a frame position*, so gapless playback and A/V
   sync are exact arithmetic rather than a guess. There is no period/buffer
   API: a client states a latency target and is told the latency it was
   granted.
4. **The seat owns the sound exactly as it owns the screen.** A sink is leased
   to a seat. A session that does not hold the lease has its streams **paused
   at a frame boundary and told so** — never silently mixed into the active
   user's speakers, never silently discarded. Fast user switching pauses and
   resumes at exact positions.
5. **Capture is an unforgeable capability with a consequence no program can
   suppress.** `CAP_AUDIO_CAPTURE` gates opening a source; beyond it, every
   live capture stream is machine state published through the System
   Information API and raised as a system notice, so the session draws a
   recording indicator the recording application cannot touch.
6. **Untrusted bytes never decode in a process holding a stream.** Every
   compressed format decodes in a minimum-capability sandbox worker holding one
   IPC endpoint and nothing else. MP3, Vorbis and Opus decoders have a long
   CVE history and every other system runs them with far more reach than they
   need.
7. **Nothing spins and nothing ticks.** The driver parks on the device
   interrupt, the mixer parks on {device notify, client doorbells, control
   endpoint}, a client parks on its ring's space-available notify. The device's
   own period interrupt is the only timer in the stack.
8. **Glitches are accounted to the frame, never hidden.** A sink that could not
   produce its mix in time emits silence for exactly the frames it missed and
   records their positions. The position never lies, so a client resynchronises
   exactly instead of drifting.
9. **The device's capabilities come from the device.** An HDA codec is read
   from its own widget graph and pin configuration defaults. There is no quirk
   table, and there could not be one: a board name in shared code is forbidden.

## Where each piece lives, and why there

| Piece | Home |
|---|---|
| File decoders (AU, WAV, FLAC, Vorbis, Opus, MPEG audio), their containers, and the one encoder (FLAC, feature-gated) | `lib/sound` |
| Mixing, format conversion, resampling, channel mapping, the clock model, routing policy, the client half | `lib/audio` |
| The device-channel serve loop every audio driver runs | `lib/audiochan` |
| Client stream ABI (`audio-v1`) | `lib/abi/src/audio.rs` |
| Device-class facts and PCM vocabulary | `lib/abi/src/driver/audio.rs` |
| In-region PCM ring transport | `lib/abi/src/driver/audio_ring.rs` |
| Device-channel control plane (`audiochan-v1`) | `lib/abi/src/driver/audio_channel.rs` |
| DMA-controller channel class trait | `lib/abi/src/driver/dmaengine.rs` |
| Sandboxed decode | `lib/sandbox::audiodecode` |
| The mixer/router service | `userland/system/audiod` |
| Drivers | `drivers/audio/<leaf>/`, `drivers/dma/<leaf>/` |
| The command player | `userland/apps/play` |
| The desktop player | `userland/apps/music` |
| The desktop sound vocabulary, shipped themes, settings and cue client | `lib/soundtheme` |
| The cue authority | `userland/system/soundd` |
| The shipped sound masters, planted | `/System/Audio/Sounds/<Theme>/` |

The split between `lib/sound` and `lib/audio` is the tree's own precedent
applied: `lib/image` decodes picture files and `lib/raster` draws pixels, so
`lib/sound` decodes sound files and `lib/audio` moves samples. Neither knows
the other. A decoder answers PCM and has no idea a device exists; the engine
mixes PCM and has no idea a file format exists. The one place they meet is a
player, which reads a file through the sandbox and writes PCM to a stream.

`lib/audiochan` is separate from `lib/audio` for the reason `lib/netchan` is
separate from `lib/net`: a driver process must not link the mixer. The driver
serves; the mixer is the one client.

## The layers

### `lib/sound` — the decoder registry, and the one encoder

Shaped exactly like `lib/image`, because it is the same job on a different
medium: `SoundFormat` / `sniff` / `probe` / `open` dispatch, one private module
per format, `DecodeLimits` weighed **before** a buffer is allocated,
format-namespaced `DecodeError` variants, `no_std`, `forbid(unsafe_code)`,
fallible allocation through `tairix_util::fallible`, checked arithmetic on
every untrusted value, every test input synthesised in test code, and a
structure-aware generator per format in one registered fuzz target.

The synthesised-input rule has exactly one exception, and it is FLAC's, for a
reason that does not generalise: a FLAC stream carries the digest of its own
decoded samples, so a foreign-encoded fixture states its own expected output
and needs no reference PCM beside it and no hash of ours to maintain. That is
what the rule was protecting against, so a handful of such streams are
committed as the conformance oracle no other approach gives that format
(§Verification). Every other format synthesises, and a fixture that cannot
verify itself is not an exception waiting to be granted.

A decoder answers a **`PcmSource`**: declared rate, channel count and channel
map, sample format, total frame count where the container states one, and
`next_block` yielding interleaved frames. Seeking is a separate, *optional*
capability a format either has (FLAC's seek table, an Ogg page's granule
position, an MP3's Xing TOC or a constant-bitrate frame index) or honestly does
not; a format with no seek structure reports seeking as unavailable rather than
scanning the file and calling the result a seek.

The decode is **pull, bounded, and streaming**: nothing decodes a whole file
into memory, because a player must start a four-hour recording on a machine
with a gigabyte of RAM. The block size is the caller's, and a decoder that
cannot answer a block without allocating more than its limits allow refuses
before allocating.

`plans/VIEW.md`'s doctrine binds here unchanged: **every format claimed is
claimed completely**, and a variant that would be half-read is refused by name
rather than guessed at. What "complete" means per format:

- **AU** (Sun/NeXT `.au`/`.snd`) — both the sampled encodings and the ADPCM
  ones: 8-bit G.711 μ-law and A-law; linear 8/16/24/32; IEEE float 32/64;
  fixed point 8/16/24/32; and ITU G.721 4-bit, G.722, and G.723 3-bit and
  5-bit ADPCM. Header-declared and unknown-length (streamed) forms, and the
  annotation field. Refused by name, each for its own reason: the encodings
  that are not sampled audio at all — fragmented sample data, DSP programs,
  and music-kit DSP commands — because turning them into samples would be
  fabrication rather than decoding; and the three DEC emphasis/compressed
  16-bit variants, whose emphasis curve and compression the format never
  specifies, so a decoder could only guess at them.
- **WAV** — RIFF and RF64/BW64 (so a file over four gibibytes is read, not
  truncated); PCM 8-bit unsigned and 16/24/32-bit signed; IEEE float 32/64;
  A-law and μ-law; MS-ADPCM and IMA/DVI ADPCM; `WAVE_FORMAT_EXTENSIBLE` with
  its channel mask and format GUID; the `fact`, `cue`, `smpl` and `LIST INFO`
  chunks; odd-length chunk padding; and a `data` chunk whose declared size
  disagrees with the file (the file wins, and the disagreement is reported).
  Refused by name: MPEG-in-WAV and GSM 6.10 — a container smuggling another
  codec is that codec's decoder's job and routing to it silently would make the
  WAV module a dispatcher.
- **FLAC** — native and Ogg-encapsulated; constant, verbatim, fixed and LPC
  subframes at every order; Rice partitioning at both parameter widths, the
  escape partition, and wasted bits; all four stereo decorrelations; every
  block size and bit depth the format allows including the 8/12/16/20/24 and
  32-bit cases; the `STREAMINFO`, `SEEKTABLE`, `VORBIS_COMMENT`, `CUESHEET`,
  `PICTURE` and `APPLICATION` metadata blocks; and both frame-header sync
  variants with their CRC-8 and CRC-16 checks.

  FLAC is the one format that can **prove its own decode**: `STREAMINFO`
  carries the MD5 of the unencoded samples, so a full decode is verified
  against the stream's own claim — which is independent of us exactly when the
  stream was encoded elsewhere, the case that makes it a conformance oracle
  rather than a consistency check (§Verification is careful about which). That
  needs an MD5 implementation, which `lib/crypto` deliberately does not carry
  because MD5 is broken as a cryptographic hash. This is not a cryptographic
  use — it is an integrity check on our own arithmetic — so it lands as a
  plainly-labelled interop digest inside `lib/sound`, not as a `lib/crypto`
  primitive, and nothing security-relevant may reach for it. A mismatch is a
  decoder defect, reported as a decode failure rather than passed off as audio.

  FLAC is also the one format that is **encoded** here, behind an off-by-
  default `encode` feature, so no shipped binary carries it (§The FLAC
  encoder).
- **MPEG audio** — MPEG-1, MPEG-2 and MPEG-2.5 Layers I, II and III. Layer III
  is what "MP3" means and is the reason the format is claimed; Layers I and II
  are the same framework's earlier members and fall out of the same header
  parse, filterbank and allocation tables, so claiming Layer III and refusing
  its two siblings would be half-reading the format. Complete means: every
  sample rate and bitrate including free format; mono, dual-channel, stereo and
  joint stereo with both MS and intensity coupling; the bit reservoir; the
  Huffman table set including the count1 tables; the IMDCT with block-type
  switching, aliasing reduction and the overlap-add; the polyphase synthesis
  filterbank; CRC-protected frames; the Xing/`Info` and VBRI headers; **LAME
  gapless delay and padding** (a gapless album is the whole point of reading
  the tag); and ID3v1, ID3v2 and APE tags skipped rather than misread as
  frames. A stream that begins mid-frame is resynchronised by a
  multiple-frame-header agreement, never by the first byte pair that looks like
  a sync word.
- **Ogg** — the container in full: page structure and CRC, packet assembly
  across page boundaries and across continued packets, granule positions,
  multiplexed logical streams, and **chained** physical streams (a stream
  concatenated onto another, which is how internet radio dumps arrive and which
  most decoders quietly truncate). Over it, two codecs:
  - **Vorbis I** — the three headers and their codebooks (all three VQ lookup
    types), floor 0 (LSP) and floor 1, residues 0, 1 and 2, channel coupling,
    the mode/mapping tables, window and MDCT at both block sizes, and the
    `OggVorbis` granule convention including the first and last packet's
    trimming.
  - **Opus** (RFC 6716) — SILK (narrow/medium/wideband, the range decoder, LSF
    and LTP, stereo prediction and unmixing, packet loss concealment's decoder
    side), CELT (MDCT, PVQ, band energy, spreading, folding, anti-collapse,
    the post-filter, transient handling), the hybrid mode and the mode
    switching between them; the TOC byte and all four packet codes including
    code 3's padding and CBR/VBR frame counts; and the Ogg encapsulation's
    `OpusHead`/`OpusTags`, pre-skip, output gain and granule mapping.

  Opus is by a wide margin the largest single item in this plan and is staged
  last for that reason. Its saving grace is that RFC 6716 ships both a
  reference decoder and a conformance vector suite, so "complete" has an
  external oracle rather than our own opinion.

A format is one module unless it is genuinely more than one codec, which is
`lib/image`'s rule: Ogg is `ogg` for the container plus `vorbis` and `opus` for
the two bitstreams, and neither codec knows the container or the other.

#### The FLAC encoder

One format is encoded as well as decoded. It is the only one, it is off by
default, and it earns its place three times over.

**The test story needs it, and this is the argument that decides it.** The
no-committed-fixtures rule means every test input is assembled in code, so a
decoder's tests must *emit* the format. `lib/image` shows where that leads:
613 lines of `png_fixture.rs`, `vp8_fixture.rs` and `vp8l_fixture.rs` — partial
encoders in all but name — and `png_fixture`'s own docs record that it is
shared rather than private precisely because a second consumer needed it. FLAC's
bitstream is not PNG's chunk-and-CRC: emitting a valid frame means LPC
subframes, Rice partitioning, wasted bits, stereo decorrelation, two CRCs and
the stream digest. A `flac_fixture.rs` that could exercise the decoder's whole
surface *is* an encoder, written somewhere it cannot be reused, documented or
fuzzed. Writing it once, properly, is less code than the three ad-hoc emitters
that would otherwise appear — the plan's own no-duplication rule reaching the
same answer §27 does.

Its three consumers:

1. **Round-trip property tests.** Encode synthesised material, decode it,
   assert identity — across every block size, bit depth, channel count,
   subframe type and partition shape. This is a *breadth* oracle and is
   honestly labelled as one: it proves the two halves agree, not that either
   matches the specification. Spec conformance stays where it already is —
   each stream's own `STREAMINFO` digest, and real files where a bug is
   suspected. A round-trip that passed while both halves shared a
   misreading is the failure mode, and is why it is the second oracle and
   not the first.
2. **The structure-aware fuzz generator** the charter requires per format.
   A generator that emits well-formed streams and then perturbs them reaches
   the decoder's interesting paths; one that emits random bytes tests the
   sync scan and nothing beyond it.
3. **The shipped sound assets** (§Default desktop sounds), converted from
   their authored masters by `cargo xtask sound-encode`, as `c-header --write`
   regenerates `include/`. No new tool crate: this is asset orchestration,
   which is xtask's job.

**Shape.** A controllable core under a chooser, because the test consumer and
the asset consumer want opposite things. The tool wants "encode this well" and
takes the chooser: predictor order search, Rice partition order search, stereo
mode selection. The tests want "emit *this* construct" — a verbatim subframe, an
escape partition, wasted bits — which a chooser would optimise away and never
produce. So the core takes explicit per-frame decisions and the chooser is a
thin layer that makes them; only the chooser is optional.

Encoding and decoding share one format model — the constants, the CRC-8 and
CRC-16, the digest, the bitstream reader and writer, the fixed predictors, the
Rice coding — rather than growing a second, divergent one.

**What it is not.** It is not a shipped library: the `encode` feature is off
for every consumer that runs on the machine, so a player, the sandbox worker
and the cue authority carry decode only, and §16.4's curated set is unchanged.
It is not a general audio-encoding surface either — no other format gains one,
and none should until something needs to write that format.

### `lib/audio` — the engine

Host-tested, `no_std`, no I/O, no window, no syscall. Everything that decides
*what samples come out* lives here so it can be tested without a machine:

- **The PCM vocabulary** — `SampleFormat` (u8, s16, s24 packed and in 32,
  s32, f32), `ChannelPosition` and `ChannelMap`, `Rate`, and `Frames`, a
  newtype over `u64` used for every position.
- **Conversion.** One saturating converter per (source, destination) pair,
  dispatched through `lib/cpuops` so a machine with SIMD uses it and one
  without is still correct. Dither is applied only where the destination is
  *narrower* than the source, is triangular-PDF by default, and is
  switchable off — because dithering a bit-exact path would destroy the
  property invariant 2 promises.
- **The mixer.** Accumulates in `f32`, applies per-stream gain once, and makes
  exactly one saturating conversion to the device format. The bit-exactness
  claim is stated precisely rather than vaguely: **a source of 24 bits or
  fewer, at unity gain, at a rate and channel map the device accepts, with no
  other stream live, reaches the device bit-exact** — because a 24-bit sample
  is exactly representable in `f32`'s mantissa and the accumulate is then the
  identity. A 32-bit *integer* source carries 24 bits of mantissa through the
  mix and the crate documents that rather than pretending otherwise; a 32-bit
  *float* source into a float device is exact trivially. An `f64` accumulator
  would make the 32-bit integer case exact too and was rejected: no consumer
  format produces meaningful 32-bit integer audio, and the honest narrower
  claim is worth more than a wider one nobody can hear.
- **One resampler.** A polyphase Kaiser-windowed-sinc with its figures
  *measured* from the built bank's own coefficients rather than asserted in
  prose: passband edge at 0.43 of the lower rate, stopband from its Nyquist,
  ≥ 100 dB rejection and < 0.1 dB ripple. Drivers never resample and clients
  never need to: this is the only resampler in the system and a second one is
  a review blocker.

  The position is an integer input frame plus an integer remainder over the
  reduced denominator — 48000/44100 is 160/147 and the step is exactly that —
  so no floating-point accumulator advances per sample and an hour ends on the
  frame the arithmetic says. The bank holds one row per denominator step where
  the denominator fits, which is every pair in the standard rate family; the
  interpolation weight between rows is then exactly zero. That is the same
  fractional-delay mechanism the linked-domain drifting ratio uses, and it also
  covers the case the plan first overlooked: a **static** pair with no common
  factor needs more rows than a bounded bank can hold, so it interpolates too.
  One code path; the exact case simply never engages the interpolation.

  Two figures fall out of the filter design rather than being chosen: a
  decimating ratio must reach the stopband by the *output's* Nyquist, so it
  needs taps in proportion to the decimation factor (capped, past which the
  transition widens rather than the per-sample work unbounding); and the
  coefficient table depends on the ratio alone, so one bank per (source rate,
  sink rate) serves every stream on a device.

  `lib/util::mathf` gained `exp` for the window and the decibel curve, since
  that module is the one home for `no_std` transcendental maths and a second
  copy in `lib/audio` would be the duplication the charter forbids.
- **Channel mapping.** An explicit matrix derived from source map to sink map,
  with the standard downmix coefficients (ITU-R BS.775) for 5.1 and 7.1 to
  stereo and a documented upmix. A pair of maps with no defined relationship
  fails closed rather than copying channel zero into everything.
- **The clock model.** Per device, a linear fit of (frame position, `Time64`)
  pairs with a rate estimate, so the *actual* rate of a device whose crystal
  says 48000 and whose reality says 47998.6 is known and reported. This is what
  makes cross-device drift a number rather than a mystery.
- **The routing policy.** Which sink a stream lands on, derived from the
  stream's declared role, the seat's lease, and the machine's configured
  default — a pure function of state, so it is host-tested exhaustively.
- **The volume model.** Per-stream, per-application, per-sink and hardware
  gain, resolved into *one* multiply applied in the mix, with hardware gain
  used where the device has it and reported as such so a user interface shows
  one number. The scale is dB with a documented taper; unity is exactly 1.0 and
  exactly bit-exact, which is what makes invariant 2 testable.
- **The stream client** — the half a program links: open, write at a position,
  query the clock, set gain, drain, close, and park on space-available.

### `audio-v1` — the client stream ABI

`lib/abi/src/audio.rs`, held to the syscall table's discipline: versioned,
hashed, and frozen from the first release. It is deliberately small, and
deliberately does **not** expose a period or buffer size.

- `Enumerate` — the sinks and sources the caller may see, each with its
  identity, its `audio:` resource reference, its channel map, its supported
  rates and formats, its jack/presence state, and whether it has hardware gain.
- `OpenStream { direction, format, rate, channels, map, role, latency_target }`
  — answers the granted latency in frames *and* in `Time64`, the shared ring's
  geometry, and the stream's clock domain. A request the device cannot meet is
  answered with what it *can* meet, so a client adapts rather than failing; a
  request the caller may not make (a source without `CAP_AUDIO_CAPTURE`) is
  refused, not downgraded.
- `Start`, `Stop`, `Drain`, `Flush` — each at an exact frame position.
- `Clock` — the (frames, `Time64`) pair and the estimated rate.
- `Gain`, `Mute` — per stream.
- `State` — running, paused, `SeatInactive`, `DeviceLost`, with the frame
  position at which the state changed and the underrun/overrun tallies.

`role` is what lets policy be a policy rather than a per-application
configuration file: `Media`, `Communication`, `Notification`, `Accessibility`.
Routing, ducking, and what survives a seat switch are decided from the role by
the one policy function, not by a list of process names.

The ring itself is `lib/abi/src/driver/audio_ring.rs`, shared by the
client→mixer and mixer→driver hops because it is the same structure: a header
carrying producer and consumer positions as **monotone `u64` frame counters**,
plus the sample area. Positions never wrap — at 192 kHz a `u64` frame counter
lasts about three million years — which deletes the entire class of
wrap-around bugs that ring buffers indexed by byte offset spend their lives
fixing. Publication is a release-store of the producer position after the
samples and an acquire-load on the consumer side. **This is a lock-free
protocol, so it carries a `loom` model**; that is not optional.

### `audiochan-v1` — the device channel

`lib/abi/src/driver/audio_channel.rs`, the `net_channel.rs` shape applied to
audio, with `lib/audiochan` the serve loop every audio driver process runs.

**Built.** `lib/abi::driver::audio::Audio` is the class trait every audio
engine implements and the serve loop is written once over;
`AudioChannelServer` is its pure per-endpoint handler and `serve` the process
loop. State is per endpoint rather than per channel, because a device presents
several sinks and sources and each is driven independently.
`ConfigureGrant::validate` is the one definition both sides apply to a grant,
so a grant the mixer would refuse to decode is never recorded by the driver —
including one whose period rounds up past its own ring ceiling, which admits
no power-of-two ring at all. The interrupt path *services* rather than merely
notifying: the region is already mapped in the driver, so the period is moved
there and one notify carries the clock pair, instead of two extra process
switches per period on the path whose whole job is not to have jitter.

The surface:

- A reserved endpoint block (`AUDIO_CHANNEL_ENDPOINT_BASE`, `"ACHAN\0\0\0"`),
  claimed by first-free binding so two audio drivers never collide without a
  central allocator. Binding requires `CAP_IPC_BIND_PRIVILEGED` so a squatter
  cannot impersonate a driver, and the endpoint is bound **restricted-sender on
  `CAP_AUDIO_DEVICE`**, so the kernel refuses at dispatch every caller but the
  mixer and the driver never re-checks.
- A `tairix,audiochan` node the driver publishes, which `devmgr` recognises and
  hands to `audiod` — the discovery half, defined beside the endpoint block so
  the key emitted and the key looked for cannot drift.
- `Facts` (the sinks and sources the device presents, their supported formats
  and rates, period bounds, hardware gain), `Attach` (the mixer creates the
  ring region, grants it, and names its notify port), `Configure` (rate,
  format, channels, period — a device reconfiguration, performed at a period
  boundary), `Start`/`Stop`/`Drain`, `Service` (the doorbell), `Gain`,
  `Detach`.
- Notifications: `PeriodElapsed { frames, sampled_at }` — the clock pair that
  invariant 3 is built on — plus `Xrun` and `JackChanged`.

**The driver copies between the shared ring and its own DMA buffer, once per
period, and that is a decision rather than an oversight.** A zero-copy
arrangement would mean the mixer writing directly into memory the device
DMA-reads, which means publishing a driver's DMA window to another process.
The driver owning its DMA window absolutely, with nothing else mapping it, is
worth more than the copy costs: at 48 kHz stereo 32-bit a five-millisecond
period is 1920 bytes, so the copy runs at 375 KiB/s. The security boundary is
bought for a rounding error.

### `audiod` — the service

**Built.** The two capabilities landed with it, because a capability needs its
live holder and its live enforcement point in the same change: the enforcement
point is the kernel, at a driver's restricted-sender endpoint, and `audiod` is
the holder. It is a host-testable engine plus a `Run` binary behind a
`program` feature, written over four injected seams — the shared-region host,
the device-channel transport, the client notifier, and the monotonic clock —
so the whole authority is exercised without a machine.

Two seams the first consumer revealed were settled in `lib/audio` rather than
worked around. `Resampler` no longer borrows its `FilterBank`: the filter
history is per stream and the coefficients are per rate pair, so the bank is
passed to `process` and a bank of the wrong ratio is refused rather than
filtered over. And `Mixer::mix` folds an *iterator* rather than a slice, so a
service whose live streams live in its own records needs no per-period
collection — the one place an otherwise allocation-free period path would
have had to allocate.

Two things `audiod` does not do yet, both waiting on work staged elsewhere.
Sinks are leased to seats by SND13; until then no sink is claimed, which is
the router's own headless case, so the router sees `leased_to: None` and any
principal may play on an unclaimed sink. And a stream's gain is always the
software multiply: the device's own control belongs to the *sink*, whose
volume surface arrives with SND15, and splitting one stream's gain into
hardware would silence every other stream on the endpoint.

`userland/system/audiod`, a `kind = "service"` bundle discovered from disk like
any other (§16.5), declaring its readiness condition so dependants gate on it.
It is the **sole holder of `CAP_AUDIO_DEVICE`** and the only process that
speaks `audiochan-v1`.

One system service, not one per user. A per-user daemon is precisely the design
that cannot arbitrate between two logged-in users over one piece of hardware —
it is why PulseAudio's multi-seat story is what it is. The device is machine
state, so the arbiter is a machine service with per-seat routing and
per-principal accounting.

Its real-time discipline is structural, not hopeful:

- The mixing thread holds `CAP_SCHED_REALTIME`; its ring and working buffers
  are pinned (`CAP_MEM_PIN`), so an audio buffer is never paged and never
  reaches swap.
- **Every buffer is allocated at stream-open and reused.** The per-period path
  allocates nothing, locks nothing for longer than a position update, and its
  work is bounded by (live streams × period frames). That is what makes
  glitchless output a property of the design rather than of the machine's mood.
- It parks. The device period interrupt wakes it; client doorbells wake it;
  the control endpoint wakes it. There is no audio tick anywhere in the system.

Every security decision it takes lands on the hash-chained audit log with a
stable event id: a capture stream opened or refused and by which principal, a
monitor stream authorised against a seat lease, a default-device or per-device
policy change, and a device bound or lost. A capture refusal is as
audit-worthy as a capture grant — "who tried" is the question an incident
asks — and the indicator invariant 5 draws is rendered from the same state, so
what the user sees and what the log records cannot disagree.

### What it costs, and what bounds it

- **Nothing is a hand-picked ceiling.** A ring's depth is derived from the
  device's own reported period bounds and the client's latency grant: a device
  whose minimum period is one millisecond gets one, and one that can only do
  ten gets ten. There is no `const` period size anywhere.
- **What *is* fixed stays fixed**, because it is a validation bound and not a
  capacity: the decoder's input-byte and output-frame ceilings in the sandbox,
  the maximum frame size on both wire protocols, and the maximum channel count.
  Widening one of those to be accommodating is a security regression, not
  flexibility.
- **Per-principal stream count and pinned audio memory are bounded through the
  resource-limit facility and fail closed.** An unprivileged process cannot
  open a thousand streams and pin the machine's memory, and one user's twenty
  streams cannot starve another's one: the mixer's per-period work is bounded
  per stream and admission is per principal.
- **Streams are pinned and are therefore not reclaimable, and that is
  deliberate.** Releasing a live audio buffer under memory pressure buys a few
  kibibytes and costs an audible glitch, so what bounds audio memory is the
  rlimit, not the pressure band. The *reclaimable* audio state is what a player
  caches — album art, decoded metadata, directory listings — and that lives
  under `lib/reclaim`'s budget like any other disposable UI state.
- **Resident memory does not scale with the machine's hardware.** It is (live
  streams × period × depth) — tens of kibibytes per stream — so a small board
  with four sound devices and nothing playing holds four sets of device facts
  and no buffers at all. Adding a device costs its facts; adding a stream costs
  a stream.
- **Mixing is single-threaded per device, on purpose.** Splitting a per-period
  mix across cores would add scheduling jitter to a path whose whole job is not
  to have any, for work that is already a rounding error against the memory
  bandwidth it touches.
- **A device that fails is contained.** A driver that dies or a device that
  disappears mid-stream moves its streams to `DeviceLost` with their positions
  intact and states the reason; the service stays up and other devices are
  untouched. `audiod` itself is restartable under the service manager's policy,
  and a restart loses the streams rather than the system.

## The clock, the latency, and the seat

**Latency.** A client names a target; the service answers the grant. The
granted latency is the device period plus the ring depth the service chose, and
it is reported in frames *and* in `Time64` so a client never has to know a
sample rate to reason about time. The mixer's own contribution is one period,
always, which is the number a reviewer will ask for.

**Clock domains.** Each device is its own domain. A stream belongs to one.
Moving a stream between domains is a re-open with the position carried across,
not a hidden asynchronous resampler quietly lying about latency. Where two
sinks genuinely must play the same material in sync — the analogue jack and
HDMI together — they are joined into a **named linked group**, and then the
adaptive resampling and its drift correction are *reported values*, not a
secret. Nobody else reports them; this is the difference between an audio stack
you can debug and one you cannot.

**The seat.** A sink is leased to a seat exactly as a display is
(`lib/seat`'s `Lease`). The rules:

- A session holding the lease has its streams mixed.
- A session that does not gets `StreamState::SeatInactive`: its streams pause
  at a frame boundary, hold their positions, and are told. On switch-back they
  resume from the exact frame. A departing user's music does not play into the
  arriving user's room, and it does not silently vanish either.
- `Notification`-role streams from a non-active session are **dropped, not
  queued** — a notification that arrives ten minutes late is noise, and the
  role is what says so.
- A sink no seat has claimed is available to any principal with a stream, which
  is the headless case (a server playing an alert has no session). On a machine
  with a graphical session the session claims its seat's sinks at login, so a
  remote login cannot make noise in the room.

**Monitoring** — capturing a sink's own mix — is authorised by *holding that
seat's lease*, not by `CAP_AUDIO_CAPTURE`. A session may monitor its own
output; nothing may monitor another principal's. No third capability is needed
because the lease already expresses exactly the right boundary.

## The capability set

Two new capabilities, and the discipline of §5.2 applied honestly to each:

- **`CAP_AUDIO_DEVICE`** — drive an audio device's rings and registers through
  `audiochan-v1`. It guards a group of resources (every audio device), has a
  live holder (`audiod`) and a live enforcement point (the kernel, at the
  driver's restricted-sender endpoint) in the same change, and no existing
  capability expresses it. It is the `CAP_NET_RAW` of audio.
- **`CAP_AUDIO_CAPTURE`** — open a capture stream on any source. It guards a
  group of resources (every microphone and line input), has a live holder (a
  recording application's manifest) and a live enforcement point (`audiod` at
  stream open), and is a privacy boundary no existing capability covers.

And three things that deliberately are **not** capabilities:

- **Playback needs none.** An ordinary program plays sound the way it draws a
  window: the authorisation is that its session holds the seat lease on the
  sink, checked at open against the kernel-attested caller. That is a more
  precise check than a capability, and a capability every program would hold is
  not a boundary.
- **Machine-wide device policy** — the default sink, per-device gain, enabling
  a device — is a write to `/System/Settings` under the settings authority that
  already exists. `audiod` reads it. Inventing `CAP_AUDIO_ADMIN` would be a
  third name for an authority already spelled.
- **Monitoring** is the seat lease, as above.
- **Cueing a desktop sound** is the seat lease for an application event and
  the existing owner of the transition for a lifecycle one. A capability every
  program would hold is not a boundary, and the authority that actually
  matters — whether a program may imitate the machine — is expressed by the
  event's class, not by a token.

## Drivers

A new device class: `drivers/audio/`, `HwDeviceClass::Audio`, and
`lib/abi/src/driver/audio.rs`'s class trait. The path namespace names the class
and the leaf names the part, so `drivers/audio/hda/` and
`/System/Drivers/audio/realtek_alc1220/` are right and
`/System/Drivers/realtek_audio/` would be a defect.

Every driver runs in **user space**, bound by discovery-match, holding only the
register window, DMA constraint and interrupt line its matched node requested.
None of them is in the bootstrap floor: nothing about reaching the driver store
needs sound.

Three of the four Tier-1 targets reach hardware the same way and are covered by
the drivers below. **`wasm32` has no registers to reach**, so its sink is the
host environment's own audio output, discovered by the host capability query
the port already normalises into the hardware tree and presented over the
identical `audiochan-v1` contract — the same shape its display and input take.
Nothing above the device channel learns which of the two it is talking to. It
lands with the target's own bring-up rather than beside the register drivers,
because the work is the port's host shim and not a device.

### `drivers/audio/virtio_snd` — the first one

virtio sound (device id 25), over the `lib/virtio` split-virtqueue transport,
so it is the cheapest complete driver and the one that gives an end-to-end
QEMU vertical on **every Tier-1 architecture**. It lands first for exactly
that reason.

**Built**, with its signed bundle at `/System/Drivers/audio/virtio_snd/Run`
and the `audiochan` node `devmgr` hands to `audiod`. The four queues (control,
event, tx, rx), the jack/PCM/chmap
information requests, `SET_PARAMS`/`PREPARE`/`START`/`STOP`/`RELEASE`, the
transfer header/status framing with its `latency_bytes`, and the
period-elapsed, xrun and jack-change events — which map onto `audiochan-v1`'s
notifications with no translation layer, because the device channel was shaped
from the same hardware reality. Both buses from one signed bundle: a
single-aperture virtio-MMIO aperture or the four role-tagged virtio-PCI
windows, shape-keyed from the grant set.

Three things it reports honestly rather than inventing, because the device
QEMU presents publishes none of them: no jacks means `JackState::Unknown`, no
channel maps means the conventional layout for the reported channel count (and
a refusal for a count with no conventional reading), and no negotiated control
elements means no `GainRange`, so the mixer applies the gain itself.

What keeps the position exact is that silence is substituted **only** where
the device would otherwise run dry — a short ring with nothing left in flight
— and counted as lost. Padding a period the device has not yet asked for would
manufacture a glitch out of frames that were merely going to arrive in time; a
drain's tail is a short transfer, not a padded one. A capture period the
mixer's ring could not hold is over-run and is counted too.

### `drivers/audio/hda` — the answer to "AC'97 or whatever motherboards use"

**Intel High Definition Audio**, not AC'97. AC'97 has not shipped on a new
motherboard since around 2006; HDA replaced it universally, and a Realtek ALC
part on a modern board is an HDA *codec* behind an HDA controller. Writing an
AC'97 driver would be dead on arrival, so it is refused by name rather than
written: QEMU's `-device AC97` is not a target.

One driver covers real hardware *and* QEMU, because QEMU emulates the same
controller (`intel-hda` / `ich9-intel-hda` with `hda-duplex`). That is unusual
and valuable: the x86_64 motherboard path is testable in CI.

- **Controller**: the PCI class-0x0403 register window; reset and the
  `STATESTS` codec enumeration; the CORB/RIRB command rings with their
  interrupt-driven response path; the DMA position buffer; and a stream
  descriptor per stream with its buffer descriptor list, cyclic ring, and
  interrupt-on-completion at period boundaries. HDA streams carry their own DMA
  engines, so this driver needs no external DMA controller.
- **Codec**: a **pure graph walk**, and nothing else. Enumerate the function
  groups, read each widget's capabilities, build the connection graph, read the
  pin configuration defaults the codec itself publishes (which jack, which
  colour, which sequence, which device type), and find the paths from converter
  to a pin with a jack. Amplifier ranges, mute, power states, and unsolicited
  responses for jack detection all come from the widget's own capability words.

  No quirk table. A board that wires a pin contrary to its own configuration
  default gets the answer its codec gave, and TAIRiX says what it found rather
  than shipping twelve thousand lines of per-laptop special cases in shared
  code — which the platform-neutrality rule forbids anyway. This is a real
  behavioural difference from Linux and the plan states its cost plainly: a
  handful of laptops whose firmware lies will present a wrong jack name until
  their firmware's own data is fixed. The alternative is a maintenance liability
  no charter-legal home exists for.
- **HDMI/DisplayPort audio on an HDA controller** (the desktop-PC case) is a
  pin widget with an ELD buffer, so it falls out of the same walk. The sink's
  identity follows the connector.

### Raspberry Pi 4 (BCM2711)

Three real paths over one seam the tree is missing.

**The seam: a DMA engine (SND5).** `DmaHost` mints DMA-able *memory* and
nothing models a DMA *controller channel*. Every Pi audio path is fed by one,
which is why none is reachable today, and SPI, SD and UART DMA want the same
operations, so it is a shared seam written for a periodic slave transfer in
general rather than for an audio ring — §The DMA-engine seam, below.

1. **HDMI audio — `drivers/audio/rpi_hdmi`.** The VC6 HDMI controller's MAI
   block: its FIFO fed by a cyclic DMA channel with the HDMI request line, the
   sample-rate divider, the CEA-861 audio InfoFrame and channel status written
   into the controller's packet RAM, and the audio clock regeneration N/CTS
   values derived from the active pixel clock. What the sink accepts comes from
   the EDID's short audio descriptors, and the sink appears and disappears with
   the connector's hotplug — because **HDMI audio is a function of a display
   connector, not an independent device**, and TAIRiX models it that way.

   **Why SND19's blocker is what it is.** TAIRiX's Pi
   display path presently takes its framebuffer from the VideoCore firmware
   through `lib/vcmailbox`, and the firmware therefore owns the HDMI
   controller — its registers, its pixel clock and its EDID. Audio needs all
   three. There are exactly two ways through, and only one of them is
   acceptable: a native encoder on the TAIRiX side (mode set, N/CTS,
   InfoFrames, EDID), which is display work belonging to `plans/PI.md`; or the
   firmware's own audio service, which on the Pi means VCHIQ — a second
   device-interconnect stack with no other use in this tree, putting the audio
   path behind closed firmware. This plan refuses the second and states the
   first as a dependency rather than smuggling either into an audio change.

2. **The 3.5 mm jack — `drivers/audio/bcm2711_pwm`.** Fully native, no
   firmware: the BCM2711 PWM block driving the two channels the A/V jack's
   audio pins are wired to, clocked from the clock manager, fed by a cyclic DMA
   channel with the PWM request line.

   Its quality claim is honest rather than flattering. PWM audio on this part
   is about eleven effective bits with a noise floor the hardware fixes, so the
   driver applies **error-feedback noise shaping** — which is what makes PWM
   audio listenable and is what the default Linux path largely does not — and
   the crate's docs state the measured result rather than claiming CD quality.

3. **I2S — `drivers/audio/bcm2711_i2s`.** The SoC's PCM/I2S peripheral, again
   DMA-fed, which is how serious audio is done on a Pi. This is where the class
   trait's modularity earns itself: the *controller* is one driver and the DAC
   on the HAT is another, bound separately through discovery — a codec needing
   no control interface binds with nothing, and one with an I2C control port
   binds through `lib/i2c` and `drivers/bus/i2c`. Two drivers composing over
   one stream is the shape every serious audio system has and is worth proving.

#### The DMA-engine seam (SND5)

**What the tree exposes.** Taken from the firmware tree `tools/mkimage`
pins (`bcm2711-rpi-4-b.dtb`, firmware 1.20260521) and the BCM2711 peripherals
document, chapter 4:

| Node | `compatible` | Registers | Channels | `brcm,dma-channel-mask` | Interrupts |
|---|---|---|---|---|---|
| `/soc/dma-controller@7e007000` | `brcm,bcm2835-dma` | `0x7e007000` + `0xb00` | 0–10: 0–6 full, 7–10 LITE | `0x7f5`: 0, 2, 4–10 | 11 (`dma0`–`dma10`); 7/8 and 9/10 share a line |
| `/scb/dma@7e007b00` | `brcm,bcm2711-dma` | `0x7e007b00` + `0x400` | 11–14, DMA4 | `0x7000`: 12–14 | 4 (`dma11`–`dma14`) |

The mask counts in the part's absolute channel numbers, and a node's first
channel is its window's offset into the 4 KiB DMA page over the `0x100`
channel stride. A LITE channel moves at most 65 532 bytes per block and has no
2D or ignore modes; its `DEBUG.LITE` bit says which it is. The legacy engines
reach RAM through `/soc`'s `dma-ranges` (bus `0xc0000000` ↔ CPU `0x0`, 1 GiB)
and peripherals at their legacy-master addresses (bus `0x7c000000` ↔ CPU
`0xfc000000`, 56 MiB); DMA4 reaches all 16 GiB untranslated and peripherals at
`0x4_7c000000`. Channel 15 and the global `INT_STATUS`/`ENABLE` registers lie
outside both nodes and are never touched: `ENABLE`'s `PAGE` fields choose which
gibibyte the uncached alias reaches, and the firmware owns them.

The default tree's consumers:

| Consumer | Controller | Specifiers | `dma-names` |
|---|---|---|---|
| `i2s@7e203000` (PCM) | `dma` | 2, 3 | `tx`, `rx` |
| `spi@7e204000` | `dma` | 6, 7 | `tx`, `rx` |
| `mmc@7e300000`, `mmcnr@7e300000` | `dma` | 11 | `rx-tx` |
| `mmc@7e202000` (SD host) | `dma` | `0x2000000d` | `rx-tx` |
| `smi@7e600000` | `dma` | 4 | `rx-tx` |
| `hdmi@7ef00700`, `hdmi@7ef05700` | `dma40` | `0x41fa000a`, `0x41fa0011` | `audio-rx` |

Neither PWM node carries `dmas`; that is SND8's prerequisite, below.

The one `#dma-cells` cell is the downstream binding, wider than upstream's
"the DREQ number": bits 4:0 are the DREQ, and above them the firmware states
how its peripheral wants serving — AXI priority (19:16), panic priority
(23:20), wide source (24), wide destination (25), no write-response wait (27),
wait for outstanding writes (28), no debug pause (29), and burst (30). They are
platform facts carried to the controller by discovery and applied by it; a set
bit outside those refuses the request rather than being ignored.

**Security.** A control block holds bus addresses and there is no IOMMU, so
whoever writes one can read and write all of RAM. The controller driver is
therefore the only process that maps the controller's registers or writes
control-block memory, and a consumer never supplies an address: it quotes
claims the kernel attests, and the driver builds every block from attested
facts alone.

- **The request line** is the consumer's own `DmaRequest` grant, which
  discovery built from its node's `dmas` entry. The consumer quotes the
  record; the driver asks the kernel whether the in-service caller holds
  exactly that grant (`call_peer_holds`, below) and refuses otherwise. The DREQ
  and the serving flags come from the attested record, never from the frame.
- **The FIFO** is a CPU-physical register address the consumer quotes. The
  driver asks whether the caller holds an MMIO grant covering the whole
  peripheral-side access (4 bytes, or 16 for a wide one), then translates it
  through its own peripheral `dma-ranges` window to the legacy-master address.
  A FIFO outside the caller's windows or outside the controller's reach is
  refused, and the peripheral side never increments.
- **The buffer** is made by the controller, never by the consumer: a
  DMA-capable shared region carved under the controller's own addressing
  constraint, whose device address the kernel reports to its creator, and a
  mapping of which the kernel grants to the requesting process. Every block's
  memory side lies inside its channel's own region by construction. It is
  Linux's rule — a dmaengine client maps its buffer against the controller's
  device — with the kernel holding it rather than the client.
- **Lifetime.** The region is refcounted by the kernel. A consumer that dies
  drops only its mapping; the controller stops the channel before releasing its
  own reference, so a consumer's death never leaves the hardware on freed
  memory. The *controller's* death is covered by the node quarantine
  (`plans/OPEN-DEFECTS.md` D167): its regions and control blocks stay out of
  the allocator until the node's next driver instance declares its device
  quiesced, which the controller's bring-up does only once it has reset every
  channel in its mask.
- **Ownership.** A channel belongs to the process instance that opened it and
  every later call must come from that instance. An `Open` for the same
  request from a different instance that the kernel attests as its holder
  reclaims the channel — stopped, its region released — because the grant, the
  authority, has moved with the node's new driver.
- **Every refusal is audited** with a stable event id naming the request and
  the reason.

**The cross-process shape.** One endpoint per controller node, from a reserved
block indexed by the node's id, bindable only by the holder of that node's
`DmaController` duty. It is the I²C `BusChild` precedent with one duty in place
of one per child, because a DMA controller's consumers are scattered across the
tree rather than beneath it and a duty per consumer would not fit a node. The
endpoint is restricted-sender on `CAP_IPC_ENDPOINT` with the grant coupled to
the call, and a `DmaRequest` grant covers *calling* its controller's endpoint —
never serving it, so no consumer can squat the controller's rendezvous.

- `Open { request }` claims the lowest free channel the mask allows, at most
  one per request.
- `Prepare { fifo, direction, period_bytes, periods }` makes the region and the
  cyclic chain, one interrupting block per period (a period split into several
  blocks where a LITE channel's limit demands, the interrupt on its last).
- `Start`; `Stop` (abort, then channel reset); `Position` (the live
  memory-side offset, read from the channel); `Close`.
- `Wait { after }` is a posted call the driver answers at the first period
  boundary past `after`, carrying the monotone byte position and the monotonic
  time the interrupt was serviced, or the channel's error bits if it faulted.
  The reply cannot be forged or lost: it comes from the endpoint's owner, a
  boundary that passed while no `Wait` was posted is answered at once, and a
  consumer that dies has its call retired by the kernel, which the driver sees
  as a refused reply and answers by stopping the channel.

**The latency consequence.** A period crosses one more process than SND4's
virtio path: interrupt → controller → consumer → `audiod`. The extra hop costs
slack, not latency. It is asynchronous — the controller replies and returns to
its wait set — and it carries the interrupt-time stamp, so the clock pair
invariant 3 builds on is not degraded by it. Output latency is still the DMA
ring's depth plus the mixer's period; the consumer's refill deadline shrinks
by one process wake, expected to be tens of microseconds against a period of
milliseconds and measured on metal with SND8 rather than assumed. It is
structural rather than chosen: a channel's interrupt must be acknowledged in
its own `CS` register, which shares its page with fourteen other channels' and
so cannot be granted alone. The mitigation is the mixer's own discipline — a real-time serve
thread whose interrupt path allocates, locks and blocks on nothing.

No memory but the sample region is shared between the two processes, so this
hop has no lock-free protocol to model: the position travels in the reply, and
a reply is a kernel IPC.

**Discovery.** `dmas`, `dma-names` and `#dma-cells` are the generic devicetree
DMA binding, so the shared walk (`kernel/arch/api/src/fdtwalk.rs`) reads them
for every FDT port rather than `kernel/arch/aarch64` alone:

- A node with `#dma-cells` is a DMA controller. It is classed
  `HwDeviceClass::Dma`, carries a `DmaController` duty naming its endpoint, and
  carries one `Dma` resource per entry of its parent bus's `dma-ranges`, each
  translated. `/soc` has two, and the existing aperture decoder folds entries
  into one span, which would misstate them as a single untranslated window of
  nearly 4 GiB.
- Each entry of a consumer's `dmas` becomes a `DmaRequest` naming its
  controller's endpoint, the specifier (up to two cells — a wider one is
  dropped, never truncated), the entry's position, and its `dma-names` string
  where that fits the record's eight bytes. A phandle resolves to the id the
  walk will assign by replaying the walk's own emission rule, so a consumer met
  before its controller still names the right endpoint.
- The Broadcom mask is a vendor property, so `kernel/arch/aarch64` overrides
  the walk's `FdtPlatform::dma_channel_mask` hook to read it and convert it to
  window-relative numbering; the hook's default reads the generic
  `dma-channel-mask`. The duty records
  whether the tree stated a mask at all, and the Broadcom driver serves nothing
  without one, because its binding makes the property mandatory.

Two properties of the walk hold with it, both found wanting against the pinned
tree:

- **A node carries up to sixteen resources** (`HW_NODE_MAX_RESOURCES`): the
  legacy controller needs fifteen, and at eight its channels 7–10 had no line.
- **A specifier is mapped only under the port's root controller**, the
  effective `interrupt-parent` found as Linux's `of_irq_find_parent` finds it;
  each port names its controller's phandle (`find_gic`, the PLIC node). Read
  as the GIC's, `hdmi0`'s `aon_intr` specifiers granted INTID 33, an ARM
  mailbox line.

**Kernel prerequisites.** Three general mechanisms, each with its holder and
enforcement point in this change:

- `shm_create_dma` — a shared region carved physically contiguous under a
  `Dma` grant, zeroed, mapped `DMA_COHERENT` in every mapping so no alias is
  cacheable, with its translated device address reported to its creator.
- `shm_grant_peer` — mint a mapping of a region for the in-service caller of an
  endpoint the grantor owns; `shm_grant` reaches only an endpoint's server.
- `call_peer_holds` — whether the in-service caller holds a grant covering a
  request line naming the controller's endpoint, or a register window:
  `call_peer_seat`'s shape, asked only by the duty holder and only about a
  caller it is actively serving.

**Scope.** SND5's driver is `drivers/dma/bcm2835`, the legacy engine
(`brcm,bcm2835-dma`), named for the binding it serves: every default-tree
consumer but HDMI names it, SND8's I²S among them. DMA4 (`brcm,bcm2711-dma`) is
a different register model and block format on its own node, whose only
default-tree consumers are the two HDMI audio paths, so it arrives with SND19
as `drivers/dma/bcm2711`, and the serve loop moves into a `lib/*` crate when
that second driver gives it two consumers. The seam has no pause (a paused DREQ-paced transfer starves
its peripheral; a consumer wanting silence writes silence), no
memory-to-memory, and no one-shot scatter-gather, which SPI and SD bring with
them when they arrive as consumers.

**Verification.** Host tests run the driver against a register-level model of
the controller that fetches control blocks from memory: the chain walked
cyclically with one interrupt per period, LITE limits, stop and abort, and the
three error bits. Every refusal has its test — an unheld request, a FIFO
outside the caller's windows or the controller's reach, a masked or exhausted
channel, a geometry the channel cannot hold. Discovery is tested over a fixture
in the pinned tree's shape carrying `dmas`, every new wire type round-trips,
and the endpoint's decoder has a fuzz harness drawing through
`tairix_fuzzseed::splitmix64`.

No QEMU vertical is reachable. The tree boots no `raspi*` machine
(`plans/PI.md`), and QEMU 11.1's `bcm2835-dma` model runs a chain to its end
synchronously with DREQ ignored, so a cyclic chain — the seam's whole purpose
— never ends and hangs the emulator. Metal acceptance stays pending until
SND8's first transfer on a Pi 4 supplies its artefact, and is recorded so in
`plans/PI.md` when the driver lands.

### `drivers/audio/usb_uac` — USB Audio Class

How most people actually connect audio in 2026: headsets, DACs, and monitors
with speakers. UAC1 and UAC2 descriptor parsing, the clock source and clock
selector units, the feature unit for volume and mute, the terminal topology
that says which endpoint is which jack, and isochronous data endpoints with
explicit or implicit feedback.

**It is reached by isochronous transfer support the tree does not have, and
this plan owns that work (SND6) rather than waiting on it.** Today `lib/usb`
models Control, Bulk and Interrupt-IN endpoints only: it names the isochronous
completion codes but has no isochronous endpoint kind and nothing that
schedules one. `plans/USB.md` listed isochronous transfers as out of scope as
"a later class driver or HCD extension"; that scope is amended, and the
extension is specified and staged here because this is the plan whose first
consumer needs it.

What it entails, and why it is not a small addition to a bulk transfer:

- **The endpoint kind and its service interval.** An isochronous endpoint is
  periodic: it moves a fixed budget of bytes every service interval whether or
  not anyone asked, and the interval comes from the endpoint descriptor's
  `bInterval` as a power-of-two microframe count, not from a queue depth.
- **Periodic bandwidth reservation.** Unlike bulk, an isochronous endpoint is
  *admitted or refused* at configure-endpoint time against the bus's periodic
  budget, which xHCI computes from the endpoint context's maximum packet size,
  burst count and multiplier. A device that does not fit is refused with a
  stated reason — the honest answer — rather than admitted and then starved.
- **Frame-indexed scheduling.** Isochronous TRBs carry the microframe they are
  to be delivered in, so the ring is filled *ahead* of the controller against
  a horizon and each transfer is placed at a frame index rather than simply
  enqueued. Falling behind the horizon is a missed service interval, which the
  controller reports and the driver must account rather than silently retry —
  a late audio packet is not a packet to send later, it is a gap.
- **Feedback.** The device's clock is not the host's. An explicit feedback
  endpoint reports the rate the device actually wants in its own fixed-point
  format; an implicit-feedback device paces the host from its own data
  endpoint instead. Either way the host's packet sizes vary per interval to
  track a device clock that drifts, which is exactly the adaptive case
  invariant 3's clock model already describes — so the correction is a
  *reported* value here too, not a hidden one.

The work lands in `lib/usb` (the endpoint kind, the service-interval model,
the feedback arithmetic — all host-testable) and `drivers/bus/usb/xhci` (the
ring, the reservation, the frame index). It is not audio-specific and is not
written as though it were: a USB camera is the next consumer, and the seam is
shaped for a periodic endpoint rather than for a sound card.

## The applications

### `play` — the command player

`kind = "command"`, in the system command store, so `play foo.flac` works on a
text console with no desktop.

There is no GNU coreutils counterpart, so §16.7 binds by its spirit rather than
its letter: the de-facto tools are `sox`'s `play`, `mpg123` and `aplay`, and
the flag spellings follow them where they agree and are stated where they do
not.

```
play [OPTION]... FILE...
  -q, --quiet              no interface, no progress
  -v, --verbose            per-file format and timing on stderr
      --ui / --no-ui       force the full-screen interface on or off
  -d, --device=SINK        an audio: resource reference
  -g, --gain=DB            gain applied to this playback
  -s, --start=TIME         begin at an offset
  -t, --duration=TIME      play for a duration
  -l, --loop[=N]           repeat each file, or the whole list
      --list-devices       enumerate sinks and exit
  -h, -?, --help           the bundle's own Help document
      --version
```

**Backgrounding is a design property, not a flag.** `play album.flac &` keeps
playing while the shell takes the terminal back, because **playback is not in
the interface loop**: the samples go to `audiod` over the stream, and the
full-screen interface is a *view* of a playback that would happen without it.
So `play` decides its interface from whether it has a terminal and holds the
foreground process group (the shell's `console_foreground` handoff), drops the
interface and keeps playing when it is backgrounded, and takes it up again when
it is foregrounded. `--ui` where no terminal exists fails closed with a stated
reason on stderr rather than half-drawing.

The full-screen interface is `lib/curses`: the playlist, the current track's
format and position, a transport line, a peak meter, and keys for
play/pause/next/previous/seek/volume/quit. It paints from state and never
blocks on I/O, for the same reason a window must not.

On fd 3 it emits the structured advisory records the standard information
stream defines: a `schema` record naming the format, rate, channels and
duration it is playing; a `summary` record with frames played and underruns;
and an `omission` record for a file skipped and why. Nothing on fd 3 changes
stdout, the exit status, or the pipeline.

An abnormal end states its reason on stderr — a refused file, a lost device, a
sink it may not open — because a player that stops silently is a defect.

### `music.app` — the desktop player

`userland/apps/music`, named `music.app` because `play.app` is taken by the
command and the build refuses two bundles claiming one name.

Single instance, one window, resident on the icon bar, over the shared app
shell (`lib/window::app`) and composed from `lib/controls` — painting no
control of its own.

**Engine (`src/lib.rs`)**, host-tested, no window and no I/O: the playlist
model, the transport state, one `Layout::for_window(w, h, theme, scale)`
producing every rectangle that render, hit-test and the tests read, renderers
that paint only from state, an album-art cache under `lib/reclaim`'s budget and
pressure bands, one pure input entry point returning state change plus damage,
and a request/answer desk `Run` services.

The interactive-surface rules are the load-bearing part and they are what stops
this app being the usual stuttering media player:

- **The seek slider is not wired to a seek.** Dragging it changes the in-memory
  position model and repaints; the seek is issued at the slider's settle point.
  One drag is one seek.
- **The volume slider is not wired to a settings write.** It sets the stream's
  gain, which is an in-memory value the mixer reads; the durable per-user
  volume is written once, coalesced, when the drag settles.
- **A paint reads nothing.** Album art, track metadata and directory listings
  are *requested* and collected later; a paint draws what has arrived and a
  meaningful placeholder for what has not.
- **The level meter repaints its own rectangle**, not the window. A meter that
  invalidated the surface sixty times a second would be the defect §28 names,
  and it would be worst on exactly the slow machine that can least afford it.

Behaviour: open a file or a folder, a playlist with reorder and shuffle and
repeat, transport with seek, per-track and per-application volume, album art
from the file's own embedded picture (decoded through the sandbox like any
other untrusted image), track metadata from the container's own tags, gapless
playback between tracks that the decoder says are gapless, an output-device
chooser, an app-declared menu, a context menu, and keyboard equivalents
throughout.

A file handed to it by the file manager opens in the running instance through
the desktop's single-instance funnel, exactly as `view.app`'s does.

**Not in it**: a spectrum analyser (it needs an FFT nothing else in the tree
wants, and nobody asked for one), an equaliser, a library database, and any
form of editing. Peak and RMS meters are the whole of the visualisation.

## Default desktop sounds

Fourteen authored masters, one closed event vocabulary, one authority that
plays them, and the same single path every other sample takes. The masters
exist and are measured (below); what this section fixes is everything around
them — who may make a sound, where the bytes live, what happens when an asset
is missing, and why a cue is not a notice.

### A cue is an occurrence, not a state edge

The reflex is to reach for the system-notice mechanism, because
`PowerConnected` looks exactly like a topic. It is the wrong mechanism and
`plans/NOTICE.md` says so in its own terms: a notice keeps no history, so a
subscriber that was not looking learns only the current value. That is right
for the desktop's appearance and wrong for a sound. Two USB sticks arriving a
second apart are two sounds, and a notification published while the machine was
busy is not a notification that never happened.

A cue is therefore an **IPC request** — an occurrence a recipient witnesses
individually — which is exactly what that plan's own rule prescribes for a
thing history must be kept for. The distinction is not pedantry: it decides
whether the second stick is audible.

The pairing is the useful half. Where the underlying fact *is* a state edge,
the principal that publishes the edge is the principal that requests the cue.
Nothing observes an edge twice, nothing sounds for a state that has not moved,
and no component needs both mechanisms.

### The vocabulary

`SoundEvent`, a closed enum in `lib/soundtheme`, never a free-form string —
for `GraphicsFamilyKind`'s reason: a consumer matches on the event, so adding
one forces every reader to say what it means rather than silently ignoring a
name it does not recognise.

| Event | Shipped asset | Who may cue it, on what attested fact |
|---|---|---|
| `Startup` | `startup` | the service manager, at the point the machine is ready for a user |
| `Shutdown` | `shutdown` | the shutdown authority (`CAP_SYSTEM_POWER`'s holder) |
| `Login` | `login` | the session authority (`userland/session/login`) |
| `Logout` | `logout` | the session authority |
| `DeviceAttached` | `usb_connected` | the hotplug authority that publishes the arrival (`volmgr` for a volume) |
| `DeviceDetached` | `usb_disconnected` | the same |
| `Notification` | `notification_soft` | any principal whose session holds the seat lease on the sink |
| `NotificationMessage` | `notification_message` | the same |
| `NotificationReminder` | `notification_reminder` | the same |
| `Error` | `error` | the same |
| `Warning` | `warning` | the same |
| `Info` | `info` | the same |
| `Bell` | `notification_soft` | the same |

Two authority classes, **no new capability**, each checked against a fact the
kernel already attests:

- **Lifecycle events** (the first six) are cued by the principal that already
  owns the transition or the edge. This is `plans/NOTICE.md`'s publish-
  authority rule applied unchanged — reach for an existing lease, ownership or
  binding before considering a capability. A `CAP_SOUND_CUE` would fail the
  capability-minimalism tests anyway: every program wants to beep, and a
  capability every program holds is not a boundary.
- **Application events** (the last seven) need what playback already needs — a
  session holding the seat lease on the target sink, checked at open against
  the kernel-attested caller. An unclaimed sink is the headless case and any
  principal with a stream may cue on it, because a server beeping at a failure
  has no session.

That split is what forecloses the obvious spoof. Without it any program could
play the login sound, or the shutdown sound, and a user has no way to tell a
real transition from an application's imitation of one. With it, a sound that
claims the machine did something can only have come from the principal that
did it.

The two halves come into force at different points, and the load-bearing one
comes first. The lifecycle check is against the caller's kernel-attested
identity and holds the moment `soundd` exists, so no application can imitate
the machine from SND21 onward. The application half scopes a cue to a sink,
and until SND13 leases sinks to seats no sink is claimed — the router's
headless case — so any principal may cue on one. That is the degradation
ordinary playback already has and no more, and it narrows for cues and for
playback together when the leases land.

`Bell` is the terminal's `^G`, and it is in the vocabulary because the consumer
is already in the tree: `userland/apps/terminal`'s parser handles `Op::Bell`
and records in a comment that no audible bell is wired. It maps to the soft
notification asset rather than one of its own — a terminal bell should be
unobtrusive, and an event-to-asset mapping is theme *data*, so two events
sharing an asset cost one cache entry and no code.

**`PowerConnected` and `PowerDisconnected` are authored and held back.** Both
assets exist and nothing in the tree can cue them: there is no power-supply or
battery interface at all, which `plans/NEW-DESKTOP-SETTINGS.md` §3 and
`plans/NEW-SWITCHBOARD.md` each state as an absent interface. An enum variant
with no cue site is speculative surface, so the two variants and their two
assets land together with the power-supply interface (`plans/DEVICES.md`'s
sensor work), not before. Twelve events ship; these two wait for a publisher,
which is the discipline the two audio capabilities were already held to.

### `lib/soundtheme` — vocabulary, shipped set, settings, client

`lib/wallpaper`'s shape applied to sound, because it is the same job: a closed
vocabulary, a shipped default set discovered at build time, a bounded settings
document, and a client that asks an authority to act. `no_std`,
`forbid(unsafe_code)`, no I/O, host-tested.

- `event` — `SoundEvent` and its authority class.
- `catalog` — the shipped themes and their assets, and the bounded listing
  model a chooser draws. A theme is a directory; adding one is dropping it in.
- `settings` — the closed registry of the sound document, read tolerantly and
  merged strictly, the two readings the pinboard document already distinguishes.
- `cue` — the client half: the request a program makes, over an injected
  transport seam, so the whole surface is host-tested without a machine.

It knows nothing about decoding (`lib/sound`) and nothing about mixing
(`lib/audio`). It is the vocabulary and the policy, and the two crates it sits
between stay ignorant of each other.

### `soundd` — the cue authority

`userland/system/soundd`, a `kind = "service"` bundle discovered from disk like
any other. It receives cue requests, resolves the event to an asset, decodes it
through the sandbox, and plays it as a `Notification`-role stream on the target
sink. It is a third player alongside `play` and `music.app`, and it is a player
rather than part of the mixer for two reasons that do not bend:

- **`audiod` holds every stream in the system**, and untrusted bytes never
  decode in a process holding a stream. A user's own chosen sound file is
  untrusted input, and orchestrating its read and decode from the mixer would
  put the one process that must never fall over downstream of a decoder.
- **`audiod`'s per-period path allocates nothing, locks nothing and blocks on
  nothing.** Cue resolution reads settings and files. Filesystem I/O on the
  thread that owes a period is the defect §28 names for an interactive loop,
  and a real-time mixer has less slack than a window does.

It is not the session either, for a simpler reason: `Startup` precedes every
session, `Shutdown` outlives them, and a headless machine has none. Only a
machine-scoped service is present for the whole set.

`soundd` holds **no capability of its own**. It plays through the ordinary
`audio-v1` client surface with no more authority than any program that makes a
sound, and it reads `/System/Audio` and the machine settings document. It never
holds `CAP_APPDATA_ADMIN` and never reaches into a user's files.

### Resolution, and what silence means

Total, in one order: the active theme's asset for the event, then the shipped
default theme's, then **silence**.

Silence is a real answer, and this is the one place the desktop's asset rules
diverge from artwork. An icon may never resolve to nothing, because a blank
surface is a broken window, which is why §10 mandates a built-in vector glyph
beneath every icon. A sound has no such floor: a system that stays quiet is a
system that stays quiet, which is exactly what a user who disabled an event
asked for. So there is no synthesised fallback beep — fabricating a tone
because a file failed to decode would be inventing output, and a decode refusal
is reported rather than covered up.

A cue that resolves to silence still succeeds, and the caller is told the
machine made no sound and why, so a component can record its own failure
without the audio path having to guess whether the quiet was intended.

### The settings, and how a user's choice reaches a machine service

Two documents, layered, with no gap and no third place:

- **The machine document** under `/System/Settings`, written through the
  settings authority that already exists — where the default sink and
  per-device gain live. It is the whole policy before login, on a headless
  machine, and after the last session ends.
- **The user's own choices**, layered over it while their session holds the
  seat: a master enable, a per-event enable, a per-event gain, a per-event
  asset override, and the theme.

The interesting half is how the second reaches `soundd` without giving a
machine service reach into a user's files. It does not fetch them: **the
session pushes the resolved policy when it claims the seat**, exactly as the
session is the only writer of the pinboard document. A user's own chosen file
arrives as a **one-shot read descriptor** — the file-picker pattern §16.5
already names — which `soundd` hands to the sandbox worker without ever holding
a filesystem capability or interpreting a byte of it.

A per-event gain defaults to unity and **the shipped theme is unity
throughout**, which is what keeps the shipped cue path bit-exact and therefore
testable against the asset itself.

### The shipped set

Fourteen masters authored as one family, all 48 kHz stereo 16-bit signed PCM,
shipped losslessly as FLAC — roughly 0.75 MiB for the set, from 3.47 MiB of
raw samples.

| Event | Asset | Seconds | Peak |
|---|---|---|---|
| `Startup` | `startup` | 3.12 | −9.2 dBFS |
| `Shutdown` | `shutdown` | 2.55 | −12.4 dBFS |
| `Login` | `login` | 1.38 | −12.9 dBFS |
| `Logout` | `logout` | 1.32 | −14.9 dBFS |
| `DeviceAttached` | `usb_connected` | 0.76 | −14.4 dBFS |
| `DeviceDetached` | `usb_disconnected` | 0.76 | −13.5 dBFS |
| `Notification`, `Bell` | `notification_soft` | 0.98 | −18.7 dBFS |
| `NotificationMessage` | `notification_message` | 1.22 | −15.7 dBFS |
| `NotificationReminder` | `notification_reminder` | 1.60 | −15.7 dBFS |
| `Error` | `error` | 1.08 | −10.5 dBFS |
| `Warning` | `warning` | 1.18 | −14.7 dBFS |
| `Info` | `info` | 0.95 | −17.6 dBFS |
| held for its publisher | `power_connected` | 1.02 | −14.9 dBFS |
| held for its publisher | `power_disconnected` | 1.02 | −15.5 dBFS |

The rate is not incidental. 48 kHz is the rate `virtio_snd` reports and
defaults to, and the native rate of an HDA or USB sink, so a shipped cue at
unity gain engages **no resampler and no conversion** and reaches the device
bit-exact. The authoring constraint and the engine's headline property are the
same property, which is what lets the vertical below assert the captured bytes
against the master's own samples. A sink that genuinely cannot take 48 kHz
resamples like any other stream and simply forfeits that assertion, not the
sound.

The authoring contract, checked at build time, failing the build when broken:

- 48 kHz, stereo, 16-bit — **decoded in full through `lib/sound` during
  discovery**, so "the system can play this" is verified rather than inferred
  from the extension.
- The decoded samples match the stream's own `STREAMINFO` digest, so a
  corrupted asset fails the build against its own claim.
- First and last frame exactly zero, so a cue neither clicks in nor out. All
  fourteen masters already satisfy this, with 7–20 ms of lead-in and a uniform
  12 ms tail.
- Within `MAX_SOUND_BYTES`, as a wallpaper is within its own bound.
- A name resolving to a live `SoundEvent`. An orphan asset fails the build,
  which is also what catches a stray `.DS_Store` swept into an asset directory.

Decoding at build time is deliberately stronger than the graphics families'
check, which validates a name, a byte bound and uniqueness. The asymmetry is
the same one that makes silence a legal answer: a bad icon degrades to a
visible fallback glyph, and a bad sound degrades to silence, which is
indistinguishable from working. Where failure is invisible at runtime, the
build must look harder.

**They ship as FLAC, and the size was measured rather than assumed.** A
fixed-predictor Rice estimate over the set puts FLAC at 22% of the bytes: 3.47
MiB becomes roughly 0.75 MiB. That saving is in the repository as well as in
every image, it compounds with each further theme the catalog is built to hold,
and it is 2.7 MiB that a Pi's SD card and the `images/tairix-web/` bundle both
pay for on every build. FLAC is lossless, so the content in the table above is
exactly what is shipped and exactly what is played.

Two properties decided it over WAV beyond the size:

- **The asset verifies itself.** `STREAMINFO`'s digest covers the unencoded
  samples, so the build-time contract check catches an asset corrupted in the
  repository, in transit, or by a bad merge — against the file's own claim
  rather than a hash we would otherwise have to maintain beside it. A WAV has
  no such check and a flipped bit in one is simply a different sound.
- **We encode it ourselves.** With the encoder above, the shipped artefact is
  produced by first-party code from an authored master, deterministically and
  auditably, rather than by a foreign binary whose output we would be
  committing unexamined into a reproducible image.

That second point is a preference rather than a prohibition, and it is worth
saying why, because `lib/wallpaper` ships 35 MiB of externally-authored JPEG
and is not a defect. A wallpaper is authored *content*: there is no in-tree
master it derives from, nothing here could produce it, and `lib/image` has no
encoder because nothing needs one. A sound asset differs on both counts — the
format is lossless, so the shipped file and the master carry identical
information, and the encoder exists anyway for the test story. The rule is the
same in both places (prefer first-party where it is feasible); feasibility is
what differs.

The cost at runtime is nothing worth counting. A cue decodes once per boot per
event into the reclaimable cache, and a second of 48 kHz stereo FLAC is a few
milliseconds of LPC and Rice work against a sandbox round trip that both
formats pay identically.

The masters are `plans/tairix-desktop-audio/` today. They are converted by
`cargo xtask sound-encode` into `lib/soundtheme/assets/TAIRiX/<event>.flac`
in the change that creates the crate, and that directory is deleted with them:
a plan directory is not an asset store, and keeping the WAV beside a lossless
encoding of it would be two copies of one thing.

**Loudness normalisation is not claimed.** The measured peaks span −18.7 to
−9.2 dBFS, and peak is not loudness. A real loudness match needs an ITU-R
BS.1770 meter, which nothing else in the tree wants, so the set is authored as
a family, the build enforces a peak ceiling rather than a loudness target, and
a user who finds one event too loud has a per-event gain. That is stated rather
than left implied by a table of peaks.

### What bounds it

- **Concurrent cues are capped**, and the cap is a containment bound rather
  than a capacity: it is what stops a cue storm opening unbounded streams in
  the mixer. Past the cap a cue is **dropped and counted**, never queued — the
  `Notification` role's own rule, for its own reason, since a sound arriving
  after the thing it announces is noise.
- **A repeat of one event by one principal inside a minimum retrigger interval
  is coalesced** and counted. Two identical notification sounds 20 ms apart are
  one notification and one bug.
- **Per-principal cue rate is limited and fails closed**, so a misbehaving
  application cannot hold the speakers.
- **The decoded-PCM cache is reclaimable**, under `lib/reclaim`'s
  `DisposableUi` budget beside the album-art cache. Nothing about a cue is
  pinned: unlike a live stream's buffers, a cached cue re-decodes in
  milliseconds and losing it under pressure costs nothing audible.
- **A cue whose sink has no audio device is dropped and counted.** `Startup` in
  particular is cued where the device may legitimately not have bound yet, and
  a startup sound eight seconds late is worse than none.
- **`Shutdown` drains on a bounded deadline** and then proceeds regardless. A
  machine that will not power off because a sound is playing is a worse defect
  than a truncated sound.

### Verification

- **Host.** The resolver over the cross-product of (event × theme × settings ×
  asset present), including every path to silence; the settings document's
  tolerant and strict readings against malformed input; the authority split,
  asserting a lifecycle event is refused to an application principal; and the
  rate limit, the retrigger coalescing and the concurrent cap, each asserting
  the drop is *counted* rather than silently lost.
- **Build.** The family contract above over the real shipped assets, through
  the same `tools/syshelp` table and loop that already validates icons,
  wallpapers and cursors.
- **QEMU.** The strong one, and nearly free because `audio_virtio_qemu_*`
  already exists: cue one event through `soundd` on a booted machine and assert
  the host-side WAV capture is **sample-exact against the shipped master's own
  decoded samples**. That single assertion covers the cue authority, the
  resolver, the sandboxed decode, the client, the mixer, the device channel and
  the driver — and it holds only because the asset is 48 kHz at unity gain,
  which is the authoring contract earning itself. Because the comparison
  decodes the master on the host and the guest decodes it independently, a
  divergence between the two is caught here as well, on top of each side's own
  digest check.
- **Fuzz.** The cue request decoder, as every IPC endpoint's must be.

## Desktop integration

Consumers of this subsystem, each owned by its own plan and named here so the
work is not re-derived:

- **Settings** — `plans/NEW-DESKTOP-SETTINGS.md` §3's `Sound` row states this
  subsystem's absence and names this file as its prerequisite; the row leaves
  §3 for a real pane when the subsystem lands. The pane is output and input
  device selection, per-device volume and mute, the default-device policy, the
  live capture list, and the desktop sounds — theme, master and per-event
  enable, per-event gain, and choosing a file of one's own, which the pane
  hands over as the one-shot descriptor rather than a path. All of it typed
  intents to the authority holder; no capability in the app.
- **The taskbar** — a volume control in the notification area with a slider
  popup, and the recording indicator invariant 5 requires. Drawn by the
  session from `audiod`'s state, so no application can suppress it.
  `plans/NEW-TASKBAR.md` owns the area; this plan is a consumer.
- **The Switchboard** — an audio section: devices, live streams with their
  owners and positions, underrun tallies, and the measured device rates.
- **The System Information API** — devices, streams, positions and glitch
  tallies. A caller sees its own streams unprivileged and other principals'
  behind `CAP_SYSINFO_GLOBAL`.
- **`audio:` resource references** — `plans/ALIAS.md` §6.11 reserves the
  scheme; this plan is its first implementation, resolving `audio:sink/default`,
  `audio:sink/<id>`, `audio:source/default` and `audio:source/<id>` through the
  shared resolver.
- **The file manager** — `lib/browse::media` gains `AudioWav`, `AudioAu`,
  `AudioFlac`, `AudioOgg`, `AudioOpus` and `AudioMpeg`, `lib/icon` gains an
  `Audio` file-kind glyph (the existing `Volume` speaker stays the volume
  control's), and `music.app`'s manifest declares the associations so a
  double-click plays.
- **`audioctl`** — a command exposing the same control surface as the Settings
  pane for a headless machine: list devices, set the default, set a device's
  volume, list live streams.

## Refused by name

- **AC'97.** Superseded by HDA on every motherboard for two decades. Writing
  one would be dead code the day it landed.
- **An exclusive or bypass path.** Invariant 2 removes its reason for existing.
- **A second mixer, resampler, or client API.** Whatever the motivation, this
  is the mess the plan exists to avoid.
- **Compressed passthrough** (AC-3/DTS/E-AC-3 bitstreams over HDMI or S/PDIF).
  It is not PCM, so it is not a mixing problem: it is an exclusive,
  non-mixable stream kind carrying bytes we can neither decode nor verify, and
  nothing in the tree produces or consumes those bitstreams. Adding the stream
  kind before there is a consumer would be speculative surface.
- **In-kernel audio.** Nothing about sound belongs below the driver-host
  boundary; the mixer is a user-space service and the drivers are user-space
  processes.
- **Per-board codec quirk tables.** See the HDA section: the cost is stated,
  and the alternative has no charter-legal home.
- **VCHIQ.** A second device-interconnect stack, used by nothing else in the
  tree, that would put the Pi's audio path behind closed firmware.
- **MIDI, audio capture-to-file utilities, an equaliser, effects processors,
  and a sound-server protocol for foreign clients.** None has a consumer.
- **A sound for every interaction.** Clicks, keystrokes, window opens and
  focus changes get none. A cue marks something the user did not ask for and
  must notice; feedback for an action they just took is what the screen is
  for, and a desktop that chirps at every click is one whose sound gets
  switched off entirely.
- **An installable third-party sound pack.** A theme is a directory of assets
  the build discovers and validates. An install format for untrusted sound
  bundles is a packaging surface with no consumer, and a user who wants their
  own sound sets it per event.
- **A synthesised fallback tone.** Where an asset is missing or refuses to
  decode, the answer is silence and a reported reason, never a fabricated
  beep.

## Prerequisites and open decisions

Two pieces of work sit below this plan's own items and neither is left as
someone else's problem:

- **A DMA-engine seam (SND5) and isochronous xHCI support (SND6)** are
  prerequisites this plan **owns and delivers**. Both are cross-cutting rather
  than audio-specific and both are specified above, shaped for their general
  case rather than for a sound card. `plans/USB.md`'s out-of-scope list is
  amended to point here for the isochronous half; nothing is silently diverged
  from.
- **A native VC6 HDMI encoder on the Pi (SND19's blocker)** is *not* owned
  here. Without it HDMI audio cannot land, and with it the work is a display
  change: mode set, N/CTS, InfoFrames and EDID, belonging to `plans/PI.md`.
  The decision is whether to take that on as part of reaching HDMI audio, or to
  ship Pi audio on the analogue jack and I2S first.

  **The recommendation is the latter.** The jack and I2S are fully native,
  unblocked by SND5, and prove the whole stack on real silicon; the encoder is
  then one clean piece of display work taken on its own merits rather than a
  large dependency dragged sideways into an audio change. There is no third
  option: the only other route to HDMI audio is refused outright rather than
  deferred, for the reason given above.

SND5's design surfaced two decisions, both taken:

- **DMA memory outlives its use by the device, across its driver's death**, for
  every DMA-mastering driver through `plans/OPEN-DEFECTS.md` D167's node
  quarantine (Fuchsia's BTI rule): a dead driver's carves are held against its
  hardware-tree node until the node's next driver declares its device
  quiesced (`DmaHost::device_quiesced`), so a cyclic chain a dead controller
  left running can never fetch its next block from reused memory. SND5's code
  needs no special case beyond resetting every masked channel before it
  declares.
- **SND5's leaf is `drivers/dma/bcm2835`**, the legacy engine; DMA4 arrives
  with SND19 (§Scope above). One crate for both would have needed a driver to
  learn which of its bind keys matched — `devmgr` hands a driver only its
  grants — and a DMA4 model nothing exercises before SND19.

SND8 inherits four facts, each needing a home before its drivers can bind or
its metal acceptance can pass:

- **Neither PWM node carries `dmas`**, so the jack's request line (DREQ 5 for
  PWM0, per the peripherals document) has no discovered source. The image
  builder already applies a firmware overlay (`disable-bt`); a first-party one
  adding the property is the likely shape.
- **PWM and I²S are `status = "disabled"`** in the pinned tree, and the walk
  emits disabled nodes and lets drivers bind them (`plans/OPEN-DEFECTS.md`
  D168).
- **Their pins need their alternate function**, and nothing in the tree sets
  one: no pinctrl or GPIO driver exists. The firmware's `config.txt` `gpio=`
  directive can set it at boot.
- **Memory below the legacy engines' 1 GiB ceiling has no reserve** against
  ordinary allocations (`plans/OPEN-DEFECTS.md` D175), so on a Pi with more
  RAM a buffer carved late on a busy system can be refused while memory above
  the ceiling is free.

One decision inside this plan is worth surfacing because it is visible to
users: **HDA codecs get no quirk table.** A small number of laptops whose
firmware misdeclares its own pin configuration will show a wrong jack name. The
alternative has no home in this tree.

## Verification

The claim "better than the others" is only worth making if it is checkable, so
the tests are chosen to check it rather than to check that nothing crashed.

**Host tests.**

- Every decoder against valid, malformed, truncated and adversarial input:
  every bit depth, every compression, degenerate geometry, overflow edges, and
  limits refused *before* allocation. Every input synthesised in test code.
- **Numeric accuracy against external oracles**, not against our own opinion:
  MPEG audio against the ISO 11172-4 / 13818-4 compliance limits, Opus against
  the RFC 6716 vectors, Vorbis against the published vectors.
- **FLAC's oracles, and what each actually proves.** Encoder/decoder
  round-trip over synthesised material is the **breadth** check — every block
  size, bit depth, channel count, subframe type and partition shape — and it
  proves the two halves agree, which a shared misreading of the specification
  would satisfy just as well. Checking a *synthesised* stream against its own
  `STREAMINFO` digest proves no more than that, because our own encoder
  computed the digest; treating it as conformance would be the weaker oracle
  wearing the stronger one's clothes.

  The digest is a genuine external oracle only over a stream some *other*
  encoder produced — and there it is an unusually good one, because the file
  states its own expected output, so the fixture needs no companion PCM and no
  hash of ours to keep in step. That is exactly the case the
  synthesise-every-input rule exists to avoid and does not cover, so FLAC is
  the one format that also keeps a small set of committed foreign-encoded
  streams: self-verifying, a few kibibytes, and worth more than any fixture we
  could write ourselves.
- The mixer's **bit-exactness property**: a 24-bit-or-narrower source at unity
  gain through the whole engine is byte-identical to its input. Property-tested
  across formats, rates, channel counts and block boundaries.
- The resampler's stopband attenuation, passband ripple and transition width
  **measured** in the test, so the documented figures cannot drift from the
  kernel.
- Channel-map matrices against the standard downmix coefficients.
- The clock model's rate estimate against a synthesised drifting device.
- The routing policy exhaustively over (role × seat state × device set).
- The three damage-correctness properties every app owes, for `music.app`, and
  the layout at several scales.

**Oracles.** The PCM ring is a lock-free producer/consumer protocol with an
`Acquire`/`Release` pairing, so it carries a `loom` model — not optional and
not satisfiable by the test matrix, which runs whichever interleaving the host
happened to pick, and on a total-store-ordered host would pass even with the
orderings downgraded to `Relaxed`. **Landed with SND3**: `tairix-abi` is
enrolled in `cargo xtask loom` and `lib/abi/tests/loom.rs` holds the models.

The enrolment is `tairix-abi` rather than `lib/audio` because the ring lives in
`lib/abi/src/driver/audio_ring.rs`. Two constraints shaped the model and are
recorded here because they also bind any later ring (`net_ring` next):

* `loom` substitutes its own atomic type, which is not eight bytes of shared
  memory, so `PcmRing::bind` cannot exist in a `--cfg loom` build — the file
  cfg-selects the atomics import, `bind` and the two header cell indices are
  `cfg(not(loom))`, and a `cfg(loom)` `over_counters` constructor takes the two
  positions directly. A `build.rs` registers the cfg, as `lib/sync` does.
* The sample area is a plain byte region two processes map, so it cannot be a
  `loom::cell::UnsafeCell` and the model checker cannot see accesses to it.
  Each side therefore brings its own area and the model hangs a
  `loom::cell::UnsafeCell` **payload** off the real edge instead: the producer
  writes the cell then calls `write`, the consumer calls `read` and reads the
  cell **only** where that returned frames, and nothing else — no join, no
  lock, no second atomic — connects the two threads.

  That last point is what makes the model sharp rather than ceremonial, and it
  is why a counter-pair-only model was rejected: per-location coherence already
  gives monotonicity, so a model with no data could not distinguish `Release`
  from `Relaxed` at all. This one can, and was **verified to fail** by
  downgrading the producer's store before it was accepted — loom reports
  "Causality violation: Concurrent read and write accesses".

The "no torn frame" half stays with `lib/abi/tests/audio_ring_spsc.rs`, which
drives both sides concurrently over one genuinely aliased region, and with
`fuzz_audio`, which drives every operation over positions a hostile peer could
have written.

`lib/audio` carries `forbid(unsafe_code)` and performs no shared-memory access
of its own — it works on slices its caller owns — so `cargo xtask miri` has
nothing there to interpret and the crate is not enrolled. `lib/audiochan` is
not enrolled either, for a different reason: its one `unsafe`, turning the
kernel's `shm_map` result into the region slice, sits in the freestanding-only
`serve` module, which has no host build for the interpreter to run, and its
soundness rests on the kernel's mapping contract, which the QEMU vertical
exercises on all three targets. The host-built half, `AudioChannelServer`,
carries no `unsafe` at all.

**`tairix-abi` is enrolled in `cargo xtask miri` too, and the enrolment found
a real one.** Both shared rings downgraded their header to a shared `&[u8]`
before `align_to`, so the atomic counters were derived from a read-only
provenance tag and *every publication was a write the borrow never granted*.
Nothing could have caught it below the interpreter: the generated code is
correct today, and the compiler is entitled to act on the aliasing claim at
any time. Both `PcmRing::bind` and `FrameRings`/`net_ring`'s `bind` now take
the mutable path (`align_to_mut`, then a shared reborrow of the atomics, whose
interior mutability is what the peer's concurrent access needs), and the
enrolment is the standing regression test. The scope is `--lib`: the
`*_ring_spsc` integration tests deliberately alias two `&mut` views over one
region because that is how two processes map one `shm` object, which is
outside the aliasing model rather than inside it, and the shipped code never
aliases within an address space.

**Fuzzing.** A structure-aware generator per format in one registered
`fuzz_sound` target; harnesses for the `audio-v1` and `audiochan-v1` decoders,
as every IPC endpoint and public ABI decoder must have. Crashing inputs enter
the regression corpus with a unit test.

**QEMU verticals — the centrepiece.** QEMU's `-audiodev wav` backend writes the
guest's audio output to a file on the host, which turns an audio test from "did
it crash" into an exact numeric assertion:

- `audio_virtio_qemu_{aarch64,x86_64,riscv64}` — **landed**: boot, discover
  the device, autoload the driver into its own process, hand its channel to
  `audiod`, run the `audiotone` fixture from the scripted root shell, and
  assert the host-side WAV is **sample-exact**. This is invariant 2 proved on
  a running machine. A PASS needs both halves — the guest's own witness (the
  stack reported `Idle` with no lost frames) and the host's capture check —
  because a mixer that silently substituted, resampled or dropped frames
  would still print the witness.

  The run is deterministic rather than a race: the client ring is sized to
  hold the whole signal, so every frame is queued before the device is
  clocked and the device cannot run dry however slowly the emulated machine
  runs. The backend's own lead-in and tail are trimmed before the comparison
  (they are the *host*'s silence, not the guest's) and the signal is built so
  no frame it contains is silent on every channel, which is what makes that
  trim safe.
- `audio_hda_qemu_x86_64` — the same over `intel-hda`, so the motherboard path
  is covered by CI and not only by hope.
- **Underrun accounting** — deliberately starve a stream and assert the
  reported missing frame positions are exactly the frames the host WAV shows as
  silence. A glitch the system reports wrongly is worse than one it reports.
- **The seat vertical** — two sessions, a switch, and the assertion that the
  departing session's samples stop at a frame boundary, do not appear in the
  host WAV, and resume from the exact frame on switch-back.
- **Decode end to end** — `play` a fixture through the sandbox, and assert the
  host WAV matches the PCM the decoder produces in a host test. That single
  assertion covers the decoder, the sandbox protocol, the client, the mixer,
  the device channel, the driver and the hardware model in one line.
- **Capture** asserts the protocol — frames arrive, positions advance, the
  timing is right, the indicator is raised — and not the content, because
  QEMU's input backends inject silence. That limit is stated rather than
  papered over.

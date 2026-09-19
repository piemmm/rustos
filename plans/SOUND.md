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
| SND3 | `lib/audio`: conversion, mixer, resampler, channel mapping, clock model, routing policy, volume model, the stream client — all host-tested, plus the ring's loom model | planned |
| SND4 | `lib/audiochan` serve loop; `drivers/audio/virtio_snd`; `userland/system/audiod`; `CAP_AUDIO_DEVICE` and `CAP_AUDIO_CAPTURE`; the end-to-end QEMU vertical asserting a sample-exact host WAV | planned |
| SND5 | `lib/abi` DMA-engine class trait (`DmaEngine`/`DmaChannel`, cyclic chains, discovered request lines); `drivers/dma/bcm2711` | planned |
| SND6 | Isochronous transfer support: the endpoint kind and service-interval scheduling in `lib/usb`, and periodic bandwidth reservation, frame-indexed rings and feedback endpoints in `drivers/bus/usb/xhci` | planned |
| SND7 | `drivers/audio/usb_uac`: UAC1 and UAC2, clock and feature units, explicit and implicit feedback | planned |
| SND8 | `drivers/audio/bcm2711_pwm` with noise shaping; `drivers/audio/bcm2711_i2s` with a separately-bound codec | planned |
| SND9 | `lib/sound`: the registry, AU and WAV complete, the sandboxed decode seam, the fuzz target | planned |
| SND10 | `userland/apps/play`, with and without the curses interface, backgroundable | planned |
| SND11 | `drivers/audio/hda`: controller, CORB/RIRB, stream descriptors, the pure-graph codec walk; the QEMU `intel-hda` vertical | planned |
| SND12 | `lib/sound`: FLAC, verified against its own STREAMINFO digest | planned |
| SND13 | Seat integration: leases, pause-and-resume across a fast user switch, the capture indicator, the notice topic; the two-session vertical | planned |
| SND14 | `userland/apps/music` | planned |
| SND15 | Desktop integration: the Settings pane, the taskbar volume control and recording indicator, Switchboard, sysinfo, the `audio:` resolver, media types and icons, `audioctl` | planned |
| SND16 | `lib/sound`: MPEG audio Layers I/II/III, verified against the ISO compliance limits | planned |
| SND17 | `lib/sound`: Ogg container and Vorbis I | planned |
| SND18 | `lib/sound`: Opus, verified against the RFC 6716 vectors | planned |
| SND19 | `drivers/audio/rpi_hdmi` | blocked: needs a native VC6 HDMI encoder — the open decision below |

Each item is complete before the next begins and carries its own tests and
documentation.

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
| File decoders (AU, WAV, FLAC, Vorbis, Opus, MPEG audio) and their containers | `lib/sound` |
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

### `lib/sound` — the decoder registry

Shaped exactly like `lib/image`, because it is the same job on a different
medium: `SoundFormat` / `sniff` / `probe` / `open` dispatch, one private module
per format, `DecodeLimits` weighed **before** a buffer is allocated,
format-namespaced `DecodeError` variants, `no_std`, `forbid(unsafe_code)`,
fallible allocation through `tairix_util::fallible`, checked arithmetic on
every untrusted value, every test input synthesised in test code, and a
structure-aware generator per format in one registered fuzz target.

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
  against the file's own claim rather than against a fixture we wrote. That
  needs an MD5 implementation, which `lib/crypto` deliberately does not carry
  because MD5 is broken as a cryptographic hash. This is not a cryptographic
  use — it is an integrity check on our own arithmetic — so it lands as a
  plainly-labelled interop digest inside `lib/sound`, not as a `lib/crypto`
  primitive, and nothing security-relevant may reach for it. A mismatch is a
  decoder defect, reported as a decode failure rather than passed off as audio.
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
- **One resampler.** A polyphase windowed-sinc with a documented kernel
  (Kaiser-windowed, stopband and transition width stated and *measured* in the
  crate's tests, not asserted in its docs). The ratio is kept as an exact
  rational where the rates admit one — 48000/44100 reduces to 160/147 — so
  there is no accumulating phase error over an hour; only a drifting ratio
  (invariant 3's linked-domain case) uses the fractional-delay path. Drivers
  never resample and clients never need to: this is the only resampler in the
  system and a second one is a review blocker.
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
audio, with `lib/audiochan` the serve loop every audio driver process runs:

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

virtio sound (device id 25), over the `lib/virtio` split-virtqueue transport
and `drivers/bus/virtio` the tree already has, so it is the cheapest complete
driver and the one that gives an end-to-end QEMU vertical on **every Tier-1
architecture**. It lands first for exactly that reason.

The four queues (control, event, tx, rx), the jack/PCM/chmap information
requests, `SET_PARAMS`/`PREPARE`/`START`/`STOP`/`RELEASE`, the transfer
header/status framing with its `latency_bytes`, and the period-elapsed, xrun
and jack-change events — which map onto `audiochan-v1`'s notifications with no
translation layer, because the device channel was shaped from the same
hardware reality.

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
nothing models a DMA *controller channel* — request a channel for a given
peripheral request line, program a cyclic control-block chain, start, stop,
read the live position, and park on the per-period interrupt. Every Pi audio
path is fed by one, which is why no Pi audio path is currently reachable at
all, and it is the plan's second priority for that reason.

It is a **shared** seam, not an audio one, and its absence is the tree's gap
rather than this plan's: SPI, SD and UART DMA all want the same four
operations, and a driver that programmed the controller's registers itself
would be the duplication the charter forbids the moment the second one
appeared. So it is `lib/abi/src/driver/dmaengine.rs` with `drivers/dma/bcm2711`
implementing it, and it is written for a periodic slave transfer in general
rather than for an audio ring.

The cyclic chain is what makes it event-driven rather than polled: the control
blocks are linked into a ring with an interrupt at each period boundary, so the
driver parks on the completion interrupt and is woken once per period — it
never reads a position register in a loop to find out where the hardware got
to. The peripheral request line is read from the node's own `dmas` property,
never a constant: a request number baked into shared code would be exactly the
board coupling the charter forbids.

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

## Desktop integration

Consumers of this subsystem, each owned by its own plan and named here so the
work is not re-derived:

- **Settings** — `plans/NEW-DESKTOP-SETTINGS.md` §3's `Sound` row states this
  subsystem's absence and names this file as its prerequisite; the row leaves
  §3 for a real pane when the subsystem lands. The pane is output and input
  device selection, per-device volume and mute, the default-device policy, and
  the live capture list — all of it typed intents to the authority holder, no
  capability in the app.
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
  here, and is the one open decision. Without it HDMI audio cannot land, and
  with it the work is a display change: mode set, N/CTS, InfoFrames and EDID,
  belonging to `plans/PI.md`. The decision is whether to take that on as part
  of reaching HDMI audio, or to ship Pi audio on the analogue jack and I2S
  first.

  **The recommendation is the latter.** The jack and I2S are fully native,
  unblocked by SND5, and prove the whole stack on real silicon; the encoder is
  then one clean piece of display work taken on its own merits rather than a
  large dependency dragged sideways into an audio change. There is no third
  option: the only other route to HDMI audio is refused outright rather than
  deferred, for the reason given above.

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
  the RFC 6716 vectors, Vorbis against the published vectors, FLAC against each
  file's own `STREAMINFO` digest.
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
orderings downgraded to `Relaxed`. Two facts bound what that model can be, and
are recorded here so SND3 decides the shape with them in hand rather than
re-deriving them:

* the ring lives in `lib/abi/src/driver/audio_ring.rs`, so the enrolment is
  **`tairix-abi`**, not `lib/audio`;
* `loom` instruments its own atomics and cells, and the sample area is a plain
  byte region two processes map — it cannot be a `loom::cell::UnsafeCell`, and
  `AtomicU64`s cannot be carved out of shared bytes by `align_to` under the
  model's substituted types. A model therefore covers the **counter pair** —
  monotonicity, occupancy, and the release/acquire edges — over a constructor
  that takes the two counters directly, and the "no torn frame" half stays with
  `lib/abi/tests/audio_ring_spsc.rs`, which drives both sides concurrently over
  one aliased region, and with `fuzz_audio`, which drives every operation over
  positions a hostile peer could have written.

`lib/audio` and `lib/audiochan` are enrolled in `cargo xtask miri` for the
shared-memory accesses.

**Fuzzing.** A structure-aware generator per format in one registered
`fuzz_sound` target; harnesses for the `audio-v1` and `audiochan-v1` decoders,
as every IPC endpoint and public ABI decoder must have. Crashing inputs enter
the regression corpus with a unit test.

**QEMU verticals — the centrepiece.** QEMU's `-audiodev wav` backend writes the
guest's audio output to a file on the host, which turns an audio test from "did
it crash" into an exact numeric assertion:

- `audio_virtio_qemu_{aarch64,x86_64,riscv64}` — boot, discover the device,
  autoload the driver, open a stream, play a known signal, and assert the
  host-side WAV is **sample-exact**. This is invariant 2 proved on a running
  machine.
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

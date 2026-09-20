# tairix-drv-audio-virtio-snd

TAIRiX virtio sound driver (`plans/SOUND.md` SND4). Stability tier:
**experimental** — it tracks the unfrozen `abi-v1` `Audio` class trait and the
`audiochan-v1` contract.

## Supported hardware

A virtio sound device (virtio 1.2 §5.14, device type 25) on either bus: the
single-aperture virtio-MMIO device a `-M virt` machine presents
(aarch64/riscv64) and the scattered virtio-PCI `virtio-sound-pci` a PC
presents (x86_64). One signed bundle binds both, because the bus-agnostic
split-virtqueue transport abstracts the bus and the kernel resolves and
role-tags every window before this process sees it.

It is the stack's first driver and the one the end-to-end QEMU verticals run
against on every Tier-1 architecture, so the whole path — mixer, device
channel, driver, hardware model — is proved here before a second driver
exists.

## What it reports is what the device said

Bring-up reads the device's own configuration (how many jacks, streams and
channel maps it presents) and then enumerates each with an information
request. There is no table keyed on what the device claims to be, and there
could not be one: a board name in a driver's logic is exactly what the charter
forbids.

Where the device publishes nothing, the driver says so rather than inventing
an answer:

* **No jacks** → every endpoint reports `JackState::Unknown`, which is what
  "this endpoint has no detection" means. It never reports `Present`.
* **No channel maps** → the conventional layout for the reported channel
  count, which is what a count with no positions means (1 is mono, 2 is
  front-left/front-right, and so on). A count with no conventional reading —
  five channels, seven — is refused rather than guessed at.
* **A position this stack cannot place** → the whole map is left unpublished
  rather than half-read.

A format the engine cannot convert is simply not advertised; a stream that
offers *only* such formats is a device fault, because it can carry no audio
this system could mix.

## Substitution, not refusal

A rate or encoding the device cannot do is answered with the nearest thing it
can — the nearest clocked rate, the widest encoding it offers — so the mixer
adapts and owns the conversion the difference implies. A period is rounded up
to a power of two inside the driver's own DMA bound, so a ring's
power-of-two depth is always a whole number of periods and a wrap never splits
one.

## Required capabilities

The process holds only what its matched node granted: `CAP_MMIO_MAP` for the
register window, `CAP_MEM_DMA` for its period buffers, `CAP_IRQ_BIND` for the
line the serve loop parks on, `CAP_SHM` to map the mixer's granted PCM
regions, `CAP_IPC_ENDPOINT` + `CAP_IPC_BIND_PRIVILEGED` to claim the reserved
device-channel endpoint, `CAP_HW_EMIT` to publish its `audiochan` node, and
`CAP_LOG_EMIT` for its readiness beacon.

It deliberately does **not** hold `CAP_AUDIO_DEVICE`. That is the authority to
*command* an audio driver, which the mixer holds and this process is the
subject of: the endpoint is bound restricted-sender on it, so the kernel
refuses every caller but the mixer at dispatch and this driver never
re-checks.

## The position never lies

A running playback device must be fed every period or it glitches
unpredictably. When the mixer's ring is short **and the device has nothing
left in flight**, the driver submits silence for exactly the frames it is
missing and adds them to the stream's loss tally. Padding a period the device
has not yet asked for would manufacture a glitch out of frames that were
merely going to arrive in time, so it does not; a drain's tail goes as a short
transfer rather than a padded one, because nothing was lost.

A capture period that the mixer's ring could not hold is over-run, and it is
counted too: a recording that silently loses frames has an invisible edit in
it.

The reported position is what the device has *clocked out*, not what it was
handed: the transfer status' `latency_bytes` is subtracted from the submitted
total.

## Runtime load and unload

Loadable and unloadable at runtime. A `Detach` releases the endpoint's
device-side stream and unmaps its region; the process exits fail-closed with a
reserved code (`tairix_audiochan::exit`) if it cannot serve at all, leaving the
machine without sound rather than wedged.

## Tests

Host tests drive the engine against the in-process `MockTransport` with a shim
that answers as the specification's device does: bring-up reading, the
substitution policy, the period accounting, the silence-and-count path, the
capture delivery, and every control refusal surfacing as its own typed error.
The end-to-end QEMU verticals (`audio_virtio_qemu_{aarch64,x86_64,riscv64}`)
play a known signal on a real machine model and assert the host-side WAV is
sample-exact.

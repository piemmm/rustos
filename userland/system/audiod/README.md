# `tairix-audiod` — the audio service

**Stability: experimental.**

The one mixer, router and audio authority (`plans/SOUND.md` SND4). One system
service, not one per user: the device is machine state, so the arbiter is a
machine service with per-seat routing and per-principal accounting. It is the
sole holder of `CAP_AUDIO_DEVICE` and the only process that speaks
`audiochan-v1`; every program plays or records through `audio-v1` and there is
no second path.

## The two halves

`src/lib.rs` is the engine — pure, host-tested, and written over four injected
seams (the shared-region host, the device-channel transport, the client
notifier, and the monotonic clock every period pair is stamped against).
`src/run.rs` is the `Run` binary behind the on-by-default `program` feature: it
backs those seams with `shm_*`, `ipc_call`, `ipc_send` and `ClockDelay`, claims
the reserved `audio-v1` rendezvous, and parks on a wait set over {control
endpoint, every bound device's notify port}. Host tooling builds only the
library, so a host test never links the userland runtime.

## Regions run in both directions

A **client ring** is created by the client and `shm_grant`ed inward; the
service maps it and checks the mapped length against the geometry it granted
before a frame moves. A **device ring** is created by the service and granted
outward to the driver. `RegionHost` spells the two separately so a handle of
unstated provenance cannot be mistaken for either.

## Real-time discipline

Every buffer is allocated at stream-open or device-configure and reused; the
per-period path allocates nothing, borrows exactly one shared region at a time,
and folds the live streams into the mixer as an iterator rather than building a
collection. Its regions are pinned and its mixing path runs at real-time
priority where the machine grants it; a refusal of either is reported and the
service keeps serving.

## Authority

Playback needs no capability: the authorisation is that the caller's session
holds the sink's seat lease, decided by the one routing policy against the
kernel-attested caller. Opening a source additionally demands
`CAP_AUDIO_CAPTURE`, read from the attested capability summary. Adopting a
driver's device channel demands `CAP_DRV_LOAD` — the authority to put a driver
on the machine is exactly the authority to tell the mixer about one, so no
third capability is minted for it. Every capture grant and refusal, every
device bound or lost, and every default-device change lands on the audit log
with a stable event id.

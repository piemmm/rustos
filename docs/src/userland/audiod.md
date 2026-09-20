# Audio service (`audiod`)

`userland/system/audiod` is the one mixer, router and audio authority
(`plans/SOUND.md` SND4). It is the **sole holder of `CAP_AUDIO_DEVICE`** and
the only process that speaks `audiochan-v1`; every program plays or records
through `audio-v1`, and there is no second path.

One system service, not one per user. The device is machine state, so the
arbiter is a machine service with per-seat routing and per-principal
accounting — a per-user daemon is precisely the design that cannot arbitrate
between two logged-in users over one piece of hardware.

## The path a sample takes

```
program ──audio-v1──▶ audiod ──audiochan-v1──▶ driver process ──▶ hardware
        client ring              device ring
```

A client creates its own PCM ring and `shm_grant`s it inward; `audiod` creates
the *device* ring and grants it outward to the driver. The two directions are
spelled separately in the service's region seam, so a handle of unstated
provenance can never be mistaken for either.

Frames move on the device's own period interrupt and nothing else. The driver
services its ring from that interrupt and sends one notification carrying the
`(frame position, monotonic time)` pair; `audiod` folds that pair into the
endpoint's clock model and refills. There is no audio tick anywhere in the
system, and nothing polls.

## What it composes

Every decision about *what samples come out* is `lib/audio`'s:

| Stage | Where |
|---|---|
| Which sink a stream lands on, and what a seat switch does to it | `route::route` |
| The ducking rule | `route::duck_millibel` |
| Source layout onto sink layout | `ChannelMatrix::derive`, once at open |
| The one rate conversion | `Resampler` over a `FilterBank` shared per rate pair |
| Summing and the one quantisation | `Mixer` |
| Four gains into one multiply | `volume::resolve` |
| What a device's rate actually is | `ClockModel` |

## Real-time discipline

Every buffer is allocated at stream-open or device-configure and reused. The
per-period path allocates nothing: it reads each client ring into that
stream's own scratch (so exactly one shared region is borrowed at a time) and
folds the live streams into the mixer as an *iterator* rather than building a
per-period collection. Its memory is pinned so an audio buffer never reaches
swap, and its mixing path runs at real-time priority; a machine that refuses
either is told so on the log and served anyway.

## Accounting

A running stream with nothing queued contributes silence for exactly the
frames it missed and takes the under-run — the device would otherwise run dry,
which is strictly worse and is what its driver would report instead. A
*draining* stream is different: the pump emits only the frames it genuinely
has, so a drain's tail is a short chunk rather than a padded one. The position
never lies, so a client resynchronises exactly rather than drifting.

## Authority

* **Playback needs no capability.** The authorisation is that the caller's
  session holds the sink's seat lease, decided by the one routing policy
  against the kernel-attested caller. A capability every program would hold is
  not a boundary.
* **Capture demands `CAP_AUDIO_CAPTURE`**, read from the caller's attested
  capability summary at stream open and never from anything the caller said.
* **Adopting a driver's channel demands `CAP_DRV_LOAD`.** The authority to put
  a driver on the machine is exactly the authority to tell the mixer about
  one, so no third capability is minted: `devmgr` holds it, and an ordinary
  program cannot reach the operation at all.

A stream id is a service-issued token checked against the attested pid, so a
guessed id reaches nothing. Every capture grant **and refusal**, every device
bound or lost, and every default-device change lands on the hash-chained audit
log with a stable event id — "who tried" is the question an incident asks.

## Not yet

Sinks are leased to seats by the seat integration (`plans/SOUND.md` SND13);
until it lands no sink is claimed, which is the router's own headless case, so
any principal may play on an unclaimed sink. A stream's gain is therefore
always the software multiply: the device's own control belongs to the *sink*,
and the per-sink volume surface arrives with the desktop integration.

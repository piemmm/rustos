# Accelerator drivers

An accelerator is a device that performs bounded units of work off-CPU on the
driver's behalf: a symmetric-cipher offload, an NPU, a media engine. What
distinguishes the class from every other driver class is that it **computes
rather than moves** — it neither presents pixels, carries frames, nor
addresses blocks.

The class trait is `tairix_abi::driver::accelerator::Accelerator`; concrete
drivers live under `drivers/accelerator/<leaf>/`. The staged design context is
`plans/NEW-SWITCHBOARD.md` (the accelerator pane the class exists to give a
device to describe).

## What the class is, and what it is not

The class surface is deliberately two methods:

| Method          | Purpose                                            | Capability gate          |
|-----------------|----------------------------------------------------|--------------------------|
| `device_report` | what the device owns, offers, and will carry       | `DriverHandle` ownership |
| `cipher`        | run one symmetric-cipher job to completion         | `DriverHandle` ownership |

`cipher` is the only work operation because a symmetric cipher is the only
workload family a device in this tree offers. A device that offers another
family — a hash, an inference, a video frame — brings that family's operation
with it, and the vocabulary evolves in place. Publishing an operation ahead of
a device that performs it and a consumer that calls it would be an interface
with no producer.

The algorithm set is read *from the device* rather than assumed
(`AcceleratorDeviceReport::ciphers`), so a device offering none refuses the
call closed instead of accepting work it cannot do.

## Keys belong to the caller

A `CipherJob` carries its key by reference for the duration of the call, and
nothing about the job outlives it. The driver stages the key into
device-visible memory to create the device's session and scrubs that staging
when the session is destroyed; it keeps no copy of its own.

That is why the virtio-crypto driver creates a session **per job**. A session
is the device's cached key schedule, and reusing one across jobs would mean
retaining the caller's key to compare the next job's against — and a retained
key is a key a compromised driver can be made to use again. The cost is two
extra control-queue round trips; the guarantee is that no key material
outlives the call that supplied it, on either side of the device boundary.

A consumer that needs the throughput needs a caller-owned session handle on
the class trait. That is a different interface and arrives with that consumer.

## A job is refused, never split

`AcceleratorDeviceReport::max_job_bytes` publishes the smaller of the device's
own advertised per-request ceiling and the driver's staging bound, and a job
above it is refused with `LengthOutOfRange`.

The driver does not split it. Splitting a cipher-block-chaining job means
carrying one block's cipher text into the next job's initialisation vector,
which is the caller's chaining decision to make, not a transformation a driver
may apply silently. Publishing the ceiling is what makes that a figure to plan
against rather than a surprise.

## Discovery

An accelerator binds through the ordinary discovery-match path and never by
naming a part. Two production classifiers put such a device in the hardware
tree as `HwDeviceClass::Accelerator`:

- **PCI** — base class `0x12` ("Processing Accelerators"), in
  `lib/pci`'s `describe_function`. Before the class existed such a card was
  reported as `Other`, so nothing above discovery could tell an offload engine
  from an unmodelled device.
- **Device tree** — the three generic node names the devicetree specification
  offers for a device that computes rather than moves: `crypto`, `dsp` and
  `video-codec`, in `kernel/arch/api`'s `fdtwalk` classifier.

A node with no matching driver is left **unbound** and logged. That is a real
state, not an error and never a panic: discovery reports the node, the match
path finds no driver, and the device manager logs the skip.

## The first member: virtio-crypto

`drivers/accelerator/virtio_crypto` drives any virtio 1.x device reporting
virtio device type 20 (virtio 1.2 §5.9), on either transport — the bind key
names the virtio *type*, not a bus. Of the device's four service families it
implements cipher, and of the cipher algorithms it implements AES-CBC with a
128-, 192- or 256-bit key.

That is what QEMU's builtin cryptodev backend offers and therefore all the
driver can be proven against. An algorithm bit the driver does not recognise
is **ignored rather than offered**, so a job is never accepted and then failed
by the device.

### The device is untrusted

Every figure read from configuration space is hostile input:

- the advertised per-request ceiling is clamped before it sizes any
  allocation, so a device claiming a preposterous ceiling cannot make the
  driver demand an arbitrarily large DMA region at bring-up;
- an unrecognised algorithm bit is ignored;
- a device that reports itself not ready, offers no cipher service, offers no
  algorithm the driver implements, or advertises no data queue is refused at
  bring-up rather than driven.

Both the control and the data path decode their reply through one
`status_to_result`, so neither can classify an outcome the other would read
differently, and a status byte the ABI does not define fails closed rather
than reading as success.

### Two bounds on an unwell device

A job that is submitted and never answered releases its caller, because the
two failure shapes are different and neither bound catches the other:

- **Silence** — a per-job deadline, so a device whose completion interrupt is
  lost, coalesced or never raised fails the job `DeviceOffline` rather than
  parking the caller inside it for ever.
- **Noise** — a wake-count bound, so a stuck or mis-routed shared interrupt
  delivering wakes with no matching completion fails the job `DeviceFault`
  rather than spinning. No deadline would catch this, because each wake resets
  the wait.

A failed job still destroys its session, so a device is never left holding the
caller's key schedule, and the job's own error is what surfaces rather than
the cleanup's.

## Tests

The two halves prove different things, and both are needed:

- **Host tests** (`drivers/accelerator/virtio_crypto/src/tests.rs`) prove the
  *protocol* against a faithful in-process peer that decodes both frame
  layouts at the spec's offsets, keeps a session table, and refuses a frame
  the driver got wrong. Its payload transform is a non-cryptographic
  invertible mix: its job is to show the key, the initialisation vector, the
  direction and the payload all reached the device and came back.
- **The QEMU vertical**
  (`tests/integration/accel_virtio_crypto_qemu_aarch64`) proves the
  *arithmetic*, against a published oracle: the cipher text the real device
  produces must equal the NIST SP 800-38A F.2 AES-128-CBC known-answer vector
  byte for byte, and the decrypt must return the plain text. A protocol the
  mock accepts but the device does not cannot pass this, and a driver that
  bound the wrong direction into its session fails even though it produced
  bytes.

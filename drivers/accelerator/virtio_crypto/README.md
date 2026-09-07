# `drivers/accelerator/virtio_crypto`

The virtio-crypto accelerator driver: the first member of the accelerator
device class (`lib/abi/src/driver/accelerator.rs`).

Stability tier: **experimental**.

## Supported hardware

Any virtio 1.x device reporting virtio device type **20** (`virtio-crypto`,
virtio 1.2 §5.9), on either transport — the bind key names the virtio *type*,
not a bus, so the same driver binds the device whether it arrives on PCI or on
an MMIO slot. Exercised against QEMU's `virtio-crypto-pci` /
`virtio-crypto-device` backed by `cryptodev-backend-builtin`.

Of the device's four service families the driver implements **cipher**, and of
the cipher algorithms it implements **AES-CBC** with a 128-, 192- or 256-bit
key. That is what QEMU's builtin backend offers and therefore all this driver
can be proven against; an algorithm bit the driver does not recognise is
ignored rather than offered, so a job is never accepted and then failed by the
device. Hash, MAC and AEAD arrive with a device that offers them and a
consumer that needs them.

## Limitations

- **One job at a time.** Jobs are serialised by the owner, so exactly one
  chain is ever outstanding and the driver uses one data queue however many
  the device offers.
- **A job is refused, never split.** The device advertises its own per-request
  ceiling; the driver publishes the smaller of that and its own staging bound
  (`MAX_STAGED_JOB_BYTES`) as `AcceleratorDeviceReport::max_job_bytes`, and
  refuses a larger job. Splitting a cipher-block-chaining job means carrying
  one block's cipher text into the next job's initialisation vector, which is
  the caller's chaining decision, not a transformation a driver may apply
  silently.
- **A session per job.** virtio-crypto binds the key *and* the direction into
  a device-side session, which a throughput-minded driver would create once
  and reuse. This one creates it, runs the job and destroys it inside the
  single `cipher` call, because reuse would mean retaining the caller's key to
  compare the next job's against. The cost is two extra control-queue round
  trips; the guarantee is that no key material outlives the call that supplied
  it, on either side of the device boundary. A consumer that needs the
  throughput needs a caller-owned session handle on the class trait — a
  different interface, arriving with that consumer.
- **No device memory reported.** A virtio-crypto device works out of the
  driver's DMA staging, which is system RAM, so it owns none of its own and
  the report says `0` rather than inventing a figure.
- **Runtime unload** is supported: `close` resets the device, and the driver
  holds no state outside its own DMA staging.

## Required capabilities

- `CAP_DRV_LOAD` — checked in `register`.

The driver requests no `CAP_DRV_KERNEL`: it runs in user space. Its DMA and
register-window authority arrive as the resource grants its matched
hardware-tree node requested, minted by the driver-spawn path — never as
ambient authority.

## The device is untrusted

Every figure read from configuration space is hostile input. The advertised
per-request ceiling is clamped before it sizes any allocation; an unrecognised
algorithm bit is ignored; and a device that reports itself not ready, offers no
cipher service, offers no algorithm the driver implements, or advertises no
data queue is refused at bring-up rather than driven. Both the control and the
data path decode their reply through one `status_to_result`, and a status byte
this ABI does not define fails closed rather than reading as success.

## Tests

- `src/tests.rs` — the protocol against a faithful in-process peer that
  decodes both frame layouts at the spec's offsets, keeps a session table, and
  refuses a frame the driver got wrong. Its payload transform is a
  non-cryptographic invertible mix: its job is to prove the key, the
  initialisation vector, the direction and the payload all reached the device
  and came back, not to compute AES.
- `tests/integration/accel_virtio_crypto_qemu_aarch64` — the device's own
  claim, against real silicon behaviour: a QEMU `virtio-crypto-device` is
  driven through the signed `.rxe` load path and its output is compared with
  the NIST SP 800-38A F.2 AES-128-CBC known-answer vectors, then decrypted
  back. A protocol the mock accepts but the device does not cannot pass this.

# `tairix-drv-bus-i2c-bcm2835`

Autoloaded user-space I²C bus driver for the **Broadcom Serial Controller**
(BSC), the I²C master every Raspberry Pi generation exposes.

## Supported hardware

Device-tree `compatible = "brcm,bcm2835-i2c"`. Later Pi parts (BCM2711,
BCM2712) present a register-compatible controller and carry the same string,
so one driver covers the family. The driver binds only through the discovery
match (`BIND_KEYS`); it names no board and no address.

## Required capabilities

| Capability | Why |
|---|---|
| `CAP_DRV_LOAD` | the load-time gate every driver clears |
| `CAP_MMIO_MAP` | the controller's register window, from the matched node's grant |
| `CAP_IRQ_BIND` | the controller's interrupt line, which every transfer parks on |
| `CAP_IPC_BIND_PRIVILEGED` | binding each child's reserved transfer endpoint |

It holds **no** clock, filesystem, or network authority, and no capability
that would let it reach a device other than the one it was matched to.

## One endpoint per child, and why the address stays here

I²C has no enumeration protocol, and probing every address is forbidden and
can be destructive, so a bus's children are known only from the platform's
device tree. Discovery therefore splits each child in two: the **duty** goes
on this bus's node (a `BusChild` resource pairing an endpoint id with that
child's bus address) and the **authority** goes on the child's own node (a
plain endpoint grant naming the same id).

This driver binds one restricted-sender endpoint per duty. The transfer wire
frame carries no address at all — the address comes from the duty grant, on
the endpoint the request arrived on — so a chip driver has no field in which
it could name a neighbour, and a compromised one still reaches only its own
part. The kernel refuses the bind unless the duty grant covers the id, so a
second privileged driver cannot impersonate the bus to a chip either.

A duty whose address the tree spelled unusably, or whose endpoint the kernel
refused, leaves that child unserved and is logged; its chip driver then fails
closed rather than talking to the wrong part.

## Interrupt-driven, never a spin

A transfer arms the controller and parks on its bound interrupt line. The
FIFO-service and completion interrupts are what advance it. The per-phase
deadline exists only to catch a controller that has stopped answering: a
slave that legitimately stretches the clock is bounded by the controller's
own stretch-timeout register, which raises its own status bit long before the
deadline could fire. Recovery drops the enable bit — the only way to abort a
transfer in flight — then clears and re-enables, so a wedged transfer never
runs on under the next caller.

## Limitations

- **No true repeated START.** The BSC drops a STOP between the two phases of
  a write-then-read. On a single-master bus, which is what a board wiring an
  RTC to the Pi's BSC has, that is indistinguishable — nothing else can move
  a chip's register pointer between the phases. A **multi-master** bus is out
  of scope for this controller.
- **The clock divider is left as the firmware programmed it.** The bus speed
  is a property of the board's wiring and the slowest part on it, and nothing
  in discovery tells a driver either, so overriding it could clock a part past
  its rating. The transfer deadline is sized for a bus an order of magnitude
  slower than 100 kHz standard mode so a slow board is never abandoned.
- **A zero-length (address-only) transfer depends on the controller.** The
  seam admits one — it is how a caller asks whether a part answers at all —
  and this driver issues it as a zero-length write phase. A controller that
  never reports such a phase complete reaches the deadline and fails closed
  rather than reporting an answer it did not get. No in-tree caller uses it;
  every chip driver names a register.
- **10-bit addressing is not supported.** No part this class reaches uses it,
  and the address type refuses the escape prefix rather than mis-addressing
  the bus.
- **Runtime unload** takes the endpoints with it: the kernel revokes the
  per-endpoint grants when the driver exits, so a chip driver's next call
  fails closed rather than reaching a stale server.

## Testing

Host unit tests drive the controller against a **simulated BSC** that
intercepts every register access — the FIFO is a real queue, the status bits
change as they would on silicon, and the controller advances exactly where it
would raise its interrupt. They cover the two-phase register read, a transfer
longer than the 16-byte FIFO (which must park), an absent part, a part that
stops acknowledging part-way, a clock-stretch timeout, and a wedged
controller reaching its deadline.

**QEMU does not model the BSC**, so there is no integration test for this
driver: no emulated machine in the matrix presents a `brcm,bcm2835-i2c` node.
The protocol and every fail-closed path are host-proven and the live bus is
an on-metal acceptance item, exactly as for the rest of `plans/PI.md`'s
mailbox work.

## References

- BCM2835 ARM Peripherals, §3 (Broadcom Serial Controller).
- I²C-bus specification and user manual, NXP UM10204.
- `plans/TIMESYNC.md` TS-4 — the staged design this driver lands.

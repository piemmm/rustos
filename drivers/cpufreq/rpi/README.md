# `tairix-drv-cpufreq-rpi`

The Raspberry Pi CPU frequency driver: the `VideoCore` firmware's ARM core
clock, and the machine's frequency **mechanism** on that board
(`plans/CPUFREQ.md`).

On a Pi the ARM clock belongs to the firmware, not to any register the ARM
cores can reach, so this driver maps nothing and takes no interrupt — its only
path to the hardware is a property exchange with the `vcmailbox` service
driver, like the Pi's PMIC clock (`drivers/rtc/rpi`).

It exists because the firmware leaves the ARM clock wherever it last put it,
which after the boot window is `arm_freq_min` — 600 MHz on a Pi 4B whose parts
are rated at 1.5 GHz. Nothing raises it again unless an OS driver asks, so a
board with no frequency driver runs at 40% of its rated speed however much work
it has.

Two targets, one crate. `src/lib.rs` is the device logic — the driver identity
(`register` + bind table), the range discovery, and the rate exchanges over the
channel — host-tested against the protocol-faithful `lib/vcmailbox` mock
firmware. `src/main.rs` is the `Run` binary `devmgr` autoloads into user space:
it takes the mechanism role with `cpufreq_bind` and parks in `cpufreq_wait`
applying the kernel governor's targets.

## Supported hardware

Any `raspberrypi,firmware-clocks` node — the node through which the Pi's
firmware exposes its clocks. A board without one leaves the driver unbound,
which is logged and is not an error.

Both ends of the operating range come from the firmware
(`RPI_FIRMWARE_GET_MIN_CLOCK_RATE` / `..._MAX_CLOCK_RATE` on
`RPI_FIRMWARE_ARM_CLK_ID`), never from a board constant, so a board whose
`config.txt` raises `arm_freq` or lifts `arm_freq_min` is driven over the range
it actually has. Targets are asked for at 100 MHz intervals — the same
operating points the vendor's own Linux `cpufreq` driver builds.

## What it decides

Nothing. The kernel's governor watches the per-CPU idle transitions and the
program-launch path and publishes a target; this driver reports the range it
can deliver and applies what it is handed. Policy is kernel-side because it
must answer within the idle transition it is reacting to.

The rate it applied is not reported back. The firmware clamps a request to the
clock's range and rounds it to a rate the PLL can synthesise, so the request is
not the truth — and the kernel has a better witness than this driver's word for
it: the per-CPU estimator measures the live core clock from the silicon's own
counters, so a rate that never took effect shows up as a measured frequency
that does not match the target.

## Required capabilities

| Capability | Why |
|---|---|
| `CAP_DRV_LOAD` | checked by `register`, as every driver's load gate |
| `CAP_MAILBOX` | reach the `vcmailbox` service's call endpoint |
| `CAP_CPUFREQ` | take the machine's frequency mechanism role |

It holds no `CAP_MMIO_MAP` and no `CAP_IRQ_BIND`: it owns no register window
and no interrupt line.

## Limitations

* **Unloadable at runtime.** The kernel releases the mechanism role with the
  process, so a replacement can take it; the clock is left wherever the last
  applied rate put it.
* **Not exercisable under QEMU.** QEMU models no `VideoCore`, so the property
  exchanges are proven against the mock firmware and the live channel is the
  on-metal acceptance item.
* **One clock domain.** The Pi's four cores share one ARM clock, which is what
  the firmware exposes; a part with per-cluster clocks would need the kernel to
  carry more than one binding.

## Stability

`experimental`.

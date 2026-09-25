# `tairix-drv-bus-virtio` — the virtio bus driver crate

The virtio protocol — queues, the MMIO and PCI transports, DMA slabs and
bounce buffers — lives in `lib/virtio`, where every virtio class driver and
the kernel-side host consume it. This crate is the bus's driver-crate entry
point and re-exports the two transports (`MmioTransport`, `PciTransport`)
and the PCI common-configuration table (`transport_pci`) for the kernel-side
consumers that bind a transport through it.

## Supported hardware

Hardware-agnostic: a transport runs over the kernel-minted register window of
whatever virtio-mmio slot or virtio-pci function discovery found.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time. It never asserts `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p tairix-drv-bus-virtio` checks `register`'s capability gate;
the protocol's tests live in `lib/virtio`.

## Public surface

The only public function is `register`; the re-exports are types.

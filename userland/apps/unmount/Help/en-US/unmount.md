## NAME

unmount — detach a mounted volume

## SYNOPSIS

`unmount [option...] name`

## DESCRIPTION

Takes the volume mounted under `name` out of service: the filesystem
and the device are flushed, the mount under `/Storage` is retracted,
and the volume's durable `id::` root is withdrawn. `name` is the
volume's catalog name (`usb1`) or its mount-point path
(`/Storage/usb1`), matched against the System Information API's mount
listing.

A volume whose device was removed while it still held uncommitted
writes stays visible as `unavailable-dirty` (or `unavailable-lost`)
in the mount listing, and a plain `unmount` refuses: its retained
data is held for a verified re-insert. `--force` is the deliberate
exit — the retained data is discarded, the volume is retracted, and
the loss is recorded in the audit log. On a healthy volume `--force`
still flushes and detaches cleanly; nothing is discarded when a
clean commit is possible.

Detaching requires the mount authority (`CAP_FS_MOUNT`); the kernel
checks it and audits every decision. The permanent boot volumes and
the system's view bindings are not detachable.

## OPTIONS

- `-f, --force` — force-unmount: retract the volume even when its
  uncommitted data cannot be committed, discarding the retained data.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `unmount usb1` — cleanly detach the volume mounted as `usb1`.
- `unmount /Storage/usb1` — the same, named by its mount point.
- `unmount --force usb1` — retract an unavailable volume, discarding
  its retained uncommitted data.

## EXIT STATUS

- `0` — the volume was detached (or the short help was written).
- `1` — the volume was not found, was not detachable, or the kernel
  refused the detach.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `mount`
- `df`
- `man`

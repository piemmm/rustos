# TAIRiX development headers

C-language development headers for the TAIRiX ABI. These let a program
written in a language other than Rust (C, C++, anything with a C FFI) call
the TAIRiX kernel/user interface.

The surface is split into one header per `lib/abi` module so a developer can
pull in exactly what they need, plus an umbrella that includes them all:

- `tairix/tairix_abi.h` — umbrella header: the ABI version plus an `#include`
  of every module header below.
- `tairix/tairix_error.h` — the stable error codes (`TAIRIX_E_*`).
- `tairix/tairix_capability.h` — the capability identifiers (`TAIRIX_CAP_*`).
- `tairix/tairix_time.h` — the 64-bit-native time types (`tairix_time64_t`,
  `tairix_duration64_t`) and their constants.
- `tairix/tairix_random.h` — the random-request flags (`TAIRIX_RANDOM_FLAG_*`)
  and the per-request byte limits (`TAIRIX_RANDOM_*_BYTES`).
- `tairix/tairix_ipc.h` — the IPC message header and port-name wire types
  (`tairix_ipc_message_header_t`, `tairix_port_name_t`) and their constants
  (`TAIRIX_IPC_*`, `TAIRIX_PORT_NAME_*`).
- `tairix/tairix_stdinfo.h` — the Standard Information Stream ABI: the
  reserved file descriptor (`TAIRIX_STDINFO_FD`), the framing version tags
  (`TAIRIX_STDINFO_VERSION_*`), and the record-kind and severity discriminants
  (`TAIRIX_STDINFO_KIND_*`, `TAIRIX_STDINFO_SEVERITY_*`).
- `tairix/tairix_manifest.h` — the signed `rxe` manifest header
  (`tairix_manifest_header_t`) and its constants (`TAIRIX_MANIFEST_*`,
  `TAIRIX_SYSCALL_TABLE_HASH_LEN`).
- `tairix/tairix_input.h` — the desktop pointer and keyboard input records:
  their magics and wire sizes, the record-kind / button / key-class / modifier
  field codes (`TAIRIX_INPUT_KIND_*`, `TAIRIX_INPUT_BUTTON_NONE`, `TAIRIX_KEY_CLASS_*`,
  `TAIRIX_MOD_*`), and the pointer-button and named-key discriminants
  (`TAIRIX_POINTER_BUTTON_*`, `TAIRIX_KEY_*`).
- `tairix/tairix_appinfo.h` — the application-bundle manifest header
  (`tairix_appinfo_header_t`) and its constants (`TAIRIX_APPINFO_*`, `TAIRIX_BUNDLE_*`,
  `TAIRIX_MIME_*`), the curated shared-library directory
  (`TAIRIX_SYSTEM_LIBRARIES_DIR`), the permitted bundle top-level entry names
  (`TAIRIX_BUNDLE_ENTRY_*`), and the library-scope discriminants
  (`TAIRIX_LIBRARY_SCOPE_*`).
- `tairix/tairix_rxe.h` — the `rxe` load-image header (`tairix_load_header_t`)
  and its constants and load-time hardening codes (`TAIRIX_LOAD_MAGIC`,
  `TAIRIX_RXE_PAGE_SIZE`, `TAIRIX_LOAD_MAX_SEGMENTS`, `TAIRIX_LOAD_FLAG_PIE`,
  `TAIRIX_SEG_FLAG_*`, `*_WIRE_LEN`), and the segment-permission discriminants
  (`TAIRIX_RXE_PERMISSION_*`).
- `tairix/tairix_process.h` — the process startup vector the kernel hands a
  freshly spawned program: the block header and string-slot wire types
  (`tairix_process_start_header_t`, `tairix_string_slot_t`), the magic
  (`TAIRIX_PROCESS_START_MAGIC`), the size limits (`TAIRIX_PROCESS_START_MAX_*`), and
  the packed `*_WIRE_LEN` sizes.
- `tairix/tairix_sysinfo.h` — the System Information API wire types
  (`tairix_sysinfo_request_header_t`, `tairix_process_list_request_t`,
  `tairix_process_record_t`, `tairix_kernel_memory_stats_t`, `tairix_uptime_t`,
  `tairix_system_identity_t`, `tairix_mount_list_request_t`, `tairix_mount_record_t`),
  the framing / query-id / registry constants (`TAIRIX_SYSINFO_*`), the
  process-state discriminants (`TAIRIX_PROCESS_STATE_*`), the inline-buffer caps
  (`TAIRIX_PROCESS_NAME_MAX`, `TAIRIX_MACHINE_ID_LEN`, `TAIRIX_HOSTNAME_MAX`,
  `TAIRIX_MOUNT_*_MAX`), and the per-record `*_WIRE_LEN` sizes.
- `tairix/tairix_driver.h` — the driver-class ABI. The core: the signed
  driver manifest header (`tairix_driver_manifest_t`) and its constants
  (`TAIRIX_DRIVER_MANIFEST_*`, `TAIRIX_DRIVER_SIGNER_PUBKEY_LEN`,
  `TAIRIX_DRIVER_SIGNATURE_LEN`), the driver-kind and buffer-class discriminants
  (`TAIRIX_DRIVER_KIND_*`, `TAIRIX_BUFFER_CLASS_*`), the driver-ABI error codes
  (`TAIRIX_DRIVER_ERROR_*`), and the no-handle sentinel
  (`TAIRIX_DRIVER_HANDLE_NONE`). Plus the driver-class POD types: the
  storage/bus/display/filesystem/input/net structs (`tairix_block_geometry_t`,
  `tairix_discard_capability_t`, `tairix_health_snapshot_t`, `tairix_bus_device_t`,
  `tairix_display_mode_t`, `tairix_accel_caps_t`, `tairix_node_info_t`,
  `tairix_dir_entry_t`, `tairix_node_times_t`, `tairix_input_event_t`,
  `tairix_mac_address_t`), the `TAIRIX_VIRTIO_PCI_*` / `TAIRIX_MAC_ADDRESS_LEN` /
  `TAIRIX_MOUNT_FLAG_*` / `TAIRIX_NODE_ID_NONE` constants, and the
  `TAIRIX_DISPLAY_FORMAT_*` / `TAIRIX_NODE_KIND_*` / `TAIRIX_INPUT_EVENT_KIND_*`
  discriminants.
- `tairix/tairix_syscall.h` — the syscall numbers (`TAIRIX_SYS_*`) and a
  prototype for each syscall entry point.

This set now covers the whole `lib/abi` public `#[repr(C)]` type surface (a
generator completeness test pins every such type's size/align and that it has
a C `typedef`). The remaining C-ABI work — the `tairix_sys_*` trap-stub runtime
and crt0 — is staged in `plans/CCOMPAT.md` (stages CC2+).

## These files are generated

Every header here is **generated** from the single source of truth in
`lib/abi` (`AGENTS.md` §2.2, §9). Do not edit them by hand. To regenerate
after changing `lib/abi`:

```
cargo xtask c-header --write
```

`cargo xtask ci` runs `cargo xtask c-header` (no `--write`), which fails
closed if a committed header has drifted from `lib/abi`.

## Calling convention

Each syscall is exported by the user-space stub library under the symbol
`tairix_sys_<name>` (for example `tairix_sys_ipc_send`). The stub library
implements each entry with an explicit `#[export_name = "tairix_sys_<name>"]`
so the Rust compiler does not mangle the symbol. The short `tairix_` / `TAIRIX_`
prefix namespaces the C-visible surface so it survives C's single flat symbol
namespace (`AGENTS.md` §9). Link against that library and include
`tairix/tairix_abi.h` to call the kernel.

`abi-v1` is not frozen yet; once it is released its layout, numbers, and
symbol names become immutable and new behaviour ships as `abi-v2`.

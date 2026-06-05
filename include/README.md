# RustOS development headers

C-language development headers for the RustOS ABI. These let a program
written in a language other than Rust (C, C++, anything with a C FFI) call
the RustOS kernel/user interface.

The surface is split into one header per `lib/abi` module so a developer can
pull in exactly what they need, plus an umbrella that includes them all:

- `rustos/rustos_abi.h` — umbrella header: the ABI version plus an `#include`
  of every module header below.
- `rustos/rustos_error.h` — the stable error codes (`ROS_E_*`).
- `rustos/rustos_capability.h` — the capability identifiers (`ROS_CAP_*`).
- `rustos/rustos_time.h` — the 64-bit-native time types (`ros_time64_t`,
  `ros_duration64_t`) and their constants.
- `rustos/rustos_random.h` — the random-request flags (`ROS_RANDOM_FLAG_*`)
  and the per-request byte limits (`ROS_RANDOM_*_BYTES`).
- `rustos/rustos_ipc.h` — the IPC message header and port-name wire types
  (`ros_ipc_message_header_t`, `ros_port_name_t`) and their constants
  (`ROS_IPC_*`, `ROS_PORT_NAME_*`).
- `rustos/rustos_stdinfo.h` — the Standard Information Stream ABI: the
  reserved file descriptor (`ROS_STDINFO_FD`), the framing version tags
  (`ROS_STDINFO_VERSION_*`), and the record-kind and severity discriminants
  (`ROS_STDINFO_KIND_*`, `ROS_STDINFO_SEVERITY_*`).
- `rustos/rustos_manifest.h` — the signed `rxe` manifest header
  (`ros_manifest_header_t`) and its constants (`ROS_MANIFEST_*`,
  `ROS_SYSCALL_TABLE_HASH_LEN`).
- `rustos/rustos_input.h` — the desktop pointer and keyboard input records:
  their magics and wire sizes, the record-kind / button / key-class / modifier
  field codes (`ROS_INPUT_KIND_*`, `ROS_INPUT_BUTTON_NONE`, `ROS_KEY_CLASS_*`,
  `ROS_MOD_*`), and the pointer-button and named-key discriminants
  (`ROS_POINTER_BUTTON_*`, `ROS_KEY_*`).
- `rustos/rustos_appinfo.h` — the application-bundle manifest header
  (`ros_appinfo_header_t`) and its constants (`ROS_APPINFO_*`, `ROS_BUNDLE_*`,
  `ROS_MIME_*`), the curated shared-library directory
  (`ROS_SYSTEM_LIBRARIES_DIR`), the permitted bundle top-level entry names
  (`ROS_BUNDLE_ENTRY_*`), and the library-scope discriminants
  (`ROS_LIBRARY_SCOPE_*`).
- `rustos/rustos_rxe.h` — the `rxe` load-image header (`ros_load_header_t`)
  and its constants and load-time hardening codes (`ROS_LOAD_MAGIC`,
  `ROS_RXE_PAGE_SIZE`, `ROS_LOAD_MAX_SEGMENTS`, `ROS_LOAD_FLAG_PIE`,
  `ROS_SEG_FLAG_*`, `*_WIRE_LEN`), and the segment-permission discriminants
  (`ROS_RXE_PERMISSION_*`).
- `rustos/rustos_process.h` — the process startup vector the kernel hands a
  freshly spawned program: the block header and string-slot wire types
  (`ros_process_start_header_t`, `ros_string_slot_t`), the magic
  (`ROS_PROCESS_START_MAGIC`), the size limits (`ROS_PROCESS_START_MAX_*`), and
  the packed `*_WIRE_LEN` sizes.
- `rustos/rustos_sysinfo.h` — the System Information API wire types
  (`ros_sysinfo_request_header_t`, `ros_process_list_request_t`,
  `ros_process_record_t`, `ros_kernel_memory_stats_t`, `ros_uptime_t`,
  `ros_system_identity_t`, `ros_mount_list_request_t`, `ros_mount_record_t`),
  the framing / query-id / registry constants (`ROS_SYSINFO_*`), the
  process-state discriminants (`ROS_PROCESS_STATE_*`), the inline-buffer caps
  (`ROS_PROCESS_NAME_MAX`, `ROS_MACHINE_ID_LEN`, `ROS_HOSTNAME_MAX`,
  `ROS_MOUNT_*_MAX`), and the per-record `*_WIRE_LEN` sizes.
- `rustos/rustos_driver.h` — the driver-class ABI. The core: the signed
  driver manifest header (`ros_driver_manifest_t`) and its constants
  (`ROS_DRIVER_MANIFEST_*`, `ROS_DRIVER_SIGNER_PUBKEY_LEN`,
  `ROS_DRIVER_SIGNATURE_LEN`), the driver-kind and buffer-class discriminants
  (`ROS_DRIVER_KIND_*`, `ROS_BUFFER_CLASS_*`), the driver-ABI error codes
  (`ROS_DRIVER_ERROR_*`), and the no-handle sentinel
  (`ROS_DRIVER_HANDLE_NONE`). Plus the driver-class POD types: the
  storage/bus/display/filesystem/input/net structs (`ros_block_geometry_t`,
  `ros_discard_capability_t`, `ros_health_snapshot_t`, `ros_bus_device_t`,
  `ros_display_mode_t`, `ros_accel_caps_t`, `ros_node_info_t`,
  `ros_dir_entry_t`, `ros_node_times_t`, `ros_input_event_t`,
  `ros_mac_address_t`), the `ROS_VIRTIO_PCI_*` / `ROS_MAC_ADDRESS_LEN` /
  `ROS_MOUNT_FLAG_*` / `ROS_NODE_ID_NONE` constants, and the
  `ROS_DISPLAY_FORMAT_*` / `ROS_NODE_KIND_*` / `ROS_INPUT_EVENT_KIND_*`
  discriminants.
- `rustos/rustos_syscall.h` — the syscall numbers (`ROS_SYS_*`) and a
  prototype for each syscall entry point.

This set now covers the whole `lib/abi` public `#[repr(C)]` type surface (a
generator completeness test pins every such type's size/align and that it has
a C `typedef`). The remaining C-ABI work — the `ros_sys_*` trap-stub runtime
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
`ros_sys_<name>` (for example `ros_sys_ipc_send`). The stub library
implements each entry with an explicit `#[export_name = "ros_sys_<name>"]`
so the Rust compiler does not mangle the symbol. The short `ros_` / `ROS_`
prefix namespaces the C-visible surface so it survives C's single flat symbol
namespace (`AGENTS.md` §9). Link against that library and include
`rustos/rustos_abi.h` to call the kernel.

`abi-v1` is not frozen yet; once it is released its layout, numbers, and
symbol names become immutable and new behaviour ships as `abi-v2`.

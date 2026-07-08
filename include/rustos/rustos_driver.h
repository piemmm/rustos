/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Driver-class ABI core: manifest, kinds, errors (AGENTS.md sec.8, sec.9).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_DRIVER_H
#define ROS_DRIVER_H

#include <stdint.h>
#include "rustos_hwtree.h"
#include "rustos_manifest.h"
#include "rustos_time.h"

/* Magic word identifying an abi-v1 driver manifest ("DRV1" little-endian). */
#define ROS_DRIVER_MANIFEST_MAGIC 0x31565244u
/* Maximum number of capability identifiers a driver manifest may request. */
#define ROS_DRIVER_MANIFEST_MAX_CAPABILITIES 64u
/* Maximum number of bind-table entries a driver manifest may declare. */
#define ROS_DRIVER_MANIFEST_MAX_BIND_KEYS 16u
/* Length, in bytes, of the Ed25519 signer public key. */
#define ROS_DRIVER_SIGNER_PUBKEY_LEN 32u
/* Length, in bytes, of the Ed25519 manifest signature. */
#define ROS_DRIVER_SIGNATURE_LEN 64u
/* Packed little-endian wire size of a driver manifest, in bytes. */
#define ROS_DRIVER_MANIFEST_WIRE_LEN 140u
/* Packed little-endian wire size of one bind-table entry, in bytes. */
#define ROS_DRIVER_BIND_KEY_WIRE_LEN 80u

/* Magic word identifying an abi-v1 driver register reply ("DRR1" little-endian). */
#define ROS_DRIVER_REGISTER_REPLY_MAGIC 0x31525244u
/* `status` value of a successful register reply; any other value is a
 * ROS_DRIVER_ERROR_* code. */
#define ROS_DRIVER_REGISTER_STATUS_OK ((int32_t)0)
/* Packed little-endian wire size of a driver register reply, in bytes. */
#define ROS_DRIVER_REGISTER_REPLY_WIRE_LEN 24u

/* Driver execution domain (uint8_t); IN_KERNEL additionally needs CAP_DRV_KERNEL. */
#define ROS_DRIVER_KIND_USER_SPACE ((uint8_t)0u)
#define ROS_DRIVER_KIND_IN_KERNEL ((uint8_t)1u)

/* Payload sensitivity hint (uint8_t); SENSITIVE requires zero-on-free. */
#define ROS_BUFFER_CLASS_NON_SENSITIVE ((uint8_t)0u)
#define ROS_BUFFER_CLASS_SENSITIVE ((uint8_t)1u)

/* Sentinel "no driver handle"; a live handle travels as a uint64_t. */
#define ROS_DRIVER_HANDLE_NONE ((uint64_t)0ull)

/* Stable driver-ABI error codes (int32_t), disjoint from ROS_E_* errno. */
#define ROS_DRIVER_ERROR_BUFFER_TOO_SMALL ((int32_t)1)
#define ROS_DRIVER_ERROR_BAD_MAGIC ((int32_t)2)
#define ROS_DRIVER_ERROR_ABI_VERSION_UNSUPPORTED ((int32_t)3)
#define ROS_DRIVER_ERROR_LENGTH_OUT_OF_RANGE ((int32_t)4)
#define ROS_DRIVER_ERROR_OUT_OF_RANGE ((int32_t)5)
#define ROS_DRIVER_ERROR_PERMISSION_DENIED ((int32_t)6)
#define ROS_DRIVER_ERROR_NOT_FOUND ((int32_t)7)
#define ROS_DRIVER_ERROR_SIGNATURE_INVALID ((int32_t)8)
#define ROS_DRIVER_ERROR_UNSUPPORTED ((int32_t)9)
#define ROS_DRIVER_ERROR_DEVICE_FAULT ((int32_t)10)
#define ROS_DRIVER_ERROR_BUSY ((int32_t)11)
#define ROS_DRIVER_ERROR_NOT_IMPLEMENTED ((int32_t)12)
#define ROS_DRIVER_ERROR_NO_SPACE ((int32_t)13)

/* PCI vendor ID assigned to virtio devices (uint16_t; virtio 1.1 sec.4.1.2). */
#define ROS_VIRTIO_PCI_VENDOR_ID ((uint16_t)0x1af4u)
/* virtio PCI capability `cfg_type` values (uint8_t; virtio 1.1 sec.4.1.4). */
#define ROS_VIRTIO_PCI_CFG_COMMON ((uint8_t)1u)
#define ROS_VIRTIO_PCI_CFG_NOTIFY ((uint8_t)2u)
#define ROS_VIRTIO_PCI_CFG_ISR ((uint8_t)3u)
#define ROS_VIRTIO_PCI_CFG_DEVICE ((uint8_t)4u)
#define ROS_VIRTIO_PCI_CFG_PCI ((uint8_t)5u)

/* Length, in bytes, of an Ethernet MAC address. */
#define ROS_MAC_ADDRESS_LEN 6u

/* Mount-flag bitmap (uint32_t); any bit outside KNOWN_MASK is reserved and rejected. */
#define ROS_MOUNT_FLAG_READ_ONLY ((uint32_t)0x1u)
#define ROS_MOUNT_FLAG_NOSUID ((uint32_t)0x2u)
#define ROS_MOUNT_FLAG_NODEV ((uint32_t)0x4u)
#define ROS_MOUNT_FLAG_NOEXEC ((uint32_t)0x8u)
#define ROS_MOUNT_FLAG_KNOWN_MASK ((uint32_t)0xfu)

/* Sentinel "no node"; a live NodeId travels as a uint64_t. */
#define ROS_NODE_ID_NONE ((uint64_t)0ull)

/* Display pixel encoding (uint8_t); named by the byte order of the first pixel. */
#define ROS_DISPLAY_FORMAT_RGBA8888 ((uint8_t)1u)
#define ROS_DISPLAY_FORMAT_BGRA8888 ((uint8_t)2u)

/* Filesystem node kind (uint8_t). */
#define ROS_NODE_KIND_DIRECTORY ((uint8_t)0u)
#define ROS_NODE_KIND_REGULAR_FILE ((uint8_t)1u)

/* Driver input-event kind (uint8_t); distinct from the windowing ROS_INPUT_KIND_*. */
#define ROS_INPUT_EVENT_KIND_KEY ((uint8_t)1u)
#define ROS_INPUT_EVENT_KIND_POINTER ((uint8_t)2u)
#define ROS_INPUT_EVENT_KIND_SCROLL ((uint8_t)3u)

/* Signed driver-manifest prefix; encoded little-endian on the wire. */
typedef struct ros_driver_manifest {
    uint32_t magic;
    uint32_t abi_version;
    uint8_t kind;
    uint8_t bind_key_count;
    uint16_t capability_count;
    uint8_t syscall_table_hash[ROS_SYSCALL_TABLE_HASH_LEN];
    uint8_t signer_pubkey[ROS_DRIVER_SIGNER_PUBKEY_LEN];
    uint8_t signature[ROS_DRIVER_SIGNATURE_LEN];
} ros_driver_manifest_t;

/* One bind-table entry: a hardware-tree match key plus the manifest's
 * bind priority (AGENTS.md sec.18.3). bind_key_count entries follow the
 * capability body; all are covered by the manifest signature. */
typedef struct ros_driver_bind_key {
    uint16_t priority;
    uint16_t reserved0;
    ros_hw_match_key_t key;
} ros_driver_bind_key_t;

/* Outcome of a spawned driver process's register() entry, sent to the
 * driver host over IPC; encoded little-endian on the wire. `status` is
 * ROS_DRIVER_REGISTER_STATUS_OK or a ROS_DRIVER_ERROR_* code; `handle` is
 * non-zero exactly when `status` is OK (informational only — the host
 * mints its own unforgeable handle). */
typedef struct ros_driver_register_reply {
    uint32_t magic;
    uint32_t abi_version;
    int32_t status;
    uint32_t reserved0;
    uint64_t handle;
} ros_driver_register_reply_t;

/* Block-device geometry (the drivers/storage class). */
typedef struct ros_block_geometry {
    uint32_t block_size;
    uint64_t block_count;
} ros_block_geometry_t;

/* Discard (TRIM/unmap) capability a block device reports. */
typedef struct ros_discard_capability {
    uint8_t supported;
    uint64_t granularity_blocks;
    uint64_t max_blocks_per_request;
} ros_discard_capability_t;

/* Point-in-time device-health snapshot (SMART / NVMe telemetry). */
typedef struct ros_health_snapshot {
    uint64_t power_on_hours;
    uint64_t unsafe_shutdowns;
    uint64_t media_errors;
    uint64_t reallocated_sectors;
    uint64_t pending_sectors;
    uint64_t uncorrectable_sectors;
    uint64_t crc_errors;
    uint16_t percentage_used;
    uint16_t available_spare;
    uint16_t temperature_kelvin;
    uint8_t critical_warning;
} ros_health_snapshot_t;

/* Identifying tuple for a discovered device (the drivers/bus class).
* `device_class` mirrors the Rust `class` field (renamed for C++). */
typedef struct ros_bus_device {
    uint32_t vendor;
    uint32_t device;
    uint16_t device_class;
    uint16_t reserved0;
    uint64_t address;
} ros_bus_device_t;

/* Active display mode (the drivers/display class); `format` is a
* ROS_DISPLAY_FORMAT_*. */
typedef struct ros_display_mode {
    uint32_t width_px;
    uint32_t height_px;
    uint32_t stride_bytes;
    uint8_t format;
} ros_display_mode_t;

/* What a hardware compositor back-end can do this frame. */
typedef struct ros_accel_caps {
    uint32_t max_layers;
    uint32_t max_width_px;
    uint32_t max_height_px;
    uint8_t per_layer_opacity;
} ros_accel_caps_t;

/* Structural metadata about a filesystem node; `kind` is a ROS_NODE_KIND_*. */
typedef struct ros_node_info {
    uint8_t kind;
    uint64_t size;
    uint64_t allocated;
} ros_node_info_t;

/* One directory entry; `node` is a NodeId (uint64_t). The entry carries the
* child's full ros_node_info_t and the opaque cursor that resumes the
* listing after it (pass it back to read_dir; 0 starts a listing). */
typedef struct ros_dir_entry {
    uint64_t node;
    ros_node_info_t info;
    uintptr_t name_len;
    uint64_t next_cursor;
} ros_dir_entry_t;

/* The four AGENTS.md sec.21 timestamps stored for a filesystem node. */
typedef struct ros_node_times {
    ros_time64_t created;
    ros_time64_t modified;
    ros_time64_t accessed;
    ros_time64_t changed;
} ros_node_times_t;

/* A mounted volume's space accounting, in whole blocks of block_size bytes.
* avail_blocks <= free_blocks <= total_blocks always holds; files/files_free
* are 0 when the format tracks no fixed inode table. */
typedef struct ros_volume_stats {
    uint32_t block_size;
    uint32_t reserved0;
    uint64_t total_blocks;
    uint64_t free_blocks;
    uint64_t avail_blocks;
    uint64_t files;
    uint64_t files_free;
} ros_volume_stats_t;

/* A single input event; `kind` is a ROS_INPUT_EVENT_KIND_*. */
typedef struct ros_input_event {
    uint8_t kind;
    uint8_t reserved0;
    uint16_t code;
    int32_t value;
} ros_input_event_t;

/* A 48-bit IEEE 802 link-layer address (the drivers/network class). */
typedef struct ros_mac_address {
    uint8_t octets[ROS_MAC_ADDRESS_LEN];
} ros_mac_address_t;

#endif /* ROS_DRIVER_H */

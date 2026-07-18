/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Driver-class ABI core: manifest, kinds, errors (AGENTS.md sec.8, sec.9).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_DRIVER_H
#define TAIRIX_DRIVER_H

#include <stdint.h>
#include "tairix_hwtree.h"
#include "tairix_manifest.h"
#include "tairix_time.h"

/* Magic word identifying an abi-v1 driver manifest ("DRV1" little-endian). */
#define TAIRIX_DRIVER_MANIFEST_MAGIC 0x31565244u
/* Maximum number of capability identifiers a driver manifest may request. */
#define TAIRIX_DRIVER_MANIFEST_MAX_CAPABILITIES 64u
/* Maximum number of bind-table entries a driver manifest may declare. */
#define TAIRIX_DRIVER_MANIFEST_MAX_BIND_KEYS 16u
/* Length, in bytes, of the Ed25519 signer public key. */
#define TAIRIX_DRIVER_SIGNER_PUBKEY_LEN 32u
/* Length, in bytes, of the Ed25519 manifest signature. */
#define TAIRIX_DRIVER_SIGNATURE_LEN 64u
/* Packed little-endian wire size of a driver manifest, in bytes. */
#define TAIRIX_DRIVER_MANIFEST_WIRE_LEN 140u
/* Packed little-endian wire size of one bind-table entry, in bytes. */
#define TAIRIX_DRIVER_BIND_KEY_WIRE_LEN 80u

/* Magic word identifying an abi-v1 driver register reply ("DRR1" little-endian). */
#define TAIRIX_DRIVER_REGISTER_REPLY_MAGIC 0x31525244u
/* `status` value of a successful register reply; any other value is a
 * TAIRIX_DRIVER_ERROR_* code. */
#define TAIRIX_DRIVER_REGISTER_STATUS_OK ((int32_t)0)
/* Packed little-endian wire size of a driver register reply, in bytes. */
#define TAIRIX_DRIVER_REGISTER_REPLY_WIRE_LEN 24u

/* Driver execution domain (uint8_t); IN_KERNEL additionally needs CAP_DRV_KERNEL. */
#define TAIRIX_DRIVER_KIND_USER_SPACE ((uint8_t)0u)
#define TAIRIX_DRIVER_KIND_IN_KERNEL ((uint8_t)1u)

/* Payload sensitivity hint (uint8_t); SENSITIVE requires zero-on-free. */
#define TAIRIX_BUFFER_CLASS_NON_SENSITIVE ((uint8_t)0u)
#define TAIRIX_BUFFER_CLASS_SENSITIVE ((uint8_t)1u)

/* Sentinel "no driver handle"; a live handle travels as a uint64_t. */
#define TAIRIX_DRIVER_HANDLE_NONE ((uint64_t)0ull)

/* Stable driver-ABI error codes (int32_t), disjoint from TAIRIX_E_* errno. */
#define TAIRIX_DRIVER_ERROR_BUFFER_TOO_SMALL ((int32_t)1)
#define TAIRIX_DRIVER_ERROR_BAD_MAGIC ((int32_t)2)
#define TAIRIX_DRIVER_ERROR_ABI_VERSION_UNSUPPORTED ((int32_t)3)
#define TAIRIX_DRIVER_ERROR_LENGTH_OUT_OF_RANGE ((int32_t)4)
#define TAIRIX_DRIVER_ERROR_OUT_OF_RANGE ((int32_t)5)
#define TAIRIX_DRIVER_ERROR_PERMISSION_DENIED ((int32_t)6)
#define TAIRIX_DRIVER_ERROR_NOT_FOUND ((int32_t)7)
#define TAIRIX_DRIVER_ERROR_SIGNATURE_INVALID ((int32_t)8)
#define TAIRIX_DRIVER_ERROR_UNSUPPORTED ((int32_t)9)
#define TAIRIX_DRIVER_ERROR_DEVICE_FAULT ((int32_t)10)
#define TAIRIX_DRIVER_ERROR_BUSY ((int32_t)11)
#define TAIRIX_DRIVER_ERROR_NOT_IMPLEMENTED ((int32_t)12)
#define TAIRIX_DRIVER_ERROR_NO_SPACE ((int32_t)13)
#define TAIRIX_DRIVER_ERROR_SEAT_REVOKED ((int32_t)14)
#define TAIRIX_DRIVER_ERROR_ENDPOINT_STALLED ((int32_t)15)

/* PCI vendor ID assigned to virtio devices (uint16_t; virtio 1.1 sec.4.1.2). */
#define TAIRIX_VIRTIO_PCI_VENDOR_ID ((uint16_t)0x1af4u)
/* virtio PCI capability `cfg_type` values (uint8_t; virtio 1.1 sec.4.1.4). */
#define TAIRIX_VIRTIO_PCI_CFG_COMMON ((uint8_t)1u)
#define TAIRIX_VIRTIO_PCI_CFG_NOTIFY ((uint8_t)2u)
#define TAIRIX_VIRTIO_PCI_CFG_ISR ((uint8_t)3u)
#define TAIRIX_VIRTIO_PCI_CFG_DEVICE ((uint8_t)4u)
#define TAIRIX_VIRTIO_PCI_CFG_PCI ((uint8_t)5u)

/* Length, in bytes, of an Ethernet MAC address. */
#define TAIRIX_MAC_ADDRESS_LEN 6u

/* Mount-flag bitmap (uint32_t); any bit outside KNOWN_MASK is reserved and rejected. */
#define TAIRIX_MOUNT_FLAG_READ_ONLY ((uint32_t)0x1u)
#define TAIRIX_MOUNT_FLAG_NOSUID ((uint32_t)0x2u)
#define TAIRIX_MOUNT_FLAG_NODEV ((uint32_t)0x4u)
#define TAIRIX_MOUNT_FLAG_NOEXEC ((uint32_t)0x8u)
#define TAIRIX_MOUNT_FLAG_KNOWN_MASK ((uint32_t)0xfu)

/* Sentinel "no node"; a live NodeId travels as a uint64_t. */
#define TAIRIX_NODE_ID_NONE ((uint64_t)0ull)

/* Display pixel encoding (uint8_t); named by the byte order of the first pixel. */
#define TAIRIX_DISPLAY_FORMAT_RGBA8888 ((uint8_t)1u)
#define TAIRIX_DISPLAY_FORMAT_BGRA8888 ((uint8_t)2u)

/* Filesystem node kind (uint8_t). */
#define TAIRIX_NODE_KIND_DIRECTORY ((uint8_t)0u)
#define TAIRIX_NODE_KIND_REGULAR_FILE ((uint8_t)1u)

/* Driver input-event kind (uint8_t); distinct from the windowing TAIRIX_INPUT_KIND_*. */
#define TAIRIX_INPUT_EVENT_KIND_KEY ((uint8_t)1u)
#define TAIRIX_INPUT_EVENT_KIND_POINTER ((uint8_t)2u)
#define TAIRIX_INPUT_EVENT_KIND_SCROLL ((uint8_t)3u)

/* Signed driver-manifest prefix; encoded little-endian on the wire. */
typedef struct tairix_driver_manifest {
    uint32_t magic;
    uint32_t abi_version;
    uint8_t kind;
    uint8_t bind_key_count;
    uint16_t capability_count;
    uint8_t syscall_table_hash[TAIRIX_SYSCALL_TABLE_HASH_LEN];
    uint8_t signer_pubkey[TAIRIX_DRIVER_SIGNER_PUBKEY_LEN];
    uint8_t signature[TAIRIX_DRIVER_SIGNATURE_LEN];
} tairix_driver_manifest_t;

/* One bind-table entry: a hardware-tree match key plus the manifest's
 * bind priority (AGENTS.md sec.18.3). bind_key_count entries follow the
 * capability body; all are covered by the manifest signature. */
typedef struct tairix_driver_bind_key {
    uint16_t priority;
    uint16_t reserved0;
    tairix_hw_match_key_t key;
} tairix_driver_bind_key_t;

/* Outcome of a spawned driver process's register() entry, sent to the
 * driver host over IPC; encoded little-endian on the wire. `status` is
 * TAIRIX_DRIVER_REGISTER_STATUS_OK or a TAIRIX_DRIVER_ERROR_* code; `handle` is
 * non-zero exactly when `status` is OK (informational only — the host
 * mints its own unforgeable handle). */
typedef struct tairix_driver_register_reply {
    uint32_t magic;
    uint32_t abi_version;
    int32_t status;
    uint32_t reserved0;
    uint64_t handle;
} tairix_driver_register_reply_t;

/* Block-device geometry (the drivers/storage class). */
typedef struct tairix_block_geometry {
    uint32_t block_size;
    uint64_t block_count;
} tairix_block_geometry_t;

/* Discard (TRIM/unmap) capability a block device reports. */
typedef struct tairix_discard_capability {
    uint8_t supported;
    uint64_t granularity_blocks;
    uint64_t max_blocks_per_request;
} tairix_discard_capability_t;

/* Point-in-time device-health snapshot (SMART / NVMe telemetry). */
typedef struct tairix_health_snapshot {
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
} tairix_health_snapshot_t;

/* Identifying tuple for a discovered device (the drivers/bus class).
* `device_class` mirrors the Rust `class` field (renamed for C++). */
typedef struct tairix_bus_device {
    uint32_t vendor;
    uint32_t device;
    uint16_t device_class;
    uint16_t reserved0;
    uint64_t address;
} tairix_bus_device_t;

/* Active display mode (the drivers/display class); `format` is a
* TAIRIX_DISPLAY_FORMAT_*. */
typedef struct tairix_display_mode {
    uint32_t width_px;
    uint32_t height_px;
    uint32_t stride_bytes;
    uint8_t format;
} tairix_display_mode_t;

/* What a hardware compositor back-end can do this frame. */
typedef struct tairix_accel_caps {
    uint32_t max_layers;
    uint32_t max_width_px;
    uint32_t max_height_px;
    uint8_t per_layer_opacity;
} tairix_accel_caps_t;

/* The four AGENTS.md sec.21 timestamps stored for a filesystem node. A
* stamp the backing format does not keep is the epoch (never a
* fabricated wall time). */
typedef struct tairix_node_times {
    tairix_time64_t created;
    tairix_time64_t modified;
    tairix_time64_t accessed;
    tairix_time64_t changed;
} tairix_node_times_t;

/* Structural metadata about a filesystem node; `kind` is a TAIRIX_NODE_KIND_*.
* `times` carries the node's four timestamps, read in the same structural
* read as kind/size. */
typedef struct tairix_node_info {
    uint8_t kind;
    uint64_t size;
    uint64_t allocated;
    tairix_node_times_t times;
} tairix_node_info_t;

/* One directory entry; `node` is a NodeId (uint64_t). The entry carries the
* child's full tairix_node_info_t (including its timestamps) and the opaque
* cursor that resumes the listing after it (pass it back to read_dir; 0
* starts a listing). */
typedef struct tairix_dir_entry {
    uint64_t node;
    tairix_node_info_t info;
    uintptr_t name_len;
    uint64_t next_cursor;
} tairix_dir_entry_t;

/* A mounted volume's space accounting, in whole blocks of block_size bytes.
* avail_blocks <= free_blocks <= total_blocks always holds; files/files_free
* are 0 when the format tracks no fixed inode table. */
typedef struct tairix_volume_stats {
    uint32_t block_size;
    uint32_t reserved0;
    uint64_t total_blocks;
    uint64_t free_blocks;
    uint64_t avail_blocks;
    uint64_t files;
    uint64_t files_free;
} tairix_volume_stats_t;

/* A single input event; `kind` is a TAIRIX_INPUT_EVENT_KIND_*. */
typedef struct tairix_input_event {
    uint8_t kind;
    uint8_t reserved0;
    uint16_t code;
    int32_t value;
} tairix_input_event_t;

/* A 48-bit IEEE 802 link-layer address (the drivers/network class). */
typedef struct tairix_mac_address {
    uint8_t octets[TAIRIX_MAC_ADDRESS_LEN];
} tairix_mac_address_t;

#endif /* TAIRIX_DRIVER_H */

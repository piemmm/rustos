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
#include "rustos_manifest.h"

/* Magic word identifying an abi-v1 driver manifest ("DRV1" little-endian). */
#define ROS_DRIVER_MANIFEST_MAGIC 0x31565244u
/* Maximum number of capability identifiers a driver manifest may request. */
#define ROS_DRIVER_MANIFEST_MAX_CAPABILITIES 64u
/* Length, in bytes, of the Ed25519 signer public key. */
#define ROS_DRIVER_SIGNER_PUBKEY_LEN 32u
/* Length, in bytes, of the Ed25519 manifest signature. */
#define ROS_DRIVER_SIGNATURE_LEN 64u
/* Packed little-endian wire size of a driver manifest, in bytes. */
#define ROS_DRIVER_MANIFEST_WIRE_LEN 140u

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

/* Signed driver-manifest prefix; encoded little-endian on the wire. */
typedef struct ros_driver_manifest {
    uint32_t magic;
    uint32_t abi_version;
    uint8_t kind;
    uint8_t reserved0;
    uint16_t capability_count;
    uint8_t syscall_table_hash[ROS_SYSCALL_TABLE_HASH_LEN];
    uint8_t signer_pubkey[ROS_DRIVER_SIGNER_PUBKEY_LEN];
    uint8_t signature[ROS_DRIVER_SIGNATURE_LEN];
} ros_driver_manifest_t;

#endif /* ROS_DRIVER_H */

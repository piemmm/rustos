/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Process startup vector handed to a freshly spawned program (AGENTS.md sec.16.5; plans/CCOMPAT.md CC3).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_PROCESS_H
#define ROS_PROCESS_H

#include <stdint.h>

/* Magic word identifying an abi-v1 startup-vector block ("PSV1" little-endian). */
#define ROS_PROCESS_START_MAGIC 0x31565350u
/* Maximum number of strings (arguments + environment entries) a vector may carry. */
#define ROS_PROCESS_START_MAX_STRINGS 4096u
/* Maximum length, in bytes, of one argument or environment string. */
#define ROS_PROCESS_START_MAX_STRING_LEN 65536u
/* Maximum total size, in bytes, of a startup-vector block. */
#define ROS_PROCESS_START_MAX_TOTAL_LEN ((uint64_t)16777216ull)

/* Packed little-endian wire size of a startup-vector header, in bytes. */
#define ROS_PROCESS_START_HEADER_WIRE_LEN 32u
/* Packed little-endian wire size of one string slot, in bytes. */
#define ROS_STRING_SLOT_WIRE_LEN 8u

/* One string's (offset, len) reference into the block; encoded little-endian. */
typedef struct ros_string_slot {
    uint32_t offset;
    uint32_t len;
} ros_string_slot_t;

/* Fixed startup-vector block prefix; followed by the slot table then string data. */
typedef struct ros_process_start_header {
    uint32_t magic;
    uint32_t abi_version;
    uint32_t arg_count;
    uint32_t env_count;
    uint64_t total_len;
    uint64_t canary;
} ros_process_start_header_t;

#endif /* ROS_PROCESS_H */

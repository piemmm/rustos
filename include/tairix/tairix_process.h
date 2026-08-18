/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Process startup vector handed to a freshly spawned program (AGENTS.md sec.16.5; plans/CCOMPAT.md CC3).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_PROCESS_H
#define TAIRIX_PROCESS_H

#include <stdint.h>

/* Magic word identifying an abi-v1 startup-vector block ("PSV1" little-endian). */
#define TAIRIX_PROCESS_START_MAGIC 0x31565350u
/* Maximum number of strings (arguments + environment entries) a vector may carry. */
#define TAIRIX_PROCESS_START_MAX_STRINGS 4096u
/* Maximum length, in bytes, of one argument or environment string. */
#define TAIRIX_PROCESS_START_MAX_STRING_LEN 65536u
/* Maximum total size, in bytes, of a startup-vector block. */
#define TAIRIX_PROCESS_START_MAX_TOTAL_LEN ((uint64_t)16777216ull)

/* `console` argument to tairix_sys_spawn: attach the child to the caller's own
 * console (any other value names an installed console index, see
 * tairix_sys_console_count). */
#define TAIRIX_CONSOLE_INHERIT ((uint64_t)0xffffffffffffffffull)
/* `target_uid` argument to tairix_sys_spawn: start the child under the
 * caller's own credential (any other value switches to that user, which
 * requires TAIRIX_CAP_SPAWN_AS_USER). */
#define TAIRIX_SPAWN_UID_INHERIT ((uint32_t)0xffffffffu)
/* `stack_len` argument to tairix_sys_thread_create: give the new thread the
 * kernel's default per-thread stack (the caller's effective stack-bytes
 * bound) instead of naming a size. */
#define TAIRIX_THREAD_STACK_DEFAULT ((uintptr_t)0u)

/* Packed little-endian wire size of a startup-vector header, in bytes. */
#define TAIRIX_PROCESS_START_HEADER_WIRE_LEN 40u
/* Packed little-endian wire size of one string slot, in bytes. */
#define TAIRIX_STRING_SLOT_WIRE_LEN 8u

/* One string's (offset, len) reference into the block; encoded little-endian. */
typedef struct tairix_string_slot {
    uint32_t offset;
    uint32_t len;
} tairix_string_slot_t;

/* Fixed startup-vector block prefix; followed by the slot table then string data. */
typedef struct tairix_process_start_header {
    uint32_t magic;
    uint32_t abi_version;
    uint32_t arg_count;
    uint32_t env_count;
    uint64_t total_len;
    uint64_t canary;
    uint64_t cpu_features;
} tairix_process_start_header_t;

#endif /* TAIRIX_PROCESS_H */

/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Canonical random-number ABI (AGENTS.md sec.22).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_RANDOM_H
#define ROS_RANDOM_H

#include <stdint.h>

/* Request flags (uint32_t). Every undefined bit is reserved and must be zero. */
#define ROS_RANDOM_FLAG_NON_BLOCKING 0x1u

/* Default per-CPU random output reserve, in bytes. */
#define ROS_RANDOM_RESERVE_DEFAULT_BYTES ((uintptr_t)2048u)
/* Maximum number of bytes a single random request may ask for. */
#define ROS_RANDOM_REQUEST_MAX_BYTES ((uintptr_t)65536u)

#endif /* ROS_RANDOM_H */

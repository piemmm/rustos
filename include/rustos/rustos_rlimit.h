/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Resource-limit ABI (AGENTS.md sec.24).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_RLIMIT_H
#define ROS_RLIMIT_H

#include <stdint.h>

/* A bound value meaning "no limit imposed" (AGENTS.md sec.24.3). */
#define ROS_RLIMIT_INFINITY ((uint64_t)18446744073709551615u)

/* Resource kinds a ros_resource_limit_t can govern (uint32_t; AGENTS.md sec.24.3). */
#define ROS_LIMIT_KIND_ADDRESS_SPACE_BYTES ((uint32_t)0u)
#define ROS_LIMIT_KIND_OPEN_STREAMS ((uint32_t)1u)
#define ROS_LIMIT_KIND_PROCESSES ((uint32_t)2u)
#define ROS_LIMIT_KIND_STACK_BYTES ((uint32_t)3u)
#define ROS_LIMIT_KIND_COUNT ((uint32_t)4u)

/* Length, in bytes, of the little-endian ros_resource_limit_t encoding. */
#define ROS_RESOURCE_LIMIT_WIRE_LEN 16u

/* A soft/hard resource-limit pair (AGENTS.md sec.24.3). */
typedef struct ros_resource_limit {
    uint64_t soft;
    uint64_t hard;
} ros_resource_limit_t;

#endif /* ROS_RLIMIT_H */

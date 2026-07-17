/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Resource-limit ABI (AGENTS.md sec.24).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_RLIMIT_H
#define TAIRIX_RLIMIT_H

#include <stdint.h>

/* A bound value meaning "no limit imposed" (AGENTS.md sec.24.3). */
#define TAIRIX_RLIMIT_INFINITY ((uint64_t)18446744073709551615u)

/* Resource kinds a tairix_resource_limit_t can govern (uint32_t; AGENTS.md sec.24.3). */
#define TAIRIX_LIMIT_KIND_ADDRESS_SPACE_BYTES ((uint32_t)0u)
#define TAIRIX_LIMIT_KIND_OPEN_STREAMS ((uint32_t)1u)
#define TAIRIX_LIMIT_KIND_PROCESSES ((uint32_t)2u)
#define TAIRIX_LIMIT_KIND_STACK_BYTES ((uint32_t)3u)
#define TAIRIX_LIMIT_KIND_PINNED_MEMORY_BYTES ((uint32_t)4u)
#define TAIRIX_LIMIT_KIND_COUNT ((uint32_t)5u)

/* Length, in bytes, of the little-endian tairix_resource_limit_t encoding. */
#define TAIRIX_RESOURCE_LIMIT_WIRE_LEN 16u

/* A soft/hard resource-limit pair (AGENTS.md sec.24.3). */
typedef struct tairix_resource_limit {
    uint64_t soft;
    uint64_t hard;
} tairix_resource_limit_t;

#endif /* TAIRIX_RLIMIT_H */

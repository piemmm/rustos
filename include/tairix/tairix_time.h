/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* 64-bit-native time types (AGENTS.md sec.21).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_TIME_H
#define TAIRIX_TIME_H

#include <stdint.h>

/* Nanoseconds in one second; the sub-second field stays in 0..this. */
#define TAIRIX_NANOS_PER_SEC 1000000000u
/* Coarse monotonic-clock granularity, ns, for callers without CAP_TIME_HIRES. */
#define TAIRIX_COARSE_CLOCK_GRANULARITY_NS 1000ull
/* Packed little-endian wire size of each time value, in bytes. */
#define TAIRIX_TIME64_WIRE_LEN 12u
#define TAIRIX_DURATION64_WIRE_LEN 12u
/* Plausibility window a time source's reading is checked against:
 * this release's epoch, and the width of the window above it.
 * Fixed validation bounds, not capacities. */
#define TAIRIX_RELEASE_EPOCH_SECS INT64_C(1767225600)
#define TAIRIX_PLAUSIBLE_FUTURE_SECS INT64_C(3155760000)

/* Absolute instant: signed seconds since the Unix epoch + canonical nanos. */
typedef struct tairix_time64 {
    int64_t secs;
    uint32_t nanos;
} tairix_time64_t;

/* Span of time: signed seconds + canonical nanos (companion to tairix_time64). */
typedef struct tairix_duration64 {
    int64_t secs;
    uint32_t nanos;
} tairix_duration64_t;

#endif /* TAIRIX_TIME_H */

/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* 64-bit-native time types (AGENTS.md sec.21).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_TIME_H
#define ROS_TIME_H

#include <stdint.h>

/* Nanoseconds in one second; the sub-second field stays in 0..this. */
#define ROS_NANOS_PER_SEC 1000000000u
/* Coarse monotonic-clock granularity, ns, for callers without CAP_TIME_HIRES. */
#define ROS_COARSE_CLOCK_GRANULARITY_NS 1000ull
/* Packed little-endian wire size of each time value, in bytes. */
#define ROS_TIME64_WIRE_LEN 12u
#define ROS_DURATION64_WIRE_LEN 12u

/* Absolute instant: signed seconds since the Unix epoch + canonical nanos. */
typedef struct ros_time64 {
    int64_t secs;
    uint32_t nanos;
} ros_time64_t;

/* Span of time: signed seconds + canonical nanos (companion to ros_time64). */
typedef struct ros_duration64 {
    int64_t secs;
    uint32_t nanos;
} ros_duration64_t;

#endif /* ROS_TIME_H */

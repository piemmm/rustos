/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Standard Information Stream ABI (AGENTS.md sec.20).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_STDINFO_H
#define ROS_STDINFO_H

#include <stdint.h>

/* Reserved stdinfo file descriptor; no component may repurpose it. */
#define ROS_STDINFO_FD 3u
/* stdinfo framing version tag for the frozen v1 framing. */
#define ROS_STDINFO_VERSION_V1 1u
/* stdinfo framing version this header set describes. */
#define ROS_STDINFO_VERSION_CURRENT 1u

/* Closed set of record kinds (uint8_t). Wire spelling is the string in parens. */
#define ROS_STDINFO_KIND_OMISSION ((uint8_t)0u) /* "omission" */
#define ROS_STDINFO_KIND_SUMMARY ((uint8_t)1u) /* "summary" */
#define ROS_STDINFO_KIND_SCHEMA ((uint8_t)2u) /* "schema" */
#define ROS_STDINFO_KIND_SUGGESTION ((uint8_t)3u) /* "suggestion" */
#define ROS_STDINFO_KIND_CONTEXT ((uint8_t)4u) /* "context" */

/* Advisory severity (uint8_t). Security events use lib/log, not fd 3. */
#define ROS_STDINFO_SEVERITY_INFO ((uint8_t)0u) /* "info" */
#define ROS_STDINFO_SEVERITY_DEBUG ((uint8_t)1u) /* "debug" */

#endif /* ROS_STDINFO_H */

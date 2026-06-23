/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Capability identifiers (AGENTS.md sec.5.2).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_CAPABILITY_H
#define ROS_CAPABILITY_H

#include <stdint.h>

/* Capability identifiers (uint16_t, the canonical CapabilityId width;
   AGENTS.md sec.5.2). Each id carries its type so call sites need no cast. */
#define ROS_CAPABILITY_ID_MAX ((uint16_t)255u)
#define ROS_CAP_FS_MOUNT ((uint16_t)1u)
#define ROS_CAP_NET_RAW ((uint16_t)2u)
#define ROS_CAP_DRV_LOAD ((uint16_t)3u)
#define ROS_CAP_DRV_KERNEL ((uint16_t)4u)
#define ROS_CAP_USER_ADMIN ((uint16_t)5u)
#define ROS_CAP_TIME_SET ((uint16_t)6u)
#define ROS_CAP_IPC_BIND_PRIVILEGED ((uint16_t)7u)
#define ROS_CAP_AUDIT_READ ((uint16_t)8u)
#define ROS_CAP_AUDIT_WRITE ((uint16_t)9u)
#define ROS_CAP_MEM_DMA ((uint16_t)10u)
#define ROS_CAP_IRQ_BIND ((uint16_t)11u)
#define ROS_CAP_MMIO_MAP ((uint16_t)12u)
#define ROS_CAP_SYSINFO_GLOBAL ((uint16_t)13u)
#define ROS_CAP_SYSINFO_KERNEL ((uint16_t)14u)
#define ROS_CAP_SYSINFO_HW ((uint16_t)15u)
#define ROS_CAP_TIME_HIRES ((uint16_t)16u)
#define ROS_CAP_PROC_SPAWN ((uint16_t)17u)
#define ROS_CAP_CONSOLE_WRITE ((uint16_t)18u)
#define ROS_CAP_CONSOLE_READ ((uint16_t)19u)
#define ROS_CAP_RLIMIT_RAISE ((uint16_t)20u)
#define ROS_CAP_USERS_READ ((uint16_t)21u)
#define ROS_CAP_INPUT_INJECT ((uint16_t)22u)
#define ROS_CAP_DISPLAY ((uint16_t)23u)
#define ROS_CAP_INPUT_READ ((uint16_t)24u)
#define ROS_CAP_MAILBOX ((uint16_t)25u)
#define ROS_CAP_LOG_EMIT ((uint16_t)26u)
#define ROS_CAP_HW_EMIT ((uint16_t)27u)

#endif /* ROS_CAPABILITY_H */

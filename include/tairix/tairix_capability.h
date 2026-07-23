/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Capability identifiers (AGENTS.md sec.5.2).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_CAPABILITY_H
#define TAIRIX_CAPABILITY_H

#include <stdint.h>

/* Capability identifiers (uint16_t, the canonical CapabilityId width;
   AGENTS.md sec.5.2). Each id carries its type so call sites need no cast. */
#define TAIRIX_CAPABILITY_ID_MAX ((uint16_t)255u)
#define TAIRIX_CAP_FS_MOUNT ((uint16_t)1u)
#define TAIRIX_CAP_NET_RAW ((uint16_t)2u)
#define TAIRIX_CAP_DRV_LOAD ((uint16_t)3u)
#define TAIRIX_CAP_DRV_KERNEL ((uint16_t)4u)
#define TAIRIX_CAP_USER_ADMIN ((uint16_t)5u)
#define TAIRIX_CAP_TIME_SET ((uint16_t)6u)
#define TAIRIX_CAP_IPC_BIND_PRIVILEGED ((uint16_t)7u)
#define TAIRIX_CAP_AUDIT_READ ((uint16_t)8u)
#define TAIRIX_CAP_AUDIT_WRITE ((uint16_t)9u)
#define TAIRIX_CAP_MEM_DMA ((uint16_t)10u)
#define TAIRIX_CAP_IRQ_BIND ((uint16_t)11u)
#define TAIRIX_CAP_MMIO_MAP ((uint16_t)12u)
#define TAIRIX_CAP_SYSINFO_GLOBAL ((uint16_t)13u)
#define TAIRIX_CAP_SYSINFO_KERNEL ((uint16_t)14u)
#define TAIRIX_CAP_SYSINFO_HW ((uint16_t)15u)
#define TAIRIX_CAP_TIME_HIRES ((uint16_t)16u)
#define TAIRIX_CAP_PROC_SPAWN ((uint16_t)17u)
#define TAIRIX_CAP_CONSOLE_WRITE ((uint16_t)18u)
#define TAIRIX_CAP_CONSOLE_READ ((uint16_t)19u)
#define TAIRIX_CAP_RLIMIT_RAISE ((uint16_t)20u)
#define TAIRIX_CAP_USERS_READ ((uint16_t)21u)
#define TAIRIX_CAP_INPUT_INJECT ((uint16_t)22u)
#define TAIRIX_CAP_DISPLAY ((uint16_t)23u)
#define TAIRIX_CAP_INPUT_READ ((uint16_t)24u)
#define TAIRIX_CAP_MAILBOX ((uint16_t)25u)
#define TAIRIX_CAP_LOG_EMIT ((uint16_t)26u)
#define TAIRIX_CAP_HW_EMIT ((uint16_t)27u)
#define TAIRIX_CAP_IPC_ENDPOINT ((uint16_t)28u)
#define TAIRIX_CAP_SHM ((uint16_t)29u)
#define TAIRIX_CAP_FS_ACCESS ((uint16_t)30u)
#define TAIRIX_CAP_SPAWN_AS_USER ((uint16_t)31u)
#define TAIRIX_CAP_SYSINFO_INTROSPECT ((uint16_t)32u)
#define TAIRIX_CAP_SEAT_ADMIN ((uint16_t)33u)
#define TAIRIX_CAP_MEM_PIN ((uint16_t)34u)
#define TAIRIX_CAP_NET_ADMIN ((uint16_t)35u)
#define TAIRIX_CAP_NET ((uint16_t)36u)
#define TAIRIX_CAP_SCHED_REALTIME ((uint16_t)37u)
#define TAIRIX_CAP_NET_BIND_PRIVILEGED ((uint16_t)38u)

#endif /* TAIRIX_CAPABILITY_H */

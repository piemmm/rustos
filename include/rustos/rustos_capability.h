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

/* Capability identifiers (AGENTS.md sec.5.2). */
#define ROS_CAPABILITY_ID_MAX 255u
#define ROS_CAP_FS_MOUNT 1u
#define ROS_CAP_NET_RAW 2u
#define ROS_CAP_DRV_LOAD 3u
#define ROS_CAP_DRV_KERNEL 4u
#define ROS_CAP_USER_ADMIN 5u
#define ROS_CAP_TIME_SET 6u
#define ROS_CAP_IPC_BIND_PRIVILEGED 7u
#define ROS_CAP_AUDIT_READ 8u
#define ROS_CAP_AUDIT_WRITE 9u
#define ROS_CAP_MEM_DMA 10u
#define ROS_CAP_IRQ_BIND 11u
#define ROS_CAP_MMIO_MAP 12u
#define ROS_CAP_SYSINFO_GLOBAL 13u
#define ROS_CAP_SYSINFO_KERNEL 14u
#define ROS_CAP_SYSINFO_HW 15u
#define ROS_CAP_TIME_HIRES 16u
#define ROS_CAP_PROC_SPAWN 17u

#endif /* ROS_CAPABILITY_H */

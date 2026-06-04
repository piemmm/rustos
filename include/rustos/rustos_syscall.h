/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Syscall numbers and C entry-point prototypes (AGENTS.md sec.9).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_SYSCALL_H
#define ROS_SYSCALL_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Syscall numbers (AGENTS.md sec.9). */
#define ROS_SYSCALL_MAX_ARGS 6u
#define ROS_SYS_YIELD 0u
#define ROS_SYS_EXIT 1u
#define ROS_SYS_IPC_SEND 2u
#define ROS_SYS_IPC_RECV 3u
#define ROS_SYS_CAP_QUERY 4u
#define ROS_SYS_CAP_DELEGATE 5u
#define ROS_SYS_CAP_REVOKE 6u
#define ROS_SYS_CLOCK_GET 7u
#define ROS_SYS_IRQ_BIND 8u
#define ROS_SYS_IRQ_WAIT 9u
#define ROS_SYS_RANDOM_GET 10u

/* Syscall entry points, implemented by the user-space stub library. */
void ros_sys_yield(void);
void ros_sys_exit(int32_t a0);
int32_t ros_sys_ipc_send(uint64_t a0, void * a1, uintptr_t a2);
int32_t ros_sys_ipc_recv(uint64_t a0, void * a1, uintptr_t a2);
uint32_t ros_sys_cap_query(uint16_t a0);
int32_t ros_sys_cap_delegate(uint64_t a0, void * a1);
int32_t ros_sys_cap_revoke(uint64_t a0, uint16_t a1);
uint64_t ros_sys_clock_get(void);
uint64_t ros_sys_irq_bind(uint32_t a0);
int32_t ros_sys_irq_wait(uint64_t a0, uint64_t a1);
uint64_t ros_sys_random_get(void * a0, uintptr_t a1, uint32_t a2);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ROS_SYSCALL_H */

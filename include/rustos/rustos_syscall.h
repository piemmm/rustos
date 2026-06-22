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
#define ROS_SYS_STREAM_WRITE 11u
#define ROS_SYS_SPAWN 12u
#define ROS_SYS_STREAM_READ 13u
#define ROS_SYS_MEM_MAP 14u
#define ROS_SYS_MEM_UNMAP 15u
#define ROS_SYS_WAIT 16u
#define ROS_SYS_RLIMIT_GET 17u
#define ROS_SYS_RLIMIT_SET 18u
#define ROS_SYS_USERS_DB_READ 19u
#define ROS_SYS_CONSOLE_COUNT 20u
#define ROS_SYS_STREAM_ECHO 21u
#define ROS_SYS_KEY_INJECT 22u
#define ROS_SYS_DISPLAY_ACQUIRE 23u
#define ROS_SYS_DISPLAY_RELEASE 24u
#define ROS_SYS_KEYBOARD_READ 25u
#define ROS_SYS_MMIO_MAP 26u
#define ROS_SYS_DMA_ALLOC 27u
#define ROS_SYS_RESOURCE_GRANTS 28u
#define ROS_SYS_HW_TREE_READ 29u
#define ROS_SYS_HW_TREE_WAIT 30u
#define ROS_SYS_IPC_CALL 31u
#define ROS_SYS_CALL_CREATE 32u
#define ROS_SYS_CALL_RECV 33u
#define ROS_SYS_CALL_REPLY 34u
#define ROS_SYS_USERS_DB_WAIT 35u

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
uint64_t ros_sys_stream_write(uint32_t a0, void * a1, uintptr_t a2);
uint64_t ros_sys_spawn(void * a0, uintptr_t a1, uint64_t a2);
uint64_t ros_sys_stream_read(uint32_t a0, void * a1, uintptr_t a2);
uint64_t ros_sys_mem_map(uintptr_t a0, uint32_t a1, uint64_t a2);
int32_t ros_sys_mem_unmap(uint64_t a0, uintptr_t a1);
uint64_t ros_sys_wait(int32_t a0, void * a1);
int32_t ros_sys_rlimit_get(uint32_t a0, void * a1);
int32_t ros_sys_rlimit_set(uint32_t a0, void * a1);
uint64_t ros_sys_users_db_read(void * a0, uintptr_t a1);
uint64_t ros_sys_console_count(void);
int32_t ros_sys_stream_echo(uint32_t a0, uint32_t a1);
uint64_t ros_sys_key_inject(void * a0, uintptr_t a1);
int32_t ros_sys_display_acquire(void);
int32_t ros_sys_display_release(void);
uint64_t ros_sys_keyboard_read(void * a0, uintptr_t a1);
uint64_t ros_sys_mmio_map(uint64_t a0);
uint64_t ros_sys_dma_alloc(uint64_t a0, uintptr_t a1, void * a2);
uint64_t ros_sys_resource_grants(void * a0, uintptr_t a1);
uint64_t ros_sys_hw_tree_read(void * a0, uintptr_t a1);
int32_t ros_sys_hw_tree_wait(uint64_t a0, uint64_t a1);
uint64_t ros_sys_ipc_call(uint64_t a0, void * a1, uintptr_t a2, void * a3, uintptr_t a4);
int32_t ros_sys_call_create(uint64_t a0, void * a1, void * a2, uintptr_t a3, uintptr_t a4, uintptr_t a5);
uint64_t ros_sys_call_recv(uint64_t a0, void * a1, uintptr_t a2, void * a3);
int32_t ros_sys_call_reply(uint64_t a0, uint64_t a1, void * a2, uintptr_t a3);
int32_t ros_sys_users_db_wait(uint64_t a0);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ROS_SYSCALL_H */

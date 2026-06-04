/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* This is the C-language view of the RustOS kernel/user ABI. It is
* generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*
* Each syscall is exported by the user-space stub library under the
* symbol `rustos_sys_<name>` (e.g. `rustos_sys_ipc_send`); link
* against that library to call the kernel from a non-Rust program.
*/

#ifndef RUSTOS_ABI_V1_H
#define RUSTOS_ABI_V1_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ABI version this header describes (AGENTS.md sec.9). */
#define RUSTOS_ABI_VERSION 1u

/* Stable abi-v1 error codes (int32_t). */
#define RUSTOS_E_BUFFER_TOO_SMALL 1
#define RUSTOS_E_BAD_ALIGNMENT 2
#define RUSTOS_E_BAD_MAGIC 3
#define RUSTOS_E_LENGTH_OUT_OF_RANGE 4
#define RUSTOS_E_OUT_OF_RANGE 5
#define RUSTOS_E_PERMISSION_DENIED 6
#define RUSTOS_E_NOT_FOUND 7
#define RUSTOS_E_DELEGATION_WIDEN 8
#define RUSTOS_E_SIGNATURE_INVALID 9
#define RUSTOS_E_ABI_VERSION_UNSUPPORTED 10
#define RUSTOS_E_MESSAGE_TOO_LARGE 11
#define RUSTOS_E_NOT_IMPLEMENTED 12
#define RUSTOS_E_TIMED_OUT 13
#define RUSTOS_E_TIMESTAMP_OUT_OF_RANGE 14
#define RUSTOS_E_NO_SPACE 15
#define RUSTOS_E_ENTROPY_NOT_READY 16
#define RUSTOS_E_ALREADY_EXISTS 17
#define RUSTOS_E_BAD_ADDRESS 18
#define RUSTOS_E_WOULD_BLOCK 19

/* Capability identifiers (AGENTS.md sec.5.2). */
#define RUSTOS_CAPABILITY_ID_MAX 255u
#define RUSTOS_CAP_FS_MOUNT 1u
#define RUSTOS_CAP_NET_RAW 2u
#define RUSTOS_CAP_DRV_LOAD 3u
#define RUSTOS_CAP_DRV_KERNEL 4u
#define RUSTOS_CAP_USER_ADMIN 5u
#define RUSTOS_CAP_TIME_SET 6u
#define RUSTOS_CAP_IPC_BIND_PRIVILEGED 7u
#define RUSTOS_CAP_AUDIT_READ 8u
#define RUSTOS_CAP_AUDIT_WRITE 9u
#define RUSTOS_CAP_MEM_DMA 10u
#define RUSTOS_CAP_IRQ_BIND 11u
#define RUSTOS_CAP_MMIO_MAP 12u
#define RUSTOS_CAP_SYSINFO_GLOBAL 13u
#define RUSTOS_CAP_SYSINFO_KERNEL 14u
#define RUSTOS_CAP_SYSINFO_HW 15u
#define RUSTOS_CAP_TIME_HIRES 16u

/* Syscall numbers (AGENTS.md sec.9). */
#define RUSTOS_SYSCALL_MAX_ARGS 6u
#define RUSTOS_SYS_YIELD 0u
#define RUSTOS_SYS_EXIT 1u
#define RUSTOS_SYS_IPC_SEND 2u
#define RUSTOS_SYS_IPC_RECV 3u
#define RUSTOS_SYS_CAP_QUERY 4u
#define RUSTOS_SYS_CAP_DELEGATE 5u
#define RUSTOS_SYS_CAP_REVOKE 6u
#define RUSTOS_SYS_CLOCK_GET 7u
#define RUSTOS_SYS_IRQ_BIND 8u
#define RUSTOS_SYS_IRQ_WAIT 9u
#define RUSTOS_SYS_RANDOM_GET 10u

/* Syscall entry points, implemented by the user-space stub library. */
void rustos_sys_yield(void);
void rustos_sys_exit(int32_t a0);
int32_t rustos_sys_ipc_send(uint64_t a0, void * a1, uintptr_t a2);
int32_t rustos_sys_ipc_recv(uint64_t a0, void * a1, uintptr_t a2);
uint32_t rustos_sys_cap_query(uint16_t a0);
int32_t rustos_sys_cap_delegate(uint64_t a0, void * a1);
int32_t rustos_sys_cap_revoke(uint64_t a0, uint16_t a1);
uint64_t rustos_sys_clock_get(void);
uint64_t rustos_sys_irq_bind(uint32_t a0);
int32_t rustos_sys_irq_wait(uint64_t a0, uint64_t a1);
uint64_t rustos_sys_random_get(void * a0, uintptr_t a1, uint32_t a2);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* RUSTOS_ABI_V1_H */

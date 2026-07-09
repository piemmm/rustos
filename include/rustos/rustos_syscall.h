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
#define ROS_SYS_STREAM_INPUT_MODE 21u
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
#define ROS_SYS_LOG_EMIT 36u
#define ROS_SYS_HW_EMIT_NODE 37u
#define ROS_SYS_HW_REMOVE_NODE 38u
#define ROS_SYS_MSI_ALLOC 39u
#define ROS_SYS_SHM_CREATE 40u
#define ROS_SYS_SHM_MAP 41u
#define ROS_SYS_SHM_UNMAP 42u
#define ROS_SYS_WAITSET_CREATE 43u
#define ROS_SYS_WAITSET_CTL 44u
#define ROS_SYS_WAITSET_WAIT 45u
#define ROS_SYS_FS_OPEN 46u
#define ROS_SYS_FS_CLOSE 47u
#define ROS_SYS_FS_READ 48u
#define ROS_SYS_FS_WRITE 49u
#define ROS_SYS_FS_READDIR 50u
#define ROS_SYS_FS_STAT 51u
#define ROS_SYS_FS_TRUNCATE 52u
#define ROS_SYS_FS_SYNC 53u
#define ROS_SYS_FS_MKDIR 54u
#define ROS_SYS_FS_UNLINK 55u
#define ROS_SYS_DMA_FREE 56u
#define ROS_SYS_FS_RENAME 57u
#define ROS_SYS_CALL_PEER_ORIGIN 58u
#define ROS_SYS_WALL_TIME_GET 59u
#define ROS_SYS_WALL_TIME_SET 60u
#define ROS_SYS_BOOT_ID_GET 61u
#define ROS_SYS_SYSINFO_INTROSPECT 62u
#define ROS_SYS_TERMINAL_SIZE 63u
#define ROS_SYS_SIGNAL 64u
#define ROS_SYS_FS_CHDIR 65u
#define ROS_SYS_FS_GETCWD 66u
#define ROS_SYS_RESOURCE_OPEN 67u
#define ROS_SYS_SELF_ORIGIN 68u
#define ROS_SYS_USERS_ADMIN 69u
#define ROS_SYS_SEAT_SWITCH 70u
#define ROS_SYS_SEAT_REVOKE 71u
#define ROS_SYS_CONSOLE_FOREGROUND 72u
#define ROS_SYS_PIPE_CREATE 73u
#define ROS_SYS_FS_SET_MODE 74u
#define ROS_SYS_PORT_RESOLVE 75u
#define ROS_SYS_FILE_MAP 76u
#define ROS_SYS_FILE_UNMAP 77u
#define ROS_SYS_POINTER_INJECT 78u
#define ROS_SYS_POINTER_READ 79u

/* wait() flag bits (uint32_t). Every undefined bit is reserved and must be zero;
* with the NONBLOCK bit set, wait() polls and returns ROS_E_WOULD_BLOCK when a
* matching child has nothing to report; with the STOPPED bit set, wait() also
* reports a child freshly stopped by ROS_SIGNAL_STOP, without reaping it. */
#define ROS_WAIT_FLAG_NONBLOCK 0x1u
#define ROS_WAIT_FLAG_STOPPED 0x2u

/* The typed record wait() writes through its status pointer: kind names the
* event (exited => value is the exit code; stopped => value is the stopping
* ROS_SIGNAL_* discriminant); 0 and every other kind are reserved. */
#define ROS_WAIT_STATUS_KIND_EXITED 1u
#define ROS_WAIT_STATUS_KIND_STOPPED 2u
typedef struct ros_wait_status {
    uint32_t kind;
    int32_t value;
} ros_wait_status_t;

/* spawn() attach block: the child's credential, base console, and one wire per
* standard descriptor (fd 0..3). Pass NULL/0 for full inherit. Every wire kind
* other than the values below (including 0) is reserved and refused; a HANDLE
* wire names a descriptor of the CALLER'S OWN open table (a file, resource, or
* pipe end), owner-checked kernel-side before any child state exists. */
#define ROS_SPAWN_ATTACH_VERSION 1u
#define ROS_SPAWN_ATTACH_LEN 48u
#define ROS_FD_WIRE_INHERIT 1u
#define ROS_FD_WIRE_INHERIT_SLOT 2u
#define ROS_FD_WIRE_CLOSED 3u
#define ROS_FD_WIRE_HANDLE 4u
typedef struct ros_fd_wire {
    uint32_t kind;
    uint32_t value;
} ros_fd_wire_t;
typedef struct ros_spawn_attach {
    uint32_t version;
    uint32_t target_uid;
    uint64_t console;
    ros_fd_wire_t wires[4];
} ros_spawn_attach_t;

/* fs_open() flag bits (uint32_t). Every undefined bit is reserved and rejected
* with ROS_E_OUT_OF_RANGE, as is a combination the contract forbids (TRUNCATE/
* APPEND without WRITE, EXCLUSIVE without CREATE, DIRECTORY with WRITE). An open
* with neither READ nor WRITE is a resolve-only handle. */
#define ROS_OPEN_FLAG_READ 0x1u
#define ROS_OPEN_FLAG_WRITE 0x2u
#define ROS_OPEN_FLAG_CREATE 0x4u
#define ROS_OPEN_FLAG_TRUNCATE 0x8u
#define ROS_OPEN_FLAG_APPEND 0x10u
#define ROS_OPEN_FLAG_DIRECTORY 0x20u
#define ROS_OPEN_FLAG_EXCLUSIVE 0x40u

/* fs_unlink() flag bits (uint32_t). Every undefined bit is reserved and rejected
* with ROS_E_OUT_OF_RANGE. 0 removes the named file or (empty) directory; with
* the DIRECTORY bit the removal succeeds only when the name is an (empty)
* directory (the atomic rmdir posture) and a non-directory is refused with
* ROS_E_NOT_A_DIRECTORY. */
#define ROS_UNLINK_FLAG_DIRECTORY 0x1u

/* fs_set_mode() permission-bit mask (the `mode` argument, uint32_t): the
* owner/group/other rwx triads plus the setuid/setgid/sticky bits. A mode
* carrying any higher bit (a file-type bit, say) is rejected with
* ROS_E_OUT_OF_RANGE, never silently masked. */
#define ROS_FS_MODE_MASK 0xfffu

/* signal() control signals (the `signal` argument, uint32_t). 0 is reserved and
* never valid; a value outside this set is rejected with ROS_E_OUT_OF_RANGE. */
#define ROS_SIGNAL_CONTINUE 1u
#define ROS_SIGNAL_TERMINATE 2u
#define ROS_SIGNAL_KILL 3u
#define ROS_SIGNAL_INTERRUPT 4u
#define ROS_SIGNAL_STOP 5u

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
uint64_t ros_sys_spawn(void * a0, uintptr_t a1, uint64_t a2, uintptr_t a3, uint64_t a4, uintptr_t a5);
uint64_t ros_sys_stream_read(uint32_t a0, void * a1, uintptr_t a2, uint64_t a3);
uint64_t ros_sys_mem_map(uintptr_t a0, uint32_t a1, uint64_t a2);
int32_t ros_sys_mem_unmap(uint64_t a0, uintptr_t a1);
uint64_t ros_sys_wait(int32_t a0, void * a1, uint32_t a2);
int32_t ros_sys_rlimit_get(uint32_t a0, void * a1);
int32_t ros_sys_rlimit_set(uint32_t a0, void * a1);
uint64_t ros_sys_users_db_read(void * a0, uintptr_t a1);
uint64_t ros_sys_console_count(void);
int32_t ros_sys_stream_input_mode(uint32_t a0, uint32_t a1);
uint64_t ros_sys_key_inject(uint64_t a0, void * a1, uintptr_t a2);
uint64_t ros_sys_display_acquire(uint64_t a0);
int32_t ros_sys_display_release(uint64_t a0);
uint64_t ros_sys_keyboard_read(uint64_t a0, void * a1, uintptr_t a2);
uint64_t ros_sys_mmio_map(uint64_t a0, uintptr_t a1, uintptr_t a2);
uint64_t ros_sys_dma_alloc(uint64_t a0, uintptr_t a1, void * a2);
uint64_t ros_sys_resource_grants(void * a0, uintptr_t a1);
uint64_t ros_sys_hw_tree_read(void * a0, uintptr_t a1);
int32_t ros_sys_hw_tree_wait(uint64_t a0, uint64_t a1);
uint64_t ros_sys_ipc_call(uint64_t a0, void * a1, uintptr_t a2, void * a3, uintptr_t a4);
int32_t ros_sys_call_create(uint64_t a0, void * a1, void * a2, uintptr_t a3, uintptr_t a4, uintptr_t a5);
uint64_t ros_sys_call_recv(uint64_t a0, void * a1, uintptr_t a2, void * a3);
int32_t ros_sys_call_reply(uint64_t a0, uint64_t a1, void * a2, uintptr_t a3);
int32_t ros_sys_users_db_wait(uint64_t a0);
int32_t ros_sys_log_emit(void * a0, uintptr_t a1);
int32_t ros_sys_hw_emit_node(void * a0, uintptr_t a1);
int32_t ros_sys_hw_remove_node(uint64_t a0);
uint64_t ros_sys_msi_alloc(void * a0, uintptr_t a1);
uint64_t ros_sys_shm_create(uintptr_t a0, void * a1);
uint64_t ros_sys_shm_map(uint64_t a0);
int32_t ros_sys_shm_unmap(uint64_t a0, uintptr_t a1);
uint64_t ros_sys_waitset_create(void);
int32_t ros_sys_waitset_ctl(uint64_t a0, uint32_t a1, uint32_t a2, uint64_t a3, uint64_t a4);
int32_t ros_sys_waitset_wait(uint64_t a0, uint64_t a1, void * a2);
uint64_t ros_sys_fs_open(void * a0, uintptr_t a1, uint32_t a2);
int32_t ros_sys_fs_close(uint32_t a0);
uint64_t ros_sys_fs_read(uint32_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t ros_sys_fs_write(uint32_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t ros_sys_fs_readdir(uint32_t a0, void * a1, uintptr_t a2);
uint64_t ros_sys_fs_stat(uint32_t a0, void * a1, uintptr_t a2);
int32_t ros_sys_fs_truncate(uint32_t a0, uint64_t a1);
int32_t ros_sys_fs_sync(uint32_t a0);
int32_t ros_sys_fs_mkdir(void * a0, uintptr_t a1);
int32_t ros_sys_fs_unlink(void * a0, uintptr_t a1, uint32_t a2);
int32_t ros_sys_dma_free(uint64_t a0, uint64_t a1);
int32_t ros_sys_fs_rename(void * a0, uintptr_t a1, void * a2, uintptr_t a3);
uint64_t ros_sys_call_peer_origin(uint64_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t ros_sys_wall_time_get(void * a0, uintptr_t a1);
int32_t ros_sys_wall_time_set(void * a0, uintptr_t a1, uint32_t a2);
uint64_t ros_sys_boot_id_get(void * a0, uintptr_t a1);
uint64_t ros_sys_sysinfo_introspect(uint32_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t ros_sys_terminal_size(uint32_t a0, void * a1, uintptr_t a2);
int32_t ros_sys_signal(int32_t a0, uint32_t a1);
int32_t ros_sys_fs_chdir(void * a0, uintptr_t a1);
uint64_t ros_sys_fs_getcwd(void * a0, uintptr_t a1);
uint64_t ros_sys_resource_open(void * a0, uintptr_t a1, uint32_t a2);
uint64_t ros_sys_self_origin(void * a0, uintptr_t a1);
uint64_t ros_sys_users_admin(void * a0, uintptr_t a1, void * a2, uintptr_t a3);
int32_t ros_sys_seat_switch(uint64_t a0, uint32_t a1);
int32_t ros_sys_seat_revoke(uint64_t a0);
int32_t ros_sys_console_foreground(uint32_t a0, int32_t a1);
int32_t ros_sys_pipe_create(void * a0);
int32_t ros_sys_fs_set_mode(void * a0, uintptr_t a1, uint32_t a2);
uint64_t ros_sys_port_resolve(void * a0, uintptr_t a1);
uint64_t ros_sys_file_map(uint32_t a0, uint64_t a1, uint64_t a2);
int32_t ros_sys_file_unmap(uint64_t a0, uint64_t a1);
uint64_t ros_sys_pointer_inject(uint64_t a0, void * a1, uintptr_t a2);
uint64_t ros_sys_pointer_read(uint64_t a0, void * a1, uintptr_t a2);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ROS_SYSCALL_H */

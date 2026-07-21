/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Syscall numbers and C entry-point prototypes (AGENTS.md sec.9).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_SYSCALL_H
#define TAIRIX_SYSCALL_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Syscall numbers (AGENTS.md sec.9). */
#define TAIRIX_SYSCALL_MAX_ARGS 6u
#define TAIRIX_SYS_YIELD 0u
#define TAIRIX_SYS_EXIT 1u
#define TAIRIX_SYS_IPC_SEND 2u
#define TAIRIX_SYS_IPC_RECV 3u
#define TAIRIX_SYS_CAP_QUERY 4u
#define TAIRIX_SYS_CAP_DELEGATE 5u
#define TAIRIX_SYS_CAP_REVOKE 6u
#define TAIRIX_SYS_CLOCK_GET 7u
#define TAIRIX_SYS_IRQ_BIND 8u
#define TAIRIX_SYS_IRQ_WAIT 9u
#define TAIRIX_SYS_RANDOM_GET 10u
#define TAIRIX_SYS_STREAM_WRITE 11u
#define TAIRIX_SYS_SPAWN 12u
#define TAIRIX_SYS_STREAM_READ 13u
#define TAIRIX_SYS_MEM_MAP 14u
#define TAIRIX_SYS_MEM_UNMAP 15u
#define TAIRIX_SYS_WAIT 16u
#define TAIRIX_SYS_RLIMIT_GET 17u
#define TAIRIX_SYS_RLIMIT_SET 18u
#define TAIRIX_SYS_USERS_DB_READ 19u
#define TAIRIX_SYS_CONSOLE_COUNT 20u
#define TAIRIX_SYS_STREAM_INPUT_MODE 21u
#define TAIRIX_SYS_KEY_INJECT 22u
#define TAIRIX_SYS_DISPLAY_ACQUIRE 23u
#define TAIRIX_SYS_DISPLAY_RELEASE 24u
#define TAIRIX_SYS_KEYBOARD_READ 25u
#define TAIRIX_SYS_MMIO_MAP 26u
#define TAIRIX_SYS_DMA_ALLOC 27u
#define TAIRIX_SYS_RESOURCE_GRANTS 28u
#define TAIRIX_SYS_HW_TREE_READ 29u
#define TAIRIX_SYS_HW_TREE_WAIT 30u
#define TAIRIX_SYS_IPC_CALL 31u
#define TAIRIX_SYS_CALL_CREATE 32u
#define TAIRIX_SYS_CALL_RECV 33u
#define TAIRIX_SYS_CALL_REPLY 34u
#define TAIRIX_SYS_USERS_DB_WAIT 35u
#define TAIRIX_SYS_LOG_EMIT 36u
#define TAIRIX_SYS_HW_EMIT_NODE 37u
#define TAIRIX_SYS_HW_REMOVE_NODE 38u
#define TAIRIX_SYS_MSI_ALLOC 39u
#define TAIRIX_SYS_SHM_CREATE 40u
#define TAIRIX_SYS_SHM_MAP 41u
#define TAIRIX_SYS_SHM_UNMAP 42u
#define TAIRIX_SYS_WAITSET_CREATE 43u
#define TAIRIX_SYS_WAITSET_CTL 44u
#define TAIRIX_SYS_WAITSET_WAIT 45u
#define TAIRIX_SYS_FS_OPEN 46u
#define TAIRIX_SYS_FS_CLOSE 47u
#define TAIRIX_SYS_FS_READ 48u
#define TAIRIX_SYS_FS_WRITE 49u
#define TAIRIX_SYS_FS_READDIR 50u
#define TAIRIX_SYS_FS_STAT 51u
#define TAIRIX_SYS_FS_TRUNCATE 52u
#define TAIRIX_SYS_FS_SYNC 53u
#define TAIRIX_SYS_FS_MKDIR 54u
#define TAIRIX_SYS_FS_UNLINK 55u
#define TAIRIX_SYS_DMA_FREE 56u
#define TAIRIX_SYS_FS_RENAME 57u
#define TAIRIX_SYS_CALL_PEER_ORIGIN 58u
#define TAIRIX_SYS_WALL_TIME_GET 59u
#define TAIRIX_SYS_WALL_TIME_SET 60u
#define TAIRIX_SYS_BOOT_ID_GET 61u
#define TAIRIX_SYS_SYSINFO_INTROSPECT 62u
#define TAIRIX_SYS_TERMINAL_SIZE 63u
#define TAIRIX_SYS_SIGNAL 64u
#define TAIRIX_SYS_FS_CHDIR 65u
#define TAIRIX_SYS_FS_GETCWD 66u
#define TAIRIX_SYS_RESOURCE_OPEN 67u
#define TAIRIX_SYS_SELF_ORIGIN 68u
#define TAIRIX_SYS_USERS_ADMIN 69u
#define TAIRIX_SYS_SEAT_SWITCH 70u
#define TAIRIX_SYS_SEAT_REVOKE 71u
#define TAIRIX_SYS_CONSOLE_FOREGROUND 72u
#define TAIRIX_SYS_PIPE_CREATE 73u
#define TAIRIX_SYS_FS_SET_MODE 74u
#define TAIRIX_SYS_PORT_RESOLVE 75u
#define TAIRIX_SYS_FILE_MAP 76u
#define TAIRIX_SYS_FILE_UNMAP 77u
#define TAIRIX_SYS_POINTER_INJECT 78u
#define TAIRIX_SYS_POINTER_READ 79u
#define TAIRIX_SYS_VOLUME_ATTACH 80u
#define TAIRIX_SYS_VOLUME_DETACH 81u
#define TAIRIX_SYS_SHM_GRANT 82u
#define TAIRIX_SYS_CALL_PEER_SEAT 83u
#define TAIRIX_SYS_FS_ATTR_GET 84u
#define TAIRIX_SYS_FS_ATTR_SET 85u
#define TAIRIX_SYS_FS_ATTR_LIST 86u
#define TAIRIX_SYS_FS_ATTR_REMOVE 87u
#define TAIRIX_SYS_PORT_BIND 88u
#define TAIRIX_SYS_BOOT_FACTS_GET 89u
#define TAIRIX_SYS_FD_GRANT 90u
#define TAIRIX_SYS_FD_REDEEM 91u
#define TAIRIX_SYS_MEM_PIN 92u
#define TAIRIX_SYS_MEM_UNPIN 93u
#define TAIRIX_SYS_SIGNAL_INTAKE 94u
#define TAIRIX_SYS_SCHED_SET_REALTIME 95u

/* wait() flag bits (uint32_t). Every undefined bit is reserved and must be zero;
* with the NONBLOCK bit set, wait() polls and returns TAIRIX_E_WOULD_BLOCK when a
* matching child has nothing to report; with the STOPPED bit set, wait() also
* reports a child freshly stopped by TAIRIX_SIGNAL_STOP, without reaping it. */
#define TAIRIX_WAIT_FLAG_NONBLOCK 0x1u
#define TAIRIX_WAIT_FLAG_STOPPED 0x2u

/* The typed record wait() writes through its status pointer: kind names the
* event (exited => value is the exit code; stopped => value is the stopping
* TAIRIX_SIGNAL_* discriminant); 0 and every other kind are reserved. */
#define TAIRIX_WAIT_STATUS_KIND_EXITED 1u
#define TAIRIX_WAIT_STATUS_KIND_STOPPED 2u
typedef struct tairix_wait_status {
    uint32_t kind;
    int32_t value;
} tairix_wait_status_t;

/* Reserved load-failure exit statuses (a tairix_wait_status_t.value when kind is
* EXITED). A spawn() returns once the child is ADMITTED, not once it is LOADED, so
* a load failure the child discovers on its own task surfaces as one of these
* exit statuses rather than as a spawn() error. They sit in a high reserved band
* well above the small codes a program passes to exit(), so a parent can tell a
* loader refusal apart from an ordinary exit: NOT_FOUND (missing or unreadable
* bundle), UNVERIFIED (bad signature / content or interface hash), MALFORMED
* (un-parseable or unfit image), OOM (out of memory building the image). */
#define TAIRIX_LOAD_NOT_FOUND ((int32_t)2136604673)
#define TAIRIX_LOAD_UNVERIFIED ((int32_t)2136604674)
#define TAIRIX_LOAD_MALFORMED ((int32_t)2136604675)
#define TAIRIX_LOAD_OOM ((int32_t)2136604676)

/* spawn() attach block: the child's credential, base console, and one wire per
* standard descriptor (fd 0..3). Pass NULL/0 for full inherit. Every wire kind
* other than the values below (including 0) is reserved and refused; a HANDLE
* wire names a descriptor of the CALLER'S OWN open table (a file, resource, or
* pipe end), owner-checked kernel-side before any child state exists. */
#define TAIRIX_SPAWN_ATTACH_VERSION 2u
#define TAIRIX_SPAWN_ATTACH_LEN 56u
/* Attach-block flags. SANDBOX starts the child as a minimum-capability
* parser sandbox: empty capability set, closed syscall allow-list, and
* every wire must be CLOSED or HANDLE (nothing ambient flows in). Any
* reserved flag bit is refused. */
#define TAIRIX_SPAWN_FLAG_SANDBOX 1u
#define TAIRIX_FD_WIRE_INHERIT 1u
#define TAIRIX_FD_WIRE_INHERIT_SLOT 2u
#define TAIRIX_FD_WIRE_CLOSED 3u
#define TAIRIX_FD_WIRE_HANDLE 4u
typedef struct tairix_fd_wire {
    uint32_t kind;
    uint32_t value;
} tairix_fd_wire_t;
typedef struct tairix_spawn_attach {
    uint32_t version;
    uint32_t target_uid;
    uint64_t console;
    uint64_t flags;
    tairix_fd_wire_t wires[4];
} tairix_spawn_attach_t;

/* fs_open() flag bits (uint32_t). Every undefined bit is reserved and rejected
* with TAIRIX_E_OUT_OF_RANGE, as is a combination the contract forbids (TRUNCATE/
* APPEND without WRITE, EXCLUSIVE without CREATE, DIRECTORY with WRITE). An open
* with neither READ nor WRITE is a resolve-only handle. */
#define TAIRIX_OPEN_FLAG_READ 0x1u
#define TAIRIX_OPEN_FLAG_WRITE 0x2u
#define TAIRIX_OPEN_FLAG_CREATE 0x4u
#define TAIRIX_OPEN_FLAG_TRUNCATE 0x8u
#define TAIRIX_OPEN_FLAG_APPEND 0x10u
#define TAIRIX_OPEN_FLAG_DIRECTORY 0x20u
#define TAIRIX_OPEN_FLAG_EXCLUSIVE 0x40u

/* fs_unlink() flag bits (uint32_t). Every undefined bit is reserved and rejected
* with TAIRIX_E_OUT_OF_RANGE. 0 removes the named file or (empty) directory; with
* the DIRECTORY bit the removal succeeds only when the name is an (empty)
* directory (the atomic rmdir posture) and a non-directory is refused with
* TAIRIX_E_NOT_A_DIRECTORY. */
#define TAIRIX_UNLINK_FLAG_DIRECTORY 0x1u

/* fs_set_mode() permission-bit mask (the `mode` argument, uint32_t): the
* owner/group/other rwx triads plus the setuid/setgid/sticky bits. A mode
* carrying any higher bit (a file-type bit, say) is rejected with
* TAIRIX_E_OUT_OF_RANGE, never silently masked. */
#define TAIRIX_FS_MODE_MASK 0xfffu

/* fs_attr_*() bounds: an extended-attribute key (a `namespace.rest`
* lib/fsmeta-grammar key) carries 1..=TAIRIX_FS_ATTR_KEY_MAX bytes, and a value
* at most TAIRIX_FS_ATTR_VALUE_MAX opaque bytes; a call outside either bound is
* rejected with TAIRIX_E_LENGTH_OUT_OF_RANGE before any copy. An absent
* attribute reads as TAIRIX_E_NO_DATA (a value may be empty, so absence is
* never an empty read), and a mount whose on-disk format stores no
* attributes answers every fs_attr_*() call with TAIRIX_E_NOT_SUPPORTED. */
#define TAIRIX_FS_ATTR_KEY_MAX 255u
#define TAIRIX_FS_ATTR_VALUE_MAX 3072u

/* signal() control signals (the `signal` argument, uint32_t). 0 is reserved and
* never valid; a value outside this set is rejected with TAIRIX_E_OUT_OF_RANGE. */
#define TAIRIX_SIGNAL_CONTINUE 1u
#define TAIRIX_SIGNAL_TERMINATE 2u
#define TAIRIX_SIGNAL_KILL 3u
#define TAIRIX_SIGNAL_INTERRUPT 4u
#define TAIRIX_SIGNAL_STOP 5u

/* signal_intake() operations (the `op` argument, uint32_t). A value outside
* this set is rejected with TAIRIX_E_OUT_OF_RANGE. With the intake enabled, a
* pending observed signal is waited on through a wait-set member of kind
* TAIRIX_WAIT_SOURCE_SIGNAL (id 0) and drained with the take operation, which
* returns the drained TAIRIX_SIGNAL_* discriminant. TAIRIX_SIGNAL_KILL is never
* observable; a second termination request while one is pending undrained
* escalates to the default terminate path. */
#define TAIRIX_SIGNAL_INTAKE_OP_ENABLE 0u
#define TAIRIX_SIGNAL_INTAKE_OP_DISABLE 1u
#define TAIRIX_SIGNAL_INTAKE_OP_TAKE 2u

/* waitset_ctl() operations (the `op` argument, uint32_t) and member source
* kinds (the `kind` argument, uint32_t). A value outside either set is
* rejected with TAIRIX_E_OUT_OF_RANGE; every member is owner-checked against the
* calling task when it is added. */
#define TAIRIX_WAITSET_OP_ADD 0u
#define TAIRIX_WAITSET_OP_DEL 1u
#define TAIRIX_WAIT_SOURCE_ENDPOINT 0u
#define TAIRIX_WAIT_SOURCE_IRQ 1u
#define TAIRIX_WAIT_SOURCE_CHILD 2u
#define TAIRIX_WAIT_SOURCE_SEAT_INPUT 3u
#define TAIRIX_WAIT_SOURCE_PORT 4u
#define TAIRIX_WAIT_SOURCE_STREAM 5u
#define TAIRIX_WAIT_SOURCE_SIGNAL 6u

/* Syscall entry points, implemented by the user-space stub library. */
void tairix_sys_yield(void);
void tairix_sys_exit(int32_t a0);
int32_t tairix_sys_ipc_send(uint64_t a0, void * a1, uintptr_t a2);
uint64_t tairix_sys_ipc_recv(uint64_t a0, void * a1, uintptr_t a2, void * a3);
uint32_t tairix_sys_cap_query(uint16_t a0);
int32_t tairix_sys_cap_delegate(uint64_t a0, void * a1);
int32_t tairix_sys_cap_revoke(uint64_t a0, uint16_t a1);
uint64_t tairix_sys_clock_get(void);
uint64_t tairix_sys_irq_bind(uint32_t a0);
int32_t tairix_sys_irq_wait(uint64_t a0, uint64_t a1);
uint64_t tairix_sys_random_get(void * a0, uintptr_t a1, uint32_t a2);
uint64_t tairix_sys_stream_write(uint32_t a0, void * a1, uintptr_t a2);
uint64_t tairix_sys_spawn(void * a0, uintptr_t a1, uint64_t a2, uintptr_t a3, uint64_t a4, uintptr_t a5);
uint64_t tairix_sys_stream_read(uint32_t a0, void * a1, uintptr_t a2, uint64_t a3);
uint64_t tairix_sys_mem_map(uintptr_t a0, uint32_t a1, uint64_t a2);
int32_t tairix_sys_mem_unmap(uint64_t a0, uintptr_t a1);
uint64_t tairix_sys_wait(int32_t a0, void * a1, uint32_t a2);
int32_t tairix_sys_rlimit_get(uint32_t a0, void * a1);
int32_t tairix_sys_rlimit_set(uint32_t a0, void * a1);
uint64_t tairix_sys_users_db_read(void * a0, uintptr_t a1);
uint64_t tairix_sys_console_count(void);
int32_t tairix_sys_stream_input_mode(uint32_t a0, uint32_t a1);
uint64_t tairix_sys_key_inject(uint64_t a0, void * a1, uintptr_t a2);
uint64_t tairix_sys_display_acquire(uint64_t a0);
int32_t tairix_sys_display_release(uint64_t a0);
uint64_t tairix_sys_keyboard_read(uint64_t a0, void * a1, uintptr_t a2);
uint64_t tairix_sys_mmio_map(uint64_t a0, uintptr_t a1, uintptr_t a2);
uint64_t tairix_sys_dma_alloc(uint64_t a0, uintptr_t a1, void * a2);
uint64_t tairix_sys_resource_grants(void * a0, uintptr_t a1);
uint64_t tairix_sys_hw_tree_read(void * a0, uintptr_t a1);
int32_t tairix_sys_hw_tree_wait(uint64_t a0, uint64_t a1);
uint64_t tairix_sys_ipc_call(uint64_t a0, void * a1, uintptr_t a2, void * a3, uintptr_t a4);
int32_t tairix_sys_call_create(uint64_t a0, void * a1, void * a2, uintptr_t a3, uintptr_t a4, uintptr_t a5);
uint64_t tairix_sys_call_recv(uint64_t a0, void * a1, uintptr_t a2, void * a3, uint32_t a4);
int32_t tairix_sys_call_reply(uint64_t a0, uint64_t a1, void * a2, uintptr_t a3);
int32_t tairix_sys_users_db_wait(uint64_t a0);
int32_t tairix_sys_log_emit(void * a0, uintptr_t a1);
int32_t tairix_sys_hw_emit_node(void * a0, uintptr_t a1);
int32_t tairix_sys_hw_remove_node(uint64_t a0);
uint64_t tairix_sys_msi_alloc(void * a0, uintptr_t a1);
uint64_t tairix_sys_shm_create(uintptr_t a0, void * a1);
uint64_t tairix_sys_shm_map(uint64_t a0, void * a1);
int32_t tairix_sys_shm_unmap(uint64_t a0, uintptr_t a1);
uint64_t tairix_sys_waitset_create(void);
int32_t tairix_sys_waitset_ctl(uint64_t a0, uint32_t a1, uint32_t a2, uint64_t a3, uint64_t a4);
int32_t tairix_sys_waitset_wait(uint64_t a0, uint64_t a1, void * a2);
uint64_t tairix_sys_fs_open(void * a0, uintptr_t a1, uint32_t a2);
int32_t tairix_sys_fs_close(uint32_t a0);
uint64_t tairix_sys_fs_read(uint32_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t tairix_sys_fs_write(uint32_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t tairix_sys_fs_readdir(uint32_t a0, void * a1, uintptr_t a2);
uint64_t tairix_sys_fs_stat(uint32_t a0, void * a1, uintptr_t a2);
int32_t tairix_sys_fs_truncate(uint32_t a0, uint64_t a1);
int32_t tairix_sys_fs_sync(uint32_t a0);
int32_t tairix_sys_fs_mkdir(void * a0, uintptr_t a1);
int32_t tairix_sys_fs_unlink(void * a0, uintptr_t a1, uint32_t a2);
int32_t tairix_sys_dma_free(uint64_t a0, uint64_t a1);
int32_t tairix_sys_fs_rename(void * a0, uintptr_t a1, void * a2, uintptr_t a3);
uint64_t tairix_sys_call_peer_origin(uint64_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t tairix_sys_wall_time_get(void * a0, uintptr_t a1);
int32_t tairix_sys_wall_time_set(void * a0, uintptr_t a1, uint32_t a2);
uint64_t tairix_sys_boot_id_get(void * a0, uintptr_t a1);
uint64_t tairix_sys_sysinfo_introspect(uint32_t a0, uint64_t a1, void * a2, uintptr_t a3);
uint64_t tairix_sys_terminal_size(uint32_t a0, void * a1, uintptr_t a2);
int32_t tairix_sys_signal(int32_t a0, uint32_t a1);
int32_t tairix_sys_fs_chdir(void * a0, uintptr_t a1);
uint64_t tairix_sys_fs_getcwd(void * a0, uintptr_t a1);
uint64_t tairix_sys_resource_open(void * a0, uintptr_t a1, uint32_t a2);
uint64_t tairix_sys_self_origin(void * a0, uintptr_t a1);
uint64_t tairix_sys_users_admin(void * a0, uintptr_t a1, void * a2, uintptr_t a3);
int32_t tairix_sys_seat_switch(uint64_t a0, uint32_t a1);
int32_t tairix_sys_seat_revoke(uint64_t a0);
int32_t tairix_sys_console_foreground(uint32_t a0, int32_t a1);
int32_t tairix_sys_pipe_create(void * a0);
int32_t tairix_sys_fs_set_mode(void * a0, uintptr_t a1, uint32_t a2);
uint64_t tairix_sys_port_resolve(void * a0, uintptr_t a1);
uint64_t tairix_sys_file_map(uint32_t a0, uint64_t a1, uint64_t a2);
int32_t tairix_sys_file_unmap(uint64_t a0, uint64_t a1);
uint64_t tairix_sys_pointer_inject(uint64_t a0, void * a1, uintptr_t a2);
uint64_t tairix_sys_pointer_read(uint64_t a0, void * a1, uintptr_t a2);
int32_t tairix_sys_volume_attach(void * a0, uintptr_t a1);
int32_t tairix_sys_volume_detach(void * a0, uintptr_t a1);
uint64_t tairix_sys_shm_grant(uint64_t a0, uint64_t a1);
uint64_t tairix_sys_call_peer_seat(uint64_t a0, uint64_t a1, uint64_t a2);
uint64_t tairix_sys_fs_attr_get(void * a0, uintptr_t a1, void * a2, uintptr_t a3, void * a4, uintptr_t a5);
int32_t tairix_sys_fs_attr_set(void * a0, uintptr_t a1, void * a2, uintptr_t a3, void * a4, uintptr_t a5);
uint64_t tairix_sys_fs_attr_list(void * a0, uintptr_t a1, uint64_t a2, void * a3, uintptr_t a4);
int32_t tairix_sys_fs_attr_remove(void * a0, uintptr_t a1, void * a2, uintptr_t a3);
int32_t tairix_sys_port_bind(uint64_t a0, uintptr_t a1, uintptr_t a2);
uint64_t tairix_sys_boot_facts_get(void * a0, uintptr_t a1);
uint64_t tairix_sys_fd_grant(uint32_t a0, uint64_t a1);
uint64_t tairix_sys_fd_redeem(uint64_t a0);
int32_t tairix_sys_mem_pin(void);
int32_t tairix_sys_mem_unpin(void);
uint64_t tairix_sys_signal_intake(uint32_t a0);
int32_t tairix_sys_sched_set_realtime(uint32_t a0);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TAIRIX_SYSCALL_H */

/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* System Information API surface (AGENTS.md sec.16.6).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_SYSINFO_H
#define TAIRIX_SYSINFO_H

#include <stdint.h>
#include "tairix_time.h"
#include "tairix_rlimit.h"
#include "tairix_driver.h"

/* sysinfo protocol version tag for the frozen v1 surface. */
#define TAIRIX_SYSINFO_VERSION_V1 1u
/* sysinfo protocol version this header set describes. */
#define TAIRIX_SYSINFO_VERSION_CURRENT 1u
/* Magic word identifying a sysinfo-v1 request ("SYI1" little-endian). */
#define TAIRIX_SYSINFO_REQUEST_MAGIC 0x31495953u
/* Maximum request/response payload length, in bytes, a header may advertise. */
#define TAIRIX_SYSINFO_MAX_PAYLOAD_LEN 1048576u
/* Inclusive upper bound on the sysinfo-v1 query identifier space. */
#define TAIRIX_SYSINFO_QUERY_ID_MAX 1023u

/* Canonical query-registry encoding constants (the hashable registry image). */
#define TAIRIX_SYSINFO_QUERY_NAME_MAX 20u
#define TAIRIX_SYSINFO_QUERY_RECORD_LEN 26u
#define TAIRIX_SYSINFO_ENCODED_QUERY_TABLE_LEN 676u
#define TAIRIX_SYSINFO_LOAD_FIXED_SHIFT 11u

/* Well-known sysinfo-v1 query identifiers (uint16_t). Do not renumber. */
#define TAIRIX_SYSINFO_QUERY_SELF_PROCESS_LIST ((uint16_t)0u)
#define TAIRIX_SYSINFO_QUERY_GLOBAL_PROCESS_LIST ((uint16_t)1u)
#define TAIRIX_SYSINFO_QUERY_KERNEL_MEMORY_STATS ((uint16_t)2u)
#define TAIRIX_SYSINFO_QUERY_HARDWARE_TREE ((uint16_t)3u)
#define TAIRIX_SYSINFO_QUERY_SYSTEM_IDENTITY ((uint16_t)4u)
#define TAIRIX_SYSINFO_QUERY_UPTIME ((uint16_t)5u)
#define TAIRIX_SYSINFO_QUERY_MOUNT_LIST ((uint16_t)6u)
#define TAIRIX_SYSINFO_QUERY_RESOURCE_LIMITS ((uint16_t)7u)
#define TAIRIX_SYSINFO_QUERY_PROCESS_IDENTITY ((uint16_t)8u)
#define TAIRIX_SYSINFO_QUERY_LOAD_AVERAGE ((uint16_t)9u)
#define TAIRIX_SYSINFO_QUERY_USER_DIRECTORY ((uint16_t)10u)

/* Process lifecycle state carried in a process record (uint8_t). */
#define TAIRIX_PROCESS_STATE_RUNNABLE ((uint8_t)0u)
#define TAIRIX_PROCESS_STATE_RUNNING ((uint8_t)1u)
#define TAIRIX_PROCESS_STATE_BLOCKED ((uint8_t)2u)
#define TAIRIX_PROCESS_STATE_ZOMBIE ((uint8_t)3u)
#define TAIRIX_PROCESS_STATE_STOPPED ((uint8_t)4u)
/* tairix_process_record.cpu sentinel: the process is not currently scheduled. */
#define TAIRIX_PROCESS_CPU_NONE ((uint8_t)255u)

/* Inline fixed-buffer capacities carried in the record types below. */
#define TAIRIX_PROCESS_NAME_MAX 32u
#define TAIRIX_MACHINE_ID_LEN 16u
#define TAIRIX_HOSTNAME_MAX 64u
#define TAIRIX_MOUNT_SOURCE_MAX 64u
#define TAIRIX_MOUNT_TARGET_MAX 64u
#define TAIRIX_MOUNT_FSTYPE_MAX 16u
#define TAIRIX_MOUNT_VOLUME_ID_LEN 16u
/* Mount availability carried in a mount record (uint8_t). */
#define TAIRIX_MOUNT_AVAILABLE ((uint8_t)0u)
#define TAIRIX_MOUNT_UNAVAILABLE_DIRTY ((uint8_t)1u)
#define TAIRIX_MOUNT_UNAVAILABLE_LOST ((uint8_t)2u)
#define TAIRIX_MOUNT_RECOVERY_CONFLICT ((uint8_t)3u)
#define TAIRIX_USER_DIRECTORY_NAME_MAX 32u

/* Packed little-endian wire size of each sysinfo record type, in bytes. */
#define TAIRIX_SYSINFO_REQUEST_HEADER_WIRE_LEN 24u
#define TAIRIX_PROCESS_LIST_REQUEST_WIRE_LEN 8u
#define TAIRIX_PROCESS_RECORD_WIRE_LEN 108u
#define TAIRIX_KERNEL_MEMORY_STATS_WIRE_LEN 40u
#define TAIRIX_UPTIME_WIRE_LEN 24u
#define TAIRIX_LOAD_AVERAGE_WIRE_LEN 24u
#define TAIRIX_SYSTEM_IDENTITY_WIRE_LEN 88u
#define TAIRIX_MOUNT_LIST_REQUEST_WIRE_LEN 8u
#define TAIRIX_MOUNT_RECORD_WIRE_LEN 216u
#define TAIRIX_RESOURCE_LIMIT_RECORD_WIRE_LEN 32u
#define TAIRIX_USER_DIRECTORY_REQUEST_WIRE_LEN 8u
#define TAIRIX_USER_DIRECTORY_RECORD_WIRE_LEN 40u

/* Byte length of a full RESOURCE_LIMITS response: one record per LimitKind. */
#define TAIRIX_SYSINFO_RESOURCE_LIMITS_REPORT_LEN 160u

/* Envelope prefixing every sysinfo request; encoded little-endian on the wire. */
typedef struct tairix_sysinfo_request_header {
    uint32_t magic;
    uint16_t version;
    uint16_t flags;
    uint16_t query;
    uint16_t reserved;
    uint32_t payload_len;
    uint64_t request_id;
} tairix_sysinfo_request_header_t;

/* Process-list request payload (offset/limit paging). */
typedef struct tairix_process_list_request {
    uint32_t offset;
    uint16_t limit;
    uint16_t flags;
} tairix_process_list_request_t;

/* One process entry. The numeric pid/parent_pid are reused across process
* lifetimes; proc_id/parent_proc_id are the kernel-attested, never-reused
* process-instance identities (correlate on those, not the numeric ids).
* `cpu` is TAIRIX_PROCESS_CPU_NONE when the process is not currently
* scheduled; cpu_time_ns is the cumulative on-CPU time and mem_bytes the
* mapped address-space size. The inline name is valid for name_len bytes. */
typedef struct tairix_process_record {
    uint64_t pid;
    uint64_t parent_pid;
    uint8_t proc_id[16];
    uint8_t parent_proc_id[16];
    uint32_t uid;
    uint32_t gid;
    uint8_t state;
    uint8_t cpu;
    uint64_t cpu_time_ns;
    uint64_t mem_bytes;
    uint8_t name_len;
    uint8_t name[TAIRIX_PROCESS_NAME_MAX];
} tairix_process_record_t;

/* Kernel memory statistics response. */
typedef struct tairix_kernel_memory_stats {
    uint64_t total_bytes;
    uint64_t free_bytes;
    uint64_t kernel_heap_bytes;
    uint64_t user_resident_bytes;
    uint32_t page_size;
    uint32_t reserved;
} tairix_kernel_memory_stats_t;

/* Uptime response: monotonic span since boot + wall-clock boot instant. */
typedef struct tairix_uptime {
    tairix_duration64_t since_boot;
    tairix_time64_t boot_time;
} tairix_uptime_t;

/* Load-average response; load1/5/15 are fixed-point with
   TAIRIX_SYSINFO_LOAD_FIXED_SHIFT fractional bits. */
typedef struct tairix_load_average {
    uint32_t load1;
    uint32_t load5;
    uint32_t load15;
    uint32_t runnable;
    uint32_t total_tasks;
    uint32_t users;
} tairix_load_average_t;

/* Machine identity response; the inline hostname is valid for hostname_len bytes. */
typedef struct tairix_system_identity {
    uint8_t machine_id[TAIRIX_MACHINE_ID_LEN];
    uint16_t version_major;
    uint16_t version_minor;
    uint16_t version_patch;
    uint8_t hostname_len;
    uint8_t hostname[TAIRIX_HOSTNAME_MAX];
} tairix_system_identity_t;

/* Mount-list request payload (offset/limit paging). */
typedef struct tairix_mount_list_request {
    uint32_t offset;
    uint16_t limit;
    uint16_t flags;
} tairix_mount_list_request_t;

/* One mount-table entry. `flags` is a MountFlags bitmap (AGENTS.md sec.5.3);
* its flag bits are defined by the filesystem driver ABI. `availability` is
* a TAIRIX_MOUNT_* state (a surprise-removed volume never reads as healthy).
* `usage` is the backing volume's space accounting (all-zero when none is
* known). `volume_id` is the volume's stable published identity (all-zero
* when the mount has none), the identity a volume_detach request names.
* The inline source/target/fstype buffers are valid for their respective
* *_len byte counts. */
typedef struct tairix_mount_record {
    uint32_t flags;
    uint8_t source_len;
    uint8_t target_len;
    uint8_t fstype_len;
    uint8_t availability;
    tairix_volume_stats_t usage;
    uint8_t volume_id[TAIRIX_MOUNT_VOLUME_ID_LEN];
    uint8_t source[TAIRIX_MOUNT_SOURCE_MAX];
    uint8_t target[TAIRIX_MOUNT_TARGET_MAX];
    uint8_t fstype[TAIRIX_MOUNT_FSTYPE_MAX];
} tairix_mount_record_t;

/* One row of the RESOURCE_LIMITS response: a resource's effective soft/hard
* bound (a tairix_resource_limit_t) and the caller's current live usage of it.
* The full response is TAIRIX_LIMIT_KIND_COUNT records in LimitKind order. */
typedef struct tairix_resource_limit_record {
    uint32_t kind;
    uint32_t reserved;
    tairix_resource_limit_t limit;
    uint64_t usage;
} tairix_resource_limit_record_t;

/* User-directory request payload (offset/limit paging). */
typedef struct tairix_user_directory_request {
    uint32_t offset;
    uint16_t limit;
    uint16_t flags;
} tairix_user_directory_request_t;

/* One account entry: the uid + username pairing, and nothing else (no
* credential material). The inline name is valid for name_len bytes. */
typedef struct tairix_user_directory_record {
    uint32_t uid;
    uint8_t name_len;
    uint8_t name[TAIRIX_USER_DIRECTORY_NAME_MAX];
} tairix_user_directory_record_t;

#endif /* TAIRIX_SYSINFO_H */

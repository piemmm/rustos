/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* System Information API surface (AGENTS.md sec.16.6).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_SYSINFO_H
#define ROS_SYSINFO_H

#include <stdint.h>
#include "rustos_time.h"
#include "rustos_rlimit.h"
#include "rustos_driver.h"

/* sysinfo protocol version tag for the frozen v1 surface. */
#define ROS_SYSINFO_VERSION_V1 1u
/* sysinfo protocol version this header set describes. */
#define ROS_SYSINFO_VERSION_CURRENT 1u
/* Magic word identifying a sysinfo-v1 request ("SYI1" little-endian). */
#define ROS_SYSINFO_REQUEST_MAGIC 0x31495953u
/* Maximum request/response payload length, in bytes, a header may advertise. */
#define ROS_SYSINFO_MAX_PAYLOAD_LEN 1048576u
/* Inclusive upper bound on the sysinfo-v1 query identifier space. */
#define ROS_SYSINFO_QUERY_ID_MAX 1023u

/* Canonical query-registry encoding constants (the hashable registry image). */
#define ROS_SYSINFO_QUERY_NAME_MAX 20u
#define ROS_SYSINFO_QUERY_RECORD_LEN 26u
#define ROS_SYSINFO_ENCODED_QUERY_TABLE_LEN 338u
#define ROS_SYSINFO_LOAD_FIXED_SHIFT 11u

/* Well-known sysinfo-v1 query identifiers (uint16_t). Do not renumber. */
#define ROS_SYSINFO_QUERY_SELF_PROCESS_LIST ((uint16_t)0u)
#define ROS_SYSINFO_QUERY_GLOBAL_PROCESS_LIST ((uint16_t)1u)
#define ROS_SYSINFO_QUERY_KERNEL_MEMORY_STATS ((uint16_t)2u)
#define ROS_SYSINFO_QUERY_HARDWARE_TREE ((uint16_t)3u)
#define ROS_SYSINFO_QUERY_SYSTEM_IDENTITY ((uint16_t)4u)
#define ROS_SYSINFO_QUERY_UPTIME ((uint16_t)5u)
#define ROS_SYSINFO_QUERY_MOUNT_LIST ((uint16_t)6u)
#define ROS_SYSINFO_QUERY_RESOURCE_LIMITS ((uint16_t)7u)
#define ROS_SYSINFO_QUERY_PROCESS_IDENTITY ((uint16_t)8u)
#define ROS_SYSINFO_QUERY_LOAD_AVERAGE ((uint16_t)9u)
#define ROS_SYSINFO_QUERY_USER_DIRECTORY ((uint16_t)10u)

/* Process lifecycle state carried in a process record (uint8_t). */
#define ROS_PROCESS_STATE_RUNNABLE ((uint8_t)0u)
#define ROS_PROCESS_STATE_RUNNING ((uint8_t)1u)
#define ROS_PROCESS_STATE_BLOCKED ((uint8_t)2u)
#define ROS_PROCESS_STATE_ZOMBIE ((uint8_t)3u)
#define ROS_PROCESS_STATE_STOPPED ((uint8_t)4u)
/* ros_process_record.cpu sentinel: the process is not currently scheduled. */
#define ROS_PROCESS_CPU_NONE ((uint8_t)255u)

/* Inline fixed-buffer capacities carried in the record types below. */
#define ROS_PROCESS_NAME_MAX 32u
#define ROS_MACHINE_ID_LEN 16u
#define ROS_HOSTNAME_MAX 64u
#define ROS_MOUNT_SOURCE_MAX 64u
#define ROS_MOUNT_TARGET_MAX 64u
#define ROS_MOUNT_FSTYPE_MAX 16u
#define ROS_USER_DIRECTORY_NAME_MAX 32u

/* Packed little-endian wire size of each sysinfo record type, in bytes. */
#define ROS_SYSINFO_REQUEST_HEADER_WIRE_LEN 24u
#define ROS_PROCESS_LIST_REQUEST_WIRE_LEN 8u
#define ROS_PROCESS_RECORD_WIRE_LEN 108u
#define ROS_KERNEL_MEMORY_STATS_WIRE_LEN 40u
#define ROS_UPTIME_WIRE_LEN 24u
#define ROS_LOAD_AVERAGE_WIRE_LEN 24u
#define ROS_SYSTEM_IDENTITY_WIRE_LEN 88u
#define ROS_MOUNT_LIST_REQUEST_WIRE_LEN 8u
#define ROS_MOUNT_RECORD_WIRE_LEN 200u
#define ROS_RESOURCE_LIMIT_RECORD_WIRE_LEN 32u
#define ROS_USER_DIRECTORY_REQUEST_WIRE_LEN 8u
#define ROS_USER_DIRECTORY_RECORD_WIRE_LEN 40u

/* Byte length of a full RESOURCE_LIMITS response: one record per LimitKind. */
#define ROS_SYSINFO_RESOURCE_LIMITS_REPORT_LEN 128u

/* Envelope prefixing every sysinfo request; encoded little-endian on the wire. */
typedef struct ros_sysinfo_request_header {
    uint32_t magic;
    uint16_t version;
    uint16_t flags;
    uint16_t query;
    uint16_t reserved;
    uint32_t payload_len;
    uint64_t request_id;
} ros_sysinfo_request_header_t;

/* Process-list request payload (offset/limit paging). */
typedef struct ros_process_list_request {
    uint32_t offset;
    uint16_t limit;
    uint16_t flags;
} ros_process_list_request_t;

/* One process entry. The numeric pid/parent_pid are reused across process
* lifetimes; proc_id/parent_proc_id are the kernel-attested, never-reused
* process-instance identities (correlate on those, not the numeric ids).
* `cpu` is ROS_PROCESS_CPU_NONE when the process is not currently
* scheduled; cpu_time_ns is the cumulative on-CPU time and mem_bytes the
* mapped address-space size. The inline name is valid for name_len bytes. */
typedef struct ros_process_record {
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
    uint8_t name[ROS_PROCESS_NAME_MAX];
} ros_process_record_t;

/* Kernel memory statistics response. */
typedef struct ros_kernel_memory_stats {
    uint64_t total_bytes;
    uint64_t free_bytes;
    uint64_t kernel_heap_bytes;
    uint64_t user_resident_bytes;
    uint32_t page_size;
    uint32_t reserved;
} ros_kernel_memory_stats_t;

/* Uptime response: monotonic span since boot + wall-clock boot instant. */
typedef struct ros_uptime {
    ros_duration64_t since_boot;
    ros_time64_t boot_time;
} ros_uptime_t;

/* Load-average response; load1/5/15 are fixed-point with
   ROS_SYSINFO_LOAD_FIXED_SHIFT fractional bits. */
typedef struct ros_load_average {
    uint32_t load1;
    uint32_t load5;
    uint32_t load15;
    uint32_t runnable;
    uint32_t total_tasks;
    uint32_t users;
} ros_load_average_t;

/* Machine identity response; the inline hostname is valid for hostname_len bytes. */
typedef struct ros_system_identity {
    uint8_t machine_id[ROS_MACHINE_ID_LEN];
    uint16_t version_major;
    uint16_t version_minor;
    uint16_t version_patch;
    uint8_t hostname_len;
    uint8_t hostname[ROS_HOSTNAME_MAX];
} ros_system_identity_t;

/* Mount-list request payload (offset/limit paging). */
typedef struct ros_mount_list_request {
    uint32_t offset;
    uint16_t limit;
    uint16_t flags;
} ros_mount_list_request_t;

/* One mount-table entry. `flags` is a MountFlags bitmap (AGENTS.md sec.5.3);
* its flag bits are defined by the filesystem driver ABI. `usage` is the
* backing volume's space accounting (all-zero when none is known). The
* inline source/target/fstype buffers are valid for their respective
* *_len byte counts. */
typedef struct ros_mount_record {
    uint32_t flags;
    uint8_t source_len;
    uint8_t target_len;
    uint8_t fstype_len;
    uint8_t reserved0;
    ros_volume_stats_t usage;
    uint8_t source[ROS_MOUNT_SOURCE_MAX];
    uint8_t target[ROS_MOUNT_TARGET_MAX];
    uint8_t fstype[ROS_MOUNT_FSTYPE_MAX];
} ros_mount_record_t;

/* One row of the RESOURCE_LIMITS response: a resource's effective soft/hard
* bound (a ros_resource_limit_t) and the caller's current live usage of it.
* The full response is ROS_LIMIT_KIND_COUNT records in LimitKind order. */
typedef struct ros_resource_limit_record {
    uint32_t kind;
    uint32_t reserved;
    ros_resource_limit_t limit;
    uint64_t usage;
} ros_resource_limit_record_t;

/* User-directory request payload (offset/limit paging). */
typedef struct ros_user_directory_request {
    uint32_t offset;
    uint16_t limit;
    uint16_t flags;
} ros_user_directory_request_t;

/* One account entry: the uid + username pairing, and nothing else (no
* credential material). The inline name is valid for name_len bytes. */
typedef struct ros_user_directory_record {
    uint32_t uid;
    uint8_t name_len;
    uint8_t name[ROS_USER_DIRECTORY_NAME_MAX];
} ros_user_directory_record_t;

#endif /* ROS_SYSINFO_H */

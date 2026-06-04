/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* IPC message header and port-name wire types (AGENTS.md sec.4).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_IPC_H
#define ROS_IPC_H

#include <stdint.h>

/* Magic word identifying an abi-v1 IPC message ("IPC1" little-endian). */
#define ROS_IPC_MESSAGE_HEADER_MAGIC 0x31435049u
/* Maximum payload length, in bytes, an IPC message header may advertise. */
#define ROS_IPC_MESSAGE_MAX_PAYLOAD_LEN 1048576u
/* Packed little-endian wire size of an IPC message header, in bytes. */
#define ROS_IPC_MESSAGE_HEADER_WIRE_LEN 32u

/* Maximum length, in bytes, of a port name (excludes the length byte). */
#define ROS_PORT_NAME_MAX_LEN 31u
/* Packed little-endian wire size of a port name, in bytes. */
#define ROS_PORT_NAME_WIRE_LEN 32u

/* IPC message header: prefixes every message; encoded little-endian on the wire. */
typedef struct ros_ipc_message_header {
    uint32_t magic;
    uint16_t version;
    uint16_t flags;
    uint64_t endpoint;
    uint64_t sender;
    uint32_t payload_len;
    uint32_t reserved;
} ros_ipc_message_header_t;

/* Validated well-known IPC port name: NUL-padded name bytes + a length byte. */
typedef struct ros_port_name {
    uint8_t bytes[ROS_PORT_NAME_MAX_LEN];
    uint8_t len;
} ros_port_name_t;

#endif /* ROS_IPC_H */

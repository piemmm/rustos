/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* IPC message header and port-name wire types (AGENTS.md sec.4).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_IPC_H
#define TAIRIX_IPC_H

#include <stdint.h>

/* Magic word identifying an abi-v1 IPC message ("IPC1" little-endian). */
#define TAIRIX_IPC_MESSAGE_HEADER_MAGIC 0x31435049u
/* Maximum payload length, in bytes, an IPC message header may advertise. */
#define TAIRIX_IPC_MESSAGE_MAX_PAYLOAD_LEN 1048576u
/* Packed little-endian wire size of an IPC message header, in bytes. */
#define TAIRIX_IPC_MESSAGE_HEADER_WIRE_LEN 32u

/* call_recv flags (uint32_t). Every undefined bit is reserved and must be zero. */
#define TAIRIX_CALL_RECV_FLAG_NON_BLOCKING 0x1u

/* Maximum length, in bytes, of a port name (excludes the length byte). */
#define TAIRIX_PORT_NAME_MAX_LEN 31u
/* Packed little-endian wire size of a port name, in bytes. */
#define TAIRIX_PORT_NAME_WIRE_LEN 32u

/* IPC message header: prefixes every message; encoded little-endian on the wire. */
typedef struct tairix_ipc_message_header {
    uint32_t magic;
    uint16_t version;
    uint16_t flags;
    uint64_t endpoint;
    uint64_t sender;
    uint32_t payload_len;
    uint32_t reserved;
} tairix_ipc_message_header_t;

/* Validated well-known IPC port name: NUL-padded name bytes + a length byte. */
typedef struct tairix_port_name {
    uint8_t bytes[TAIRIX_PORT_NAME_MAX_LEN];
    uint8_t len;
} tairix_port_name_t;

#endif /* TAIRIX_IPC_H */

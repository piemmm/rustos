/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Signed rxe manifest header (AGENTS.md sec.9).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_MANIFEST_H
#define TAIRIX_MANIFEST_H

#include <stdint.h>

/* Magic word identifying an abi-v1 manifest ("RXM1" little-endian). */
#define TAIRIX_MANIFEST_MAGIC 0x314d5852u
/* Maximum number of capability identifiers a manifest may request. */
#define TAIRIX_MANIFEST_MAX_CAPABILITIES 64u
/* Length, in bytes, of the linked syscall-table hash (SHA-256). */
#define TAIRIX_SYSCALL_TABLE_HASH_LEN 32u
/* Packed little-endian wire size of a manifest header, in bytes. */
#define TAIRIX_MANIFEST_HEADER_WIRE_LEN 144u

/* Signed rxe manifest prefix; encoded little-endian on the wire. */
typedef struct tairix_manifest_header {
    uint32_t magic;
    uint32_t abi_version;
    uint32_t flags;
    uint16_t capability_count;
    uint16_t reserved0;
    uint8_t syscall_table_hash[TAIRIX_SYSCALL_TABLE_HASH_LEN];
    uint8_t signer_pubkey[32];
    uint8_t signature[64];
} tairix_manifest_header_t;

#endif /* TAIRIX_MANIFEST_H */

/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* rxe load-image table and load-time hardening (AGENTS.md sec.9, sec.19.2).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_RXE_H
#define TAIRIX_RXE_H

#include <stdint.h>

/* Magic word identifying an abi-v1 load header ("RXEL" little-endian). */
#define TAIRIX_LOAD_MAGIC 0x4c455852u
/* Page size the load image is expressed in, in bytes. */
#define TAIRIX_RXE_PAGE_SIZE ((uint64_t)4096ull)
/* Maximum number of segment records a single load image may carry. */
#define TAIRIX_LOAD_MAX_SEGMENTS ((uintptr_t)64u)
/* Maximum number of needed-library references a load image may declare. */
#define TAIRIX_LOAD_MAX_NEEDED ((uintptr_t)8u)
/* Maximum length, in bytes, of a needed-library reference path. */
#define TAIRIX_LIBREF_MAX ((uintptr_t)255u)

/* Load-header flag bits (uint32_t). Every undefined bit must be zero. */
/* The image is position-independent (PIE); required by sec.19.2. */
#define TAIRIX_LOAD_FLAG_PIE 0x1u

/* Segment flag bits (uint32_t) in a packed segment record. */
#define TAIRIX_SEG_FLAG_READ 0x1u
#define TAIRIX_SEG_FLAG_WRITE 0x2u
#define TAIRIX_SEG_FLAG_EXEC 0x4u

/* Packed little-endian wire size of a load header, in bytes. */
#define TAIRIX_LOAD_HEADER_WIRE_LEN 56u
/* Packed little-endian wire size of one segment record, in bytes. */
#define TAIRIX_SEGMENT_WIRE_LEN 40u
/* Packed little-endian wire size of one needed-library record, in bytes. */
#define TAIRIX_NEEDED_LIBRARY_WIRE_LEN 256u

/* W^X-clean permission a segment is mapped with (uint8_t). */
#define TAIRIX_RXE_PERMISSION_READ_ONLY ((uint8_t)0u)
#define TAIRIX_RXE_PERMISSION_READ_EXECUTE ((uint8_t)1u)
#define TAIRIX_RXE_PERMISSION_READ_WRITE ((uint8_t)2u)

/* Fixed rxe load-image prefix; encoded little-endian on the wire. */
typedef struct tairix_load_header {
    uint32_t magic;
    uint32_t abi_version;
    uint32_t flags;
    uint16_t segment_count;
    uint16_t needed_count;
    uint64_t entry;
    uint8_t cfi_tag[32];
} tairix_load_header_t;

#endif /* TAIRIX_RXE_H */

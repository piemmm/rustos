/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* log_emit diagnostic-record ABI (AGENTS.md sec.19.4 / sec.20).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_LOG_H
#define TAIRIX_LOG_H

#include <stdint.h>

/*
 * Wire layout of a log_emit record (all scalars little-endian):
 *   offset 0: uint8_t  level        (0..=TAIRIX_LOG_LEVEL_MAX)
 *   offset 1: uint8_t  field_count  (<= TAIRIX_LOG_FIELDS_MAX)
 *   offset 2: uint16_t message_len  (<= TAIRIX_LOG_MESSAGE_MAX)
 *   offset 4: uint32_t event_id
 *   offset 8: message bytes (message_len, UTF-8)
 *   then field_count records, each:
 *     uint8_t key_len   (<= TAIRIX_LOG_FIELD_KEY_MAX)
 *     key bytes         (key_len, UTF-8)
 *     a typed field value: a 1-byte TAIRIX_FIELD_TAG_* discriminant
 *       followed by its little-endian payload. The whole encoded
 *       value is <= TAIRIX_LOG_FIELD_VALUE_MAX bytes. Payloads:
 *         NULL: none.  BOOL: 1 byte (0|1).
 *         SIGNED/UNSIGNED: 8 bytes.  TIME/DURATION: 12 bytes.
 *         DECIMAL: int64 mantissa + uint8 scale (9 bytes).
 *         STR/BYTES: uint16 len then len bytes.
 *         UUID: 16 bytes.  MAC: 6 bytes.
 *         IP: uint8 family (4|6) then 4 or 16 bytes.
 *         ERROR: int32.  CAP: uint16.
 *         LIST: uint8 elem-tag, uint16 count, then count payloads.
 */
/* Highest valid level byte (the Critical discriminant). */
#define TAIRIX_LOG_LEVEL_MAX ((uint8_t)5u)
/* Maximum message length, in bytes. */
#define TAIRIX_LOG_MESSAGE_MAX ((uintptr_t)120u)
/* Maximum number of structured key/value fields. */
#define TAIRIX_LOG_FIELDS_MAX ((uintptr_t)8u)
/* Maximum field key length, in bytes. */
#define TAIRIX_LOG_FIELD_KEY_MAX ((uintptr_t)32u)
/* Maximum encoded field-value length, in bytes (tag + payload). */
#define TAIRIX_LOG_FIELD_VALUE_MAX ((uintptr_t)256u)
/* Fixed record header length, in bytes. */
#define TAIRIX_LOG_RECORD_HEADER_LEN ((uintptr_t)8u)
/* Maximum encoded record length, in bytes. */
#define TAIRIX_LOG_RECORD_MAX ((uintptr_t)2440u)

/* Field-value type tags: the first byte of an encoded field value. */
#define TAIRIX_FIELD_TAG_NULL ((uint8_t)0u)
#define TAIRIX_FIELD_TAG_BOOL ((uint8_t)1u)
#define TAIRIX_FIELD_TAG_SIGNED ((uint8_t)2u)
#define TAIRIX_FIELD_TAG_UNSIGNED ((uint8_t)3u)
#define TAIRIX_FIELD_TAG_DECIMAL ((uint8_t)4u)
#define TAIRIX_FIELD_TAG_TIME ((uint8_t)5u)
#define TAIRIX_FIELD_TAG_DURATION ((uint8_t)6u)
#define TAIRIX_FIELD_TAG_STR ((uint8_t)7u)
#define TAIRIX_FIELD_TAG_BYTES ((uint8_t)8u)
#define TAIRIX_FIELD_TAG_UUID ((uint8_t)9u)
#define TAIRIX_FIELD_TAG_IP ((uint8_t)10u)
#define TAIRIX_FIELD_TAG_MAC ((uint8_t)11u)
#define TAIRIX_FIELD_TAG_ERROR ((uint8_t)12u)
#define TAIRIX_FIELD_TAG_CAP ((uint8_t)13u)
#define TAIRIX_FIELD_TAG_LIST ((uint8_t)14u)

#endif /* TAIRIX_LOG_H */

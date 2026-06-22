/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* log_emit diagnostic-record ABI (AGENTS.md sec.19.4 / sec.20).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_LOG_H
#define ROS_LOG_H

#include <stdint.h>

/*
 * Wire layout of a log_emit record (all scalars little-endian):
 *   offset 0: uint8_t  level        (0..=ROS_LOG_LEVEL_MAX)
 *   offset 1: uint8_t  field_count  (<= ROS_LOG_FIELDS_MAX)
 *   offset 2: uint16_t message_len  (<= ROS_LOG_MESSAGE_MAX)
 *   offset 4: uint32_t event_id
 *   offset 8: message bytes (message_len, UTF-8)
 *   then field_count records: uint8_t key_len, uint8_t value_len,
 *        key bytes, value bytes (both UTF-8).
 */
/* Highest valid level byte (the Error discriminant). */
#define ROS_LOG_LEVEL_MAX ((uint8_t)4u)
/* Maximum message length, in bytes. */
#define ROS_LOG_MESSAGE_MAX ((uintptr_t)120u)
/* Maximum number of structured key/value fields. */
#define ROS_LOG_FIELDS_MAX ((uintptr_t)8u)
/* Maximum field key length, in bytes. */
#define ROS_LOG_FIELD_KEY_MAX ((uintptr_t)32u)
/* Maximum field value length, in bytes. */
#define ROS_LOG_FIELD_VALUE_MAX ((uintptr_t)96u)
/* Fixed record header length, in bytes. */
#define ROS_LOG_RECORD_HEADER_LEN ((uintptr_t)8u)
/* Maximum encoded record length, in bytes. */
#define ROS_LOG_RECORD_MAX ((uintptr_t)1168u)

#endif /* ROS_LOG_H */

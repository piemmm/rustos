/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Stable abi-v1 error codes (Errno discriminants).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_ERROR_H
#define TAIRIX_ERROR_H

/* Stable abi-v1 error codes (int32_t). */
#define TAIRIX_E_BUFFER_TOO_SMALL 1
#define TAIRIX_E_BAD_ALIGNMENT 2
#define TAIRIX_E_BAD_MAGIC 3
#define TAIRIX_E_LENGTH_OUT_OF_RANGE 4
#define TAIRIX_E_OUT_OF_RANGE 5
#define TAIRIX_E_PERMISSION_DENIED 6
#define TAIRIX_E_NOT_FOUND 7
#define TAIRIX_E_DELEGATION_WIDEN 8
#define TAIRIX_E_SIGNATURE_INVALID 9
#define TAIRIX_E_ABI_VERSION_UNSUPPORTED 10
#define TAIRIX_E_MESSAGE_TOO_LARGE 11
#define TAIRIX_E_NOT_IMPLEMENTED 12
#define TAIRIX_E_TIMED_OUT 13
#define TAIRIX_E_TIMESTAMP_OUT_OF_RANGE 14
#define TAIRIX_E_NO_SPACE 15
#define TAIRIX_E_ENTROPY_NOT_READY 16
#define TAIRIX_E_ALREADY_EXISTS 17
#define TAIRIX_E_BAD_ADDRESS 18
#define TAIRIX_E_WOULD_BLOCK 19
#define TAIRIX_E_OUT_OF_MEMORY 20
#define TAIRIX_E_CROSS_VOLUME 21
#define TAIRIX_E_NOT_A_DIRECTORY 22
#define TAIRIX_E_NOT_EMPTY 23
#define TAIRIX_E_SEAT_BUSY 24
#define TAIRIX_E_SEAT_NOT_OWNER 25
#define TAIRIX_E_SEAT_REVOKED 26
#define TAIRIX_E_NOT_FOREGROUND 27
#define TAIRIX_E_BROKEN_PIPE 28
#define TAIRIX_E_ENDPOINT_STALLED 29
#define TAIRIX_E_DEVICE_FAULT 30
#define TAIRIX_E_NO_DATA 31
#define TAIRIX_E_NOT_SUPPORTED 32
#define TAIRIX_E_INTERRUPTED 33
#define TAIRIX_E_ADDRESS_IN_USE 34
#define TAIRIX_E_ADDRESS_UNAVAILABLE 35
#define TAIRIX_E_NETWORK_UNREACHABLE 36
#define TAIRIX_E_NOT_CONNECTED 37
#define TAIRIX_E_LIMIT_EXCEEDED 38
#define TAIRIX_E_MEDIUM_ERROR 39
#define TAIRIX_E_DEVICE_OFFLINE 40
#define TAIRIX_E_BUSY 41
#define TAIRIX_E_LINK_LOOP 42
#define TAIRIX_E_IS_A_DIRECTORY 43
#define TAIRIX_E_TOO_MANY_LINKS 44
#define TAIRIX_E_NOT_ATTACHED 45

#endif /* TAIRIX_ERROR_H */

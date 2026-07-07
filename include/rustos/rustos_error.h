/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Stable abi-v1 error codes (Errno discriminants).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_ERROR_H
#define ROS_ERROR_H

/* Stable abi-v1 error codes (int32_t). */
#define ROS_E_BUFFER_TOO_SMALL 1
#define ROS_E_BAD_ALIGNMENT 2
#define ROS_E_BAD_MAGIC 3
#define ROS_E_LENGTH_OUT_OF_RANGE 4
#define ROS_E_OUT_OF_RANGE 5
#define ROS_E_PERMISSION_DENIED 6
#define ROS_E_NOT_FOUND 7
#define ROS_E_DELEGATION_WIDEN 8
#define ROS_E_SIGNATURE_INVALID 9
#define ROS_E_ABI_VERSION_UNSUPPORTED 10
#define ROS_E_MESSAGE_TOO_LARGE 11
#define ROS_E_NOT_IMPLEMENTED 12
#define ROS_E_TIMED_OUT 13
#define ROS_E_TIMESTAMP_OUT_OF_RANGE 14
#define ROS_E_NO_SPACE 15
#define ROS_E_ENTROPY_NOT_READY 16
#define ROS_E_ALREADY_EXISTS 17
#define ROS_E_BAD_ADDRESS 18
#define ROS_E_WOULD_BLOCK 19
#define ROS_E_OUT_OF_MEMORY 20
#define ROS_E_CROSS_VOLUME 21
#define ROS_E_NOT_A_DIRECTORY 22
#define ROS_E_NOT_EMPTY 23
#define ROS_E_SEAT_BUSY 24
#define ROS_E_SEAT_NOT_OWNER 25
#define ROS_E_SEAT_REVOKED 26

#endif /* ROS_ERROR_H */

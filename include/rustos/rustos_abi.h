/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Umbrella header: the whole abi-v1 C surface in one include.
* Each syscall is exported by the user-space stub library under the
* symbol `ros_sys_<name>` (e.g. `ros_sys_ipc_send`); link against
* that library to call the kernel from a non-Rust program.
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_ABI_H
#define ROS_ABI_H

/* ABI version this header set describes (AGENTS.md sec.9). */
#define ROS_ABI_VERSION 1u

#include "rustos_error.h"
#include "rustos_capability.h"
#include "rustos_time.h"
#include "rustos_random.h"
#include "rustos_rlimit.h"
#include "rustos_memory.h"
#include "rustos_hwtree.h"
#include "rustos_ipc.h"
#include "rustos_stdinfo.h"
#include "rustos_manifest.h"
#include "rustos_input.h"
#include "rustos_appinfo.h"
#include "rustos_rxe.h"
#include "rustos_process.h"
#include "rustos_sysinfo.h"
#include "rustos_driver.h"
#include "rustos_syscall.h"

#endif /* ROS_ABI_H */

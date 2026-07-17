/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Umbrella header: the whole abi-v1 C surface in one include.
* Each syscall is exported by the user-space stub library under the
* symbol `tairix_sys_<name>` (e.g. `tairix_sys_ipc_send`); link against
* that library to call the kernel from a non-Rust program.
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_ABI_H
#define TAIRIX_ABI_H

/* ABI version this header set describes (AGENTS.md sec.9). */
#define TAIRIX_ABI_VERSION 1u

#include "tairix_error.h"
#include "tairix_capability.h"
#include "tairix_time.h"
#include "tairix_random.h"
#include "tairix_log.h"
#include "tairix_rlimit.h"
#include "tairix_memory.h"
#include "tairix_hwtree.h"
#include "tairix_ipc.h"
#include "tairix_stdinfo.h"
#include "tairix_manifest.h"
#include "tairix_input.h"
#include "tairix_appinfo.h"
#include "tairix_rxe.h"
#include "tairix_process.h"
#include "tairix_sysinfo.h"
#include "tairix_driver.h"
#include "tairix_syscall.h"

#endif /* TAIRIX_ABI_H */

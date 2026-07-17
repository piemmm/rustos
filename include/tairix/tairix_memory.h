/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Anonymous-memory mem_map flag bits (plans/SPAWN.md SP5).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_MEMORY_H
#define TAIRIX_MEMORY_H

#include <stdint.h>

/* mem_map flags (uint32_t). Every undefined bit is reserved and must be zero. */
#define TAIRIX_MAP_FLAG_FIXED 0x1u

#endif /* TAIRIX_MEMORY_H */

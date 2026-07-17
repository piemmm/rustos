/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Desktop pointer and keyboard input ABI (AGENTS.md sec.9, sec.10).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_INPUT_H
#define TAIRIX_INPUT_H

#include <stdint.h>

#define TAIRIX_POINTER_INPUT_MAGIC 0x314e4950u
#define TAIRIX_KEY_INPUT_MAGIC 0x314e494bu
#define TAIRIX_POINTER_INPUT_WIRE_LEN 20u
#define TAIRIX_KEY_INPUT_WIRE_LEN 20u

/* Record `kind` codes: pointer moves/clicks then key down/up (uint16_t). */
#define TAIRIX_INPUT_KIND_MOVED_BY ((uint16_t)0u)
#define TAIRIX_INPUT_KIND_PRESSED ((uint16_t)1u)
#define TAIRIX_INPUT_KIND_RELEASED ((uint16_t)2u)
#define TAIRIX_INPUT_KIND_KEY_PRESSED ((uint16_t)1u)
#define TAIRIX_INPUT_KIND_KEY_RELEASED ((uint16_t)2u)

/* `button` (motion=none, else a button) and keyboard `key_class` codes (uint16_t). */
#define TAIRIX_INPUT_BUTTON_NONE ((uint16_t)0u)
#define TAIRIX_POINTER_BUTTON_PRIMARY ((uint16_t)1u)
#define TAIRIX_POINTER_BUTTON_SECONDARY ((uint16_t)2u)
#define TAIRIX_POINTER_BUTTON_MIDDLE ((uint16_t)3u)
#define TAIRIX_KEY_CLASS_CHAR ((uint16_t)0u)
#define TAIRIX_KEY_CLASS_NAMED ((uint16_t)1u)

/* Modifier bits held while a key event was produced (uint16_t). */
#define TAIRIX_MOD_SHIFT ((uint16_t)0x1u)
#define TAIRIX_MOD_CTRL ((uint16_t)0x2u)
#define TAIRIX_MOD_ALT ((uint16_t)0x4u)
#define TAIRIX_MOD_META ((uint16_t)0x8u)
#define TAIRIX_MOD_MASK ((uint16_t)0xfu)

/* Named non-character key codes carried in a record's `named` field (uint16_t). */
#define TAIRIX_KEY_ENTER ((uint16_t)1u)
#define TAIRIX_KEY_ESCAPE ((uint16_t)2u)
#define TAIRIX_KEY_BACKSPACE ((uint16_t)3u)
#define TAIRIX_KEY_TAB ((uint16_t)4u)
#define TAIRIX_KEY_DELETE ((uint16_t)5u)
#define TAIRIX_KEY_INSERT ((uint16_t)6u)
#define TAIRIX_KEY_HOME ((uint16_t)7u)
#define TAIRIX_KEY_END ((uint16_t)8u)
#define TAIRIX_KEY_PAGE_UP ((uint16_t)9u)
#define TAIRIX_KEY_PAGE_DOWN ((uint16_t)10u)
#define TAIRIX_KEY_LEFT ((uint16_t)11u)
#define TAIRIX_KEY_RIGHT ((uint16_t)12u)
#define TAIRIX_KEY_UP ((uint16_t)13u)
#define TAIRIX_KEY_DOWN ((uint16_t)14u)
#define TAIRIX_KEY_F1 ((uint16_t)15u)
#define TAIRIX_KEY_F2 ((uint16_t)16u)
#define TAIRIX_KEY_F3 ((uint16_t)17u)
#define TAIRIX_KEY_F4 ((uint16_t)18u)
#define TAIRIX_KEY_F5 ((uint16_t)19u)
#define TAIRIX_KEY_F6 ((uint16_t)20u)
#define TAIRIX_KEY_F7 ((uint16_t)21u)
#define TAIRIX_KEY_F8 ((uint16_t)22u)
#define TAIRIX_KEY_F9 ((uint16_t)23u)
#define TAIRIX_KEY_F10 ((uint16_t)24u)
#define TAIRIX_KEY_F11 ((uint16_t)25u)
#define TAIRIX_KEY_F12 ((uint16_t)26u)

#endif /* TAIRIX_INPUT_H */

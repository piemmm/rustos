/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Architecture-neutral hardware tree (AGENTS.md sec.18.1).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_HWTREE_H
#define TAIRIX_HWTREE_H

#include <stdint.h>

/* Hardware-tree ABI version. */
#define TAIRIX_HWTREE_VERSION 1u
/* Parent id marking a node with no parent (a tree root). */
#define TAIRIX_HW_NODE_ROOT 4294967295u

/* Array bounds. */
#define TAIRIX_HW_COMPATIBLE_MAX ((uintptr_t)64u)
#define TAIRIX_HW_NODE_MAX_MATCH_KEYS ((uintptr_t)4u)
#define TAIRIX_HW_NODE_MAX_RESOURCES ((uintptr_t)8u)

/* Packed little-endian wire sizes, in bytes. */
#define TAIRIX_HW_MATCH_KEY_WIRE_LEN 76u
#define TAIRIX_HW_RESOURCE_WIRE_LEN 32u
#define TAIRIX_HW_NODE_WIRE_LEN 577u

/* Device classes (uint16_t). */
#define TAIRIX_HW_CLASS_ROOT ((uint16_t)0u)
#define TAIRIX_HW_CLASS_BUS ((uint16_t)1u)
#define TAIRIX_HW_CLASS_CPU ((uint16_t)2u)
#define TAIRIX_HW_CLASS_MEMORY ((uint16_t)3u)
#define TAIRIX_HW_CLASS_TIMER ((uint16_t)4u)
#define TAIRIX_HW_CLASS_INTERRUPT_CONTROLLER ((uint16_t)5u)
#define TAIRIX_HW_CLASS_DISPLAY ((uint16_t)6u)
#define TAIRIX_HW_CLASS_INPUT ((uint16_t)7u)
#define TAIRIX_HW_CLASS_NETWORK ((uint16_t)8u)
#define TAIRIX_HW_CLASS_STORAGE ((uint16_t)9u)
#define TAIRIX_HW_CLASS_SERIAL ((uint16_t)10u)
#define TAIRIX_HW_CLASS_OTHER ((uint16_t)65535u)

/* Match-key kinds (uint16_t). */
#define TAIRIX_HW_MATCH_COMPATIBLE ((uint16_t)0u)
#define TAIRIX_HW_MATCH_PCI ((uint16_t)1u)
#define TAIRIX_HW_MATCH_USB ((uint16_t)2u)
#define TAIRIX_HW_MATCH_VIRTIO ((uint16_t)3u)

/* Resource kinds (uint16_t). */
#define TAIRIX_HW_RES_MMIO ((uint16_t)0u)
#define TAIRIX_HW_RES_IRQ ((uint16_t)1u)
#define TAIRIX_HW_RES_PORT ((uint16_t)2u)
#define TAIRIX_HW_RES_DMA ((uint16_t)3u)
#define TAIRIX_HW_RES_BUS_WINDOW ((uint16_t)4u)
#define TAIRIX_HW_RES_ENDPOINT ((uint16_t)5u)
#define TAIRIX_HW_RES_SHARED ((uint16_t)6u)
#define TAIRIX_HW_RES_FRAMEBUFFER ((uint16_t)7u)

/* One match key on a node. Mirrors the #[repr(C)] layout; the packed
* little-endian wire size is TAIRIX_HW_MATCH_KEY_WIRE_LEN. */
typedef struct tairix_hw_match_key {
    uint16_t kind;
    uint8_t compatible_len;
    uint16_t vendor;
    uint16_t product;
    uint32_t class_code;
    uint8_t compatible[TAIRIX_HW_COMPATIBLE_MAX];
} tairix_hw_match_key_t;

/* One resource a node exposes, as a capability-grant request. */
typedef struct tairix_hw_resource {
    uint16_t kind;
    uint16_t capability;
    uint32_t flags;
    uint64_t base;
    uint64_t length;
    uint64_t translated_base;
} tairix_hw_resource_t;

/* One node in the hardware tree. Mirrors the #[repr(C)] layout; the
* packed little-endian wire size is TAIRIX_HW_NODE_WIRE_LEN. */
typedef struct tairix_hw_node {
    uint32_t id;
    uint32_t parent;
    uint16_t device_class;
    uint8_t match_key_count;
    uint8_t resource_count;
    tairix_hw_match_key_t match_keys[TAIRIX_HW_NODE_MAX_MATCH_KEYS];
    tairix_hw_resource_t resources[TAIRIX_HW_NODE_MAX_RESOURCES];
} tairix_hw_node_t;

#endif /* TAIRIX_HWTREE_H */

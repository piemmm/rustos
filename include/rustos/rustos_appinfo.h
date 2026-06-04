/*
* RustOS abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Application-bundle manifest ABI (AGENTS.md sec.16.5, sec.16.4).
*
* This is part of the C-language view of the RustOS kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef ROS_APPINFO_H
#define ROS_APPINFO_H

#include <stdint.h>

/* Magic word identifying an abi-v1 AppInfo manifest ("RAI1" little-endian). */
#define ROS_APPINFO_MAGIC 0x31494152u
/* Maximum number of capability identifiers a manifest may request. */
#define ROS_APPINFO_MAX_CAPABILITIES 64u
/* Maximum number of MIME / file-type associations a bundle may declare. */
#define ROS_APPINFO_MAX_MIME 32u
/* Maximum length, in bytes, of a bundle identifier. */
#define ROS_BUNDLE_ID_MAX 64u
/* Maximum length, in bytes, of a bundle's human-readable name. */
#define ROS_BUNDLE_NAME_MAX 64u
/* Maximum length, in bytes, of a bundle version string. */
#define ROS_BUNDLE_VERSION_MAX 32u
/* Maximum length, in bytes, of one declared MIME-type string. */
#define ROS_MIME_TYPE_MAX 64u
/* Encoded length of one MIME-type body entry (length byte + buffer). */
#define ROS_MIME_ENTRY_LEN 65u
/* Packed little-endian wire size of an AppInfo header, in bytes. */
#define ROS_APPINFO_HEADER_WIRE_LEN 340u

/* Curated, OS-provided shared-library directory (AGENTS.md sec.16.4). */
#define ROS_SYSTEM_LIBRARIES_DIR "/System/Libraries"

/* Fixed set of names permitted at a bundle's top level (AGENTS.md sec.16.5). */
#define ROS_BUNDLE_ENTRY_APPINFO "AppInfo"
#define ROS_BUNDLE_ENTRY_RUN "Run"
#define ROS_BUNDLE_ENTRY_CODE "Code"
#define ROS_BUNDLE_ENTRY_LIBRARIES "Libraries"
#define ROS_BUNDLE_ENTRY_RESOURCES "Resources"
#define ROS_BUNDLE_ENTRY_DEFAULTSETTINGS "DefaultSettings"
#define ROS_BUNDLE_ENTRY_DOCUMENTATION "Documentation"

/* Which permitted root a shared-library reference resolved against (uint8_t). */
#define ROS_LIBRARY_SCOPE_BUNDLE ((uint8_t)0u)
#define ROS_LIBRARY_SCOPE_SYSTEM ((uint8_t)1u)

/* Signed AppInfo manifest prefix; encoded little-endian on the wire. */
typedef struct ros_appinfo_header {
    uint32_t magic;
    uint32_t abi_version;
    uint32_t flags;
    uint16_t capability_count;
    uint16_t mime_count;
    uint8_t id_len;
    uint8_t name_len;
    uint8_t version_len;
    uint8_t reserved0;
    uint8_t id[ROS_BUNDLE_ID_MAX];
    uint8_t name[ROS_BUNDLE_NAME_MAX];
    uint8_t version[ROS_BUNDLE_VERSION_MAX];
    uint8_t syscall_table_hash[32];
    uint8_t content_hash[32];
    uint8_t signer_pubkey[32];
    uint8_t signature[64];
} ros_appinfo_header_t;

#endif /* ROS_APPINFO_H */

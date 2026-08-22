/*
* TAIRiX abi-v1 C development header.
*
* GENERATED FILE - DO NOT EDIT BY HAND.
*
* Application-bundle manifest ABI (AGENTS.md sec.16.5, sec.16.4).
*
* This is part of the C-language view of the TAIRiX kernel/user ABI.
* It is generated from the single source of truth in `lib/abi` by
* `cargo xtask c-header --write` and verified on every CI run by
* `cargo xtask c-header`. Edit `lib/abi` and regenerate; never edit
* this file directly (AGENTS.md sec.2.2, sec.9).
*/

#ifndef TAIRIX_APPINFO_H
#define TAIRIX_APPINFO_H

#include <stdint.h>
#include "tairix_manifest.h"

/* Magic word identifying an abi-v1 AppInfo manifest ("RAI1" little-endian). */
#define TAIRIX_APPINFO_MAGIC 0x31494152u
/* Maximum number of capability identifiers a manifest may request. */
#define TAIRIX_APPINFO_MAX_CAPABILITIES 64u
/* Maximum number of MIME / file-type associations a bundle may declare. */
#define TAIRIX_APPINFO_MAX_MIME 32u
/* Maximum length, in bytes, of a bundle identifier. */
#define TAIRIX_BUNDLE_ID_MAX 64u
/* Maximum length, in bytes, of a bundle's human-readable name. */
#define TAIRIX_BUNDLE_NAME_MAX 64u
/* Maximum length, in bytes, of a bundle version string. */
#define TAIRIX_BUNDLE_VERSION_MAX 32u
/* Maximum length, in bytes, of one declared MIME-type string. */
#define TAIRIX_MIME_TYPE_MAX 64u
/* Encoded length of one MIME-type body entry (length byte + buffer). */
#define TAIRIX_MIME_ENTRY_LEN 65u
/* Maximum length, in bytes, of a bundle's library icon asset name. */
#define TAIRIX_LIBRARY_ICON_MAX 64u
/* Maximum length, in bytes, of a bundle's one-line purpose. */
#define TAIRIX_BUNDLE_PURPOSE_MAX 96u
/* Maximum length, in bytes, of a bundle's author attribution. */
#define TAIRIX_BUNDLE_AUTHOR_MAX 64u
/* Packed little-endian wire size of an AppInfo header, in bytes. */
#define TAIRIX_APPINFO_HEADER_WIRE_LEN 664u

/* Curated, OS-provided shared-library directory (AGENTS.md sec.16.4). */
#define TAIRIX_SYSTEM_LIBRARIES_DIR "/System/Libraries"

/* Fixed set of names permitted at a bundle's top level (AGENTS.md sec.16.5). */
#define TAIRIX_BUNDLE_ENTRY_APPINFO "AppInfo"
#define TAIRIX_BUNDLE_ENTRY_RUN "Run"
#define TAIRIX_BUNDLE_ENTRY_CODE "Code"
#define TAIRIX_BUNDLE_ENTRY_LIBRARIES "Libraries"
#define TAIRIX_BUNDLE_ENTRY_RESOURCES "Resources"
#define TAIRIX_BUNDLE_ENTRY_DEFAULTSETTINGS "DefaultSettings"
#define TAIRIX_BUNDLE_ENTRY_HELP "Help"

/* Which permitted root a shared-library reference resolved against (uint8_t). */
#define TAIRIX_LIBRARY_SCOPE_BUNDLE ((uint8_t)0u)
#define TAIRIX_LIBRARY_SCOPE_SYSTEM ((uint8_t)1u)

/* Program-library listing wire byte (`library` field): not listed, or the
 * folder the bundle files itself under (uint8_t). */
#define TAIRIX_APPINFO_LIBRARY_NONE ((uint8_t)0u)
#define TAIRIX_APPINFO_LIBRARY_ACCESSORIES ((uint8_t)1u)
#define TAIRIX_APPINFO_LIBRARY_GRAPHICS ((uint8_t)2u)
#define TAIRIX_APPINFO_LIBRARY_INTERNET ((uint8_t)3u)
#define TAIRIX_APPINFO_LIBRARY_MULTIMEDIA ((uint8_t)4u)
#define TAIRIX_APPINFO_LIBRARY_OFFICE ((uint8_t)5u)
#define TAIRIX_APPINFO_LIBRARY_PROGRAMMING ((uint8_t)6u)
#define TAIRIX_APPINFO_LIBRARY_GAMES ((uint8_t)7u)
#define TAIRIX_APPINFO_LIBRARY_SYSTEMTOOLS ((uint8_t)8u)
#define TAIRIX_APPINFO_LIBRARY_UTILITIES ((uint8_t)9u)
#define TAIRIX_APPINFO_LIBRARY_OTHER ((uint8_t)10u)

/* Signed AppInfo manifest prefix; encoded little-endian on the wire. */
typedef struct tairix_appinfo_header {
    uint32_t magic;
    uint32_t abi_version;
    uint32_t flags;
    uint16_t capability_count;
    uint16_t mime_count;
    uint8_t id_len;
    uint8_t name_len;
    uint8_t version_len;
    uint8_t library_icon_len;
    uint8_t library;
    uint8_t purpose_len;
    uint8_t author_len;
    uint8_t reserved0[1];
    uint8_t id[TAIRIX_BUNDLE_ID_MAX];
    uint8_t name[TAIRIX_BUNDLE_NAME_MAX];
    uint8_t version[TAIRIX_BUNDLE_VERSION_MAX];
    uint8_t library_icon[TAIRIX_LIBRARY_ICON_MAX];
    uint8_t purpose[TAIRIX_BUNDLE_PURPOSE_MAX];
    uint8_t author[TAIRIX_BUNDLE_AUTHOR_MAX];
    uint8_t syscall_table_hash[TAIRIX_SYSCALL_TABLE_HASH_LEN];
    uint8_t content_hash[32];
    uint8_t signer_pubkey[32];
    uint8_t publisher_pubkey[32];
    uint8_t publisher_cert[64];
    uint8_t signature[64];
} tairix_appinfo_header_t;
#endif /* TAIRIX_APPINFO_H */

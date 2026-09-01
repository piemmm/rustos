//! Boot-time system-configuration loader.
//!
//! The operator's boot-time policy lives in one text document on the
//! encrypted root, `/System/Settings/Configuration/system.conf`, whose single
//! definition is the [`tairix_sysconfig`] engine. This module is the kernel's
//! reader of the cache-policy portion of that store: as the encrypted root is
//! unlocked it reads the document off the just-mounted volume, parses it, and
//! applies the operator's caching switches to the live cache-admission control
//! ([`CACHE_CONTROL`](crate::cache_control::CACHE_CONTROL)).
//!
//! It mirrors the `/System/Security` database readers ([`crate::users`],
//! [`crate::groups`]): the same bounded, fail-closed `read_bootstrap_file`
//! under the kernel's capability-less uid-0 bootstrap identity, and the same
//! "audit the outcome, never abort the boot" posture.
//!
//! # Fail-safe, not fail-closed
//!
//! Unlike a security database, an absent or malformed configuration store is
//! **not** an error that stops the system: every SMARTRAM cache is a
//! reclaimable accelerator that is never the source of truth, so the safe
//! fallback is the all-caches-enabled default (today's behaviour). A missing
//! store is the normal fresh-install case; a present-but-malformed store is
//! audited as a rejection and the defaults are applied. The unlock never fails
//! over the cache policy.

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use tairix_log::{FieldValue, Level, Sink};
use tairix_sysconfig::{CacheClass, SystemConfig, CONFIG_PATH, MAX_CONFIG_LEN};

use crate::audit::{emit, AuditEvent};
use crate::cache_control::CacheControl;
use crate::fs::{read_bootstrap_file, BootstrapReadError, VfsError};

/// Read `system.conf` off the mounted root `fs`, parse it, and apply the
/// operator's caching switches to `control` and the pointer-button chatter
/// window to the seat's live control.
///
/// Production passes the process-global
/// [`CACHE_CONTROL`](crate::cache_control::CACHE_CONTROL) — the very control
/// every cache consults — so the operator's policy takes effect on each
/// cache's next operation. Called once from the encrypted-root unlock, right
/// after the users and groups databases load. The store text carries no
/// credential bytes (it is the public configuration document), so the read
/// buffer needs no zeroisation.
///
/// Never fails: an absent store applies the [`SystemConfig::default`]
/// (all caches enabled) and audits [`AuditEvent::SystemConfigApplied`] with
/// `source: default`; a present-but-malformed store applies the same defaults
/// and audits [`AuditEvent::SystemConfigRejected`]. A well-formed store is
/// applied and audited [`AuditEvent::SystemConfigApplied`] with
/// `source: store`.
pub fn load_and_apply_system_config<F>(fs: &mut F, control: &CacheControl, audit: &dyn Sink)
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let (config, source) = match read_bootstrap_file(fs, CONFIG_PATH, MAX_CONFIG_LEN) {
        Ok(buf) => match core::str::from_utf8(&buf).map(SystemConfig::parse) {
            Ok(Ok(config)) => (config, "store"),
            Ok(Err(_)) => {
                audit_rejected(audit, "malformed");
                (SystemConfig::default(), "default")
            }
            Err(_) => {
                audit_rejected(audit, "not_utf8");
                (SystemConfig::default(), "default")
            }
        },
        // A missing store is the normal default case, not a rejection.
        Err(BootstrapReadError::Vfs(VfsError::NotFound)) => (SystemConfig::default(), "default"),
        Err(err) => {
            audit_rejected(audit, read_error_cause(err));
            (SystemConfig::default(), "default")
        }
    };

    control.apply(&config);
    // The pointer-button chatter window is seat policy, applied to the same
    // process-global control every seat reads on each button edge.
    crate::seat::CLICK_DEBOUNCE.set_ms(config.input_mouse_debounce_ms);
    audit_applied(audit, &config, source);
}

/// The stable `cause` string for a bootstrap-read refusal other than a
/// missing file (which is the normal default case, never a rejection).
const fn read_error_cause(err: BootstrapReadError) -> &'static str {
    match err {
        BootstrapReadError::Vfs(_) => "read_error",
        BootstrapReadError::NotAFile => "not_a_file",
        BootstrapReadError::TooLarge => "too_large",
        BootstrapReadError::ShortRead => "short_read",
    }
}

/// Audit the applied cache policy: the store `source` and the effective mode
/// of each cache class (the master `cache.all` ceiling already folded in).
fn audit_applied(audit: &dyn Sink, config: &SystemConfig, source: &'static str) {
    emit(
        audit,
        Level::Info,
        AuditEvent::SystemConfigApplied,
        &[
            tairix_log::Field {
                key: "source",
                value: FieldValue::Str(source),
            },
            class_field("cache.filesystem", config, CacheClass::Filesystem),
            class_field("cache.block", config, CacheClass::Block),
            class_field("cache.transform", config, CacheClass::Transform),
            class_field("cache.semantic", config, CacheClass::Semantic),
        ],
    );
}

/// One audit field naming a cache class's effective mode (`auto` / `off`).
fn class_field<'a>(
    key: &'a str,
    config: &SystemConfig,
    class: CacheClass,
) -> tairix_log::Field<'a> {
    tairix_log::Field {
        key,
        value: FieldValue::Str(config.effective_cache(class).as_str()),
    }
}

/// Audit a store that was present but could not be read or parsed; the
/// defaults are applied instead (fail-safe).
fn audit_rejected(audit: &dyn Sink, cause: &'static str) {
    emit(
        audit,
        Level::Warn,
        AuditEvent::SystemConfigRejected,
        &[
            tairix_log::Field {
                key: "path",
                value: FieldValue::Str(CONFIG_PATH),
            },
            tairix_log::Field {
                key: "cause",
                value: FieldValue::Str(cause),
            },
        ],
    );
}

#[cfg(test)]
#[path = "syscfg_tests.rs"]
mod tests;

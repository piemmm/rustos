//! `cargo xtask devids` implementation.
//!
//! The `lspci`/`lsusb` command apps name devices through `lib/devids`, whose
//! data is vetted snapshots of the public PCI and USB ID databases
//! (plans/DEVICES.md DEVICE1). This command owns the import pipeline; the
//! grammar, vetting filter, and table codec live in `lib/devids` itself
//! (one definition, shared with the runtime consumers).
//!
//! - `cargo xtask devids` (no arguments; part of `cargo xtask ci`) re-runs
//!   the converter over the committed snapshots in `lib/devids/assets/` and
//!   fails closed on any drift against the committed tables (each inside its
//!   consuming command bundle's `Resources/` —
//!   `userland/apps/lspci/Resources/pci.ids.bin`,
//!   `userland/apps/lsusb/Resources/usb.ids.bin`), exactly like `c-header`
//!   and `font-atlas`.
//! - `cargo xtask devids --write` regenerates the committed tables from the
//!   committed snapshots.
//! - `cargo xtask devids --fetch` is **developer-run only** — builds stay
//!   offline and reproducible, so neither CI nor any build step ever
//!   touches the network. It downloads both databases, runs the `lib/devids`
//!   vetting filter, rewrites the committed snapshots with a provenance
//!   header (upstream URL/version/date, fetch date, SHA-256 of the raw
//!   download, licence statement, transport), and regenerates the tables so
//!   the tree stays self-consistent. The refresh diff is human-reviewed like
//!   any other change.
//!
//! Transport honesty: `pci.ids` is fetched over verified TLS. The `usb.ids`
//! upstream (`linux-usb.org` and its mirrors) publishes no valid TLS
//! endpoint, so it is fetched over the canonical HTTP URL upstream
//! documents; integrity comes from the recorded SHA-256 and the human
//! review of the snapshot diff, and the runtime never refetches (databases
//! update only through the signed system-update path).
//!
//! Encoding honesty: today's `usb.ids` carries the odd stray ISO-8859-1
//! byte in an otherwise UTF-8 file. The fetch repairs exactly the byte
//! sequences that are not valid UTF-8 by promoting each byte to its
//! Latin-1 code point (valid UTF-8 elsewhere is left untouched), records
//! the repair in the provenance header, and then vets the repaired text
//! strictly.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command as Process;
use std::time::{SystemTime, UNIX_EPOCH};

use tairix_devids::{textdb, DbKind};

use crate::Context;

/// One database's import pinning: where it comes from, where it lives, and
/// what the provenance header records about it.
struct Database {
    kind: DbKind,
    /// Human name used in messages and the provenance header.
    name: &'static str,
    /// Fetch URL (see the module docs for the transport rationale).
    url: &'static str,
    /// Provenance-header transport statement.
    transport: &'static str,
    /// Provenance-header licence statement.
    licence: &'static str,
    /// Workspace-relative committed snapshot path.
    snapshot: &'static str,
    /// Workspace-relative committed compact-table path: the consuming
    /// command bundle's `Resources/` file, so the table ships inside the
    /// self-contained bundle with no second copy in the tree.
    table: &'static str,
}

/// The two imported databases.
const DATABASES: &[Database] = &[
    Database {
        kind: DbKind::Pci,
        name: "pci.ids",
        url: "https://pci-ids.ucw.cz/v2.2/pci.ids",
        transport: "HTTPS (TLS verified)",
        licence: "dual GPL-2.0-or-later / BSD-3-Clause (upstream header below)",
        snapshot: "lib/devids/assets/pci.ids",
        table: "userland/apps/lspci/Resources/pci.ids.bin",
    },
    Database {
        kind: DbKind::Usb,
        name: "usb.ids",
        url: "http://www.linux-usb.org/usb.ids",
        transport: "HTTP (upstream publishes no valid TLS endpoint; integrity \
                    is the SHA-256 below plus human review of this diff)",
        licence: "GPL-2.0-or-later (as distributed with usbutils; the upstream \
                  file carries no licence header)",
        snapshot: "lib/devids/assets/usb.ids",
        table: "userland/apps/lsusb/Resources/usb.ids.bin",
    },
];

/// Entry point for `cargo xtask devids [--fetch | --write]`.
pub fn run(ctx: &Context, args: &[std::ffi::OsString]) -> Result<(), String> {
    let mut fetch = false;
    let mut write = false;
    for arg in args {
        match arg.to_str() {
            Some("--fetch") => fetch = true,
            Some("--write") => write = true,
            _ => {
                return Err(format!(
                    "devids: unexpected argument {}; usage: cargo xtask devids [--fetch | --write]",
                    arg.display()
                ));
            }
        }
    }
    if fetch {
        eprintln!("xtask: [devids --fetch] importing upstream databases");
        return run_fetch(ctx);
    }
    if write {
        eprintln!("xtask: [devids --write] regenerating the compact tables");
        return run_write(ctx);
    }
    eprintln!("xtask: [devids] verifying lib/devids snapshot/table sync");
    run_verify(ctx)
}

/// Parse one committed snapshot and encode its table.
fn compile(root: &Path, db: &Database) -> Result<Vec<u8>, String> {
    let path = root.join(db.snapshot);
    let bytes =
        std::fs::read(&path).map_err(|e| format!("devids: cannot read {}: {e}", path.display()))?;
    let parsed = textdb::parse(db.kind, &bytes)
        .map_err(|e| format!("devids: {} fails vetting: {e}", path.display()))?;
    Ok(parsed.encode())
}

/// Regenerate the committed tables from the committed snapshots (`--write`).
fn run_write(ctx: &Context) -> Result<(), String> {
    for db in DATABASES {
        let encoded = compile(&ctx.workspace_root, db)?;
        let path = ctx.workspace_root.join(db.table);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("devids: cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, &encoded)
            .map_err(|e| format!("devids: cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Verify the committed tables match a fresh compile of the committed
/// snapshots (the `ci` drift guard). Fails closed with the regeneration
/// command on any mismatch.
fn run_verify(ctx: &Context) -> Result<(), String> {
    for db in DATABASES {
        let encoded = compile(&ctx.workspace_root, db)?;
        let path = ctx.workspace_root.join(db.table);
        let committed = std::fs::read(&path).map_err(|_| drifted(db))?;
        if committed != encoded {
            return Err(drifted(db));
        }
    }
    Ok(())
}

fn drifted(db: &Database) -> String {
    format!(
        "devids: `{}` is out of sync with `{}`; \
         run `cargo xtask devids --write` and commit the result.",
        db.table, db.snapshot,
    )
}

/// Import both upstream databases (`--fetch`): download, repair encoding,
/// vet, rewrite the snapshots with provenance headers, and regenerate the
/// tables. Developer-run only; never CI or the build.
fn run_fetch(ctx: &Context) -> Result<(), String> {
    let today = utc_date_today()?;
    for db in DATABASES {
        let raw = download(db)?;
        let digest = tairix_crypto::sha256(&raw);
        let (text, repaired) = promote_latin1(&raw);
        let parsed = textdb::parse(db.kind, text.as_bytes())
            .map_err(|e| format!("devids: fetched {} fails vetting: {e}", db.name))?;
        let counts = parsed.counts();
        let version = header_field(&text, "Version:")
            .ok_or_else(|| format!("devids: fetched {} has no `Version:` header", db.name))?;
        let date = header_field(&text, "Date:")
            .ok_or_else(|| format!("devids: fetched {} has no `Date:` header", db.name))?;
        let encoding = if repaired == 0 {
            "as downloaded (valid UTF-8)".to_string()
        } else {
            format!("{repaired} invalid byte(s) promoted from ISO-8859-1 to UTF-8")
        };
        let mut snapshot = String::new();
        let _ = write!(
            snapshot,
            "# TAIRiX vetted snapshot of the public {name} database.\n\
             # Imported by `cargo xtask devids --fetch`; do not hand-edit — refetch\n\
             # and re-review instead. The compact lookup table is generated from\n\
             # this file by\n\
             # `cargo xtask devids --write` and drift-checked by `cargo xtask ci`.\n\
             # Upstream URL: {url}\n\
             # Upstream version: {version}\n\
             # Upstream date: {date}\n\
             # Fetch date: {today}\n\
             # Transport: {transport}\n\
             # Raw download SHA-256: {sha}\n\
             # Encoding: {encoding}\n\
             # Licence: {licence}\n\n",
            name = db.name,
            url = db.url,
            transport = db.transport,
            sha = hex(&digest),
            licence = db.licence,
        );
        snapshot.push_str(&text);
        // The final snapshot (header included) must itself pass vetting:
        // this is exactly what `--write` and the CI gate will parse.
        textdb::parse(db.kind, snapshot.as_bytes())
            .map_err(|e| format!("devids: {} snapshot fails vetting: {e}", db.name))?;
        let path = ctx.workspace_root.join(db.snapshot);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("devids: cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, snapshot.as_bytes())
            .map_err(|e| format!("devids: cannot write {}: {e}", path.display()))?;
        eprintln!(
            "xtask: [devids --fetch] {}: version {version}, {} vendors, {} devices, \
             {} class names ({encoding})",
            db.name, counts.vendors, counts.devices, counts.classes,
        );
    }
    run_write(ctx)
}

/// Download `db` with the system `curl`, failing closed on any transport or
/// HTTP error. An external process like the QEMU/toolchain wrappers; used
/// only by the developer-run `--fetch`.
fn download(db: &Database) -> Result<Vec<u8>, String> {
    let output = Process::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--max-time", "120", "--output", "-"])
        .arg(db.url)
        .output()
        .map_err(|e| format!("devids: cannot run curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "devids: fetching {} failed: {}",
            db.url,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout.is_empty() {
        return Err(format!("devids: fetching {} returned no data", db.url));
    }
    Ok(output.stdout)
}

/// Decode `raw` as UTF-8, promoting each byte of any *invalid* sequence to
/// its ISO-8859-1 code point. Valid UTF-8 (including multi-byte sequences)
/// is left untouched; the returned count is the number of promoted bytes.
fn promote_latin1(raw: &[u8]) -> (String, usize) {
    let mut out = String::with_capacity(raw.len());
    let mut promoted = 0usize;
    let mut rest = raw;
    loop {
        match std::str::from_utf8(rest) {
            Ok(tail) => {
                out.push_str(tail);
                return (out, promoted);
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // The prefix was just validated.
                out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or(""));
                let bad = e.error_len().unwrap_or(rest.len() - valid);
                for &b in &rest[valid..valid + bad] {
                    out.push(char::from(b));
                    promoted += 1;
                }
                rest = &rest[valid + bad..];
            }
        }
    }
}

/// The value of the first `# ... <key> <value>` comment line, e.g.
/// `#\tVersion: 2026.07.09` (pci.ids) or `# Version: 2026.06.26` (usb.ids).
fn header_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .take_while(|l| l.is_empty() || l.starts_with('#'))
        .find_map(|l| {
            let (_, value) = l.split_once(key)?;
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
}

/// Lowercase-hex rendering of a digest.
fn hex(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Today's UTC date as `YYYY-MM-DD`, from the system clock (the fetch is a
/// developer action; the date is provenance, not build input).
fn utc_date_today() -> Result<String, String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("devids: system clock is before the epoch: {e}"))?
        .as_secs();
    let days =
        i64::try_from(secs / 86_400).map_err(|e| format!("devids: system clock overflow: {e}"))?;
    let (y, m, d) = tairix_abi::time::civil_from_days(days);
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_latin1_leaves_valid_utf8_untouched() {
        let (text, promoted) = promote_latin1("héllo — ✓".as_bytes());
        assert_eq!(text, "héllo — ✓");
        assert_eq!(promoted, 0);
    }

    #[test]
    fn promote_latin1_promotes_only_the_invalid_bytes() {
        // The real-world shape: one stray ISO-8859-1 acute accent inside an
        // otherwise UTF-8 file that also holds a valid multi-byte sequence.
        let raw = b"a\xb4b \xc3\xa9";
        let (text, promoted) = promote_latin1(raw);
        assert_eq!(text, "a´b é");
        assert_eq!(promoted, 1);
    }

    #[test]
    fn header_field_reads_both_upstream_header_shapes() {
        let pci = "#\n#\tVersion: 2026.07.09\n#\tDate:    2026-07-09 03:15:02\n";
        assert_eq!(header_field(pci, "Version:").as_deref(), Some("2026.07.09"));
        let usb = "#\n# Version: 2026.06.26\n# Date:    2026-06-26 20:34:02\n";
        assert_eq!(
            header_field(usb, "Date:").as_deref(),
            Some("2026-06-26 20:34:02")
        );
        // The scan stops at the first entry line: a smuggled later comment
        // cannot claim the version.
        let late = "0001  V\n# Version: 9\n";
        assert_eq!(header_field(late, "Version:"), None);
    }

    #[test]
    fn committed_snapshots_stay_inside_the_crate_assets() {
        for db in DATABASES {
            assert!(db.snapshot.starts_with("lib/devids/assets/"));
        }
        // Each compiled table lives inside its consuming command bundle's
        // `Resources/` directory — the self-contained-bundle home, with no
        // second copy anywhere in the tree.
        assert_eq!(
            DATABASES[0].table,
            "userland/apps/lspci/Resources/pci.ids.bin"
        );
        assert_eq!(
            DATABASES[1].table,
            "userland/apps/lsusb/Resources/usb.ids.bin"
        );
    }
}

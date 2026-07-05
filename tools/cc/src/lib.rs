//! `rustos-cc` — an audited, version-pinned, checksummed C toolchain wrapper.
//!
//! RustOS is a Rust-only OS; this crate does **not** add C to
//! the codebase. It is host-only build glue that lets a
//! single QEMU integration test *host* a small C program to prove the
//! generated `abi-v1` C header, the `ros_sys_*` syscall stub runtime
//! (`lib/abi-sys`), and the crt0 startup object (`lib/crt0`) agree with the
//! Rust side end to end (`plans/CCOMPAT.md` stage CC5). Hosting a C program is
//! a different thing from authoring the OS in C.
//!
//! # Why a wrapper, not a raw `Command`
//!
//! the charter forbids unaudited shell-outs to external build tools: every
//! external invocation must be version-pinned and checksummed. This crate is
//! the single, auditable gateway to `clang` and `ld.lld`:
//!
//! * **Version-pinned.** [`Toolchain::discover`] runs `--version`, parses the
//!   banner, and fails closed unless the tool reports exactly
//!   [`REQUIRED_CLANG_VERSION`] / [`REQUIRED_LLD_VERSION`] (supply-chain integrity).
//!   Bumping the pin is a deliberate change, like the toolchain pin in
//!   `rust-toolchain.toml`.
//! * **Checksummed.** Every resolved binary is SHA-256-hashed with the audited
//!   `lib/crypto`. The digest is recorded for the audit
//!   trail and, when the caller pins an expected digest (via the
//!   `RUSTOS_CC_CLANG_SHA256` / `RUSTOS_CC_LLD_SHA256` environment variables),
//!   verified — a mismatch fails closed.
//!
//! # Targets
//!
//! Only the three **native** Tier-1 targets are in scope ([`CTarget`]); wasm32
//! has no trap instruction and is excluded from the C runtime
//! (`plans/CCOMPAT.md` §1).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod checksum;
mod target;
mod version;

pub use checksum::{digest, parse_hex, to_hex};
pub use target::{compile_argv, link_argv, CTarget, CompileRequest, LinkRequest};
pub use version::{parse_clang_version, parse_lld_version};

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustos_crypto::Sha256Digest;

/// The exact `clang` version the wrapper accepts. Bumping it is a deliberate,
/// reviewed change.
pub const REQUIRED_CLANG_VERSION: &str = "22.1.8";

/// The exact `ld.lld` version the wrapper accepts.
pub const REQUIRED_LLD_VERSION: &str = "22.1.8";

/// The two external tools the wrapper drives.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Tool {
    Clang,
    Lld,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Clang => "clang",
            Tool::Lld => "ld.lld",
        }
    }

    /// Default executable name searched on `PATH`.
    fn default_binary(self) -> &'static str {
        match self {
            Tool::Clang => "clang",
            Tool::Lld => "ld.lld",
        }
    }

    /// Environment variable that overrides the binary path.
    fn path_env(self) -> &'static str {
        match self {
            Tool::Clang => "RUSTOS_CC_CLANG",
            Tool::Lld => "RUSTOS_CC_LLD",
        }
    }

    /// Environment variable carrying an optional pinned SHA-256 (hex).
    fn sha_env(self) -> &'static str {
        match self {
            Tool::Clang => "RUSTOS_CC_CLANG_SHA256",
            Tool::Lld => "RUSTOS_CC_LLD_SHA256",
        }
    }

    fn required_version(self) -> &'static str {
        match self {
            Tool::Clang => REQUIRED_CLANG_VERSION,
            Tool::Lld => REQUIRED_LLD_VERSION,
        }
    }

    fn parse_version(self, banner: &str) -> Option<String> {
        match self {
            Tool::Clang => parse_clang_version(banner),
            Tool::Lld => parse_lld_version(banner),
        }
    }

    /// The `bin/` directories a packaged LLVM of the pinned `major` version
    /// installs into, most-specific first. `clang` ships in the `llvm`
    /// formula/package; `ld.lld` ships in the separate `lld` Homebrew formula
    /// (and inside `llvm` on Debian), so its list also probes the `lld`
    /// prefixes. All platforms' paths are listed unconditionally; missing ones
    /// are skipped by the caller.
    fn install_bin_dirs(self, major: &str) -> Vec<String> {
        let mut dirs = Vec::new();
        if self == Tool::Lld {
            dirs.push("/opt/homebrew/opt/lld/bin".to_string());
            dirs.push("/usr/local/opt/lld/bin".to_string());
        }
        for base in ["/opt/homebrew/opt", "/usr/local/opt"] {
            dirs.push(format!("{base}/llvm/bin"));
            dirs.push(format!("{base}/llvm@{major}/bin"));
        }
        dirs.push(format!("/usr/lib/llvm-{major}/bin"));
        dirs.push(format!("/usr/lib/llvm{major}/bin"));
        dirs
    }

    /// A resolution failure message naming what was searched and how to install
    /// or pin the pinned-version tool, so a build never has to hunt for it.
    fn install_hint(self, searched: &[String]) -> String {
        let major = major_of(self.required_version());
        let (brew, apt) = match self {
            Tool::Clang => ("brew install llvm", format!("apt install clang-{major}")),
            Tool::Lld => ("brew install lld", format!("apt install lld-{major}")),
        };
        let searched = if searched.is_empty() {
            "nothing on PATH or in the known LLVM prefixes".to_string()
        } else {
            searched.join(", ")
        };
        format!(
            "no {label} {version} found (searched: {searched}); install it \
             (macOS: `{brew}`, Debian/Ubuntu: `{apt}` from apt.llvm.org) or set \
             {env} to its path",
            label = self.label(),
            version = self.required_version(),
            env = self.path_env(),
        )
    }
}

/// An audited record of a resolved toolchain binary.
#[derive(Clone, Debug)]
pub struct ToolRecord {
    /// Stable label (`clang` / `ld.lld`).
    pub label: &'static str,
    /// Absolute or `PATH`-resolved path to the binary that was run.
    pub path: PathBuf,
    /// Version string the binary reported (matches the pinned requirement).
    pub version: String,
    /// SHA-256 of the binary's bytes.
    pub sha256: Sha256Digest,
}

impl ToolRecord {
    /// A single audit line, e.g.
    /// `clang 22.1.8 sha256=… path=/usr/bin/clang`.
    #[must_use]
    pub fn audit_line(&self) -> String {
        format!(
            "{} {} sha256={} path={}",
            self.label,
            self.version,
            to_hex(&self.sha256),
            self.path.display()
        )
    }
}

/// A discovered, validated `clang` + `ld.lld` toolchain.
#[derive(Clone, Debug)]
pub struct Toolchain {
    /// The validated `clang` record.
    pub clang: ToolRecord,
    /// The validated `ld.lld` record.
    pub lld: ToolRecord,
}

impl Toolchain {
    /// Discover, version-check, and checksum `clang` and `ld.lld`.
    ///
    /// Each tool is resolved from its override environment variable
    /// (`RUSTOS_CC_CLANG` / `RUSTOS_CC_LLD`) or `PATH`, hashed, and checked
    /// against the pinned version; if an expected digest is pinned in the
    /// environment it is also verified. The first failure is returned as a
    /// [`CcError`].
    pub fn discover() -> Result<Self, CcError> {
        Ok(Self {
            clang: resolve_tool(Tool::Clang)?,
            lld: resolve_tool(Tool::Lld)?,
        })
    }

    /// Audit lines for both tools, for a build script to print.
    #[must_use]
    pub fn audit_lines(&self) -> Vec<String> {
        vec![self.clang.audit_line(), self.lld.audit_line()]
    }

    /// Compile one C translation unit to a relocatable object.
    pub fn compile(&self, req: &CompileRequest<'_>) -> Result<(), CcError> {
        let argv = compile_argv(req);
        run(&self.clang.path, &argv, Phase::Compile)
    }

    /// Link objects + static archives into a position-independent executable.
    pub fn link(&self, req: &LinkRequest<'_>) -> Result<(), CcError> {
        let argv = link_argv(req);
        run(&self.lld.path, &argv, Phase::Link)
    }
}

/// Which toolchain phase an invocation belongs to (for error reporting).
#[derive(Copy, Clone)]
enum Phase {
    Compile,
    Link,
}

/// Resolve, version-check, and checksum a single tool.
fn resolve_tool(tool: Tool) -> Result<ToolRecord, CcError> {
    let (path, banner) = select_path(tool)?;

    let bytes = std::fs::read(&path).map_err(|source| CcError::Io {
        context: format!("reading {} for checksum: {}", tool.label(), path.display()),
        source,
    })?;
    let sha256 = digest(&bytes);

    if let Some(expected_hex) = read_env(tool.sha_env()) {
        match parse_hex(&expected_hex) {
            Some(expected) if expected == sha256 => {}
            Some(_) | None => {
                return Err(CcError::ChecksumMismatch {
                    tool: tool.label(),
                    expected: expected_hex.trim().to_string(),
                    found: to_hex(&sha256),
                });
            }
        }
    }

    let found = tool
        .parse_version(&banner)
        .ok_or_else(|| CcError::VersionQuery {
            tool: tool.label(),
            message: format!("could not parse a version from: {}", banner.trim()),
        })?;
    if found != tool.required_version() {
        return Err(CcError::VersionMismatch {
            tool: tool.label(),
            expected: tool.required_version(),
            found,
        });
    }

    Ok(ToolRecord {
        label: tool.label(),
        path,
        version: found,
        sha256,
    })
}

/// Locate a binary of the tool whose reported version matches the pin, and
/// return its path together with the `--version` banner selection relied on.
///
/// Resolution order — the "priming" that lets a plain `cargo xtask ci` find the
/// pinned toolchain with no manual configuration:
///
/// 1. The explicit override (`RUSTOS_CC_CLANG` / `RUSTOS_CC_LLD`). It is
///    **authoritative**: if it does not point at a file, or points at the wrong
///    version, resolution fails closed rather than silently searching elsewhere
///    — an override exists precisely to be obeyed.
/// 2. Otherwise, an ordered list of well-known locations for the pinned
///    version ([`tool_candidates`]) — the versioned `clang-NN` / `ld.lld-NN`
///    name on `PATH`, an unpacked official LLVM release archive of exactly the
///    pinned version (`LLVM-<version>-<OS>-<arch>/bin` or `llvm-<version>/bin`
///    under `~/toolchains`, `~`, `/opt`, or `/usr/local`), the Homebrew
///    (`/opt/homebrew`, `/usr/local`) and Debian (`/usr/lib/llvm-NN`) LLVM
///    install prefixes, and finally the bare name on `PATH`. The first
///    candidate whose reported version is *exactly* the pin is chosen; every
///    other candidate (e.g. an Apple/system `clang` of the wrong version) is
///    skipped, not accepted.
///
/// If nothing matches, the error names every location searched and how to
/// install or pin the toolchain, so neither a developer nor an automated build
/// has to hunt for it.
fn select_path(tool: Tool) -> Result<(PathBuf, String), CcError> {
    if let Some(override_path) = read_env(tool.path_env()) {
        let p = PathBuf::from(override_path);
        if !p.is_file() {
            return Err(CcError::ToolNotFound {
                tool: tool.label(),
                hint: format!(
                    "{} does not point at a file: {}",
                    tool.path_env(),
                    p.display()
                ),
            });
        }
        let banner = version_banner(tool, &p)?;
        return Ok((p, banner));
    }

    let required = tool.required_version();
    let mut searched: Vec<String> = Vec::new();
    for candidate in tool_candidates(tool) {
        if !candidate.is_file() {
            continue;
        }
        let shown = candidate.display().to_string();
        if searched.contains(&shown) {
            continue;
        }
        searched.push(shown);
        let Ok(banner) = version_banner(tool, &candidate) else {
            continue;
        };
        if tool.parse_version(&banner).as_deref() == Some(required) {
            return Ok((candidate, banner));
        }
    }

    Err(CcError::ToolNotFound {
        tool: tool.label(),
        hint: tool.install_hint(&searched),
    })
}

/// The ordered, platform-neutral list of places a pinned tool may live.
///
/// Every entry for every OS is listed unconditionally; non-existent paths are
/// simply skipped by [`select_path`], so the crate needs no `cfg(target_os)`
/// fork (it is not in the target-conditional allow-list).
fn tool_candidates(tool: Tool) -> Vec<PathBuf> {
    let major = major_of(tool.required_version());
    let binary = tool.default_binary();
    let mut out: Vec<PathBuf> = Vec::new();

    // 1. The versioned name a distro package installs on PATH (clang-22, ld.lld-22).
    if let Some(p) = find_on_path(&format!("{binary}-{major}")) {
        out.push(p);
    }
    // 2. Unpacked official LLVM release archives of exactly the pinned version.
    for dir in release_bin_dirs(tool.required_version()) {
        out.push(dir.join(binary));
    }
    // 3. Well-known versioned install prefixes for the pinned major.
    for dir in tool.install_bin_dirs(major) {
        out.push(Path::new(&dir).join(binary));
    }
    // 4. The bare name on PATH (may be a system default; version-checked before use).
    if let Some(p) = find_on_path(binary) {
        out.push(p);
    }
    out
}

/// The base directories an official LLVM release archive is commonly unpacked
/// into: a per-user `~/toolchains` collection, the home directory itself, and
/// the machine-wide `/opt` / `/usr/local` prefixes. A missing `HOME` simply
/// drops the per-user bases; every path is probed and skipped if absent, so no
/// platform fork is needed.
fn release_base_dirs() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        bases.push(home.join("toolchains"));
        bases.push(home);
    }
    bases.push(PathBuf::from("/opt"));
    bases.push(PathBuf::from("/usr/local"));
    bases
}

/// The `bin/` directories of unpacked official LLVM release archives of
/// exactly `version` beneath each of `bases`.
///
/// Covers the upstream archive layout `LLVM-<version>-<OS>-<arch>/bin`
/// (e.g. `LLVM-22.1.8-Linux-X64/bin`, matched by directory-name prefix so
/// every OS/arch suffix is found without naming platforms) and a plain
/// `llvm-<version>/bin` prefix. Scanned entries are sorted so candidate order
/// is deterministic; the exact-version match on the binary still gates
/// selection either way.
fn release_bin_dirs_under(bases: &[PathBuf], version: &str) -> Vec<PathBuf> {
    let archive_prefix = format!("LLVM-{version}-");
    let mut out = Vec::new();
    for base in bases {
        let mut unpacked: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.starts_with(&archive_prefix) {
                    unpacked.push(entry.path().join("bin"));
                }
            }
        }
        unpacked.sort();
        out.extend(unpacked);
        out.push(base.join(format!("llvm-{version}")).join("bin"));
    }
    out
}

/// [`release_bin_dirs_under`] over the standard [`release_base_dirs`].
fn release_bin_dirs(version: &str) -> Vec<PathBuf> {
    release_bin_dirs_under(&release_base_dirs(), version)
}

/// The major-version component of a pinned version string (`"22.1.8"` → `"22"`).
fn major_of(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}

/// The first directory on `PATH` that holds an executable named `binary`.
fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// Run `<path> --version` and return its combined banner text.
fn version_banner(tool: Tool, path: &Path) -> Result<String, CcError> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|source| CcError::Io {
            context: format!("running `{} --version`", tool.label()),
            source,
        })?;
    if !output.status.success() {
        return Err(CcError::VersionQuery {
            tool: tool.label(),
            message: format!("`--version` exited with {}", output.status),
        });
    }
    let mut banner = String::from_utf8_lossy(&output.stdout).into_owned();
    banner.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(banner)
}

/// Spawn a toolchain phase and turn a non-zero exit into a [`CcError`].
fn run(program: &Path, argv: &[std::ffi::OsString], phase: Phase) -> Result<(), CcError> {
    let output = Command::new(program)
        .args(argv)
        .output()
        .map_err(|source| CcError::Io {
            context: format!("spawning {}", program.display()),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let status = output.status.code();
    match phase {
        Phase::Compile => Err(CcError::Compile { status, stderr }),
        Phase::Link => Err(CcError::Link { status, stderr }),
    }
}

/// Read a non-empty environment variable, or `None`.
fn read_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// An error from discovering or driving the C toolchain.
#[derive(Debug)]
pub enum CcError {
    /// A required tool could not be located.
    ToolNotFound {
        /// Tool label.
        tool: &'static str,
        /// Human-readable resolution hint.
        hint: String,
    },
    /// `--version` could not be run or parsed.
    VersionQuery {
        /// Tool label.
        tool: &'static str,
        /// Detail.
        message: String,
    },
    /// The tool reported a version other than the pinned requirement.
    VersionMismatch {
        /// Tool label.
        tool: &'static str,
        /// The pinned, required version.
        expected: &'static str,
        /// The version the tool reported.
        found: String,
    },
    /// A pinned SHA-256 did not match the resolved binary.
    ChecksumMismatch {
        /// Tool label.
        tool: &'static str,
        /// The pinned digest (hex).
        expected: String,
        /// The computed digest (hex).
        found: String,
    },
    /// `clang` failed to compile the C source.
    Compile {
        /// Process exit code, if any.
        status: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
    /// `ld.lld` failed to link the image.
    Link {
        /// Process exit code, if any.
        status: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },
    /// An underlying I/O failure.
    Io {
        /// What was being attempted.
        context: String,
        /// The source error.
        source: std::io::Error,
    },
}

impl fmt::Display for CcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CcError::ToolNotFound { tool, hint } => {
                write!(f, "rustos-cc: {tool} not found: {hint}")
            }
            CcError::VersionQuery { tool, message } => {
                write!(f, "rustos-cc: {tool} version query failed: {message}")
            }
            CcError::VersionMismatch {
                tool,
                expected,
                found,
            } => write!(
                f,
                "rustos-cc: {tool} version {found} is not the pinned {expected} \
                 (AGENTS.md §12); install the pinned version or update the pin"
            ),
            CcError::ChecksumMismatch {
                tool,
                expected,
                found,
            } => write!(
                f,
                "rustos-cc: {tool} SHA-256 {found} does not match the pinned {expected}"
            ),
            CcError::Compile { status, stderr } => {
                write!(f, "rustos-cc: clang failed ({status:?}):\n{stderr}")
            }
            CcError::Link { status, stderr } => {
                write!(f, "rustos-cc: ld.lld failed ({status:?}):\n{stderr}")
            }
            CcError::Io { context, source } => {
                write!(f, "rustos-cc: I/O error {context}: {source}")
            }
        }
    }
}

impl std::error::Error for CcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CcError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_versions_are_the_expected_release() {
        assert_eq!(REQUIRED_CLANG_VERSION, "22.1.8");
        assert_eq!(REQUIRED_LLD_VERSION, "22.1.8");
        assert_eq!(Tool::Clang.required_version(), REQUIRED_CLANG_VERSION);
        assert_eq!(Tool::Lld.required_version(), REQUIRED_LLD_VERSION);
    }

    #[test]
    fn tool_env_var_names_are_namespaced() {
        assert_eq!(Tool::Clang.path_env(), "RUSTOS_CC_CLANG");
        assert_eq!(Tool::Lld.path_env(), "RUSTOS_CC_LLD");
        assert_eq!(Tool::Clang.sha_env(), "RUSTOS_CC_CLANG_SHA256");
        assert_eq!(Tool::Lld.sha_env(), "RUSTOS_CC_LLD_SHA256");
    }

    #[test]
    fn audit_line_is_stable_and_complete() {
        let record = ToolRecord {
            label: "clang",
            path: PathBuf::from("/usr/bin/clang"),
            version: "22.1.8".to_string(),
            sha256: digest(b""),
        };
        let line = record.audit_line();
        assert!(line.starts_with("clang 22.1.8 sha256="));
        assert!(line.contains("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
        assert!(line.ends_with("path=/usr/bin/clang"));
    }

    #[test]
    fn missing_tool_when_path_override_is_bogus() {
        // This is the only test that touches this variable, so the
        // set/remove pair cannot race another test's expectations.
        std::env::set_var("RUSTOS_CC_CLANG", "/definitely/not/a/real/clang");
        let err = select_path(Tool::Clang).expect_err("bogus override must fail");
        std::env::remove_var("RUSTOS_CC_CLANG");
        assert!(matches!(err, CcError::ToolNotFound { .. }));
    }

    #[test]
    fn major_of_extracts_leading_component() {
        assert_eq!(major_of("22.1.8"), "22");
        assert_eq!(major_of("22"), "22");
        assert_eq!(major_of(""), "");
    }

    #[test]
    fn clang_install_dirs_cover_homebrew_and_debian() {
        let dirs = Tool::Clang.install_bin_dirs("22");
        assert!(dirs.iter().any(|d| d == "/opt/homebrew/opt/llvm/bin"));
        assert!(dirs.iter().any(|d| d == "/opt/homebrew/opt/llvm@22/bin"));
        assert!(dirs.iter().any(|d| d == "/usr/local/opt/llvm/bin"));
        assert!(dirs.iter().any(|d| d == "/usr/lib/llvm-22/bin"));
        // clang is not in the standalone `lld` formula.
        assert!(!dirs.iter().any(|d| d.contains("/lld/")));
    }

    #[test]
    fn lld_install_dirs_also_probe_the_standalone_lld_formula() {
        let dirs = Tool::Lld.install_bin_dirs("22");
        // The Homebrew `lld` formula prefix is probed before the `llvm` one.
        let lld = dirs
            .iter()
            .position(|d| d == "/opt/homebrew/opt/lld/bin")
            .expect("homebrew lld prefix present");
        let llvm = dirs
            .iter()
            .position(|d| d == "/opt/homebrew/opt/llvm/bin")
            .expect("homebrew llvm prefix present");
        assert!(lld < llvm, "lld formula prefix must be searched first");
        assert!(dirs.iter().any(|d| d == "/usr/lib/llvm-22/bin"));
    }

    #[test]
    fn release_base_dirs_cover_home_and_machine_prefixes() {
        let bases = release_base_dirs();
        assert!(bases.iter().any(|b| b == Path::new("/opt")));
        assert!(bases.iter().any(|b| b == Path::new("/usr/local")));
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            let toolchains = bases
                .iter()
                .position(|b| *b == home.join("toolchains"))
                .expect("~/toolchains base present");
            let home_pos = bases
                .iter()
                .position(|b| *b == home)
                .expect("home base present");
            assert!(toolchains < home_pos, "~/toolchains is searched first");
        }
    }

    #[test]
    fn release_bin_dirs_find_only_the_pinned_version() {
        let base =
            std::env::temp_dir().join(format!("rustos-cc-release-bin-dirs-{}", std::process::id()));
        std::fs::create_dir_all(base.join("LLVM-22.1.8-Linux-X64/bin")).unwrap();
        std::fs::create_dir_all(base.join("LLVM-21.1.0-Linux-X64/bin")).unwrap();

        let dirs = release_bin_dirs_under(std::slice::from_ref(&base), "22.1.8");
        let unpacked = base.join("LLVM-22.1.8-Linux-X64").join("bin");
        let plain = base.join("llvm-22.1.8").join("bin");
        let scanned = dirs
            .iter()
            .position(|d| *d == unpacked)
            .expect("unpacked official archive bin dir present");
        let listed = dirs
            .iter()
            .position(|d| *d == plain)
            .expect("plain llvm-<version> bin dir present");
        assert!(scanned < listed, "scanned archives come first");
        assert!(
            !dirs
                .iter()
                .any(|d| d.starts_with(base.join("LLVM-21.1.0-Linux-X64"))),
            "a different version's archive is never a candidate"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn release_bin_dirs_skip_missing_bases() {
        let missing = PathBuf::from("/definitely/not/a/real/base");
        let dirs = release_bin_dirs_under(std::slice::from_ref(&missing), "22.1.8");
        assert_eq!(dirs, vec![missing.join("llvm-22.1.8").join("bin")]);
    }

    #[test]
    fn candidates_include_release_archive_bin_dirs() {
        let candidates = tool_candidates(Tool::Clang);
        assert!(candidates
            .iter()
            .any(|c| c.ends_with("opt/llvm-22.1.8/bin/clang")));
        let lld = tool_candidates(Tool::Lld);
        assert!(lld
            .iter()
            .any(|c| c.ends_with("opt/llvm-22.1.8/bin/ld.lld")));
    }

    #[test]
    fn candidates_join_the_binary_name_onto_install_dirs() {
        let candidates = tool_candidates(Tool::Clang);
        assert!(candidates
            .iter()
            .any(|c| c.ends_with("opt/homebrew/opt/llvm/bin/clang")));
        let lld = tool_candidates(Tool::Lld);
        assert!(lld
            .iter()
            .any(|c| c.ends_with("opt/homebrew/opt/lld/bin/ld.lld")));
    }

    #[test]
    fn install_hint_names_the_version_and_how_to_get_it() {
        let hint = Tool::Clang.install_hint(&["/usr/bin/clang".to_string()]);
        assert!(hint.contains("clang 22.1.8"));
        assert!(hint.contains("/usr/bin/clang"));
        assert!(hint.contains("brew install llvm"));
        assert!(hint.contains("apt install clang-22"));
        assert!(hint.contains("RUSTOS_CC_CLANG"));

        let lld_hint = Tool::Lld.install_hint(&[]);
        assert!(lld_hint.contains("brew install lld"));
        assert!(lld_hint.contains("apt install lld-22"));
        assert!(lld_hint.contains("nothing"));
    }

    #[test]
    fn version_mismatch_message_mentions_the_pin() {
        let err = CcError::VersionMismatch {
            tool: "clang",
            expected: "22.1.8",
            found: "17.0.6".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("17.0.6"));
        assert!(text.contains("22.1.8"));
    }
}

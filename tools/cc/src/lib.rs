//! `rustos-cc` — an audited, version-pinned, checksummed C toolchain wrapper.
//!
//! RustOS is a Rust-only OS (`AGENTS.md` §1); this crate does **not** add C to
//! the codebase. It is host-only build glue (`AGENTS.md` §12) that lets a
//! single QEMU integration test *host* a small C program to prove the
//! generated `abi-v1` C header, the `ros_sys_*` syscall stub runtime
//! (`lib/abi-sys`), and the crt0 startup object (`lib/crt0`) agree with the
//! Rust side end to end (`plans/CCOMPAT.md` stage CC5). Hosting a C program is
//! a different thing from authoring the OS in C (`AGENTS.md` §1).
//!
//! # Why a wrapper, not a raw `Command`
//!
//! `AGENTS.md` §12 forbids unaudited shell-outs to external build tools: every
//! external invocation must be version-pinned and checksummed. This crate is
//! the single, auditable gateway to `clang` and `ld.lld`:
//!
//! * **Version-pinned.** [`Toolchain::discover`] runs `--version`, parses the
//!   banner, and fails closed unless the tool reports exactly
//!   [`REQUIRED_CLANG_VERSION`] / [`REQUIRED_LLD_VERSION`] (`AGENTS.md` §19.3 —
//!   supply-chain integrity).
//!   Bumping the pin is a deliberate change, like the toolchain pin in
//!   `rust-toolchain.toml`.
//! * **Checksummed.** Every resolved binary is SHA-256-hashed with the audited
//!   `lib/crypto` (`AGENTS.md` §2.12). The digest is recorded for the audit
//!   trail and, when the caller pins an expected digest (via the
//!   `RUSTOS_CC_CLANG_SHA256` / `RUSTOS_CC_LLD_SHA256` environment variables),
//!   verified — a mismatch fails closed (`AGENTS.md` §2.9).
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
/// reviewed change (`AGENTS.md` §12 / §19.3).
pub const REQUIRED_CLANG_VERSION: &str = "18.1.3";

/// The exact `ld.lld` version the wrapper accepts.
pub const REQUIRED_LLD_VERSION: &str = "18.1.3";

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
    /// `clang 18.1.3 sha256=… path=/usr/bin/clang`.
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
    let path = resolve_path(tool)?;

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

    let banner = version_banner(tool, &path)?;
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

/// Resolve a tool's binary path from its override variable or `PATH`.
fn resolve_path(tool: Tool) -> Result<PathBuf, CcError> {
    if let Some(override_path) = read_env(tool.path_env()) {
        let p = PathBuf::from(override_path);
        if p.is_file() {
            return Ok(p);
        }
        return Err(CcError::ToolNotFound {
            tool: tool.label(),
            hint: format!(
                "{} does not point at a file: {}",
                tool.path_env(),
                p.display()
            ),
        });
    }

    let binary = tool.default_binary();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(CcError::ToolNotFound {
        tool: tool.label(),
        hint: format!(
            "`{binary}` not found on PATH; set {} to its path",
            tool.path_env()
        ),
    })
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
        assert_eq!(REQUIRED_CLANG_VERSION, "18.1.3");
        assert_eq!(REQUIRED_LLD_VERSION, "18.1.3");
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
            version: "18.1.3".to_string(),
            sha256: digest(b""),
        };
        let line = record.audit_line();
        assert!(line.starts_with("clang 18.1.3 sha256="));
        assert!(line.contains("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
        assert!(line.ends_with("path=/usr/bin/clang"));
    }

    #[test]
    fn missing_tool_when_path_override_is_bogus() {
        // This is the only test that touches this variable, so the
        // set/remove pair cannot race another test's expectations.
        std::env::set_var("RUSTOS_CC_CLANG", "/definitely/not/a/real/clang");
        let err = resolve_path(Tool::Clang).expect_err("bogus override must fail");
        std::env::remove_var("RUSTOS_CC_CLANG");
        assert!(matches!(err, CcError::ToolNotFound { .. }));
    }

    #[test]
    fn version_mismatch_message_mentions_the_pin() {
        let err = CcError::VersionMismatch {
            tool: "clang",
            expected: "18.1.3",
            found: "17.0.6".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("17.0.6"));
        assert!(text.contains("18.1.3"));
    }
}

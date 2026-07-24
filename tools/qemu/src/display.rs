//! Interactive display-backend selection for `cargo xtask run`.
//!
//! A headless integration test ([`crate::Runner::run`]) always passes
//! `-display none` and captures the serial console, so it never needs a
//! window. The developer-facing interactive session
//! ([`crate::Runner::run_interactive`]), by contrast, must open a real host
//! window showing the guest's `ramfb` scan-out.
//!
//! Older QEMU builds shipped GTK/SDL compiled in and, given no `-display`
//! flag, opened a GTK window by default. A modern QEMU built *without* GTK or
//! SDL instead falls back to a headless VNC server (`-vnc localhost:0`): the
//! process starts, but no window ever appears — the guest is only reachable
//! by separately attaching a VNC client. Relying on QEMU's implicit default
//! display is therefore not portable across builds.
//!
//! This module makes the interactive display **explicit**: it probes the
//! QEMU binary's actual `-display help` output, selects a backend that opens
//! a window on its own (GTK preferred, then SDL), and — because a machine may
//! carry several QEMU builds — searches a small ordered set of candidate
//! binaries for one that can. If none can present a window it fails loud with
//! an actionable message rather than silently starting an invisible VNC
//! server.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A QEMU display backend that opens a native host window by itself.
///
/// Ordered by preference in [`WindowBackend::PREFERENCE`]: GTK first (native
/// menus and window integration), then SDL. Backends that do *not* open a
/// window unaided — `none`, `curses`, `dbus`, `vnc`, `spice-app`,
/// `egl-headless` — are deliberately excluded: the interactive session's
/// whole purpose is a visible window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowBackend {
    /// QEMU's GTK UI (`-display gtk`).
    Gtk,
    /// QEMU's SDL UI (`-display sdl`).
    Sdl,
}

impl WindowBackend {
    /// Windowing backends in descending order of preference.
    pub const PREFERENCE: [WindowBackend; 2] = [WindowBackend::Gtk, WindowBackend::Sdl];

    /// The `-display <name>` token QEMU accepts for this backend.
    #[must_use]
    pub fn qemu_name(self) -> &'static str {
        match self {
            WindowBackend::Gtk => "gtk",
            WindowBackend::Sdl => "sdl",
        }
    }
}

/// A QEMU binary that can present a windowed interactive session, together
/// with the windowing backend it will be driven with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractiveQemu {
    /// The binary to spawn (either a bare name resolved via `PATH` or an
    /// absolute path).
    pub binary: PathBuf,
    /// The windowing backend to pass as `-display <backend>`.
    pub backend: WindowBackend,
}

/// Environment variable that pins the interactive QEMU binary explicitly.
///
/// When set, it is the *only* candidate considered: an interactive run either
/// uses it (if it supports a windowing backend) or fails loud, rather than
/// silently falling back to a different binary the developer did not choose.
pub const QEMU_BIN_ENV: &str = "TAIRIX_QEMU_BIN";

/// Parse the backend names out of `qemu-system-* -display help` output.
///
/// The output is a header line (`Available display backend types:`) followed
/// by one backend name per line, then a blank line and free-form prose. Only
/// the names between the header and that blank line are returned, so the
/// trailing prose can never be mistaken for a backend.
#[must_use]
pub fn parse_display_backends(help_output: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_list = false;
    for line in help_output.lines() {
        let trimmed = line.trim();
        if !in_list {
            if trimmed.eq_ignore_ascii_case("Available display backend types:") {
                in_list = true;
            }
            continue;
        }
        // The list ends at the first blank line after the header.
        if trimmed.is_empty() {
            break;
        }
        // A backend name is a single bare token; anything with whitespace is
        // the start of the trailing prose, so stop defensively.
        if trimmed.split_whitespace().count() != 1 {
            break;
        }
        names.push(trimmed.to_string());
    }
    names
}

/// Choose the most-preferred windowing backend present in `available`.
///
/// Returns `None` when the build carries no window-opening backend (e.g. a
/// headless-only QEMU offering just `none`/`curses`/`dbus`/`vnc`).
#[must_use]
pub fn pick_backend(available: &[String]) -> Option<WindowBackend> {
    WindowBackend::PREFERENCE
        .into_iter()
        .find(|b| available.iter().any(|a| a == b.qemu_name()))
}

/// Ordered candidate QEMU binaries to probe for an interactive session.
///
/// When `override_bin` is `Some` it is the sole candidate (an explicit
/// developer choice must not be silently overridden). Otherwise the search is
/// `PATH` first (the developer's default QEMU), then the two conventional
/// install prefixes, so a distribution build that still carries GTK/SDL is
/// found even when a locally built, GUI-less QEMU shadows it on `PATH`.
#[must_use]
pub fn candidate_paths(binary_name: &str, override_bin: Option<OsString>) -> Vec<PathBuf> {
    if let Some(bin) = override_bin {
        return vec![PathBuf::from(bin)];
    }
    vec![
        PathBuf::from(binary_name),
        PathBuf::from("/usr/bin").join(binary_name),
        PathBuf::from("/usr/local/bin").join(binary_name),
    ]
}

/// Resolve a candidate to its canonical executable path, or `None` if it is
/// not an existing file.
///
/// A path-like candidate is canonicalised directly; a bare name is searched
/// for on `PATH`. Canonicalisation is what lets the caller de-duplicate the
/// `PATH` entry against an identical absolute-path candidate.
fn resolve_executable(candidate: &Path) -> Option<PathBuf> {
    let is_path_like = candidate.components().count() > 1;
    if is_path_like {
        return candidate.canonicalize().ok().filter(|p| p.is_file());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(candidate))
        .find_map(|p| p.canonicalize().ok().filter(|p| p.is_file()))
}

/// Query a QEMU binary for the display backends it was built with.
///
/// Returns an empty vector if the binary cannot be run (missing, not
/// executable) or produced no parseable list; the caller treats that the same
/// as "no windowing backend", so a broken candidate is simply skipped.
fn available_backends(binary: &Path) -> Vec<String> {
    let output = Command::new(binary)
        .arg("-display")
        .arg("help")
        .output()
        .ok();
    match output {
        Some(out) => {
            // QEMU prints the list on stdout; tolerate builds that use stderr.
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&out.stderr).into_owned();
            }
            parse_display_backends(&text)
        }
        None => Vec::new(),
    }
}

/// Select a QEMU binary and windowing backend for an interactive session.
///
/// Probes [`candidate_paths`] in order and returns the first binary that
/// offers a window-opening backend ([`pick_backend`]).
///
/// # Errors
///
/// Returns a human-readable, actionable message when no candidate offers a
/// windowing backend — including which binaries were probed and the backends
/// each reported — so the developer can install a GUI-capable QEMU or point
/// [`QEMU_BIN_ENV`] at one instead of staring at a window that never opens.
pub fn select_interactive(binary_name: &str) -> Result<InteractiveQemu, String> {
    let override_bin = std::env::var_os(QEMU_BIN_ENV);
    let overridden = override_bin.is_some();
    let candidates = candidate_paths(binary_name, override_bin);

    let mut probed: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for candidate in candidates {
        let Some(canonical) = resolve_executable(&candidate) else {
            if overridden {
                probed.push((candidate, Vec::new()));
            }
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let backends = available_backends(&canonical);
        if let Some(backend) = pick_backend(&backends) {
            return Ok(InteractiveQemu {
                binary: canonical,
                backend,
            });
        }
        probed.push((canonical, backends));
    }

    Err(no_windowing_backend_message(
        binary_name,
        overridden,
        &probed,
    ))
}

/// Build the fail-loud diagnostic for [`select_interactive`].
fn no_windowing_backend_message(
    binary_name: &str,
    overridden: bool,
    probed: &[(PathBuf, Vec<String>)],
) -> String {
    let mut msg = format!(
        "no QEMU binary with a windowing display backend (gtk or sdl) was \
         found for {binary_name}, so `cargo xtask run` cannot open a window.\n"
    );
    if probed.is_empty() {
        msg.push_str("  (no candidate binary could be run)\n");
    } else {
        msg.push_str("  probed:\n");
        for (path, backends) in probed {
            let list = if backends.is_empty() {
                "could not be run".to_string()
            } else {
                backends.join(", ")
            };
            let _ = writeln!(msg, "    {} — display backends: {list}", path.display());
        }
    }
    if overridden {
        let _ = writeln!(
            msg,
            "  {QEMU_BIN_ENV} is set, so only that binary was considered."
        );
    }
    msg.push_str(
        "  A QEMU built without GTK/SDL defaults to a headless VNC server, \
         which opens no window. Install a QEMU built with GTK or SDL support \
         (e.g. your distribution's qemu-system package), or set ",
    );
    msg.push_str(QEMU_BIN_ENV);
    msg.push_str(" to one that has it.");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_names_between_header_and_blank_line() {
        let help = "Available display backend types:\n\
                    none\n\
                    gtk\n\
                    sdl\n\
                    curses\n\
                    \n\
                    Some display backends support suboptions...\n";
        let names = parse_display_backends(help);
        assert_eq!(names, vec!["none", "gtk", "sdl", "curses"]);
    }

    #[test]
    fn parses_headless_only_build_list() {
        // The QEMU 11 build that regressed the interactive window: no gtk/sdl.
        let help = "Available display backend types:\n\
                    none\n\
                    curses\n\
                    dbus\n\
                    \n\
                    Some display backends support suboptions...\n";
        let names = parse_display_backends(help);
        assert_eq!(names, vec!["none", "curses", "dbus"]);
        assert_eq!(pick_backend(&names), None);
    }

    #[test]
    fn parse_ignores_trailing_prose_without_a_blank_line() {
        // Defensive: a multi-word line ends the list even if no blank line
        // separates it from the trailing prose.
        let help = "Available display backend types:\n\
                    gtk\n\
                    For a short list of the suboptions see -help.\n";
        assert_eq!(parse_display_backends(help), vec!["gtk"]);
    }

    #[test]
    fn parse_returns_empty_without_a_header() {
        assert!(parse_display_backends("garbage output\nmore garbage\n").is_empty());
    }

    #[test]
    fn pick_prefers_gtk_over_sdl() {
        let names = vec!["sdl".to_string(), "gtk".to_string(), "none".to_string()];
        assert_eq!(pick_backend(&names), Some(WindowBackend::Gtk));
    }

    #[test]
    fn pick_falls_back_to_sdl_when_no_gtk() {
        let names = vec!["none".to_string(), "sdl".to_string()];
        assert_eq!(pick_backend(&names), Some(WindowBackend::Sdl));
    }

    #[test]
    fn pick_none_when_only_headless_backends() {
        let names = vec!["none".to_string(), "vnc".to_string(), "dbus".to_string()];
        assert_eq!(pick_backend(&names), None);
    }

    #[test]
    fn backend_qemu_names_are_the_display_tokens() {
        assert_eq!(WindowBackend::Gtk.qemu_name(), "gtk");
        assert_eq!(WindowBackend::Sdl.qemu_name(), "sdl");
    }

    #[test]
    fn candidate_paths_default_to_path_then_conventional_prefixes() {
        let paths = candidate_paths("qemu-system-aarch64", None);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("qemu-system-aarch64"),
                PathBuf::from("/usr/bin/qemu-system-aarch64"),
                PathBuf::from("/usr/local/bin/qemu-system-aarch64"),
            ]
        );
    }

    #[test]
    fn candidate_paths_override_is_the_sole_candidate() {
        let paths = candidate_paths(
            "qemu-system-aarch64",
            Some(OsString::from("/opt/qemu/bin/qemu-system-aarch64")),
        );
        assert_eq!(
            paths,
            vec![PathBuf::from("/opt/qemu/bin/qemu-system-aarch64")]
        );
    }

    #[test]
    fn error_message_lists_probed_binaries_and_is_actionable() {
        let probed = vec![(
            PathBuf::from("/usr/local/bin/qemu-system-aarch64"),
            vec!["none".to_string(), "curses".to_string(), "dbus".to_string()],
        )];
        let msg = no_windowing_backend_message("qemu-system-aarch64", false, &probed);
        assert!(msg.contains("/usr/local/bin/qemu-system-aarch64"));
        assert!(msg.contains("none, curses, dbus"));
        assert!(msg.contains(QEMU_BIN_ENV));
        assert!(msg.contains("GTK or SDL"));
    }
}

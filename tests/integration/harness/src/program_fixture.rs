//! The glue every freestanding fixture program's build script shares: the
//! one linker script they all link with, the one nested cross-compile recipe
//! ([`GuestBuild`](crate::program_fixture::GuestBuild)) that turns a guest
//! crate into the `rxe` image the kernel spawn path loads, and the one
//! emitter of the `PROGRAM_RXE` + `USER_BIAS` source they `include!`.
//!
//! Each vertical used to carry its own byte-identical copy of all three, so
//! a change to the program layout, the link recipe, or the emitted source
//! had to be made dozens of times to stay consistent. They live here once
//! instead.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tairix_abi::syscall::SYSCALL_TABLE_HASH_LEN;

use crate::dep_info;
use crate::pie::{self, PieArch};
use crate::USER_IMAGE_BIAS;

/// The single linker script every freestanding fixture program links with:
/// it roots `tairix-rt`'s `_start` and lays the PIE image out for the bias
/// the kernel spawn path maps a child at.
///
/// Absolute, resolved when this crate is compiled, so a consuming build
/// script names it without walking a relative path of its own.
/// [`GuestBuild::program_rxe`] registers it and guards against its content
/// changing; a script that hands the path to a *linker of its own* (the C
/// verticals' `tairix_cc` link step) registers it itself.
pub const PROGRAM_LD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/program.ld");

/// One nested `cargo build` of a fixture's guest crate for a freestanding
/// target.
///
/// Every vertical that spawns a separately-linked EL0 program shares this
/// recipe: a private target directory under `OUT_DIR`, the outer build's
/// `RUSTFLAGS` cleared so the target-scoped position-independent recipe
/// wins, `-Z build-std` compiling `core`/`compiler_builtins`/`alloc` as PIC
/// alongside the guest, and — the part no hand-kept list can get right —
/// the outer fixture's freshness taken from the compiler's own dep-info
/// record, so an edit anywhere in the guest's dependency closure rebuilds
/// the embedded blob instead of leaving the vertical to pass against code
/// that is no longer in the tree.
pub struct GuestBuild<'a> {
    /// Directory the nested `cargo` runs in — the fixture's own manifest
    /// directory, so it resolves this workspace.
    pub manifest_dir: &'a str,
    /// The fixture's `OUT_DIR`. The private target directory and its
    /// freshness sidecar live inside it.
    pub out_dir: &'a str,
    /// Freestanding target the guest is cross-compiled for.
    pub arch: PieArch,
    /// Cargo package to build, which also names its artefact.
    pub package: &'a str,
    /// Distinguishes two builds of one `package` that differ only in
    /// [`Self::env`] — a parent/child role. One directory holds one artefact
    /// per package, so sharing it would leave each role reading whichever
    /// binary was linked last. `None` for a package built once.
    pub variant: Option<&'a str>,
    /// Environment the guest's own source reads through `env!`, so this
    /// script stays the single source of truth for the constants both halves
    /// of a fixture share. It is part of the freshness stamp, so a changed
    /// value rebuilds the guest whether or not the guest crate declared
    /// `cargo:rerun-if-env-changed` for it.
    pub env: &'a [(&'a str, String)],
}

impl GuestBuild<'_> {
    /// Cross-compile the guest position-independent against the shared
    /// [`PROGRAM_LD`] and convert the linked ELF into the `rxe` load image
    /// the kernel spawn path accepts, with relocations baked for
    /// [`USER_IMAGE_BIAS`] and the kernel's syscall `cfi_tag` stamped in.
    ///
    /// # Panics
    ///
    /// If the nested build fails, its ELF is unreadable, or the ELF is not
    /// convertible: a fixture that cannot produce its program must fail the
    /// build loudly rather than embed a stale or empty blob.
    #[must_use]
    pub fn program_rxe(&self, cfi_tag: &[u8; SYSCALL_TABLE_HASH_LEN]) -> Vec<u8> {
        let elf_path = self.run(&self.program_recipe());
        let elf =
            fs::read(&elf_path).unwrap_or_else(|e| panic!("read {}: {e}", elf_path.display()));
        crate::elf2rxe::elf_to_rxe(&elf, cfi_tag, USER_IMAGE_BIAS)
            .unwrap_or_else(|e| panic!("convert {} into an rxe image: {e:?}", elf_path.display()))
    }

    /// Cross-compile the guest as a position-independent `staticlib` and
    /// return the archive path, for a vertical that links a foreign object
    /// against the runtime rather than spawning a Rust program directly.
    ///
    /// `compiler-builtins-mem` is built in because the foreign object
    /// expects `memcpy` and friends from the runtime it links, and `alloc`
    /// is left out because an archive registers no global allocator.
    ///
    /// # Panics
    ///
    /// If the nested build fails, for the reason
    /// [`program_rxe`](Self::program_rxe) states.
    #[must_use]
    pub fn static_archive(&self) -> PathBuf {
        self.run(&self.archive_recipe())
    }

    /// The recipe [`Self::program_rxe`] builds under.
    fn program_recipe(&self) -> Recipe<'static> {
        Recipe {
            linker_script: Some(PROGRAM_LD),
            unstable: &["-Z", "build-std=core,compiler_builtins,alloc"],
            profile_args: &[],
            profile_dir: "debug",
            artefact: self.package.to_string(),
        }
    }

    /// The recipe [`Self::static_archive`] builds under.
    fn archive_recipe(&self) -> Recipe<'static> {
        Recipe {
            linker_script: None,
            unstable: &[
                "-Z",
                "build-std=core,compiler_builtins",
                "-Z",
                "build-std-features=compiler-builtins-mem",
            ],
            profile_args: &["--release"],
            profile_dir: "release",
            artefact: format!("lib{}.a", self.package.replace('-', "_")),
        }
    }

    /// Drive the nested `cargo build` and return the artefact's path,
    /// having registered every input it consumed with the outer cargo.
    fn run(&self, recipe: &Recipe<'_>) -> PathBuf {
        let triple = self.arch.target_triple();
        let target_dir = PathBuf::from(self.out_dir).join(self.private_target_dir_name());

        let mut rustflags = String::from("-C relocation-model=pie");
        if let Some(script) = recipe.linker_script {
            println!("cargo:rerun-if-changed={script}");
            let _ = write!(rustflags, " -C link-arg=-pie -C link-arg=-T{script}");
        }
        pie::wipe_target_dir_on_stamp_change(&target_dir, self.stamp(recipe).as_deref());

        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.current_dir(self.manifest_dir)
            // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS`
            // into this build script's environment and both outrank the
            // target-scoped variable, so a nested cargo would inherit the
            // outer kernel's flags and drop the link recipe. Clearing them
            // leaves the target-scoped flags winning, and applying only to
            // the guest's freestanding crates — never its own host build
            // script.
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTFLAGS")
            .env(self.arch.rustflags_env_var(), rustflags)
            // `--locked` so a build script can never rewrite the committed
            // lockfile behind the outer build's back.
            .args(["build", "--locked", "-p", self.package, "--target", triple])
            .args(recipe.unstable)
            .args(recipe.profile_args)
            .arg("--target-dir")
            .arg(&target_dir);
        for (key, value) in self.env {
            cmd.env(key, value);
        }
        let status = cmd
            .status()
            .unwrap_or_else(|e| panic!("spawn cargo to build {}: {e}", self.label()));
        assert!(status.success(), "building {} failed", self.label());

        let artefacts = target_dir.join(triple).join(recipe.profile_dir);
        dep_info::emit_dep_info_reruns(
            &artefacts.join(recipe.dep_info_name()),
            Path::new(self.manifest_dir),
            &target_dir,
        );
        artefacts.join(&recipe.artefact)
    }

    /// The private target directory's name inside `OUT_DIR`.
    fn private_target_dir_name(&self) -> String {
        match self.variant {
            Some(variant) => format!("{}-{variant}-target", self.package),
            None => format!("{}-target", self.package),
        }
    }

    /// How this build names itself in a failure message: what a reader would
    /// have to type to reproduce it.
    fn label(&self) -> String {
        match self.variant {
            Some(variant) => format!("{} ({variant})", self.package),
            None => self.package.to_string(),
        }
    }

    /// The inputs this build cannot leave to cargo's own fingerprint — the
    /// linker script's *content*, of which the `RUSTFLAGS` string carries
    /// only the path, and the environment, which cargo tracks only where the
    /// guest crate itself declared `cargo:rerun-if-env-changed` for it —
    /// framed length-first so two different sets cannot render the same
    /// bytes.
    ///
    /// `None` when the script cannot be read, which the freshness guard
    /// treats as changed.
    fn stamp(&self, recipe: &Recipe<'_>) -> Option<Vec<u8>> {
        let mut stamp = Vec::new();
        if let Some(script) = recipe.linker_script {
            push_stamp_field(&mut stamp, &fs::read(script).ok()?);
        }
        for (key, value) in self.env {
            push_stamp_field(&mut stamp, key.as_bytes());
            push_stamp_field(&mut stamp, value.as_bytes());
        }
        Some(stamp)
    }
}

/// The half of the nested recipe that differs between a position-independent
/// program image and the archive a foreign object links against.
struct Recipe<'a> {
    /// Linker script the guest links with, named in the target-scoped
    /// `RUSTFLAGS` and hashed into the freshness stamp. `None` for an
    /// archive, which links nothing.
    linker_script: Option<&'a str>,
    /// The `-Z` arguments, `build-std` among them.
    unstable: &'a [&'a str],
    /// Profile-selecting arguments; empty for cargo's `dev` profile.
    profile_args: &'a [&'a str],
    /// Directory cargo writes this profile's artefacts under.
    profile_dir: &'a str,
    /// The artefact's file name inside that directory.
    artefact: String,
}

impl Recipe<'_> {
    /// The dep-info file rustc writes beside this artefact, recording every
    /// source the compilation read.
    fn dep_info_name(&self) -> PathBuf {
        Path::new(&self.artefact).with_extension("d")
    }
}

/// Append one length-prefixed field to a freshness stamp.
fn push_stamp_field(stamp: &mut Vec<u8>, field: &[u8]) {
    stamp.extend_from_slice(&field.len().to_le_bytes());
    stamp.extend_from_slice(field);
}

/// Buffer length [`format_grouped_hex`] renders into: `0x`, sixteen hex
/// digits, and the three `_` separators between the four digit groups.
pub const GROUPED_HEX_LEN: usize = 2 + 16 + 3;

/// Render `value` as a Rust hexadecimal literal with `_` between each group
/// of four digits (`0x0010_0000_0000`), into `out`.
///
/// The build script emits Rust source the crate `include!`s, and generated
/// source is linted like any other: a bare `{value:#x}` there trips the
/// "long literal lacking separators" lint at the *include site*, where it
/// cannot be fixed. Grouping at the point of emission keeps the generated
/// file idiomatic on its own terms, instead of stamping a blanket lint
/// exemption into it that would also hide a future real finding in the same
/// file. Leading all-zero groups are dropped, but never the last one, so
/// zero renders as `0x0000`. It renders into the caller's buffer rather
/// than allocating, so the kernel image builder can also call it while
/// laying out its own generated constants.
pub fn format_grouped_hex(value: u64, out: &mut [u8; GROUPED_HEX_LEN]) -> &str {
    let mut digits = [b'0'; 16];
    for (i, slot) in digits.iter_mut().enumerate() {
        let nibble = ((value >> ((15 - i) * 4)) & 0xf) as u8;
        *slot = match nibble {
            0..=9 => b'0' + nibble,
            _ => b'a' + (nibble - 10),
        };
    }
    // Whole groups only: every emitted group is four digits wide, which is
    // exactly the shape the lint accepts.
    let mut first = 0;
    while first + 4 < digits.len() && digits[first..first + 4] == [b'0'; 4] {
        first += 4;
    }

    out[0] = b'0';
    out[1] = b'x';
    let mut len = 2;
    for group in digits[first..].as_chunks::<4>().0 {
        if len > 2 {
            out[len] = b'_';
            len += 1;
        }
        out[len..len + 4].copy_from_slice(group);
        len += 4;
    }
    // Every byte written above is ASCII, so the prefix is valid UTF-8; fall
    // back to the empty string rather than panicking if that were broken.
    core::str::from_utf8(&out[..len]).unwrap_or("")
}

/// Open a fixture source: the generated-file banner and the shared
/// [`USER_IMAGE_BIAS`] the images' relocations were baked for, emitted as
/// `USER_BIAS`.
///
/// The bias is rendered through [`format_grouped_hex`] because the emitted
/// file is compiled and linted like any other source; a bare
/// `0x1000000000` there trips the missing-separator lint at the `include!`
/// site, where it cannot be fixed.
///
/// A caller pins its own extra constants onto the returned string, appends
/// each image with [`push_rxe_blob`], and writes the whole thing with
/// [`write_fixture`].
#[must_use]
pub fn fixture_header() -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str("/// Virtual base every program image is mapped at.\n");
    let mut bias = [0u8; GROUPED_HEX_LEN];
    let _ = writeln!(
        out,
        "pub const USER_BIAS: u64 = {};",
        format_grouped_hex(USER_IMAGE_BIAS, &mut bias)
    );
    out
}

/// Append one converted `rxe` image to a fixture source as
/// `pub const <name>: &[u8]`, documented as the image of `description`
/// ("the heap fixture program", "the parent role").
///
/// A host build passes an empty `rxe`, so the vertical's inert stub still
/// compiles against the same names.
pub fn push_rxe_blob(out: &mut String, name: &str, description: &str, rxe: &[u8]) {
    let _ = writeln!(out, "/// The converted `rxe` image of {description}.");
    let _ = write!(out, "pub const {name}: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}

/// Write a completed fixture `source` to `path`.
///
/// # Panics
///
/// If the write fails: a build that could not emit its fixture must fail
/// loudly rather than leave a stale one in place for the compiler to pick
/// up.
pub fn write_fixture(path: &Path, source: &str) {
    fs::write(path, source).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build of `tairix-test-probe` with the given pinned environment.
    fn probe<'a>(env: &'a [(&'a str, String)]) -> GuestBuild<'a> {
        GuestBuild {
            manifest_dir: ".",
            out_dir: "/out",
            arch: PieArch::Aarch64,
            package: "tairix-test-probe",
            variant: None,
            env,
        }
    }

    /// No fixture build script may enumerate a guest crate's cargo-tracked
    /// inputs by hand. An edit to a crate the guest *depends on* matches no
    /// such list, so the outer script does not rerun, the nested build is
    /// never re-driven, and the vertical passes against a blob built from
    /// code that is no longer in the tree — the dangerous failure, because
    /// it is green. [`GuestBuild`] derives the closure from the compiler's
    /// own dep-info record instead.
    #[test]
    fn no_fixture_build_script_enumerates_its_guest_inputs_by_hand() {
        let verticals = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the harness sits inside tests/integration");
        let mut offenders: Vec<String> = Vec::new();
        for entry in fs::read_dir(verticals).expect("read tests/integration") {
            let script = entry.expect("read a vertical").path().join("build.rs");
            let Ok(source) = fs::read_to_string(&script) else {
                continue;
            };
            for (number, line) in source.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for hand_kept in ["/src/main.rs", "/src/lib.rs", "/Cargo.toml"] {
                    if code.contains(hand_kept) {
                        offenders.push(format!("{}:{}", script.display(), number + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these build scripts name a guest's cargo-tracked inputs by hand \
             instead of deriving the closure through GuestBuild: {offenders:#?}"
        );
    }

    /// Each variant of one package gets its own private target directory,
    /// because cargo fingerprints neither the environment that selected the
    /// role nor the artefact it produced: sharing a directory would hand
    /// the second role the first one's binary.
    #[test]
    fn a_variant_gets_its_own_target_directory_and_says_so_when_it_fails() {
        let single = probe(&[]);
        assert_eq!(single.private_target_dir_name(), "tairix-test-probe-target");
        assert_eq!(single.label(), "tairix-test-probe");

        let roled = GuestBuild {
            variant: Some("parent"),
            ..probe(&[])
        };
        assert_eq!(
            roled.private_target_dir_name(),
            "tairix-test-probe-parent-target"
        );
        assert_eq!(roled.label(), "tairix-test-probe (parent)");
    }

    /// Cargo writes the dep-info beside the artefact under the artefact's
    /// own stem, which differs between a program binary and an archive.
    #[test]
    fn the_dep_info_sits_beside_the_artefact_it_describes() {
        assert_eq!(
            probe(&[]).program_recipe().dep_info_name(),
            Path::new("tairix-test-probe.d")
        );
        let archive = probe(&[]).archive_recipe();
        assert_eq!(archive.artefact, "libtairix_test_probe.a");
        assert_eq!(archive.dep_info_name(), Path::new("libtairix_test_probe.d"));
    }

    /// The stamp frames every field by length, so two environments whose
    /// concatenation is identical still compare as different and force the
    /// clean rebuild.
    #[test]
    fn the_freshness_stamp_frames_each_field_by_length() {
        let env_only = probe(&[]).archive_recipe();
        let split_early = [("AB", String::from("C"))];
        let split_late = [("A", String::from("BC"))];
        assert_ne!(
            probe(&split_early).stamp(&env_only),
            probe(&split_late).stamp(&env_only)
        );
        assert_eq!(
            probe(&split_early).stamp(&env_only),
            probe(&split_early).stamp(&env_only)
        );
    }

    /// The program recipe stamps the linker script's content, since the
    /// `RUSTFLAGS` string cargo fingerprints carries only its path. An
    /// unreadable script yields no stamp, which the guard reads as changed.
    #[test]
    fn the_freshness_stamp_covers_the_linker_script_content() {
        let build = probe(&[]);
        let stamped = build
            .stamp(&build.program_recipe())
            .expect("the shared program.ld is readable");
        let script = fs::read(PROGRAM_LD).expect("read the shared program.ld");
        assert!(stamped.ends_with(&script), "the script content is stamped");

        let missing = Recipe {
            linker_script: Some("/tairix-no-such-program.ld"),
            ..build.program_recipe()
        };
        assert!(build.stamp(&missing).is_none());
    }

    /// A composed fixture source carries the shared bias as a grouped
    /// literal, the caller's own pinned constants, and every image byte.
    #[test]
    fn a_composed_fixture_source_carries_the_bias_and_every_byte() {
        let mut src = fixture_header();
        assert!(
            src.contains("pub const USER_BIAS: u64 = 0x0010_0000_0000;"),
            "{src}"
        );
        src.push_str("pub const ROUNDS: u64 = 3;\n");
        push_rxe_blob(
            &mut src,
            "PROGRAM_RXE",
            "the probe fixture program",
            &[0x7f, 0x45, 0x4c, 0x46],
        );
        assert!(
            src.contains("/// The converted `rxe` image of the probe fixture program."),
            "{src}"
        );
        assert!(src.contains("pub const ROUNDS: u64 = 3;"), "{src}");
        assert!(src.contains("0x7f, 0x45, 0x4c, 0x46,"), "{src}");
        // A host build emits the empty form, which must still be valid source.
        let mut stub = fixture_header();
        push_rxe_blob(&mut stub, "PROGRAM_RXE", "the probe fixture program", &[]);
        assert!(
            stub.contains("pub const PROGRAM_RXE: &[u8] = &[\n];"),
            "{stub}"
        );
    }

    #[test]
    fn grouped_hex_literals_are_separated_every_four_digits() {
        let mut buf = [0u8; GROUPED_HEX_LEN];
        // The user-image bias the generator emits: the regression this
        // helper exists for, a bare `0x1000000000` in generated source.
        assert_eq!(
            format_grouped_hex(0x10_0000_0000, &mut buf),
            "0x0010_0000_0000"
        );
        // Leading all-zero groups are dropped so the literal stays short…
        assert_eq!(format_grouped_hex(0x1234, &mut buf), "0x1234");
        assert_eq!(format_grouped_hex(0x1_2345, &mut buf), "0x0001_2345");
        // …but never the last group, so zero is still a literal.
        assert_eq!(format_grouped_hex(0, &mut buf), "0x0000");
        // Full width round-trips every nibble.
        assert_eq!(
            format_grouped_hex(u64::MAX, &mut buf),
            "0xffff_ffff_ffff_ffff"
        );
        assert_eq!(
            format_grouped_hex(0x0123_4567_89ab_cdef, &mut buf),
            "0x0123_4567_89ab_cdef"
        );
    }

    #[test]
    fn grouped_hex_output_parses_back_to_the_value_it_rendered() {
        // The generated file is compiled, so the rendering must be a Rust
        // literal that means exactly the value asked for — a separator in
        // the wrong place would silently change a mapped base address.
        for value in [
            0,
            1,
            0xffff,
            0x1_0000,
            0x10_0000_0000,
            0xdead_beef_cafe_f00d,
            u64::MAX,
        ] {
            let mut buf = [0u8; GROUPED_HEX_LEN];
            let rendered = format_grouped_hex(value, &mut buf);
            let mut parsed: u64 = 0;
            for digit in rendered.trim_start_matches("0x").bytes() {
                if digit == b'_' {
                    continue;
                }
                let nibble = char::from(digit).to_digit(16).expect("hex digit");
                parsed = (parsed << 4) | u64::from(nibble);
            }
            assert_eq!(parsed, value, "{rendered}");
            // Every group is exactly four digits — the shape the
            // separator lint accepts without complaint.
            for group in rendered.trim_start_matches("0x").split('_') {
                assert_eq!(group.len(), 4, "{rendered}");
            }
        }
    }
}

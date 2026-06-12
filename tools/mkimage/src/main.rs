//! `rustos-mkimage` — author flashable RustOS platform images.
//!
//! ```text
//! rustos-mkimage rpi \
//!     --kernel <rustos-kernel ELF> \
//!     --firmware <dir with the pinned blobs> \
//!     [--manifest <firmware.lock>] \
//!     [--profile debug|installer] \
//!     [--out images/rustos-aarch64-rpi-<profile>.img] \
//!     [--root-key <key file>] [--root-key-out <key file>]
//! ```
//!
//! The normal entry point is `cargo xtask image --target aarch64-rpi`
//! (or `cargo xtask build --target aarch64-rpi`), which builds the kernel
//! and calls this crate as a library; the binary exists for direct,
//! scripted use. See `docs/src/install/raspberry_pi.md`.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use rustos_mkimage::firmware::FirmwareManifest;
use rustos_mkimage::{
    build_rpi_image, volume_key_from_hex, volume_key_to_hex, EntropySource, HostEntropy,
    ImageProfile, VolumeKey,
};

fn main() -> ExitCode {
    let argv: Vec<OsString> = env::args_os().skip(1).collect();
    match run(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("rustos-mkimage: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Parsed `rpi` subcommand arguments.
struct RpiArgs {
    kernel: PathBuf,
    firmware_dir: PathBuf,
    manifest: PathBuf,
    profile: ImageProfile,
    out: PathBuf,
    root_key_in: Option<PathBuf>,
    root_key_out: Option<PathBuf>,
}

fn run(argv: &[OsString]) -> Result<(), String> {
    let Some((subcommand, rest)) = argv.split_first() else {
        return Err(usage());
    };
    if subcommand != "rpi" {
        return Err(usage());
    }
    let rpi = parse_rpi_args(rest)?;

    let manifest_text = std::fs::read_to_string(&rpi.manifest)
        .map_err(|e| format!("cannot read manifest {}: {e}", rpi.manifest.display()))?;
    let manifest = FirmwareManifest::parse(&manifest_text).map_err(|e| e.to_string())?;
    let firmware = manifest
        .load_dir(&rpi.firmware_dir)
        .map_err(|e| e.to_string())?;

    let kernel_elf = std::fs::read(&rpi.kernel)
        .map_err(|e| format!("cannot read kernel ELF {}: {e}", rpi.kernel.display()))?;

    let root_key: VolumeKey = if let Some(path) = &rpi.root_key_in {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read root-key file {}: {e}", path.display()))?;
        volume_key_from_hex(&text).map_err(|e| e.to_string())?
    } else {
        let mut key = [0u8; rustos_mkimage::VOLUME_KEY_LEN];
        HostEntropy
            .fill(&mut key)
            .map_err(|e| format!("host entropy unavailable: {e:?}"))?;
        key
    };

    let built = build_rpi_image(
        &kernel_elf,
        &firmware,
        &root_key,
        &mut HostEntropy,
        rpi.profile,
    )
    .map_err(|e| e.to_string())?;

    if let Some(parent) = rpi.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(&rpi.out, &built.image)
        .map_err(|e| format!("cannot write image {}: {e}", rpi.out.display()))?;

    let key_out = rpi
        .root_key_out
        .unwrap_or_else(|| rpi.out.with_extension("rootkey"));
    write_key_file(&key_out, &volume_key_to_hex(&built.root_key))?;

    println!(
        "wrote {} ({} bytes) and root volume key {}",
        rpi.out.display(),
        built.image.len(),
        key_out.display()
    );
    Ok(())
}

/// Write the root-key file with owner-only permissions: it is the mount
/// secret for the image's root volume (`AGENTS.md` §5.4 secret hygiene).
fn write_key_file(path: &std::path::Path, body: &str) -> Result<(), String> {
    std::fs::write(path, body)
        .map_err(|e| format!("cannot write root-key file {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot restrict root-key file {}: {e}", path.display()))?;
    }
    Ok(())
}

fn parse_rpi_args(rest: &[OsString]) -> Result<RpiArgs, String> {
    let mut kernel = None;
    let mut firmware_dir = None;
    let mut manifest = None;
    let mut profile = None;
    let mut out = None;
    let mut root_key_in = None;
    let mut root_key_out = None;

    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let Some(flag) = flag.to_str() else {
            return Err("arguments must be valid UTF-8".into());
        };
        let mut value = |name: &str| -> Result<PathBuf, String> {
            it.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "--kernel" => kernel = Some(value("--kernel")?),
            "--firmware" => firmware_dir = Some(value("--firmware")?),
            "--manifest" => manifest = Some(value("--manifest")?),
            "--profile" => {
                let name = value("--profile")?;
                let name = name
                    .to_str()
                    .ok_or_else(|| "--profile value must be valid UTF-8".to_string())?;
                profile = Some(ImageProfile::from_label(name).ok_or_else(|| {
                    format!("unknown profile {name:?}; expected `debug` or `installer`")
                })?);
            }
            "--out" => out = Some(value("--out")?),
            "--root-key" => root_key_in = Some(value("--root-key")?),
            "--root-key-out" => root_key_out = Some(value("--root-key-out")?),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }

    let profile = profile.unwrap_or(ImageProfile::Debug);
    Ok(RpiArgs {
        kernel: kernel.ok_or_else(|| format!("--kernel is required\n{}", usage()))?,
        firmware_dir: firmware_dir.ok_or_else(|| format!("--firmware is required\n{}", usage()))?,
        manifest: manifest.unwrap_or_else(|| {
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/firmware.lock"))
        }),
        profile,
        out: out.unwrap_or_else(|| {
            PathBuf::from(format!("images/rustos-aarch64-rpi-{}.img", profile.label()))
        }),
        root_key_in,
        root_key_out,
    })
}

fn usage() -> String {
    "usage: rustos-mkimage rpi --kernel <elf> --firmware <dir> \
     [--manifest <firmware.lock>] [--profile debug|installer] [--out <img>] \
     [--root-key <file>] [--root-key-out <file>]"
        .into()
}

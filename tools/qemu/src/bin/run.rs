//! Thin CLI wrapper around [`tairix_qemu::Runner`].
//!
//! Intended for manual debugging — `cargo xtask test --qemu` drives the
//! library directly and never shells out to this binary. Keeping the wrapper
//! in-tree means a developer chasing a flake can reproduce the *exact* QEMU
//! invocation the test runner used, without having to read xtask source.
//!
//! Usage:
//! ```text
//! cargo run -p tairix-qemu --bin tairix-qemu-run -- \
//!     --kernel path/to/kernel.elf [--arch x86_64|riscv64|aarch64] [--cpus N] \
//!     [--timeout-secs S] [--virtio-blk path/to/disk.img ...] \
//!     [--virtio-net-dgram <qemu-sock> <peer-sock> <capture.pcap>] [--ramfb] \
//!     [--virtio-keyboard <ready-marker> <qkeycode>]
//! ```

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use tairix_qemu::{Arch, Outcome, Runner, Spec};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let mut kernel: Option<PathBuf> = None;
    let mut cpus: u32 = 1;
    let mut timeout = Duration::from_secs(30);
    let mut block_images: Vec<PathBuf> = Vec::new();
    let mut net_dgram: Option<(PathBuf, PathBuf, PathBuf)> = None;
    let mut arch = Arch::X86_64;
    let mut ramfb = false;
    let mut keyboard: Option<(String, String)> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--kernel" => kernel = args.next().map(PathBuf::from),
            "--arch" => {
                arch = match args.next().as_deref() {
                    Some("x86_64") => Arch::X86_64,
                    Some("riscv64") => Arch::Riscv64,
                    Some("aarch64") => Arch::Aarch64,
                    _ => return usage_err("--arch must be x86_64, riscv64, or aarch64"),
                };
            }
            "--ramfb" => ramfb = true,
            "--virtio-keyboard" => {
                let (Some(marker), Some(key)) = (args.next(), args.next()) else {
                    return usage_err("--virtio-keyboard needs <ready-marker> <qkeycode>");
                };
                keyboard = Some((marker, key));
            }
            "--cpus" => {
                let Some(v) = args.next() else {
                    return usage_err("--cpus needs a value");
                };
                let Ok(n) = v.parse::<u32>() else {
                    return usage_err("--cpus must be a u32");
                };
                cpus = n.max(1);
            }
            "--timeout-secs" => {
                let Some(v) = args.next() else {
                    return usage_err("--timeout-secs needs a value");
                };
                let Ok(n) = v.parse::<u64>() else {
                    return usage_err("--timeout-secs must be a u64");
                };
                timeout = Duration::from_secs(n);
            }
            "--virtio-blk" => {
                let Some(v) = args.next() else {
                    return usage_err("--virtio-blk needs a path");
                };
                block_images.push(PathBuf::from(v));
            }
            "--virtio-net-dgram" => {
                let (Some(qemu_sock), Some(peer_sock), Some(pcap)) =
                    (args.next(), args.next(), args.next())
                else {
                    return usage_err("--virtio-net-dgram needs <qemu-sock> <peer-sock> <pcap>");
                };
                net_dgram = Some((
                    PathBuf::from(qemu_sock),
                    PathBuf::from(peer_sock),
                    PathBuf::from(pcap),
                ));
            }
            "-h" | "--help" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            other => return usage_err(&format!("unknown argument `{other}`")),
        }
    }

    let Some(kernel) = kernel else {
        return usage_err("--kernel is required");
    };

    let base = match arch {
        Arch::X86_64 => Spec::for_x86_64_kernel(kernel),
        Arch::Riscv64 => Spec::for_riscv64_kernel(kernel),
        Arch::Aarch64 => Spec::for_aarch64_kernel(kernel),
    };
    let mut spec = base.with_cpus(cpus).with_timeout(timeout);
    for image in block_images {
        spec = spec.with_virtio_blk(image);
    }
    if let Some((qemu_sock, peer_sock, pcap)) = net_dgram {
        spec = spec.with_virtio_net_dgram(qemu_sock, peer_sock, pcap);
    }
    if ramfb {
        spec = spec.with_ramfb();
    }
    if let Some((marker, key)) = keyboard {
        spec = spec.with_virtio_keyboard(marker, key);
    }

    report(Runner::run(&spec))
}

/// Translate a [`Runner::run`] result into a process exit code, printing
/// the captured serial log on a non-pass outcome.
fn report(result: std::io::Result<Outcome>) -> ExitCode {
    match result {
        Ok(Outcome::Pass { .. }) => ExitCode::SUCCESS,
        Ok(Outcome::Fail { status, serial }) => {
            eprintln!("tairix-qemu-run: FAIL (qemu status {status})");
            eprint!("{serial}");
            ExitCode::from(1)
        }
        Ok(Outcome::Timeout { budget, serial }) => {
            eprintln!(
                "tairix-qemu-run: HUNG: the guest fell silent for its whole {budget:?} inactivity budget; the transcript's last line is the stall point"
            );
            eprint!("{serial}");
            ExitCode::from(2)
        }
        Ok(Outcome::RuntimeCeilingExceeded {
            ceiling,
            silent_for,
            serial,
        }) => {
            eprintln!(
                "tairix-qemu-run: UNFINISHED at the {ceiling:?} runtime ceiling \
                 (guest still alive, silent for {silent_for:?})"
            );
            eprint!("{serial}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("tairix-qemu-run: could not spawn QEMU: {e}");
            ExitCode::from(3)
        }
    }
}

fn usage() -> &'static str {
    "usage: tairix-qemu-run --kernel <path> [--arch x86_64|riscv64|aarch64] [--cpus N] \
[--timeout-secs S] [--virtio-blk <image> ...] \
[--virtio-net-dgram <qemu-sock> <peer-sock> <pcap>] [--ramfb] \
[--virtio-keyboard <ready-marker> <qkeycode>]"
}

fn usage_err(msg: &str) -> ExitCode {
    eprintln!("tairix-qemu-run: {msg}\n{}", usage());
    ExitCode::from(2)
}

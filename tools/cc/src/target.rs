//! Native Tier-1 C compilation targets and the argv the wrapper builds for
//! `clang` and `ld.lld`.
//!
//! The set of targets is closed to the three **native** Tier-1 targets
//! (`AGENTS.md` §1); `wasm32` has no trap instruction and is out of scope for
//! the C runtime (`plans/CCOMPAT.md` §1). The argv builders are pure functions
//! so the flag recipe is unit-tested without invoking a real toolchain.

use std::ffi::OsString;
use std::path::Path;

/// A native Tier-1 target a C program can be cross-compiled for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CTarget {
    /// `riscv64gc-unknown-none-elf` (the `virt` board; the first vertical).
    Riscv64,
    /// `aarch64-unknown-none`.
    Aarch64,
    /// `x86_64-unknown-none`.
    X86_64,
}

impl CTarget {
    /// Human-readable name used in audit and error messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            CTarget::Riscv64 => "riscv64",
            CTarget::Aarch64 => "aarch64",
            CTarget::X86_64 => "x86_64",
        }
    }

    /// The `--target=` triple handed to `clang`.
    ///
    /// A bare-metal ELF triple is used so `clang` emits a freestanding object
    /// with no host-libc assumptions; the Rust startup/runtime is supplied
    /// separately by the crt0 + `abi-sys` static archive at link time.
    #[must_use]
    pub fn clang_triple(self) -> &'static str {
        match self {
            CTarget::Riscv64 => "riscv64-unknown-elf",
            CTarget::Aarch64 => "aarch64-unknown-elf",
            CTarget::X86_64 => "x86_64-unknown-elf",
        }
    }

    /// Architecture-specific `clang` flags pinning the exact ISA/ABI the
    /// matching Rust target uses, so the C object and the Rust archive agree
    /// on calling convention and register width.
    ///
    /// Only the riscv64 vertical is exercised today; the other two targets
    /// declare their ABI flags here so the wrapper stays a single multi-target
    /// definition (`AGENTS.md` §2.2) rather than growing a second copy when
    /// the aarch64 / x86_64 verticals land.
    #[must_use]
    pub fn clang_arch_flags(self) -> &'static [&'static str] {
        match self {
            // `rv64gc` + `lp64d`: the exact ISA/ABI of
            // `riscv64gc-unknown-none-elf`.
            CTarget::Riscv64 => &["-march=rv64gc", "-mabi=lp64d"],
            CTarget::Aarch64 => &[],
            // Kernel-style code model: no red zone (the kernel may run on the
            // same stack across the trap) matching `x86_64-unknown-none`.
            CTarget::X86_64 => &["-mno-red-zone"],
        }
    }
}

/// A request to compile one C translation unit to a relocatable object.
#[derive(Debug)]
pub struct CompileRequest<'a> {
    /// Target the object is compiled for.
    pub target: CTarget,
    /// The `.c` source file.
    pub source: &'a Path,
    /// The `.o` object file to write.
    pub object: &'a Path,
    /// Header search directories (each becomes a `-I`), in order.
    pub include_dirs: &'a [&'a Path],
}

/// A request to link relocatable objects and static archives into a
/// position-independent executable.
#[derive(Debug)]
pub struct LinkRequest<'a> {
    /// Target the image is linked for.
    pub target: CTarget,
    /// Object files to link, in order.
    pub objects: &'a [&'a Path],
    /// Static archives (`.a`) to link, in order. The Rust crt0 + `abi-sys`
    /// runtime archive goes here.
    pub archives: &'a [&'a Path],
    /// Linker script (`-T`) rooting `_start` and laying out W^X segments.
    pub linker_script: &'a Path,
    /// The PIE ELF to write.
    pub output: &'a Path,
}

/// Flags shared by every C compilation, regardless of target.
///
/// * `-fPIC` / position-independent: the image is loaded as a relocatable
///   `rxe` PIE (`AGENTS.md` §19.2).
/// * `-ffreestanding` / `-nostdlib`: no host libc; the program reaches the
///   kernel only through the `ros_sys_*` runtime.
/// * `-fstack-protector-strong`: emit the §19.2 stack canary; crt0 supplies
///   `__stack_chk_guard` / `__stack_chk_fail`.
/// * `-Wall -Wextra -Werror`: senior-quality bar (`AGENTS.md` §2.6).
const COMMON_COMPILE_FLAGS: &[&str] = &[
    "-fPIC",
    "-ffreestanding",
    "-nostdlib",
    "-fstack-protector-strong",
    "-fno-asynchronous-unwind-tables",
    "-Os",
    "-std=c17",
    "-Wall",
    "-Wextra",
    "-Werror",
];

/// Build the full `clang` argument vector for a compile request.
#[must_use]
pub fn compile_argv(req: &CompileRequest<'_>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    argv.push(OsString::from(format!(
        "--target={}",
        req.target.clang_triple()
    )));
    for flag in req.target.clang_arch_flags() {
        argv.push(OsString::from(*flag));
    }
    for flag in COMMON_COMPILE_FLAGS {
        argv.push(OsString::from(*flag));
    }
    for dir in req.include_dirs {
        argv.push(OsString::from("-I"));
        argv.push(dir.as_os_str().to_os_string());
    }
    argv.push(OsString::from("-c"));
    argv.push(req.source.as_os_str().to_os_string());
    argv.push(OsString::from("-o"));
    argv.push(req.object.as_os_str().to_os_string());
    argv
}

/// Build the full `ld.lld` argument vector for a link request.
///
/// The image is a hardened PIE: `-pie` makes it position-independent,
/// `--gc-sections` drops unreferenced sections (so only the crt0 archive
/// members actually used are pulled in), `-z noexecstack` marks the stack
/// non-executable, and the linker script enforces W^X page-granular segments
/// (`AGENTS.md` §19.2).
#[must_use]
pub fn link_argv(req: &LinkRequest<'_>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        OsString::from("-pie"),
        OsString::from("--gc-sections"),
        OsString::from("-z"),
        OsString::from("noexecstack"),
        OsString::from("-T"),
        req.linker_script.as_os_str().to_os_string(),
        OsString::from("-o"),
        req.output.as_os_str().to_os_string(),
    ];
    for object in req.objects {
        argv.push(object.as_os_str().to_os_string());
    }
    for archive in req.archives {
        argv.push(archive.as_os_str().to_os_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn triples_are_bare_metal_elf() {
        assert_eq!(CTarget::Riscv64.clang_triple(), "riscv64-unknown-elf");
        assert_eq!(CTarget::Aarch64.clang_triple(), "aarch64-unknown-elf");
        assert_eq!(CTarget::X86_64.clang_triple(), "x86_64-unknown-elf");
    }

    #[test]
    fn riscv64_pins_the_rv64gc_lp64d_abi() {
        assert_eq!(
            CTarget::Riscv64.clang_arch_flags(),
            &["-march=rv64gc", "-mabi=lp64d"]
        );
    }

    #[test]
    fn compile_argv_is_pic_freestanding_and_canary_protected() {
        let src = PathBuf::from("/p/main.c");
        let obj = PathBuf::from("/p/main.o");
        let inc = PathBuf::from("/p/include");
        let includes = [inc.as_path()];
        let req = CompileRequest {
            target: CTarget::Riscv64,
            source: &src,
            object: &obj,
            include_dirs: &includes,
        };
        let argv = compile_argv(&req);
        let joined: Vec<String> = argv
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(joined[0], "--target=riscv64-unknown-elf");
        assert!(joined.iter().any(|a| a == "-fPIC"));
        assert!(joined.iter().any(|a| a == "-ffreestanding"));
        assert!(joined.iter().any(|a| a == "-fstack-protector-strong"));
        assert!(joined.iter().any(|a| a == "-Werror"));
        assert!(joined.iter().any(|a| a == "-march=rv64gc"));
        // `-I <dir>` then `-c source -o object` at the tail, in order.
        let i_pos = joined.iter().position(|a| a == "-I").expect("has -I");
        assert_eq!(joined[i_pos + 1], "/p/include");
        let c_pos = joined.iter().position(|a| a == "-c").expect("has -c");
        assert_eq!(joined[c_pos + 1], "/p/main.c");
        assert_eq!(joined[joined.len() - 2], "-o");
        assert_eq!(joined[joined.len() - 1], "/p/main.o");
    }

    #[test]
    fn link_argv_is_a_hardened_pie_with_objects_before_archives() {
        let obj = PathBuf::from("/p/main.o");
        let ar = PathBuf::from("/p/libshim.a");
        let script = PathBuf::from("/p/program.ld");
        let out = PathBuf::from("/p/prog.elf");
        let objects = [obj.as_path()];
        let archives = [ar.as_path()];
        let req = LinkRequest {
            target: CTarget::Riscv64,
            objects: &objects,
            archives: &archives,
            linker_script: &script,
            output: &out,
        };
        let argv = link_argv(&req);
        let joined: Vec<String> = argv
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(joined.iter().any(|a| a == "-pie"));
        assert!(joined.iter().any(|a| a == "--gc-sections"));
        assert!(joined
            .windows(2)
            .any(|w| w[0] == "-z" && w[1] == "noexecstack"));
        let t_pos = joined.iter().position(|a| a == "-T").expect("has -T");
        assert_eq!(joined[t_pos + 1], "/p/program.ld");
        // The linker resolves archive members on demand, so objects must
        // precede archives on the command line.
        let obj_pos = joined.iter().position(|a| a == "/p/main.o").expect("obj");
        let ar_pos = joined
            .iter()
            .position(|a| a == "/p/libshim.a")
            .expect("archive");
        assert!(obj_pos < ar_pos);
    }
}

//! ELF → flat `kernel8.img` conversion.
//!
//! The Pi firmware does not load ELF: it copies `kernel8.img` byte-for-byte
//! to physical [`KERNEL_LOAD_ADDR`] and branches to its first byte. The
//! converter therefore takes the freestanding `tairix-kernel` ELF (linked by
//! `aarch64-rpi4.ld` at that address) and lays its `PT_LOAD` file bytes out
//! at their physical offsets, zero-filling any inter-segment gap. Trailing
//! `.bss` is *not* emitted — the boot stub clears it — so the image stays as
//! small as the file-backed content.
//!
//! Every header field that steers the layout is validated fail-closed: a kernel linked at the wrong address, an entry point
//! that is not the image start, or an overlapping/oversized segment is a
//! build defect the converter refuses rather than an image that faults on
//! metal.

use crate::MkimageError;

/// Physical address the Pi firmware loads `kernel8.img` to, and the entry
/// point it branches to (`docs/src/platform/aarch64.md`, "Boot protocol").
pub const KERNEL_LOAD_ADDR: u64 = 0x8_0000;

/// Physical address the QEMU `virt` board's kernel links at
/// (`kernel/arch/aarch64/link/aarch64-virt.ld`): 2 MiB above the
/// board's RAM base, leaving low RAM for the DTB QEMU generates.
pub const VIRT_KERNEL_LOAD_ADDR: u64 = 0x4020_0000;

/// RAM base of the QEMU `virt` board — the base the Linux-boot
/// protocol's `text_offset` is measured from, and where QEMU places the
/// generated DTB it passes in `x0`.
const VIRT_RAM_BASE: u64 = 0x4000_0000;

/// Byte length of the arm64 Linux `Image` header
/// [`build_virt_boot_image`] prepends (Linux
/// `Documentation/arch/arm64/booting.rst`).
const IMAGE_HEADER_LEN: usize = 64;

/// `code0` of the emitted `Image` header: the AArch64 `b #64`
/// instruction, branching over the 64-byte header into the kernel's
/// first byte (the boot stub's `_start`).
const IMAGE_HEADER_BRANCH: u32 = 0x1400_0010;

/// Upper bound on the emitted flat image, in bytes. A defence bound against a malformed ELF demanding a huge zero-fill,
/// not a kernel-size capacity: the kernel is a few MiB; 64 MiB is far
/// beyond any honest layout.
pub const MAX_FLAT_BYTES: u64 = 64 << 20;

const ELF64_HEADER_LEN: usize = 64;
const ELF64_PHENTSIZE: usize = 56;
const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;

/// One `PT_LOAD` segment's file-backed extent.
struct LoadSeg {
    paddr: u64,
    filesz: u64,
    memsz: u64,
    offset: usize,
}

/// Convert the freestanding aarch64 kernel ELF `elf` into flat bytes
/// loaded byte-for-byte at `load_addr` and entered at their first byte
/// (the Pi image passes [`KERNEL_LOAD_ADDR`]; the QEMU `virt` boot image
/// passes [`VIRT_KERNEL_LOAD_ADDR`]).
///
/// # Errors
///
/// [`MkimageError::KernelElf`] if `elf` is not a statically-linked
/// little-endian `ELF64` aarch64 executable whose `PT_LOAD` layout starts
/// and enters at `load_addr`, or if any segment is truncated,
/// overlapping, or beyond [`MAX_FLAT_BYTES`].
pub fn elf_to_flat(elf: &[u8], load_addr: u64) -> Result<Vec<u8>, MkimageError> {
    if elf.len() < ELF64_HEADER_LEN {
        return Err(MkimageError::KernelElf(
            "file too short for an ELF64 header",
        ));
    }
    if elf[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(MkimageError::KernelElf("not an ELF file"));
    }
    if elf[4] != 2 || elf[5] != 1 {
        return Err(MkimageError::KernelElf("not a little-endian ELF64 image"));
    }
    if read_u16(elf, 16)? != ET_EXEC {
        return Err(MkimageError::KernelElf(
            "not an ET_EXEC image (the kernel is statically linked, not PIE)",
        ));
    }
    if read_u16(elf, 18)? != EM_AARCH64 {
        return Err(MkimageError::KernelElf("not an aarch64 image"));
    }
    let entry = read_u64(elf, 24)?;
    let phoff = read_u64(elf, 32)?;
    let phentsize = usize::from(read_u16(elf, 54)?);
    let phnum = usize::from(read_u16(elf, 56)?);
    if phentsize != ELF64_PHENTSIZE {
        return Err(MkimageError::KernelElf("unexpected program-header size"));
    }

    let mut loads = decode_loads(elf, phoff, phnum)?;
    loads.sort_by_key(|s| s.paddr);
    let Some(first) = loads.first() else {
        return Err(MkimageError::KernelElf("no PT_LOAD segments"));
    };
    if first.paddr != load_addr {
        return Err(MkimageError::KernelElf(
            "kernel is not linked at the requested load address",
        ));
    }
    if entry != load_addr {
        return Err(MkimageError::KernelElf(
            "kernel entry point is not the image start (the loader branches to the first byte)",
        ));
    }
    for pair in loads.windows(2) {
        let prev_end = pair[0]
            .paddr
            .checked_add(pair[0].memsz)
            .ok_or(MkimageError::KernelElf("segment end overflows"))?;
        if pair[1].paddr < prev_end {
            return Err(MkimageError::KernelElf("PT_LOAD segments overlap"));
        }
    }

    let flat_len = loads
        .iter()
        .map(|s| s.paddr - load_addr + s.filesz)
        .max()
        .unwrap_or(0);
    if flat_len == 0 {
        return Err(MkimageError::KernelElf("image has no file-backed bytes"));
    }
    if flat_len > MAX_FLAT_BYTES {
        return Err(MkimageError::KernelElf("flat image exceeds the size bound"));
    }
    let flat_len =
        usize::try_from(flat_len).map_err(|_| MkimageError::KernelElf("image too large"))?;

    let mut flat = vec![0u8; flat_len];
    for seg in &loads {
        let filesz = usize::try_from(seg.filesz)
            .map_err(|_| MkimageError::KernelElf("segment too large"))?;
        let dst = usize::try_from(seg.paddr - load_addr)
            .map_err(|_| MkimageError::KernelElf("segment too far"))?;
        flat[dst..dst + filesz].copy_from_slice(&elf[seg.offset..seg.offset + filesz]);
    }
    Ok(flat)
}

/// Convert a QEMU-`virt`-linked kernel ELF into the raw boot image
/// `qemu-system-aarch64 -M virt -kernel` loads with a DTB pointer in
/// `x0`.
///
/// QEMU's ELF `-kernel` path passes **no** DTB (`x0 = 0`), so the
/// interactive `cargo xtask run` session boots the *Linux-boot* path
/// instead: an arm64 `Image` header whose `text_offset` places the
/// header at `link address − 64` — landing the kernel's first byte
/// exactly at its [`VIRT_KERNEL_LOAD_ADDR`] link address — and whose
/// `code0` branches over the header into the boot stub. QEMU then
/// generates the board's real device tree, places it at the RAM base,
/// and enters with `x0` pointing at it, exactly like the Pi firmware.
///
/// # Errors
///
/// As [`elf_to_flat`], against [`VIRT_KERNEL_LOAD_ADDR`].
pub fn build_virt_boot_image(elf: &[u8]) -> Result<Vec<u8>, MkimageError> {
    let flat = elf_to_flat(elf, VIRT_KERNEL_LOAD_ADDR)?;
    let text_offset = VIRT_KERNEL_LOAD_ADDR - VIRT_RAM_BASE - IMAGE_HEADER_LEN as u64;
    let image_size = (IMAGE_HEADER_LEN + flat.len()) as u64;
    let mut image = Vec::with_capacity(IMAGE_HEADER_LEN + flat.len());
    image.extend_from_slice(&IMAGE_HEADER_BRANCH.to_le_bytes()); // code0
    image.extend_from_slice(&0u32.to_le_bytes()); // code1
    image.extend_from_slice(&text_offset.to_le_bytes());
    // A zero image_size makes loaders fall back to the legacy fixed
    // offset and ignore text_offset, so it must be honest.
    image.extend_from_slice(&image_size.to_le_bytes());
    image.extend_from_slice(&0u64.to_le_bytes()); // flags: little-endian
    image.extend_from_slice(&[0u8; 24]); // res2..res4
    image.extend_from_slice(b"ARM\x64"); // magic
    image.extend_from_slice(&[0u8; 4]); // res5
    image.extend_from_slice(&flat);
    Ok(image)
}

/// Decode and bound-check every `PT_LOAD` program header.
fn decode_loads(elf: &[u8], phoff: u64, phnum: usize) -> Result<Vec<LoadSeg>, MkimageError> {
    let phoff =
        usize::try_from(phoff).map_err(|_| MkimageError::KernelElf("program headers truncated"))?;
    let mut loads = Vec::new();
    for i in 0..phnum {
        let at = phoff
            .checked_add(i * ELF64_PHENTSIZE)
            .ok_or(MkimageError::KernelElf("program headers truncated"))?;
        if read_u32(elf, at)? != PT_LOAD {
            continue;
        }
        let offset = read_u64(elf, at + 8)?;
        let paddr = read_u64(elf, at + 24)?;
        let filesz = read_u64(elf, at + 32)?;
        let memsz = read_u64(elf, at + 40)?;
        if filesz > memsz {
            return Err(MkimageError::KernelElf(
                "segment file size exceeds memory size",
            ));
        }
        let offset = usize::try_from(offset)
            .map_err(|_| MkimageError::KernelElf("segment offset out of range"))?;
        let file_end = u64::try_from(offset)
            .ok()
            .and_then(|o| o.checked_add(filesz))
            .ok_or(MkimageError::KernelElf("segment extent overflows"))?;
        if file_end > elf.len() as u64 {
            return Err(MkimageError::KernelElf("segment extends past the file"));
        }
        if filesz == 0 && memsz == 0 {
            continue;
        }
        loads.push(LoadSeg {
            paddr,
            filesz,
            memsz,
            offset,
        });
    }
    Ok(loads)
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, MkimageError> {
    let end = at
        .checked_add(2)
        .ok_or(MkimageError::KernelElf("truncated field"))?;
    let slice = bytes
        .get(at..end)
        .ok_or(MkimageError::KernelElf("truncated field"))?;
    Ok(u16::from_le_bytes(
        slice
            .try_into()
            .map_err(|_| MkimageError::KernelElf("truncated field"))?,
    ))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, MkimageError> {
    let end = at
        .checked_add(4)
        .ok_or(MkimageError::KernelElf("truncated field"))?;
    let slice = bytes
        .get(at..end)
        .ok_or(MkimageError::KernelElf("truncated field"))?;
    Ok(u32::from_le_bytes(
        slice
            .try_into()
            .map_err(|_| MkimageError::KernelElf("truncated field"))?,
    ))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, MkimageError> {
    let end = at
        .checked_add(8)
        .ok_or(MkimageError::KernelElf("truncated field"))?;
    let slice = bytes
        .get(at..end)
        .ok_or(MkimageError::KernelElf("truncated field"))?;
    Ok(u64::from_le_bytes(
        slice
            .try_into()
            .map_err(|_| MkimageError::KernelElf("truncated field"))?,
    ))
}

/// Test-only builders shared with the crate-level assembly tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{
        ELF64_HEADER_LEN, ELF64_PHENTSIZE, EM_AARCH64, ET_EXEC, KERNEL_LOAD_ADDR, PT_LOAD,
    };

    pub fn w16(buf: &mut [u8], at: usize, x: u16) {
        buf[at..at + 2].copy_from_slice(&x.to_le_bytes());
    }
    pub fn w32(buf: &mut [u8], at: usize, x: u32) {
        buf[at..at + 4].copy_from_slice(&x.to_le_bytes());
    }
    pub fn w64(buf: &mut [u8], at: usize, x: u64) {
        buf[at..at + 8].copy_from_slice(&x.to_le_bytes());
    }

    pub struct Seg {
        pub paddr: u64,
        pub bytes: Vec<u8>,
        pub bss: u64,
    }

    /// A minimal valid kernel ELF: one code segment of `code` at the Pi
    /// load address, entered at its first byte.
    pub fn sample_kernel(code: &[u8]) -> Vec<u8> {
        sample_elf(
            KERNEL_LOAD_ADDR,
            &[Seg {
                paddr: KERNEL_LOAD_ADDR,
                bytes: code.to_vec(),
                bss: 0,
            }],
        )
    }

    /// A minimal `ET_EXEC` aarch64 ELF with the given segments, entered at
    /// `entry`.
    pub fn sample_elf(entry: u64, segs: &[Seg]) -> Vec<u8> {
        let phnum = segs.len();
        let data_at = ELF64_HEADER_LEN + phnum * ELF64_PHENTSIZE;
        let mut out = vec![0u8; data_at];
        out[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // little-endian
        out[6] = 1; // EV_CURRENT
        w16(&mut out, 16, ET_EXEC);
        w16(&mut out, 18, EM_AARCH64);
        w32(&mut out, 20, 1);
        w64(&mut out, 24, entry);
        w64(&mut out, 32, ELF64_HEADER_LEN as u64); // e_phoff
        w16(&mut out, 52, u16::try_from(ELF64_HEADER_LEN).expect("fits")); // e_ehsize
        w16(&mut out, 54, u16::try_from(ELF64_PHENTSIZE).expect("fits"));
        w16(&mut out, 56, u16::try_from(phnum).expect("fits"));

        let mut offset = data_at as u64;
        for (i, seg) in segs.iter().enumerate() {
            let at = ELF64_HEADER_LEN + i * ELF64_PHENTSIZE;
            w32(&mut out, at, PT_LOAD);
            w32(&mut out, at + 4, 0b101); // R+X; the converter ignores flags
            w64(&mut out, at + 8, offset);
            w64(&mut out, at + 16, seg.paddr); // p_vaddr
            w64(&mut out, at + 24, seg.paddr); // p_paddr
            w64(&mut out, at + 32, seg.bytes.len() as u64);
            w64(&mut out, at + 40, seg.bytes.len() as u64 + seg.bss);
            w64(&mut out, at + 48, 16);
            offset += seg.bytes.len() as u64;
        }
        for seg in segs {
            out.extend_from_slice(&seg.bytes);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{sample_elf, sample_kernel, w16, w64, Seg};
    use super::*;

    #[test]
    fn virt_boot_image_carries_a_valid_arm64_image_header() {
        let code = [0xde, 0xad, 0xbe, 0xef];
        let elf = sample_elf(
            VIRT_KERNEL_LOAD_ADDR,
            &[Seg {
                paddr: VIRT_KERNEL_LOAD_ADDR,
                bytes: code.to_vec(),
                bss: 0,
            }],
        );
        let image = build_virt_boot_image(&elf).expect("valid virt kernel converts");
        assert_eq!(image.len(), IMAGE_HEADER_LEN + code.len());
        // code0 branches over the header into the kernel's first byte.
        assert_eq!(&image[..4], &IMAGE_HEADER_BRANCH.to_le_bytes());
        // text_offset places the *payload* exactly at the link address.
        let text_offset = u64::from_le_bytes(image[8..16].try_into().expect("8 bytes"));
        assert_eq!(
            VIRT_RAM_BASE + text_offset + IMAGE_HEADER_LEN as u64,
            VIRT_KERNEL_LOAD_ADDR
        );
        // image_size is honest (a zero would make loaders ignore
        // text_offset).
        let image_size = u64::from_le_bytes(image[16..24].try_into().expect("8 bytes"));
        assert_eq!(image_size, image.len() as u64);
        assert_eq!(&image[56..60], b"ARM\x64");
        assert_eq!(&image[IMAGE_HEADER_LEN..], &code);
    }

    #[test]
    fn virt_boot_image_rejects_a_pi_linked_kernel() {
        // A kernel linked for the Pi's 0x8_0000 must never be wrapped as
        // a virt boot image (it would fault at the wrong load address).
        let elf = sample_kernel(&[0u8; 8]);
        assert!(build_virt_boot_image(&elf).is_err());
    }

    #[test]
    fn lays_segments_out_at_their_physical_offsets() {
        let elf = sample_elf(
            KERNEL_LOAD_ADDR,
            &[
                Seg {
                    paddr: KERNEL_LOAD_ADDR,
                    bytes: vec![0x11; 8],
                    bss: 0,
                },
                Seg {
                    paddr: KERNEL_LOAD_ADDR + 16,
                    bytes: vec![0x22; 4],
                    bss: 32,
                },
            ],
        );
        let flat = elf_to_flat(&elf, KERNEL_LOAD_ADDR).expect("valid kernel converts");
        // 8 code bytes, an 8-byte zero gap, 4 data bytes; bss not emitted.
        assert_eq!(flat.len(), 20);
        assert_eq!(&flat[..8], &[0x11; 8]);
        assert_eq!(&flat[8..16], &[0u8; 8]);
        assert_eq!(&flat[16..], &[0x22; 4]);
    }

    #[test]
    fn rejects_non_elf_and_truncated_input() {
        assert!(elf_to_flat(b"not an elf", KERNEL_LOAD_ADDR).is_err());
        let elf = sample_elf(
            KERNEL_LOAD_ADDR,
            &[Seg {
                paddr: KERNEL_LOAD_ADDR,
                bytes: vec![0x11; 8],
                bss: 0,
            }],
        );
        assert!(elf_to_flat(&elf[..elf.len() - 4], KERNEL_LOAD_ADDR).is_err());
    }

    #[test]
    fn rejects_wrong_class_machine_and_type() {
        let good = sample_elf(
            KERNEL_LOAD_ADDR,
            &[Seg {
                paddr: KERNEL_LOAD_ADDR,
                bytes: vec![0x11; 8],
                bss: 0,
            }],
        );
        let mut wrong_class = good.clone();
        wrong_class[4] = 1;
        assert!(elf_to_flat(&wrong_class, KERNEL_LOAD_ADDR).is_err());

        let mut wrong_machine = good.clone();
        w16(&mut wrong_machine, 18, 0x3e); // EM_X86_64
        assert!(elf_to_flat(&wrong_machine, KERNEL_LOAD_ADDR).is_err());

        let mut pie = good;
        w16(&mut pie, 16, 3); // ET_DYN
        assert!(elf_to_flat(&pie, KERNEL_LOAD_ADDR).is_err());
    }

    #[test]
    fn rejects_wrong_load_address_and_entry() {
        let wrong_base = sample_elf(
            0x4020_0000,
            &[Seg {
                paddr: 0x4020_0000,
                bytes: vec![0x11; 8],
                bss: 0,
            }],
        );
        assert!(elf_to_flat(&wrong_base, KERNEL_LOAD_ADDR).is_err());

        let wrong_entry = sample_elf(
            KERNEL_LOAD_ADDR + 4,
            &[Seg {
                paddr: KERNEL_LOAD_ADDR,
                bytes: vec![0x11; 8],
                bss: 0,
            }],
        );
        assert!(elf_to_flat(&wrong_entry, KERNEL_LOAD_ADDR).is_err());
    }

    #[test]
    fn rejects_overlapping_segments() {
        let elf = sample_elf(
            KERNEL_LOAD_ADDR,
            &[
                Seg {
                    paddr: KERNEL_LOAD_ADDR,
                    bytes: vec![0x11; 8],
                    bss: 32,
                },
                Seg {
                    paddr: KERNEL_LOAD_ADDR + 8,
                    bytes: vec![0x22; 4],
                    bss: 0,
                },
            ],
        );
        assert!(elf_to_flat(&elf, KERNEL_LOAD_ADDR).is_err());
    }

    #[test]
    fn rejects_an_image_beyond_the_size_bound() {
        // A one-byte tail placed past the bound forces a huge zero-fill.
        let elf = sample_elf(
            KERNEL_LOAD_ADDR,
            &[
                Seg {
                    paddr: KERNEL_LOAD_ADDR,
                    bytes: vec![0x11; 8],
                    bss: 0,
                },
                Seg {
                    paddr: KERNEL_LOAD_ADDR + MAX_FLAT_BYTES,
                    bytes: vec![0x22; 4],
                    bss: 0,
                },
            ],
        );
        assert!(elf_to_flat(&elf, KERNEL_LOAD_ADDR).is_err());
    }

    #[test]
    fn rejects_filesz_beyond_memsz_and_truncated_segment() {
        let mut bad_sz = sample_elf(
            KERNEL_LOAD_ADDR,
            &[Seg {
                paddr: KERNEL_LOAD_ADDR,
                bytes: vec![0x11; 8],
                bss: 0,
            }],
        );
        // p_memsz < p_filesz
        w64(&mut bad_sz, ELF64_HEADER_LEN + 40, 4);
        assert!(elf_to_flat(&bad_sz, KERNEL_LOAD_ADDR).is_err());

        let mut past_eof = sample_elf(
            KERNEL_LOAD_ADDR,
            &[Seg {
                paddr: KERNEL_LOAD_ADDR,
                bytes: vec![0x11; 8],
                bss: 0,
            }],
        );
        // p_filesz/p_memsz reach past the end of the file.
        w64(&mut past_eof, ELF64_HEADER_LEN + 32, 1 << 20);
        w64(&mut past_eof, ELF64_HEADER_LEN + 40, 1 << 20);
        assert!(elf_to_flat(&past_eof, KERNEL_LOAD_ADDR).is_err());
    }
}

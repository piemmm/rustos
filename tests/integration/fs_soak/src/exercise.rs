//! The single, filesystem-agnostic soak exerciser.
//!
//! One body drives every [`SoakFs`]:
//! an integrity round-trip across nested directories with a remount
//! re-verification, then the fail-closed extremes. Everything is
//! deterministic from the per-iteration `seed`, so a failure reproduces.

use tairix_abi::driver::filesystem::{FilesystemRead, NodeId, NodeKind};
use tairix_abi::DriverError;

use crate::{RamBlock, SoakFs};

// Note: `FilesystemWrite` is reached through the `SoakFs` supertrait, so
// it is intentionally not imported here.

/// Per-file size when filling the data region, in bytes (2 MiB). The
/// soak's devices are always ≥ 256 MiB (FAT32's floor), so ext4 uses
/// 4096-byte blocks and every driver's single-file map reaches well
/// past 2 MiB (ext4's classic map alone reaches ~4 MiB at that block
/// size) — the fill therefore reports `NoSpace` from a genuinely full
/// volume, never a per-file map limit. Larger files also keep the file
/// count modest, so the fill is not dominated by directory growth.
const FILL_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Deterministic content byte for `(seed, file, offset)`. The inputs are run
/// through a SplitMix64-style avalanche so adjacent offsets yield unrelated
/// bytes: every block is then high-entropy and distinct, so the content is
/// neither compressible nor deduplicable and the fill genuinely consumes
/// physical blocks (a flat ramp would collide mod 256 and dedupe away under
/// `docs/src/filesystem/arxfs-spec.md` §9). Taking the low byte avoids any
/// narrowing `as` cast.
fn byte_at(seed: u64, file: u64, offset: u64) -> u8 {
    let mut x = seed
        ^ file.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ offset.wrapping_mul(0xD1B5_4A32_D192_ED03);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^= x >> 33;
    x.to_le_bytes()[0]
}

/// Build `len` bytes of deterministic content for file `file`.
fn content(seed: u64, file: u64, len: usize) -> Vec<u8> {
    (0..len).map(|o| byte_at(seed, file, o as u64)).collect()
}

/// Map a driver result into a descriptive soak error tagged with `what`
/// and the reproducing `seed`.
fn ck<T>(r: Result<T, DriverError>, what: &str, seed: u64) -> Result<T, String> {
    r.map_err(|e| format!("seed {seed:#x}: {what}: unexpected {e:?}"))
}

/// Assert that an operation failed with exactly `want`. Callers pass the
/// operation's `.err()` so the success payload is dropped, keeping this
/// free of a moved generic value (clippy `needless_pass_by_value`).
fn want_err(
    got: Option<DriverError>,
    want: DriverError,
    what: &str,
    seed: u64,
) -> Result<(), String> {
    match got {
        Some(e) if e == want => Ok(()),
        Some(e) => Err(format!(
            "seed {seed:#x}: {what}: expected {want:?}, got {e:?}"
        )),
        None => Err(format!("seed {seed:#x}: {what}: expected {want:?}, got Ok")),
    }
}

/// Read the whole of file `node` (known `len`) into a buffer, looping
/// until the file is exhausted.
fn read_all<F: FilesystemRead>(
    fs: &mut F,
    node: NodeId,
    len: usize,
    what: &str,
    seed: u64,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        let n = ck(fs.read_at(node, done as u64, &mut out[done..]), what, seed)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}

/// Create file `name` in `dir` with `len` deterministic bytes (file id
/// `file`), then read it back and verify.
fn write_and_verify<F: SoakFs>(
    fs: &mut F,
    dir: NodeId,
    name: &[u8],
    file: u64,
    len: usize,
    seed: u64,
) -> Result<(), String> {
    let body = content(seed, file, len);
    ck(
        fs.create(dir, name, NodeKind::RegularFile),
        "create file",
        seed,
    )?;
    let n = ck(fs.write_at(dir, name, 0, &body), "write_at", seed)?;
    if n != len {
        return Err(format!(
            "seed {seed:#x}: write_at short write: {n} of {len}"
        ));
    }
    let node = ck(fs.lookup(dir, name), "lookup after write", seed)?;
    let info = ck(fs.node_info(node), "node_info after write", seed)?;
    if info.size != len as u64 {
        return Err(format!(
            "seed {seed:#x}: size {} != written {len}",
            info.size
        ));
    }
    let back = read_all(fs, node, len, "read_at after write", seed)?;
    if back != body {
        return Err(format!(
            "seed {seed:#x}: read-back mismatch for file {file}"
        ));
    }
    Ok(())
}

/// Confirm file `name` in `dir` reads back as `len` deterministic bytes
/// of file id `file` (used after a remount).
fn verify_file<F: SoakFs>(
    fs: &mut F,
    dir: NodeId,
    name: &[u8],
    file: u64,
    len: usize,
    seed: u64,
) -> Result<(), String> {
    let node = ck(fs.lookup(dir, name), "lookup on verify", seed)?;
    let info = ck(fs.node_info(node), "node_info on verify", seed)?;
    if info.size != len as u64 {
        return Err(format!(
            "seed {seed:#x}: size {} != expected {len} after remount",
            info.size
        ));
    }
    let back = read_all(fs, node, len, "read_at on verify", seed)?;
    if back != content(seed, file, len) {
        return Err(format!(
            "seed {seed:#x}: content mismatch after remount for file {file}"
        ));
    }
    Ok(())
}

/// Collect the names a directory lists, terminating at the first `None`.
fn list_names<F: FilesystemRead>(
    fs: &mut F,
    dir: NodeId,
    seed: u64,
) -> Result<Vec<Vec<u8>>, String> {
    let mut names = Vec::new();
    let mut cursor = 0u64;
    let mut steps = 0u64;
    let mut buf = [0u8; 256];
    while let Some(entry) = ck(fs.read_dir(dir, cursor, &mut buf), "read_dir", seed)? {
        names.push(buf[..entry.name_len].to_vec());
        if entry.next_cursor == cursor {
            return Err(format!("seed {seed:#x}: read_dir cursor did not advance"));
        }
        cursor = entry.next_cursor;
        steps += 1;
        if steps > 1_000_000 {
            return Err(format!("seed {seed:#x}: read_dir did not terminate"));
        }
    }
    Ok(names)
}

/// Run one deterministic soak iteration over a fresh `device_bytes`
/// volume.
///
/// # Errors
/// Returns a descriptive, seed-tagged error on any driver failure or
/// integrity mismatch.
pub fn exercise<F: SoakFs>(device_bytes: u64, seed: u64) -> Result<(), String> {
    let block = RamBlock::new(device_bytes);
    let mut fs = ck(F::format_volume(block), "format", seed)?;
    let root = fs.root();

    // --- Integrity round-trip across a nested directory. ---
    let root_files: [(&[u8], usize); 4] = [
        (b"alpha", 4096),
        (b"bravo", 100),
        (b"charlie", 9000),
        (b"delta", 1),
    ];
    ck(
        fs.create(root, b"sub", NodeKind::Directory),
        "create dir",
        seed,
    )?;
    let sub = ck(fs.lookup(root, b"sub"), "lookup sub", seed)?;
    for (i, (name, len)) in root_files.iter().enumerate() {
        write_and_verify(&mut fs, root, name, i as u64, *len, seed)?;
    }
    write_and_verify(&mut fs, sub, b"deep", 100, 2048, seed)?;

    // The listing surfaces every created child.
    let listed = list_names(&mut fs, root, seed)?;
    for (name, _) in &root_files {
        if !listed.iter().any(|n| n.as_slice() == *name) {
            return Err(format!(
                "seed {seed:#x}: {} missing from root listing",
                String::from_utf8_lossy(name)
            ));
        }
    }
    if !listed.iter().any(|n| n.as_slice() == b"sub") {
        return Err(format!("seed {seed:#x}: sub missing from root listing"));
    }

    // Truncate one file shorter and confirm the prefix survives.
    ck(fs.truncate(root, b"alpha", 2048), "truncate", seed)?;
    {
        let node = ck(fs.lookup(root, b"alpha"), "lookup truncated", seed)?;
        let info = ck(fs.node_info(node), "node_info truncated", seed)?;
        if info.size != 2048 {
            return Err(format!(
                "seed {seed:#x}: truncated size {} != 2048",
                info.size
            ));
        }
        let back = read_all(&mut fs, node, 2048, "read truncated", seed)?;
        if back != content(seed, 0, 2048) {
            return Err(format!("seed {seed:#x}: truncated prefix mismatch"));
        }
    }

    // Remove one file; it must vanish.
    ck(fs.remove(root, b"bravo"), "remove", seed)?;
    want_err(
        fs.lookup(root, b"bravo").err(),
        DriverError::NotFound,
        "lookup removed",
        seed,
    )?;

    // --- Remount and re-verify the survivors. ---
    let mut fs = ck(fs.remount(), "remount", seed)?;
    let root = fs.root();
    let sub = ck(fs.lookup(root, b"sub"), "lookup sub after remount", seed)?;
    verify_file(&mut fs, root, b"alpha", 0, 2048, seed)?;
    verify_file(&mut fs, root, b"charlie", 2, 9000, seed)?;
    verify_file(&mut fs, root, b"delta", 3, 1, seed)?;
    verify_file(&mut fs, sub, b"deep", 100, 2048, seed)?;
    want_err(
        fs.lookup(root, b"bravo").err(),
        DriverError::NotFound,
        "lookup removed after remount",
        seed,
    )?;

    // --- Fail-closed extremes. ---
    want_err(
        fs.create(root, b"alpha", NodeKind::RegularFile).err(),
        DriverError::Busy,
        "duplicate create",
        seed,
    )?;
    want_err(
        fs.create(root, b"", NodeKind::RegularFile).err(),
        DriverError::LengthOutOfRange,
        "empty name",
        seed,
    )?;
    let oversize = vec![b'x'; 1024];
    want_err(
        fs.create(root, &oversize, NodeKind::RegularFile).err(),
        DriverError::LengthOutOfRange,
        "oversize name",
        seed,
    )?;
    want_err(
        fs.remove(root, b"sub").err(),
        DriverError::Busy,
        "remove non-empty dir",
        seed,
    )?;

    exercise_exhaustion(&mut fs, root, device_bytes, seed)?;
    Ok(())
}

/// Fill the data region with bounded files until allocation reports
/// `NoSpace`, confirm that is the terminator, then prove that freeing a
/// file lets allocation resume.
fn exercise_exhaustion<F: SoakFs>(
    fs: &mut F,
    root: NodeId,
    device_bytes: u64,
    seed: u64,
) -> Result<(), String> {
    // Each fill file gets *distinct* content (a per-file id mixed into every
    // byte). ARXFS deduplicates identical data records, so filling with one
    // repeated buffer would share a single chunk and never exhaust the volume;
    // unique content makes the fill genuinely consume space and reach
    // `NoSpace` (`docs/src/filesystem/arxfs-spec.md` §9).
    let fill_base = 0x5000_0000u64;
    let max_files = (device_bytes / FILL_FILE_BYTES as u64) + 16;
    let mut idx = 0u64;
    let mut last_fill: Option<Vec<u8>> = None;
    loop {
        if idx > max_files {
            return Err(format!(
                "seed {seed:#x}: wrote {idx} files without ever reporting NoSpace"
            ));
        }
        let name = format!("fill{idx:06}").into_bytes();
        match fs.create(root, &name, NodeKind::RegularFile) {
            Ok(_) => {}
            Err(DriverError::NoSpace) => break,
            Err(e) => {
                return Err(format!("seed {seed:#x}: fill create: unexpected {e:?}"));
            }
        }
        let body = content(seed, fill_base + idx, FILL_FILE_BYTES);
        match fs.write_at(root, &name, 0, &body) {
            Ok(_) => {
                last_fill = Some(name);
                idx += 1;
            }
            Err(DriverError::NoSpace) => break,
            Err(e) => {
                return Err(format!("seed {seed:#x}: fill write: unexpected {e:?}"));
            }
        }
    }

    // Freeing space lets allocation resume: NoSpace is not terminal.
    let Some(victim) = last_fill else {
        return Err(format!(
            "seed {seed:#x}: volume reported NoSpace before any fill file landed"
        ));
    };
    ck(fs.remove(root, &victim), "remove fill victim", seed)?;
    ck(
        fs.create(root, b"after", NodeKind::RegularFile),
        "create after free",
        seed,
    )?;
    let probe = vec![0xCDu8; 4096];
    ck(
        fs.write_at(root, b"after", 0, &probe),
        "write after free",
        seed,
    )?;
    Ok(())
}

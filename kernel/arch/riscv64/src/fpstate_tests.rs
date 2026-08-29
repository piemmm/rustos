//! Host tests for the floating-point state policy: the `sstatus.FS` field
//! accessors, the two decisions a trap makes, and the first-use opcode test.
//! The register moves are riscv64 assembly and are covered by the QEMU
//! vertical.

use super::{on_entry, on_return, touches_fp_state, FpArea, Fs, OnReturn, TrapAnchor, FS_MASK};

#[test]
fn the_field_round_trips_every_encoding() {
    for (fs, raw) in [
        (Fs::Off, 0),
        (Fs::Initial, 1),
        (Fs::Clean, 2),
        (Fs::Dirty, 3),
    ] {
        assert_eq!(Fs::of(raw << 13), fs);
        assert_eq!(fs as u64, raw);
        assert_eq!(Fs::of(fs.written_into(0)), fs);
    }
}

#[test]
fn writing_the_field_disturbs_no_other_bit() {
    // Every bit outside the field must survive, in both directions.
    let noise = !FS_MASK;
    for fs in [Fs::Off, Fs::Initial, Fs::Clean, Fs::Dirty] {
        let written = fs.written_into(noise);
        assert_eq!(written & !FS_MASK, noise);
        assert_eq!(Fs::of(written), fs);
        assert_eq!(fs.written_into(0) & !FS_MASK, 0);
    }
}

#[test]
fn a_trap_saves_only_a_dirty_file_and_marks_it_clean() {
    // The saved copy then matches the registers, so a task that computes
    // without writing again is not saved twice.
    assert_eq!(on_entry(Fs::Dirty), Some(Fs::Clean));
    for fs in [Fs::Off, Fs::Initial, Fs::Clean] {
        assert_eq!(on_entry(fs), None, "{fs:?} needs no save");
    }
}

#[test]
fn a_task_owning_no_state_returns_with_floating_point_off() {
    // This is what makes the residue unreadable rather than merely stale:
    // a task with no state of its own cannot access the file at all.
    assert_eq!(on_return(false), OnReturn::LeaveOff);
    assert_eq!(on_return(true), OnReturn::Reload);
}

#[test]
fn a_fresh_area_owns_nothing() {
    assert!(!FpArea::EMPTY.owned());
    assert!(!TrapAnchor::EMPTY.fp.owned());
    assert_eq!(TrapAnchor::EMPTY.kernel_tp, 0);
}

#[test]
fn the_anchor_holds_the_kernel_tp_first_and_the_file_at_a_fixed_offset() {
    // The save/restore assembly names `regs` at 16 bytes into the area, and
    // the trap vector reads the kernel `tp` at offset 0 of the anchor.
    assert_eq!(core::mem::offset_of!(TrapAnchor, kernel_tp), 0);
    assert_eq!(core::mem::offset_of!(TrapAnchor, fp), 8);
    assert_eq!(core::mem::offset_of!(FpArea, owned), 0);
    assert_eq!(core::mem::offset_of!(FpArea, fcsr), 8);
    assert_eq!(core::mem::offset_of!(FpArea, regs), 16);
    // 16-byte aligned so the trap frame built below it stays aligned.
    assert_eq!(core::mem::align_of::<TrapAnchor>(), 16);
    assert!(core::mem::size_of::<TrapAnchor>().is_multiple_of(16));
}

#[test]
fn every_fp_opcode_group_is_recognised() {
    for (bytes, what) in [
        (&0x0005_b507u32.to_le_bytes()[..], "fld"),
        (&0x00c5_b027u32.to_le_bytes()[..], "fsd"),
        (&0x02c5_8553u32.to_le_bytes()[..], "fadd.d"),
        (&0x62c5_8543u32.to_le_bytes()[..], "fmadd.d"),
        (&0x62c5_8547u32.to_le_bytes()[..], "fmsub.d"),
        (&0x62c5_854bu32.to_le_bytes()[..], "fnmsub.d"),
        (&0x62c5_854fu32.to_le_bytes()[..], "fnmadd.d"),
        (&0xf200_0553u32.to_le_bytes()[..], "fmv.d.x"),
        (&0x0010_2573u32.to_le_bytes()[..], "csrrs fflags"),
        (&0x0020_2573u32.to_le_bytes()[..], "csrrs frm"),
        (&0x0030_2573u32.to_le_bytes()[..], "csrrs fcsr"),
        (&0x0030_5573u32.to_le_bytes()[..], "csrrwi fcsr"),
        (&0x2000u16.to_le_bytes()[..], "c.fld"),
        (&0xa000u16.to_le_bytes()[..], "c.fsd"),
        (&0x2002u16.to_le_bytes()[..], "c.fldsp"),
        (&0xa002u16.to_le_bytes()[..], "c.fsdsp"),
    ] {
        assert!(touches_fp_state(bytes), "{what} reaches the FP file");
    }
}

#[test]
fn integer_work_is_not_mistaken_for_fp() {
    // `ecall` and `sret` share SYSTEM with the `fcsr` CSR forms, and `fence`
    // only looks floating-point; a task must not have FP enabled by any of
    // them.
    for (bytes, what) in [
        (&0x0015_0513u32.to_le_bytes()[..], "addi"),
        (&0x0005_b503u32.to_le_bytes()[..], "ld"),
        (&0x00c5_b023u32.to_le_bytes()[..], "sd"),
        (&0x3000_2573u32.to_le_bytes()[..], "csrrs mstatus"),
        (&0x0000_0073u32.to_le_bytes()[..], "ecall"),
        (&0x1020_0073u32.to_le_bytes()[..], "sret"),
        (&0x0000_100fu32.to_le_bytes()[..], "fence.i"),
        (&0x0ff0_000fu32.to_le_bytes()[..], "fence"),
        (&0x4398u16.to_le_bytes()[..], "c.lw"),
        (&0x6398u16.to_le_bytes()[..], "c.ld"),
        (&0x4501u16.to_le_bytes()[..], "c.li"),
    ] {
        assert!(
            !touches_fp_state(bytes),
            "{what} does not reach the FP file"
        );
    }
}

#[test]
fn unreadable_parcels_leave_the_fault_fatal() {
    // Fail closed: anything that cannot be read as one whole RV64GC
    // instruction must not enable floating point on a guess.
    assert!(!touches_fp_state(&[]));
    assert!(!touches_fp_state(&[0x07]));
    // LOAD-FP with only three of its four bytes present.
    assert!(!touches_fp_state(&0x0005_b507u32.to_le_bytes()[..3]));
    // Reserved 6- and 8-byte parcels, whole and truncated.
    assert!(!touches_fp_state(&[0x1f, 0x00, 0x00, 0x00, 0x00, 0x00]));
    assert!(!touches_fp_state(&[
        0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
    ]));
    assert!(!touches_fp_state(&[0x1f, 0x00]));
    // The defined illegal instruction.
    assert!(!touches_fp_state(&0x0000u16.to_le_bytes()));
}

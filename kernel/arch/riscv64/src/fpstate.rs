//! Per-task floating-point state for riscv64.
//!
//! `riscv64gc` is a hard-float ABI, so user code may use `f0`–`f32` and
//! `fcsr` at any time, and firmware hands S-mode `sstatus.FS = Dirty` — FP
//! enabled — before the kernel runs. Without the state below, two tasks share
//! one physical register file: each reads whatever the last one left there.
//! That is an isolation *and* a confidentiality failure, so the kernel owns
//! the field rather than inheriting it.
//!
//! The policy is lazy, and `FS` is what makes it free rather than merely
//! cheap:
//!
//! * A task starts with FP **off** and no state. It cannot read the file, so
//!   it cannot see a predecessor's residue, and there is nothing to save or
//!   restore on its behalf — a task that never computes in floating point
//!   pays nothing at all.
//! * Its first floating-point instruction therefore traps. The handler zeroes
//!   the file, gives the task `Initial`, and retries the instruction; from
//!   then on the task owns FP state.
//! * The hardware promotes `Initial`/`Clean` to `Dirty` on the first write, so
//!   a trap saves the file only when the task actually changed it.
//! * The kernel itself runs with FP **off**, which turns "the kernel must not
//!   use floating point" from an assumption into a fault. It emits none today
//!   and has no per-task place to keep any.
//!
//! The save area lives in the task's own trap anchor at the top of its kernel
//! stack, so it needs no allocation and no publication: switching stacks
//! switches FP state.

/// Bit position of the two-bit `sstatus.FS` field.
const FS_SHIFT: u32 = 13;

/// The two-bit `sstatus.FS` field, in place.
pub const FS_MASK: u64 = 0b11 << FS_SHIFT;

/// `sstatus.FS` encodings (privileged spec table 4.2).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Fs {
    /// FP unusable: every access raises an illegal instruction.
    Off = 0,
    /// Enabled, register file at its initial value.
    Initial = 1,
    /// Enabled, file matches the saved copy.
    Clean = 2,
    /// Enabled, file has been written since it was last saved.
    Dirty = 3,
}

impl Fs {
    /// The field's value in `sstatus`.
    #[must_use]
    pub const fn of(sstatus: u64) -> Self {
        match (sstatus & FS_MASK) >> FS_SHIFT {
            0 => Self::Off,
            1 => Self::Initial,
            2 => Self::Clean,
            _ => Self::Dirty,
        }
    }

    /// `sstatus` with the field replaced by `self`.
    #[must_use]
    pub const fn written_into(self, sstatus: u64) -> u64 {
        (sstatus & !FS_MASK) | ((self as u64) << FS_SHIFT)
    }
}

/// A task's saved floating-point register file.
///
/// `#[repr(C)]` because the offsets below are named by the save/restore
/// assembly; `regs` starts at 16 bytes into the enclosing [`TrapAnchor`].
#[repr(C)]
pub struct FpArea {
    /// Non-zero once this task owns floating-point state.
    owned: u64,
    /// Saved `fcsr` (the register is 32-bit; a word keeps `regs` aligned).
    fcsr: u64,
    /// Saved `f0`–`f31`.
    regs: [u64; 32],
}

/// Offset within [`FpArea`] of the ownership word, so the user-entry sequence
/// can zero it without naming the field's layout twice.
pub const FP_OWNED_OFFSET: u64 = core::mem::offset_of!(FpArea, owned) as u64;

impl FpArea {
    /// An area owning no state.
    pub const EMPTY: Self = Self {
        owned: 0,
        fcsr: 0,
        regs: [0; 32],
    };

    /// Whether this task owns floating-point state.
    #[must_use]
    pub const fn owned(&self) -> bool {
        self.owned != 0
    }
}

/// The per-task, kernel-only block `sscratch` points at while a task runs in
/// U-mode.
///
/// It sits at the top of the task's kernel-stack window with the trap frame
/// built immediately below, so the floating-point area rides the stack and
/// needs neither an allocation nor a per-CPU publication.
#[repr(C, align(16))]
pub struct TrapAnchor {
    /// Kernel `tp` of the hart the task is running on, reloaded by the trap
    /// vector before any other register is touched.
    pub kernel_tp: u64,
    /// This task's floating-point state.
    pub fp: FpArea,
}

impl TrapAnchor {
    /// An anchor for a task that has not run: no hart, no FP state.
    pub const EMPTY: Self = Self {
        kernel_tp: 0,
        fp: FpArea::EMPTY,
    };
}

/// What a trap must do about floating point on its way back to U-mode.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum OnReturn {
    /// The task owns no state; leave FP off so it cannot read the file.
    LeaveOff,
    /// Reload the task's file and hand it back enabled.
    Reload,
}

/// Whether an entering trap must save the file, and the `FS` the task should
/// carry afterwards.
///
/// Saving only on `Dirty` is what keeps an FP-using task's cost proportional
/// to the work it actually did: a task that read floating point but wrote none
/// since its last save has an area that still matches the registers.
#[must_use]
pub const fn on_entry(fs: Fs) -> Option<Fs> {
    match fs {
        Fs::Dirty => Some(Fs::Clean),
        Fs::Off | Fs::Initial | Fs::Clean => None,
    }
}

/// What the return path owes a task whose area reports `owned`.
#[must_use]
pub const fn on_return(owned: bool) -> OnReturn {
    if owned {
        OnReturn::Reload
    } else {
        OnReturn::LeaveOff
    }
}

/// Whether the instruction encoded at the front of `code` reads or writes the
/// `F`/`D` register file or `fcsr`.
///
/// A task runs with `FS` at `Off` until it first needs floating point, so that
/// first use arrives as an illegal-instruction trap and the handler must tell
/// "enable FP and retry" apart from a genuinely undefined instruction.
///
/// The opcode groups are read straight from the unprivileged ISA rather than
/// borrowed from `lib/disasm`: that crate renders instructions as text and so
/// allocates, and linking it here would force a global allocator on every
/// consumer of this crate — including the minimal test kernels that rightly
/// have none. Ten lines of opcode test is the cheaper side of that trade.
///
/// Allocates nothing and decodes nothing further, so it is callable from a
/// fault path. `false` for an empty, truncated, or reserved-length parcel:
/// anything that cannot be read as one whole RV64GC instruction leaves the
/// fault fatal (fail closed).
#[must_use]
pub fn touches_fp_state(code: &[u8]) -> bool {
    if code.len() < 2 {
        return false;
    }
    let first = u16::from_le_bytes([code[0], code[1]]);
    match parcel_bytes(first) {
        2 => compressed_touches_fp_state(u32::from(first)),
        4 if code.len() >= 4 => {
            full_touches_fp_state(u32::from_le_bytes([code[0], code[1], code[2], code[3]]))
        }
        _ => false,
    }
}

/// Instruction length in bytes declared by the low bits of the first parcel
/// (unprivileged ISA, expanded-length encoding). Reserved longer parcels
/// answer 0: RV64GC defines none, and none can reach the FP unit.
const fn parcel_bytes(first: u16) -> usize {
    if first & 0b11 != 0b11 {
        2
    } else if first & 0b1_1100 != 0b1_1100 {
        4
    } else {
        0
    }
}

/// Bits `[hi:lo]` of `word`, inclusive.
const fn bits(word: u32, hi: u32, lo: u32) -> u32 {
    (word >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// The 32-bit major opcodes that reach the FP register file or `fcsr`.
const fn full_touches_fp_state(word: u32) -> bool {
    match bits(word, 6, 0) {
        // LOAD-FP, STORE-FP, the four fused multiply-adds, and OP-FP.
        0b000_0111 | 0b010_0111 | 0b100_0011 | 0b100_0111 | 0b100_1011 | 0b100_1111
        | 0b101_0011 => true,
        // SYSTEM reaches `fcsr` only through a real CSR access — `funct3` of
        // zero is `ecall`/`ebreak`/`*ret`/`sfence` — naming fflags, frm, or fcsr.
        0b111_0011 => bits(word, 14, 12) & 0b11 != 0 && matches!(bits(word, 31, 20), 0x001..=0x003),
        _ => false,
    }
}

/// The compressed forms that reach the FP register file: `c.fld`/`c.fsd` in
/// quadrant 0 and `c.fldsp`/`c.fsdsp` in quadrant 2, which share their two
/// `funct3` values. RV64 has no `c.flw`/`c.fsw` — those encodings are
/// `c.ld`/`c.sd` — so these are the whole set.
const fn compressed_touches_fp_state(parcel: u32) -> bool {
    matches!(parcel & 3, 0 | 2) && matches!(bits(parcel, 15, 13), 1 | 5)
}

// --- Register moves (riscv64 only) --------------------------------------

/// Save `f0`–`f31` and `fcsr` into `anchor`'s area and mark it owned.
///
/// # Safety
///
/// `anchor` must be the running task's live [`TrapAnchor`], and `sstatus.FS`
/// must be enabled — the caller establishes both from the trap frame.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn save_file(anchor: *mut TrapAnchor) {
    let area = unsafe { core::ptr::addr_of_mut!((*anchor).fp) };
    // SAFETY: `area` addresses the caller's live anchor, whose `regs` field
    // starts 16 bytes in and spans 32 doublewords; FP is enabled, so the
    // stores are legal. `fcsr` is read with `frcsr` into a spare integer
    // register and written to the word ahead of them.
    unsafe {
        core::arch::asm!(
            "fsd f0, 16({a})",
            "fsd f1, 24({a})",
            "fsd f2, 32({a})",
            "fsd f3, 40({a})",
            "fsd f4, 48({a})",
            "fsd f5, 56({a})",
            "fsd f6, 64({a})",
            "fsd f7, 72({a})",
            "fsd f8, 80({a})",
            "fsd f9, 88({a})",
            "fsd f10, 96({a})",
            "fsd f11, 104({a})",
            "fsd f12, 112({a})",
            "fsd f13, 120({a})",
            "fsd f14, 128({a})",
            "fsd f15, 136({a})",
            "fsd f16, 144({a})",
            "fsd f17, 152({a})",
            "fsd f18, 160({a})",
            "fsd f19, 168({a})",
            "fsd f20, 176({a})",
            "fsd f21, 184({a})",
            "fsd f22, 192({a})",
            "fsd f23, 200({a})",
            "fsd f24, 208({a})",
            "fsd f25, 216({a})",
            "fsd f26, 224({a})",
            "fsd f27, 232({a})",
            "fsd f28, 240({a})",
            "fsd f29, 248({a})",
            "fsd f30, 256({a})",
            "fsd f31, 264({a})",
            "frcsr {t}",
            "sd {t}, 8({a})",
            a = in(reg) area,
            t = out(reg) _,
            options(nostack, preserves_flags)
        );
        (*area).owned = 1;
    }
}

/// Reload `f0`–`f31` and `fcsr` from `anchor`'s area.
///
/// # Safety
///
/// As [`save_file`]: `anchor` must be the running task's live anchor with an
/// owned area, and FP must be enabled for the loads.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn reload_file(anchor: *const TrapAnchor) {
    let area = unsafe { core::ptr::addr_of!((*anchor).fp) };
    // SAFETY: same region and the same enablement precondition as the save;
    // `fscsr` writes the control register from a scratch integer register.
    unsafe {
        core::arch::asm!(
            "ld {t}, 8({a})",
            "fscsr {t}",
            "fld f0, 16({a})",
            "fld f1, 24({a})",
            "fld f2, 32({a})",
            "fld f3, 40({a})",
            "fld f4, 48({a})",
            "fld f5, 56({a})",
            "fld f6, 64({a})",
            "fld f7, 72({a})",
            "fld f8, 80({a})",
            "fld f9, 88({a})",
            "fld f10, 96({a})",
            "fld f11, 104({a})",
            "fld f12, 112({a})",
            "fld f13, 120({a})",
            "fld f14, 128({a})",
            "fld f15, 136({a})",
            "fld f16, 144({a})",
            "fld f17, 152({a})",
            "fld f18, 160({a})",
            "fld f19, 168({a})",
            "fld f20, 176({a})",
            "fld f21, 184({a})",
            "fld f22, 192({a})",
            "fld f23, 200({a})",
            "fld f24, 208({a})",
            "fld f25, 216({a})",
            "fld f26, 224({a})",
            "fld f27, 232({a})",
            "fld f28, 240({a})",
            "fld f29, 248({a})",
            "fld f30, 256({a})",
            "fld f31, 264({a})",
            a = in(reg) area,
            t = out(reg) _,
            options(nostack, preserves_flags)
        );
    }
}

/// Zero `f0`–`f31` and `fcsr`, then mark `anchor`'s area owned.
///
/// Called when a task first needs floating point. Zeroing is the whole point:
/// the physical file still holds whatever the previous task left in it.
///
/// # Safety
///
/// As [`save_file`].
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn adopt_zeroed_file(anchor: *mut TrapAnchor) {
    // SAFETY: the caller's live anchor. Writing the area and then reloading
    // from it leaves both the saved copy and the registers zeroed, so the
    // task cannot observe a predecessor's values.
    unsafe {
        let area = core::ptr::addr_of_mut!((*anchor).fp);
        (*area).fcsr = 0;
        (*area).regs = [0; 32];
        (*area).owned = 1;
        reload_file(anchor);
    }
}

/// Save the interrupted task's file if it dirtied it, then leave the kernel
/// with FP off, so a kernel floating-point instruction faults loudly instead
/// of silently clobbering the task's live registers.
///
/// Setting `Off` is allowed to discard the register file, which is why an
/// owned area always holds a valid saved copy by the time this returns.
///
/// # Safety
///
/// `anchor` must be the interrupted task's live anchor and `frame_sstatus` its
/// trap frame's saved `sstatus`; the caller establishes that the trap came
/// from U-mode.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn on_trap_from_user(anchor: *mut TrapAnchor, frame_sstatus: &mut u64) {
    if let Some(after) = on_entry(Fs::of(*frame_sstatus)) {
        // SAFETY: the caller's live anchor; FP is enabled for the stores.
        unsafe {
            set_live_fs(Fs::Dirty);
            save_file(anchor);
        }
        *frame_sstatus = after.written_into(*frame_sstatus);
    }
    // SAFETY: changes only S-mode floating-point enablement.
    unsafe { set_live_fs(Fs::Off) };
}

/// Hand the task back its register file, or hand it back with FP off when it
/// owns none — which is what makes a predecessor's residue unreadable rather
/// than merely stale.
///
/// # Safety
///
/// As [`on_trap_from_user`]. Runs last before the vector's epilogue, which
/// touches no floating-point register.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn on_return_to_user(anchor: *mut TrapAnchor, frame_sstatus: &mut u64) {
    // SAFETY: the caller's live anchor.
    let owned = unsafe { (*anchor).fp.owned() };
    match on_return(owned) {
        OnReturn::Reload => {
            // SAFETY: an owned area holds a valid saved copy; FP is enabled
            // for the loads.
            unsafe {
                set_live_fs(Fs::Dirty);
                reload_file(anchor);
            }
            *frame_sstatus = Fs::Clean.written_into(*frame_sstatus);
        }
        OnReturn::LeaveOff => {
            // SAFETY: changes only S-mode floating-point enablement.
            unsafe { set_live_fs(Fs::Off) };
            *frame_sstatus = Fs::Off.written_into(*frame_sstatus);
        }
    }
}

/// Give a task taking its first floating-point instruction a zeroed file, so
/// it starts from nothing rather than whatever the last task left behind.
///
/// `false` when the task already owns state: the instruction was genuinely
/// illegal rather than a first use, and retrying it would loop forever.
///
/// # Safety
///
/// As [`on_trap_from_user`].
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn adopt_on_first_use(anchor: *mut TrapAnchor) -> bool {
    // SAFETY: the caller's live anchor.
    if unsafe { (*anchor).fp.owned() } {
        return false;
    }
    // SAFETY: FP is enabled for the zeroing reload.
    unsafe {
        set_live_fs(Fs::Dirty);
        adopt_zeroed_file(anchor);
    }
    true
}

/// Set the live `sstatus.FS` field, which governs the kernel's own access.
///
/// # Safety
///
/// Writing `sstatus.FS` changes only floating-point enablement for S-mode
/// execution; the value a task returns to comes from its trap frame.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) unsafe fn set_live_fs(fs: Fs) {
    // SAFETY: `csrc`/`csrs sstatus` clear and set exactly the named bits with
    // no memory side effects.
    unsafe {
        core::arch::asm!("csrc sstatus, {m}", m = in(reg) FS_MASK, options(nomem, nostack));
        if fs != Fs::Off {
            let bits = (fs as u64) << FS_SHIFT;
            core::arch::asm!("csrs sstatus, {b}", b = in(reg) bits, options(nomem, nostack));
        }
    }
}

#[cfg(test)]
#[path = "fpstate_tests.rs"]
mod tests;

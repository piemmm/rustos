//! Broadcom Serial Controller (BSC) I²C bus driver (`brcm,bcm2835-i2c`).
//!
//! The BSC is the I²C master the whole Raspberry Pi family exposes: a
//! 16-byte FIFO, a slave-address register, a transfer-length register, and a
//! status register whose bits drive one maskable interrupt. This crate is the
//! controller logic — the register sequence one transfer runs — with the
//! process, the endpoints, and the grants left to the `Run` binary.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`Bsc`] is the controller, and [`Bsc::port`] narrows it to one child's
//! [`I2cPort`] — the shape a chip driver's requests are served against, with
//! the address coming from the bus driver's own duty grant rather than from
//! the wire.
//!
//! # Interrupt-driven, with a liveness backstop
//!
//! A transfer arms the controller and then **parks** on its interrupt line
//! through the injected [`BusWait`] seam: the FIFO-service and completion
//! interrupts are what advance it, never a spin. The per-phase deadline
//! exists only to catch a controller that has stopped answering — a slave that legitimately stretches the clock is bounded by
//! the controller's own stretch-timeout register, which raises its own status
//! bit long before the deadline could fire.
//!
//! # What this controller cannot do
//!
//! The BSC cannot emit a true repeated START: it drops a STOP between the two
//! phases of a write-then-read. On a single-master bus — which is what a
//! board wiring an RTC to the Pi's BSC has — that is indistinguishable, since
//! nothing else can move a chip's register pointer between the phases; a
//! multi-master bus is out of scope for this controller.
//!
//! The clock divider is left exactly as the firmware programmed it. The bus
//! speed is a property of the board's wiring and the slowest part on it, and
//! nothing in discovery tells a driver either, so overriding it could clock a
//! part past its rating.
//!
//! Reference: BCM2835 ARM Peripherals, §3 (BSC).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::i2c::{I2cAddress, I2cPort, MAX_TRANSFER_LEN};
use tairix_abi::driver::WindowError;
use tairix_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey, RegisterWindow,
};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses: the host re-issues its own host-local
/// handle when binding the driver, and this constant is the on-the-wire
/// signal that the load-time gate cleared. The bytes spell `"BSC1"`.
const REGISTER_HANDLE_MARKER: u64 = 0x4253_4331_0000_0001;

/// Device-tree `compatible` string of the BSC as the Raspberry Pi bindings
/// spell it. Every Pi generation's controller is register-compatible with the
/// BCM2835's, so the later parts carry this string too.
pub const BSC_COMPATIBLE: &[u8] = b"brcm,bcm2835-i2c";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from, so the
/// match data can never drift from the driver.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(BSC_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Bytes of register block the controller occupies (`C` through `CLKT`).
pub const REGISTER_BLOCK_LEN: usize = 0x20;

/// Control register.
const C: usize = 0x00;
/// Status register.
const S: usize = 0x04;
/// Data-length register.
const DLEN: usize = 0x08;
/// Slave-address register.
const A: usize = 0x0C;
/// Data FIFO.
const FIFO: usize = 0x10;

/// `C`: controller enabled.
const C_I2CEN: u32 = 1 << 15;
/// `C`: raise an interrupt when the RX FIFO needs reading.
const C_INTR: u32 = 1 << 10;
/// `C`: raise an interrupt when the TX FIFO needs writing.
const C_INTT: u32 = 1 << 9;
/// `C`: raise an interrupt on transfer completion.
const C_INTD: u32 = 1 << 8;
/// `C`: start a transfer with the current `A`, `DLEN`, and direction.
const C_ST: u32 = 1 << 7;
/// `C`: clear both FIFOs (a two-bit field; either non-zero value clears).
const C_CLEAR: u32 = 0b01 << 4;
/// `C`: the transfer is a read.
const C_READ: u32 = 1 << 0;

/// `S`: the slave held the clock past the controller's stretch timeout
/// (write to clear).
const S_CLKT: u32 = 1 << 9;
/// `S`: the slave did not acknowledge (write to clear).
const S_ERR: u32 = 1 << 8;
/// `S`: the RX FIFO holds at least one byte.
const S_RXD: u32 = 1 << 5;
/// `S`: the TX FIFO has room for at least one byte.
const S_TXD: u32 = 1 << 4;
/// `S`: the transfer completed (write to clear).
const S_DONE: u32 = 1 << 1;

/// Every latched `S` bit a phase clears before and after itself.
const S_LATCHED: u32 = S_CLKT | S_ERR | S_DONE;

/// Bit periods one byte on the wire costs: eight data bits plus the
/// acknowledge.
const BITS_PER_BYTE: u64 = 9;

/// Byte periods the START, the address byte, and the STOP add to a phase.
const FRAMING_BYTES: u64 = 2;

/// The slowest bus rate the transfer deadline is sized for.
///
/// The driver does not program the clock divider (see the module docs), so
/// the deadline must cover the slowest rate a board could plausibly be left
/// at — an order of magnitude below the 100 kHz standard mode — or a
/// legitimately slow bus would be abandoned mid-transfer. It bounds a
/// controller that has stopped answering, not the bus's speed.
const SLOWEST_BUS_HZ: u64 = 10_000;

/// Nanoseconds a phase of `bytes` bytes may take before the controller is
/// treated as wedged.
fn phase_deadline_ns(bytes: usize) -> u64 {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    let periods = bytes
        .saturating_add(FRAMING_BYTES)
        .saturating_mul(BITS_PER_BYTE);
    periods.saturating_mul(1_000_000_000) / SLOWEST_BUS_HZ
}

/// The controller's register block, as the driver reaches it.
///
/// Metal drives this over the capability-gated [`RegisterWindow`] the matched
/// node's grant maps; a host test drives it over a simulated controller, so
/// the FIFO handshake and the status transitions are proven without silicon.
pub trait Registers {
    /// Read the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `offset` is outside the mapped
    /// register window.
    fn read32(&self, offset: usize) -> Result<u32, DriverError>;

    /// Write `value` to the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `offset` is outside the mapped
    /// register window.
    fn write32(&self, offset: usize, value: u32) -> Result<(), DriverError>;

    /// Bytes of register block reachable through this seam.
    fn block_len(&self) -> usize;
}

impl Registers for RegisterWindow {
    fn read32(&self, offset: usize) -> Result<u32, DriverError> {
        self.read_u32(offset).map_err(WindowError::as_driver_error)
    }

    fn write32(&self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.write_u32(offset, value)
            .map_err(WindowError::as_driver_error)
    }

    fn block_len(&self) -> usize {
        self.len()
    }
}

/// How a transfer waits for the controller to make progress.
///
/// On metal this is the bound interrupt line and the kernel monotonic clock;
/// in a host test it is a simulated controller that steps as the driver
/// parks, which is exactly what the real one does.
pub trait BusWait {
    /// Monotonic nanoseconds.
    fn now_ns(&self) -> u64;

    /// Park until the controller signals, or `timeout_ns` elapses.
    ///
    /// A refused park must return rather than spin: the caller re-reads the
    /// clock and fails closed once the deadline is spent.
    fn wait(&self, timeout_ns: u64);
}

/// Direction of one phase of a transfer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Phase {
    Write,
    Read,
}

/// The Broadcom Serial Controller.
pub struct Bsc<'a> {
    regs: &'a dyn Registers,
    wait: &'a dyn BusWait,
}

impl<'a> Bsc<'a> {
    /// Bind the driver to the controller behind `regs`, enabling it and
    /// clearing any state a previous owner left behind.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if the granted window is shorter
    /// than the register block, or [`DriverError::DeviceFault`] if a register
    /// access is refused.
    pub fn new(regs: &'a dyn Registers, wait: &'a dyn BusWait) -> Result<Self, DriverError> {
        if regs.block_len() < REGISTER_BLOCK_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let bsc = Self { regs, wait };
        bsc.quiesce()?;
        Ok(bsc)
    }

    /// Narrow the controller to the child at `address` — the [`I2cPort`] one
    /// transfer endpoint's requests are served against.
    #[must_use]
    pub const fn port(&self, address: I2cAddress) -> Port<'_> {
        Port { bsc: self, address }
    }

    /// Stop any transfer, clear both FIFOs and every latched status bit, and
    /// leave the controller enabled and idle.
    ///
    /// Dropping the enable bit is the only way to abort a transfer already in
    /// flight — the control register has no abort — so the sequence disables,
    /// clears, and re-enables rather than leaving a wedged transfer running
    /// under the next caller.
    fn quiesce(&self) -> Result<(), DriverError> {
        self.write_reg(C, C_CLEAR)?;
        self.write_reg(S, S_LATCHED)?;
        self.write_reg(C, C_I2CEN | C_CLEAR)
    }

    /// Run one write-then-read transfer against `address`.
    fn transfer(
        &self,
        address: I2cAddress,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), DriverError> {
        if write.len() > MAX_TRANSFER_LEN || read.len() > MAX_TRANSFER_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        self.write_reg(A, u32::from(address.get()))?;
        // Both phases empty is the address-only probe: a zero-length write
        // addresses the part and stops, and its acknowledge is the answer.
        if !write.is_empty() || read.is_empty() {
            self.run_phase(Phase::Write, &mut [], write)?;
        }
        if read.is_empty() {
            return Ok(());
        }
        // A part that acknowledged the write phase is demonstrably there, so
        // a refusal of the read phase's address is a fault of this transfer
        // rather than an absent part.
        self.run_phase(Phase::Read, read, &[]).map_err(|err| {
            if err == DriverError::NotFound && !write.is_empty() {
                DriverError::DeviceFault
            } else {
                err
            }
        })
    }

    /// Arm one phase and service its FIFO until the controller reports the
    /// phase complete, parking on the interrupt between rounds.
    ///
    /// Exactly one of `read`/`write` is non-empty for a phase that moves
    /// data; a write phase with both empty is the address-only probe.
    fn run_phase(&self, phase: Phase, read: &mut [u8], write: &[u8]) -> Result<(), DriverError> {
        let len = if phase == Phase::Read {
            read.len()
        } else {
            write.len()
        };
        let remaining_reg = u32::try_from(len).map_err(|_| DriverError::OutOfRange)?;
        self.write_reg(S, S_LATCHED)?;
        self.write_reg(DLEN, remaining_reg)?;
        let mut control = C_I2CEN | C_CLEAR | C_ST | C_INTD;
        control |= match phase {
            Phase::Read => C_READ | C_INTR,
            Phase::Write => C_INTT,
        };
        self.write_reg(C, control)?;

        let deadline = self.wait.now_ns().saturating_add(phase_deadline_ns(len));
        let mut moved = 0usize;
        loop {
            let status = self.read_reg(S)?;
            if status & S_ERR != 0 {
                // `DLEN` reads back the bytes still to move once a transfer
                // has begun, so an untouched count means the part never
                // answered its address at all; a count that has moved means
                // it is there and stopped acknowledging, which is a fault of
                // the transfer rather than an absent part. Read before the
                // abort, which resets it.
                let untouched = self.read_reg(DLEN)? == remaining_reg;
                self.quiesce()?;
                return Err(if untouched {
                    DriverError::NotFound
                } else {
                    DriverError::DeviceFault
                });
            }
            if status & S_CLKT != 0 {
                self.quiesce()?;
                return Err(DriverError::DeviceFault);
            }
            moved += self.service(phase, read, write, moved)?;
            // Re-read rather than trusting the pre-service snapshot: the
            // phase usually completes while the FIFO is being drained, and
            // the stale bit would cost a needless park.
            if self.read_reg(S)? & S_DONE != 0 {
                break;
            }
            let now = self.wait.now_ns();
            if now >= deadline {
                self.quiesce()?;
                return Err(DriverError::DeviceFault);
            }
            self.wait.wait(deadline - now);
        }
        // The controller stopped: drain whatever the last completion left in
        // the FIFO before judging the phase.
        moved += self.service(phase, read, write, moved)?;
        self.write_reg(S, S_DONE)?;
        if moved == len {
            Ok(())
        } else {
            // A short transfer is a failure, never a partially-filled
            // buffer the caller could mistake for a reading.
            self.quiesce()?;
            Err(DriverError::DeviceFault)
        }
    }

    /// Move as many bytes as the FIFO currently allows, returning how many.
    fn service(
        &self,
        phase: Phase,
        read: &mut [u8],
        write: &[u8],
        moved: usize,
    ) -> Result<usize, DriverError> {
        let mut done = 0usize;
        match phase {
            Phase::Read => {
                while moved + done < read.len() && self.read_reg(S)? & S_RXD != 0 {
                    // The FIFO is byte-wide; the register's upper bits read
                    // as zero, so the narrowing is total.
                    let byte = self.read_reg(FIFO)?.to_le_bytes()[0];
                    read[moved + done] = byte;
                    done += 1;
                }
            }
            Phase::Write => {
                while moved + done < write.len() && self.read_reg(S)? & S_TXD != 0 {
                    self.write_reg(FIFO, u32::from(write[moved + done]))?;
                    done += 1;
                }
            }
        }
        Ok(done)
    }

    fn read_reg(&self, offset: usize) -> Result<u32, DriverError> {
        self.regs.read32(offset)
    }

    fn write_reg(&self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.regs.write32(offset, value)
    }
}

/// One child of the bus, bound to the address its duty grant named.
///
/// This is the whole of what a chip driver's transfer endpoint resolves to:
/// the address is held here, on the bus driver's side, so no request can
/// reach a part other than the one its endpoint belongs to.
pub struct Port<'a> {
    bsc: &'a Bsc<'a>,
    address: I2cAddress,
}

impl I2cPort for Port<'_> {
    fn transfer(&self, write: &[u8], read: &mut [u8]) -> Result<(), DriverError> {
        self.bsc.transfer(self.address, write, read)
    }
}

/// Driver entry point.
///
/// # Errors
///
/// [`DriverError::PermissionDenied`] if the host did not grant
/// [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`]. Driving the bus additionally needs
/// the mapped register window and the bound interrupt line the matched node
/// requested, and the privilege to bind each child's transfer endpoint.
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

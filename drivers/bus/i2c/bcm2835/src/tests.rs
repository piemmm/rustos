//! Host unit tests for the BSC controller, driven against a simulated
//! controller that steps exactly where the real one raises its interrupt.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::driver::i2c::{I2cAddress, I2cPort, MAX_TRANSFER_LEN};
use tairix_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey, HW_COMPATIBLE_MAX,
};

use super::{
    phase_deadline_ns, register, Bsc, BusWait, Registers, BIND_KEYS, BSC_COMPATIBLE, C_CLEAR,
    C_I2CEN, C_INTD, C_INTR, C_INTT, C_READ, C_ST, REGISTER_BLOCK_LEN, S_CLKT, S_DONE, S_ERR,
    S_RXD, S_TXD,
};

/// The controller's own FIFO depth in bytes (BCM2835 ARM Peripherals §3.2),
/// so a transfer longer than it exercises the service-and-park round trip.
const FIFO_DEPTH: usize = 16;

/// How the modelled part answers a transfer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PartBehaviour {
    /// Acknowledges and moves the whole phase.
    Present,
    /// Never acknowledges its address.
    Absent,
    /// Acknowledges, then stops acknowledging after `after` bytes.
    RefusesAfter(usize),
    /// Holds the clock past the controller's stretch timeout.
    Stretches,
    /// Acknowledges but delivers nothing, so the transfer never completes.
    Wedged,
}

/// A simulated BSC plus the part on its bus.
///
/// It intercepts every register access, so the FIFO is a real queue and the
/// status bits change exactly as they would on silicon. It steps on
/// [`BusWait::wait`], because that is when the controller makes progress and
/// raises its interrupt.
struct Sim {
    state: RefCell<SimState>,
}

struct SimState {
    control: u32,
    status: u32,
    dlen: u32,
    address: u32,
    /// Bytes the driver has pushed into the TX FIFO and the part has not
    /// yet taken.
    fifo_tx: VecDeque<u8>,
    /// Bytes the part has received across the whole transfer.
    taken: Vec<u8>,
    /// The bytes waiting for the driver to read.
    rx: VecDeque<u8>,
    /// What the part will hand back on a read phase.
    to_deliver: VecDeque<u8>,
    /// Bytes still to move in the active phase.
    remaining: u32,
    active: bool,
    /// Whether a phase has been started since the last abort, which is what
    /// makes `DLEN` read back the remaining count.
    started: bool,
    behaviour: PartBehaviour,
    now_ns: u64,
    /// Register writes the driver issued to `C` with the start bit set.
    starts: Vec<u32>,
    waits: usize,
}

impl Sim {
    fn new(behaviour: PartBehaviour, to_deliver: &[u8]) -> Self {
        Self {
            state: RefCell::new(SimState {
                control: 0,
                status: 0,
                dlen: 0,
                address: 0,
                fifo_tx: VecDeque::new(),
                taken: Vec::new(),
                rx: VecDeque::new(),
                to_deliver: to_deliver.iter().copied().collect(),
                remaining: 0,
                active: false,
                started: false,
                behaviour,
                now_ns: 0,
                starts: Vec::new(),
                waits: 0,
            }),
        }
    }

    fn present(to_deliver: &[u8]) -> Self {
        Self::new(PartBehaviour::Present, to_deliver)
    }

    fn written(&self) -> Vec<u8> {
        self.state.borrow().taken.clone()
    }

    fn addressed(&self) -> u32 {
        self.state.borrow().address
    }

    fn starts(&self) -> Vec<u32> {
        self.state.borrow().starts.clone()
    }

    fn waits(&self) -> usize {
        self.state.borrow().waits
    }
}

impl SimState {
    /// Recompute the FIFO-readiness bits the driver polls.
    fn refresh(&mut self) {
        self.status &= !(S_RXD | S_TXD);
        if !self.rx.is_empty() {
            self.status |= S_RXD;
        }
        // The TX side has room whenever a write phase is active and the
        // controller has not yet been handed the whole length.
        if self.active && self.control & C_READ == 0 && self.fifo_tx.len() < FIFO_DEPTH {
            self.status |= S_TXD;
        }
    }

    /// Advance the transfer as the silicon would between interrupts.
    fn step(&mut self) {
        if !self.active {
            return;
        }
        match self.behaviour {
            PartBehaviour::Absent => {
                // The controller latches DONE alongside the error: the
                // transfer is over either way.
                self.status |= S_ERR | S_DONE;
                self.active = false;
                return;
            }
            PartBehaviour::Stretches => {
                self.status |= S_CLKT | S_DONE;
                self.active = false;
                return;
            }
            PartBehaviour::Wedged => return,
            PartBehaviour::RefusesAfter(after) => {
                let moved = usize::try_from(self.dlen.saturating_sub(self.remaining))
                    .expect("a phase is short");
                if moved >= after {
                    self.status |= S_ERR | S_DONE;
                    self.active = false;
                    return;
                }
            }
            PartBehaviour::Present => {}
        }
        if self.control & C_READ == 0 {
            // The part consumes whatever the driver has queued in the FIFO.
            while self.remaining > 0 {
                let Some(byte) = self.fifo_tx.pop_front() else {
                    break;
                };
                self.taken.push(byte);
                self.remaining -= 1;
            }
            if self.remaining == 0 {
                self.active = false;
                self.status |= S_DONE;
            }
        } else {
            // The part delivers up to a FIFO's worth.
            let room = FIFO_DEPTH - self.rx.len();
            for _ in 0..room.min(usize::try_from(self.remaining).expect("a phase is short")) {
                let byte = self.to_deliver.pop_front().unwrap_or(0);
                self.rx.push_back(byte);
                self.remaining -= 1;
            }
            if self.remaining == 0 {
                self.active = false;
                self.status |= S_DONE;
            }
        }
        self.refresh();
    }
}

impl Registers for Sim {
    fn read32(&self, offset: usize) -> Result<u32, DriverError> {
        if offset >= REGISTER_BLOCK_LEN {
            return Err(DriverError::DeviceFault);
        }
        let mut state = self.state.borrow_mut();
        Ok(match offset {
            super::C => state.control,
            super::S => {
                state.refresh();
                state.status
            }
            // Once a transfer has begun, `DLEN` reads back what is left to
            // move rather than what was asked for.
            super::DLEN => {
                if state.started {
                    state.remaining
                } else {
                    state.dlen
                }
            }
            super::A => state.address,
            super::FIFO => u32::from(state.rx.pop_front().unwrap_or(0)),
            _ => 0,
        })
    }

    fn write32(&self, offset: usize, value: u32) -> Result<(), DriverError> {
        if offset >= REGISTER_BLOCK_LEN {
            return Err(DriverError::DeviceFault);
        }
        let mut state = self.state.borrow_mut();
        match offset {
            super::C => {
                state.control = value & !(C_CLEAR | C_ST);
                // Dropping the enable bit aborts a transfer in flight; there
                // is no other abort.
                if value & C_I2CEN == 0 {
                    state.active = false;
                    state.started = false;
                }
                if value & C_CLEAR != 0 {
                    state.fifo_tx.clear();
                    state.rx.clear();
                }
                if value & C_ST != 0 {
                    state.starts.push(value);
                    state.remaining = state.dlen;
                    state.active = true;
                    state.started = true;
                    // A zero-length phase is the address-only probe: the
                    // acknowledge is the whole answer.
                    if state.remaining == 0 && state.behaviour == PartBehaviour::Present {
                        state.active = false;
                        state.status |= S_DONE;
                    }
                }
                state.refresh();
            }
            // The status register is write-one-to-clear for its latched bits.
            super::S => {
                state.status &= !(value & (S_CLKT | S_ERR | S_DONE));
            }
            super::DLEN => state.dlen = value,
            super::A => state.address = value,
            super::FIFO => {
                state.fifo_tx.push_back(value.to_le_bytes()[0]);
                state.refresh();
            }
            _ => {}
        }
        Ok(())
    }

    fn block_len(&self) -> usize {
        REGISTER_BLOCK_LEN
    }
}

impl BusWait for Sim {
    fn now_ns(&self) -> u64 {
        self.state.borrow().now_ns
    }

    fn wait(&self, timeout_ns: u64) {
        let mut state = self.state.borrow_mut();
        state.waits += 1;
        // A park costs time whether or not the controller advances; a wedged
        // one therefore reaches the deadline rather than looping for ever.
        state.now_ns = state.now_ns.saturating_add(timeout_ns.min(1_000_000));
        state.step();
    }
}

fn address() -> I2cAddress {
    I2cAddress::new(0x68).expect("usable")
}

#[test]
fn bring_up_enables_the_controller_and_clears_stale_state() {
    let sim = Sim::present(&[]);
    // A previous owner left every latched bit set and the controller off.
    sim.state.borrow_mut().status = S_CLKT | S_ERR | S_DONE;
    Bsc::new(&sim, &sim).expect("binds");
    let state = sim.state.borrow();
    assert_eq!(state.control & C_I2CEN, C_I2CEN);
    assert_eq!(state.status & (S_CLKT | S_ERR | S_DONE), 0);
}

#[test]
fn a_short_grant_is_refused_rather_than_read_past() {
    struct Stub;
    impl Registers for Stub {
        fn read32(&self, _offset: usize) -> Result<u32, DriverError> {
            Err(DriverError::DeviceFault)
        }
        fn write32(&self, _offset: usize, _value: u32) -> Result<(), DriverError> {
            Err(DriverError::DeviceFault)
        }
        fn block_len(&self) -> usize {
            REGISTER_BLOCK_LEN - 1
        }
    }
    let sim = Sim::present(&[]);
    assert_eq!(
        Bsc::new(&Stub, &sim).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn a_register_read_is_one_write_phase_then_one_read_phase() {
    let sim = Sim::present(&[0x11, 0x22, 0x33]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let mut out = [0u8; 3];
    bsc.port(address()).transfer(&[0x00], &mut out).expect("ok");
    assert_eq!(out, [0x11, 0x22, 0x33]);
    assert_eq!(sim.written(), alloc::vec![0x00]);
    assert_eq!(sim.addressed(), u32::from(address().get()));

    // Two starts, in order: a write phase then a read phase, each asking for
    // the completion interrupt and its own FIFO-service interrupt.
    let starts = sim.starts();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0] & C_READ, 0);
    assert_eq!(starts[0] & C_INTT, C_INTT);
    assert_eq!(starts[1] & C_READ, C_READ);
    assert_eq!(starts[1] & C_INTR, C_INTR);
    for start in starts {
        assert_eq!(start & C_INTD, C_INTD, "every phase asks for completion");
        assert_eq!(start & C_I2CEN, C_I2CEN);
    }
}

#[test]
fn a_transfer_longer_than_the_fifo_parks_and_resumes() {
    let payload: Vec<u8> = (0..MAX_TRANSFER_LEN)
        .map(|i| u8::try_from(i).expect("small"))
        .collect();
    assert!(
        payload.len() > FIFO_DEPTH,
        "the FIFO must be the bottleneck"
    );

    let sim = Sim::present(&payload);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let mut out = [0u8; MAX_TRANSFER_LEN];
    bsc.port(address()).transfer(&[0x00], &mut out).expect("ok");
    assert_eq!(&out[..], &payload[..]);
    assert!(
        sim.waits() > 0,
        "a transfer past the FIFO must park, never spin"
    );
}

#[test]
fn a_write_phase_hands_over_exactly_the_bytes_asked_for() {
    let sim = Sim::present(&[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let payload: Vec<u8> = (0..MAX_TRANSFER_LEN)
        .map(|i| u8::try_from(i).expect("small"))
        .collect();
    bsc.port(address()).transfer(&payload, &mut []).expect("ok");
    assert_eq!(sim.written(), payload);
    assert_eq!(sim.starts().len(), 1, "a write-only transfer is one phase");
}

#[test]
fn an_absent_part_reads_as_not_found() {
    let sim = Sim::new(PartBehaviour::Absent, &[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let mut out = [0u8; 2];
    assert_eq!(
        bsc.port(address()).transfer(&[0x00], &mut out),
        Err(DriverError::NotFound)
    );
    assert_eq!(out, [0, 0], "no half-read is mistaken for data");
    // The controller is left idle and its latched bits cleared, so the next
    // transfer starts from a known state.
    let state = sim.state.borrow();
    assert!(!state.active);
    assert_eq!(state.status & (S_CLKT | S_ERR | S_DONE), 0);
}

#[test]
fn a_part_that_stops_acknowledging_part_way_is_a_fault_not_an_absence() {
    let payload: Vec<u8> = (0..MAX_TRANSFER_LEN)
        .map(|i| u8::try_from(i).expect("small"))
        .collect();
    let sim = Sim::new(PartBehaviour::RefusesAfter(FIFO_DEPTH), &[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    assert_eq!(
        bsc.port(address()).transfer(&payload, &mut []),
        Err(DriverError::DeviceFault),
        "the part is there; the transfer is what failed"
    );
}

#[test]
fn a_clock_stretch_timeout_fails_closed() {
    let sim = Sim::new(PartBehaviour::Stretches, &[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let mut out = [0u8; 4];
    assert_eq!(
        bsc.port(address()).transfer(&[0x00], &mut out),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(out, [0; 4]);
}

#[test]
fn a_wedged_controller_gives_up_at_the_deadline_rather_than_spinning() {
    let sim = Sim::new(PartBehaviour::Wedged, &[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let mut out = [0u8; 4];
    assert_eq!(
        bsc.port(address()).transfer(&[0x00], &mut out),
        Err(DriverError::DeviceFault)
    );
    // The park is what consumed the budget: the loop never span.
    assert!(sim.waits() > 0);
    assert!(sim.now_ns() >= phase_deadline_ns(1));
    let state = sim.state.borrow();
    assert!(!state.active, "a wedged transfer is stopped, not abandoned");
}

#[test]
fn an_over_long_phase_is_refused_before_the_bus_is_touched() {
    let sim = Sim::present(&[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let long = [0u8; MAX_TRANSFER_LEN + 1];
    assert_eq!(
        bsc.port(address()).transfer(&long, &mut []),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        bsc.port(address())
            .transfer(&[], &mut [0u8; MAX_TRANSFER_LEN + 1]),
        Err(DriverError::LengthOutOfRange)
    );
    assert!(sim.starts().is_empty());
}

#[test]
fn an_address_only_transfer_probes_and_stops() {
    let sim = Sim::present(&[]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    bsc.port(address()).transfer(&[], &mut []).expect("ok");
    assert_eq!(sim.starts().len(), 1);
    assert_eq!(sim.state.borrow().dlen, 0);
    assert!(sim.written().is_empty());
}

#[test]
fn each_port_carries_its_own_address_and_the_wire_carries_none() {
    let sim = Sim::present(&[0xAB]);
    let bsc = Bsc::new(&sim, &sim).expect("binds");
    let other = I2cAddress::new(0x51).expect("usable");
    let mut out = [0u8; 1];
    bsc.port(other).transfer(&[0x00], &mut out).expect("ok");
    assert_eq!(
        sim.addressed(),
        u32::from(other.get()),
        "the address comes from the port the request was served on"
    );
}

#[test]
fn the_deadline_grows_with_the_phase_and_covers_the_slowest_bus() {
    // A byte at the slowest rate the driver tolerates is nine bit periods.
    assert!(phase_deadline_ns(MAX_TRANSFER_LEN) > phase_deadline_ns(1));
    assert!(phase_deadline_ns(0) > 0, "framing alone still takes time");
    // Nothing overflows at an absurd length.
    assert!(phase_deadline_ns(usize::MAX) > 0);
}

/// A [`DriverHost`] double that reports exactly the capabilities a test
/// grants it.
struct Host {
    caps: &'static [CapabilityId],
}

impl DriverHost for Host {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.caps.contains(&cap)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

#[test]
fn register_requires_the_load_capability() {
    assert!(register(&Host {
        caps: &[CapabilityId::DRV_LOAD]
    })
    .is_ok());
    assert_eq!(
        register(&Host { caps: &[] }).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn the_bind_table_names_the_controller_and_fits_the_abi_bound() {
    assert_eq!(BIND_KEYS.len(), 1);
    assert!(BSC_COMPATIBLE.len() <= HW_COMPATIBLE_MAX);
    let expected = HwMatchKey::compatible(BSC_COMPATIBLE).expect("fits");
    assert_eq!(BIND_KEYS[0].key, expected);
}

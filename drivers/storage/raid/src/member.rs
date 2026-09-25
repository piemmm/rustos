//! One member's data window, lent to a single block client at a time.
//!
//! The composer maps each member's window once and drives the device through a
//! block client that stages every transfer in it. Two clients over one window
//! would be two exclusive views of the same bytes, so the window is lent: a
//! client can be built only while no other holds it, and dropping the client
//! returns it. A membership whose agent has gone marks its window departed, so a
//! client still composed into an array fails every transfer rather than reaching
//! a device the membership no longer vouches for.

use alloc::rc::Rc;
use core::cell::Cell;

use tairix_abi::blkio::{BlkDeviceClass, BlkDeviceName};
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;
use tairix_abi::sysinfo::MountAvailability;

#[derive(Default)]
struct WindowState {
    lent: Cell<bool>,
    departed: Cell<bool>,
}

/// The composer's hold on one member's mapped data window.
///
/// Cloning yields another handle to the same window: the member table keeps
/// one, and the loan a client holds is another.
#[derive(Clone, Default)]
pub struct MemberWindow(Rc<WindowState>);

impl MemberWindow {
    /// A window no client holds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lend the window to one client, or [`None`] while another holds it.
    #[must_use]
    pub fn lend(&self) -> Option<WindowLease> {
        if self.0.lent.replace(true) {
            return None;
        }
        Some(WindowLease(self.clone()))
    }

    /// Whether a client holds the window.
    #[must_use]
    pub fn is_lent(&self) -> bool {
        self.0.lent.get()
    }

    /// End the membership: a client still holding the window fails every
    /// transfer from here on.
    pub fn depart(&self) {
        self.0.departed.set(true);
    }

    /// Whether the membership has ended.
    #[must_use]
    pub fn departed(&self) -> bool {
        self.0.departed.get()
    }

    fn return_loan(&self) {
        self.0.lent.set(false);
    }
}

/// The one loan of a member's window. Dropping it returns the window.
pub struct WindowLease(MemberWindow);

impl Drop for WindowLease {
    fn drop(&mut self) {
        self.0.return_loan();
    }
}

/// A member's block client and the loan of the window it stages through.
pub struct MemberDevice<B> {
    // Declared first so it drops first: the window is returned only once no
    // client over it remains.
    device: B,
    lease: WindowLease,
}

impl<B> MemberDevice<B> {
    /// Pair `device` with the loan of the window it was built over.
    #[must_use]
    pub fn new(device: B, lease: WindowLease) -> Self {
        Self { device, lease }
    }

    /// Whether the membership this device belongs to has ended.
    #[must_use]
    pub fn departed(&self) -> bool {
        self.lease.0.departed()
    }

    fn reachable(&self) -> Result<(), DriverError> {
        if self.departed() {
            return Err(DriverError::DeviceOffline);
        }
        Ok(())
    }
}

impl<B: Block> Block for MemberDevice<B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        self.device.geometry()
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.reachable()?;
        self.device.read_blocks(lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.reachable()?;
        self.device.write_blocks(lba, buf)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.reachable()?;
        self.device.flush()
    }

    fn device_class(&self) -> BlkDeviceClass {
        self.device.device_class()
    }

    fn device_name(&self) -> BlkDeviceName {
        self.device.device_name()
    }

    fn backing_availability(&self) -> MountAvailability {
        self.device.backing_availability()
    }
}

#[cfg(test)]
mod tests;

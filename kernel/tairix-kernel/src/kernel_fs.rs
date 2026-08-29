//! The mounted-volume filesystem driver, type-erased.
//!
//! [`KernelFs`] is the one trait bound every registered mount driver
//! satisfies: the structural surfaces the secured VFS delegates to —
//! read, write, security, and the whole-volume space accounting the
//! mount snapshot reports — plus [`Send`] (the mount lives behind a
//! sleeping lock shared across the per-CPU syscall handlers). The
//! blanket impl makes every concrete driver (a `ARXFS<…>`, a
//! `CachedFs<…>` wrapper) a `KernelFs`; the `Box<dyn KernelFs>`
//! forwarding impls let the boxed, board-specific driver be the single
//! concrete type the boot-time statics name.
//!
//! Architecture-neutral (it names only `tairix_abi` types), so the
//! arch-neutral unlock policy (`crate::root_mount`) and the
//! account-administration storage (`crate::user_admin_backing`) can use
//! it on every instruction set, while the boot wiring that registers
//! drivers (`crate::system_mount`) stays gated on the ports with a
//! storage floor.

use alloc::boxed::Box;

use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrsFs, FilesystemAttrsProvider, FilesystemRead, FilesystemSecurity,
    FilesystemStats, FilesystemWrite, NodeId, NodeInfo, NodeKind, NodeSecurity, VolumeStats,
    WritebackHost,
};
use tairix_abi::driver::DriverHandle;
use tairix_abi::DriverError;

/// The mounted-volume filesystem driver, type-erased. See the module
/// docs.
pub trait KernelFs:
    FilesystemRead
    + FilesystemWrite
    + FilesystemSecurity
    + FilesystemStats
    + FilesystemAttrsProvider
    + Send
{
}

impl<T> KernelFs for T where
    T: FilesystemRead
        + FilesystemWrite
        + FilesystemSecurity
        + FilesystemStats
        + FilesystemAttrsProvider
        + Send
{
}

impl FilesystemRead for Box<dyn KernelFs> {
    fn read_link(&mut self, node: NodeId, out: &mut [u8]) -> Result<usize, DriverError> {
        (**self).read_link(node, out)
    }

    fn root(&self) -> NodeId {
        (**self).root()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        (**self).node_info(node)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        (**self).lookup(dir, name)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        (**self).read_at(file, offset, buf)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        (**self).read_dir(dir, index, name_out)
    }
}

impl FilesystemWrite for Box<dyn KernelFs> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        (**self).create(dir, name, kind)
    }

    fn create_link(
        &mut self,
        dir: NodeId,
        name: &[u8],
        target: &[u8],
    ) -> Result<NodeId, DriverError> {
        (**self).create_link(dir, name, target)
    }

    fn link(&mut self, dir: NodeId, name: &[u8], node: NodeId) -> Result<(), DriverError> {
        (**self).link(dir, name, node)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        (**self).write_at(dir, name, offset, data)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        (**self).truncate(dir, name, size)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        (**self).remove(dir, name)
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        (**self).rename(src_dir, src_name, dst_dir, dst_name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        (**self).flush()
    }

    fn set_writeback_host(&mut self, volume: DriverHandle, host: &'static dyn WritebackHost) {
        (**self).set_writeback_host(volume, host);
    }
}

impl FilesystemSecurity for Box<dyn KernelFs> {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        (**self).security(node)
    }

    fn set_security(&mut self, node: NodeId, security: NodeSecurity) -> Result<(), DriverError> {
        (**self).set_security(node, security)
    }
}

impl FilesystemStats for Box<dyn KernelFs> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        (**self).stats()
    }
}

impl FilesystemAttrsProvider for Box<dyn KernelFs> {
    fn attrs_fs(&mut self) -> Option<&mut dyn FilesystemAttrsFs> {
        (**self).attrs_fs()
    }
}

#[cfg(test)]
mod tests {
    use super::KernelFs;
    use alloc::boxed::Box;
    use tairix_kernel_core::fs::wrapper_conformance as conformance;

    /// The boxed mount driver is what the kernel's real filesystem service
    /// holds, so a facet method it left defaulted refused on every mounted
    /// volume while every test that built a driver directly still passed.
    ///
    /// The attribute facet is claimed here (through
    /// [`FilesystemAttrsProvider`](tairix_abi::driver::filesystem::FilesystemAttrsProvider))
    /// but `FilesystemAttrs` itself is not, so the provider hands out the
    /// *inner* driver's view — which the suite drives.
    #[test]
    fn the_boxed_mount_driver_forwards_every_facet_method() {
        let mut boxed: Box<dyn KernelFs> = Box::new(conformance::fixture());
        conformance::assert_read_forwards(&mut boxed);
        conformance::assert_write_forwards(&mut boxed);
        conformance::assert_security_forwards(&mut boxed);
        conformance::assert_stats_forwards(&mut boxed);
        conformance::assert_attrs_forwards(&mut boxed);
    }
}

//! Host tests for the RAID0 stripe over a fault-injecting [`Block`] double.

use super::{ArrayHealth, StripeArray, StripeError, StripeMember};
use core::cell::Cell;
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::{BufferClass, DriverError};

const BS: u32 = 512;
/// Logical blocks per member.
const MB: u64 = 8;
/// One member's byte capacity (`BS * MB`), written with a literal so the
/// constant needs no cast.
const CAP: usize = 512 * 8;
/// The stripe unit used across the tests, in logical blocks.
const CHUNK: u32 = 2;
/// The number of members in the standard test array.
const MEMBERS: usize = 3;

/// An in-memory block device with post-assembly-injectable faults (via
/// [`Cell`], so a test flips a fault through the shared borrow the array's
/// [`StripeArray::member`] hands back while the array owns the member).
struct StripeBlock {
    store: [u8; CAP],
    geo: Cell<BlockGeometry>,
    present: Cell<bool>,
    read_fault: Cell<Option<DriverError>>,
    write_fault: Cell<Option<DriverError>>,
    /// A single member-local block that returns [`DriverError::MediumError`]
    /// on read — a latent bad sector, not a dead device.
    medium_block: Cell<Option<u64>>,
    flush_fault: Cell<bool>,
}

impl StripeBlock {
    fn new() -> Self {
        Self {
            store: [0u8; CAP],
            geo: Cell::new(BlockGeometry {
                block_size: BS,
                block_count: MB,
            }),
            present: Cell::new(true),
            read_fault: Cell::new(None),
            write_fault: Cell::new(None),
            medium_block: Cell::new(None),
            flush_fault: Cell::new(false),
        }
    }

    fn with_geometry(self, geo: BlockGeometry) -> Self {
        self.geo.set(geo);
        self
    }

    fn absent() -> Self {
        let d = Self::new();
        d.present.set(false);
        d
    }

    fn span(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let bs = BS as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(bs))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > CAP {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }

    /// The first byte of member-local block `lba` in this member's store.
    fn block_byte(&self, lba: u64) -> u8 {
        self.store[usize::try_from(lba).unwrap() * BS as usize]
    }
}

impl Block for StripeBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        if self.present.get() {
            Ok(self.geo.get())
        } else {
            Err(DriverError::DeviceOffline)
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        if let Some(e) = self.read_fault.get() {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        if let Some(bad) = self.medium_block.get() {
            let blocks = (buf.len() / BS as usize) as u64;
            if bad >= lba && bad < lba + blocks {
                return Err(DriverError::MediumError);
            }
        }
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        if let Some(e) = self.write_fault.get() {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        if self.flush_fault.get() {
            Err(DriverError::DeviceFault)
        } else {
            Ok(())
        }
    }
}

/// The standard three-member table, each a fresh empty member.
fn members() -> [StripeMember<StripeBlock>; MEMBERS] {
    [
        StripeMember::new(StripeBlock::new()),
        StripeMember::new(StripeBlock::new()),
        StripeMember::new(StripeBlock::new()),
    ]
}

#[test]
fn assemble_presents_the_sum_of_member_capacities() {
    let mut m = members();
    let array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    assert_eq!(array.member_count(), MEMBERS);
    assert_eq!(
        array.array_geometry(),
        BlockGeometry {
            block_size: BS,
            block_count: MB * MEMBERS as u64,
        }
    );
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn a_striped_write_and_read_round_trips_across_every_chunk() {
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    let total = usize::try_from(MB * MEMBERS as u64).unwrap() * BS as usize;
    // A distinct byte per logical block, so a mis-mapped chunk is caught.
    let mut out = [0u8; { 8 * 3 * 512 }];
    assert_eq!(out.len(), total);
    for blk in 0..(MB * MEMBERS as u64) {
        let b = u8::try_from(blk % 251).unwrap();
        let base = usize::try_from(blk).unwrap() * BS as usize;
        out[base..base + BS as usize].fill(b);
    }
    array.write_blocks(0, &out).unwrap();
    let mut back = [0u8; { 8 * 3 * 512 }];
    array.read_blocks(0, &mut back).unwrap();
    assert_eq!(out, back);
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn blocks_land_on_the_expected_member_and_local_lba() {
    // chunk = 2, members = 3. Logical block b -> member (b/2)%3, local lba
    // ((b/2)/3)*2 + b%2. Write byte==block index, then read the striping back
    // off the raw member stores.
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    for blk in 0..(MB * MEMBERS as u64) {
        let b = u8::try_from(blk).unwrap();
        let buf = [b; BS as usize];
        array.write_blocks(blk, &buf).unwrap();
    }
    let chunk = u64::from(CHUNK);
    let mc = MEMBERS as u64;
    for blk in 0..(MB * MEMBERS as u64) {
        let expect_member = usize::try_from((blk / chunk) % mc).unwrap();
        let expect_lba = (blk / chunk / mc) * chunk + blk % chunk;
        let device = array.member(expect_member).unwrap().device();
        assert_eq!(
            device.block_byte(expect_lba),
            u8::try_from(blk).unwrap(),
            "logical block {blk} should sit on member {expect_member} lba {expect_lba}"
        );
    }
}

#[test]
fn a_cross_stripe_read_gathers_from_every_member() {
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    let all = usize::try_from(MB * MEMBERS as u64).unwrap() * BS as usize;
    let mut src = [0u8; { 8 * 3 * 512 }];
    for (i, byte) in src.iter_mut().enumerate() {
        *byte = u8::try_from(i % 253).unwrap();
    }
    assert_eq!(src.len(), all);
    array.write_blocks(0, &src).unwrap();
    // A five-block window starting at block 1 spans member0 (block1), member1
    // (blocks 2,3) and member2 (blocks 4,5) — three members, two chunk edges.
    let start = BS as usize;
    let span = 5usize * BS as usize;
    let mut back = [0u8; 5 * 512];
    array.read_blocks(1, &mut back).unwrap();
    assert_eq!(back, src[start..start + span]);
}

#[test]
fn assemble_rejects_every_ill_formed_array() {
    // Empty member table.
    let mut empty: [StripeMember<StripeBlock>; 0] = [];
    assert_eq!(
        StripeArray::assemble(&mut empty, CHUNK).err(),
        Some(StripeError::NoMembers)
    );

    // Zero stripe unit.
    let mut m = members();
    assert_eq!(
        StripeArray::assemble(&mut m, 0).err(),
        Some(StripeError::ZeroChunk)
    );

    // A member that cannot report geometry fails the whole assembly closed
    // (no redundancy to serve its blocks).
    let mut m = [
        StripeMember::new(StripeBlock::new()),
        StripeMember::new(StripeBlock::absent()),
    ];
    assert_eq!(
        StripeArray::assemble(&mut m, CHUNK).err(),
        Some(StripeError::MemberUnavailable)
    );

    // Members of different sizes cannot stripe evenly.
    let mut m = [
        StripeMember::new(StripeBlock::new()),
        StripeMember::new(StripeBlock::new().with_geometry(BlockGeometry {
            block_size: BS,
            block_count: MB + u64::from(CHUNK),
        })),
    ];
    assert_eq!(
        StripeArray::assemble(&mut m, CHUNK).err(),
        Some(StripeError::GeometryMismatch)
    );

    // A member whose size is not a whole number of stripe chunks.
    let mut m = [StripeMember::new(StripeBlock::new().with_geometry(
        BlockGeometry {
            block_size: BS,
            block_count: 7, // not a multiple of CHUNK=2
        },
    ))];
    assert_eq!(
        StripeArray::assemble(&mut m, CHUNK).err(),
        Some(StripeError::UnalignedGeometry)
    );

    // A degenerate geometry.
    let mut m = [StripeMember::new(StripeBlock::new().with_geometry(
        BlockGeometry {
            block_size: BS,
            block_count: 0,
        },
    ))];
    assert_eq!(
        StripeArray::assemble(&mut m, CHUNK).err(),
        Some(StripeError::ZeroGeometry)
    );
}

#[test]
fn a_whole_device_fault_fails_the_array_closed_for_good() {
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    // Member 1 goes offline. Block 2 lives on member 1 (chunk 1 -> member 1).
    array
        .member(1)
        .unwrap()
        .device()
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = [0u8; BS as usize];
    assert_eq!(
        array.read_blocks(2, &mut buf),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(array.health(), ArrayHealth::Failed);
    // The array is now failed closed for good: a read that only touches the
    // healthy member 0 (block 0) still fails closed, because the array can no
    // longer present a complete logical block space.
    assert_eq!(
        array.read_blocks(0, &mut buf),
        Err(DriverError::DeviceOffline)
    );
    // ...and so does a write and a flush.
    assert_eq!(array.write_blocks(0, &buf), Err(DriverError::DeviceOffline));
    assert_eq!(array.flush(), Err(DriverError::DeviceOffline));
}

#[test]
fn a_per_block_media_error_fails_only_that_request() {
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    // A latent bad sector at member 0, local block 1 — that is logical block 1
    // (chunk 0, offset 1 -> member 0 lba 1).
    array.member(0).unwrap().device().medium_block.set(Some(1));
    let mut buf = [0u8; BS as usize];
    // Reading the bad logical block surfaces the medium error...
    assert_eq!(
        array.read_blocks(1, &mut buf),
        Err(DriverError::MediumError)
    );
    // ...but the device is still reachable, so the array stays optimal and
    // unrelated stripes keep serving.
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.read_blocks(0, &mut buf), Ok(()));
    assert_eq!(array.read_blocks(2, &mut buf), Ok(()));
}

#[test]
fn flush_commits_every_member_and_fails_closed_when_one_cannot() {
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    assert_eq!(array.flush(), Ok(()));
    // A member that cannot flush is a durability failure for its stripes: the
    // whole flush fails closed and the array drops that member for good.
    array.member(2).unwrap().device().flush_fault.set(true);
    assert_eq!(array.flush(), Err(DriverError::DeviceFault));
    assert_eq!(array.health(), ArrayHealth::Failed);
}

#[test]
fn out_of_range_and_misaligned_requests_are_refused_before_any_member() {
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    let mut buf = [0u8; BS as usize];
    // Past the end of the summed array (24 blocks).
    assert_eq!(
        array.read_blocks(MB * MEMBERS as u64, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
    // An overflowing extent.
    assert_eq!(
        array.read_blocks(u64::MAX, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
    // A non-block-multiple / empty buffer.
    assert_eq!(
        array.read_blocks(0, &mut buf[..BS as usize - 1]),
        Err(DriverError::BufferTooSmall)
    );
    assert_eq!(
        array.read_blocks(0, &mut buf[..0]),
        Err(DriverError::BufferTooSmall)
    );
    // A refused request never touched a member, so the array is still optimal.
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn buffer_class_is_forwarded_to_the_members() {
    // The with-class variants delegate to the members carrying the class, so a
    // sensitive transfer round-trips exactly as the plain one.
    let mut m = members();
    let mut array = StripeArray::assemble(&mut m, CHUNK).unwrap();
    let payload = [0xABu8; 3 * 512];
    array
        .write_blocks_with_class(2, &payload, BufferClass::Sensitive)
        .unwrap();
    let mut back = [0u8; 3 * 512];
    array
        .read_blocks_with_class(2, &mut back, BufferClass::Sensitive)
        .unwrap();
    assert_eq!(payload, back);
}

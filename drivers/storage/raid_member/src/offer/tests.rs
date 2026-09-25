//! Host tests for what a member agent's offer names.
//!
//! The kernel is modelled only as far as the exchange uses it: region ids come
//! from one machine-wide counter, grant handles from a counter per holder, a
//! delegation hands the recipient the handle it already holds for a region,
//! and a map resolves the caller's own handle. That is enough to show which of
//! the agent's values the composer can map the window by.

extern crate std;

use std::vec::Vec;

use super::Transport;
use tairix_abi::Errno;

/// The device's block-service endpoint id.
const ENDPOINT: u64 = 0x0056_424B_0000_0003;

/// The agent's own hardware-tree node.
const NODE: u32 = 0x31;

/// The leaf driver that created the device's window.
const LEAF: usize = 0;
/// The member agent, holding the window through its matched node.
const AGENT: usize = 1;
/// The array composer the agent delegates to.
const COMPOSER: usize = 2;

/// The two namespaces delegation and mapping use.
struct Grants {
    last_region: u64,
    /// Each holder's regions in the order it was granted them: the handle
    /// naming `held[holder][i]` is `i + 1`.
    held: [Vec<u64>; 3],
}

impl Grants {
    fn new() -> Self {
        Self {
            last_region: 0,
            held: [Vec::new(), Vec::new(), Vec::new()],
        }
    }

    /// Create a region owned by `holder`, as `shm_create` does.
    fn create(&mut self, holder: usize) -> u64 {
        self.last_region += 1;
        let region = self.last_region;
        self.mint(holder, region);
        region
    }

    /// Mint `holder` a grant for `region`, returning the handle it already
    /// holds for it if it has one.
    fn mint(&mut self, holder: usize, region: u64) -> u64 {
        let table = &mut self.held[holder];
        if !table.contains(&region) {
            table.push(region);
        }
        let index = table
            .iter()
            .position(|&held| held == region)
            .expect("the region was just granted");
        u64::try_from(index + 1).expect("a handle fits")
    }

    /// `shm_grant` of `region` from `from` to `to`: the recipient's handle,
    /// or `-errno` when `from` holds no grant for it.
    fn shm_grant(&mut self, from: usize, region: u64, to: usize) -> i64 {
        if !self.held[from].contains(&region) {
            return -i64::from(Errno::NotFound.as_i32());
        }
        i64::try_from(self.mint(to, region)).expect("a handle fits")
    }

    /// The region `holder` maps when it presents `handle` to `shm_map`.
    fn shm_map(&self, holder: usize, handle: u64) -> Option<u64> {
        let index = usize::try_from(handle.checked_sub(1)?).ok()?;
        self.held[holder].get(index).copied()
    }
}

/// A device's window, created by its leaf driver and held by its agent.
fn device_window(kernel: &mut Grants) -> Transport {
    let window = kernel.create(LEAF);
    kernel.mint(AGENT, window);
    Transport {
        endpoint: ENDPOINT,
        window,
    }
}

#[test]
fn the_composer_maps_exactly_the_window_the_agent_delegated() {
    let mut kernel = Grants::new();
    // Other drivers' regions and the composer's own array window come first,
    // so the device's region id and the composer's handle for it differ.
    for _ in 0..4 {
        kernel.create(LEAF);
    }
    kernel.create(COMPOSER);
    let transport = device_window(&mut kernel);

    let offer = transport
        .offer(kernel.shm_grant(AGENT, transport.window, COMPOSER), NODE)
        .expect("the delegation minted the composer a handle");
    assert_eq!(
        kernel.shm_map(COMPOSER, offer.window_grant),
        Some(transport.window),
        "the composer maps the window by the handle the offer names"
    );
    assert_eq!(offer.endpoint, ENDPOINT, "an endpoint is called by its id");
    assert_eq!(offer.node, NODE);

    let again = transport
        .offer(kernel.shm_grant(AGENT, transport.window, COMPOSER), NODE)
        .expect("re-delegating is idempotent");
    assert_eq!(
        again, offer,
        "a re-offer names the window the composer already holds, which is how it \
         refuses a second membership over one window"
    );
}

#[test]
fn a_region_id_that_names_another_of_the_composers_grants_is_never_mapped() {
    let mut kernel = Grants::new();
    let transport = device_window(&mut kernel);
    // The composer publishes arrays of its own, so one of its handles now
    // equals the device's region id and names one of its array windows.
    let array_window = kernel.create(COMPOSER);
    kernel.create(COMPOSER);
    assert_eq!(
        kernel.shm_map(COMPOSER, transport.window),
        Some(array_window),
        "the premise: read as a handle, the region id names the array's window"
    );

    let offer = transport
        .offer(kernel.shm_grant(AGENT, transport.window, COMPOSER), NODE)
        .expect("the delegation minted the composer a handle");
    assert_eq!(
        kernel.shm_map(COMPOSER, offer.window_grant),
        Some(transport.window),
        "the composer must stage this member's transfers through the member's \
         window, never through its own array's"
    );
}

#[test]
fn a_refused_delegation_is_reported_rather_than_offered() {
    let mut kernel = Grants::new();
    let window = kernel.create(LEAF);
    let transport = Transport {
        endpoint: ENDPOINT,
        window,
    };
    // The agent was never granted the window, so the kernel refuses to
    // delegate it and there is nothing to offer.
    assert_eq!(
        transport.offer(kernel.shm_grant(AGENT, window, COMPOSER), NODE),
        Err(Errno::NotFound)
    );
    // Zero is neither a handle the kernel issues nor an errno.
    assert_eq!(transport.offer(0, NODE), Err(Errno::NotImplemented));
}

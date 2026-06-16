//! Deterministic hardware-node ↔ driver bind-table match resolution
//! (`AGENTS.md` §18.3).
//!
//! Matching is pure data comparison: a hardware-tree node carries the
//! match keys its discoverer emitted ([`rustos_abi::HwNode::match_keys`]),
//! a driver candidate carries the bind table its signed manifest declared,
//! and a bind-table entry matches a node when its [`HwMatchKey`] matches one
//! of the node's keys ([`HwMatchKey::matches`] — exact for
//! `compatible`/virtio, and class-with-optional-vendor/device-wildcard for
//! PCI/USB, so a generic class driver binds without hard-coding a device
//! id). When several drivers match the same node the highest matched bind
//! priority wins; an unbroken tie across *different* drivers is a packaging
//! defect and the node is refused a binding — never a coin-flip
//! (`AGENTS.md` §2.1, §18.3).
//!
//! # Why this is its own crate
//!
//! The same match policy is needed in two places that cannot share a crate
//! across the §17.4 layering boundary:
//!
//! * the user-space device manager (`userland/system/devmgr`), the §18.3
//!   autoload owner; and
//! * the kernel's interim in-kernel driver-candidate catalogue
//!   (`kernel/rustos-kernel`), which brings the Pi 4 USB chain up by the
//!   same data-driven match until the user-space driver-host-over-IPC path
//!   lands (PLAN Stage 4.HW item 5).
//!
//! The kernel may not depend on a `userland/*` crate (§17.4), so the policy
//! lives here in `lib/*` as the single definition both reach — never
//! duplicated (`AGENTS.md` §2.2).
//!
//! # Stability
//!
//! Tier: `experimental` (per `AGENTS.md` §6). The wire formats compared
//! (hardware-tree match keys, bind-table entries) are owned by `rustos-abi`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_abi::{DriverBindKey, HwMatchKey};

/// One autoload candidate: a driver image's logical path plus the bind
/// table decoded — fail-closed — from its signed manifest.
///
/// The caller supplies the decoded table (the drvhost load gate already
/// validates every entry via `ParsedImage::decode_bind_table`); the
/// match resolver never re-parses image bytes itself, keeping the §17.4
/// layering intact.
#[derive(Copy, Clone, Debug)]
pub struct DriverCandidate<'a> {
    /// Logical image path understood by the driver-host load gate.
    pub path: &'a str,
    /// The candidate's decoded bind table.
    pub bind_keys: &'a [DriverBindKey],
}

/// Outcome of resolving one node against every candidate.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MatchResolution {
    /// No candidate's bind table matches any of the node's keys.
    Unmatched,
    /// Exactly one candidate holds the highest matched priority.
    Winner {
        /// Index of the winning candidate in the caller's slice.
        candidate: usize,
        /// The winning bind priority.
        priority: u16,
    },
    /// Two or more *distinct* candidates matched at the same highest
    /// priority — a packaging defect (`AGENTS.md` §18.3).
    Tie {
        /// The tied highest priority.
        priority: u16,
    },
}

/// The highest priority at which `bind_keys` matches any of
/// `node_keys`, or [`None`] when nothing matches.
///
/// Ties *within* one candidate's own table are harmless (the same
/// driver wins either way), so only the maximum is reported.
#[must_use]
pub fn best_bind_priority(node_keys: &[HwMatchKey], bind_keys: &[DriverBindKey]) -> Option<u16> {
    let mut best: Option<u16> = None;
    for bind in bind_keys {
        if node_keys.iter().any(|node| bind.key.matches(node)) {
            best = Some(match best {
                Some(current) if current >= bind.priority => current,
                _ => bind.priority,
            });
        }
    }
    best
}

/// Resolve a node's match keys against every candidate, deterministically.
///
/// The result depends only on the key sets and priorities, never on
/// iteration order: the strict-maximum candidate wins, and an equal
/// maximum held by two distinct candidates is reported as a
/// [`MatchResolution::Tie`] regardless of where they sit in the slice.
#[must_use]
pub fn resolve(node_keys: &[HwMatchKey], candidates: &[DriverCandidate<'_>]) -> MatchResolution {
    let mut winner: Option<(usize, u16)> = None;
    let mut tied = false;
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(priority) = best_bind_priority(node_keys, candidate.bind_keys) else {
            continue;
        };
        match winner {
            None => {
                winner = Some((index, priority));
            }
            Some((_, best)) if priority > best => {
                winner = Some((index, priority));
                tied = false;
            }
            Some((_, best)) if priority == best => {
                tied = true;
            }
            Some(_) => {}
        }
    }
    match winner {
        None => MatchResolution::Unmatched,
        Some((_, priority)) if tied => MatchResolution::Tie { priority },
        Some((candidate, priority)) => MatchResolution::Winner {
            candidate,
            priority,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compat(s: &[u8]) -> HwMatchKey {
        match HwMatchKey::compatible(s) {
            Ok(key) => key,
            Err(_) => unreachable!("test compatible strings fit HW_COMPATIBLE_MAX"),
        }
    }

    #[test]
    fn exact_compatible_match_wins() {
        let node = [compat(b"brcm,bcm2711-emmc2")];
        let table = [DriverBindKey::new(5, compat(b"brcm,bcm2711-emmc2"))];
        let candidates = [DriverCandidate {
            path: "/d/emmc2",
            bind_keys: &table,
        }];
        assert_eq!(
            resolve(&node, &candidates),
            MatchResolution::Winner {
                candidate: 0,
                priority: 5
            }
        );
    }

    #[test]
    fn numeric_keys_match_on_all_fields() {
        let node = [HwMatchKey::pci(0x8086, 0x10D3, 0x0200_0000)];
        let exact = [DriverBindKey::new(
            1,
            HwMatchKey::pci(0x8086, 0x10D3, 0x0200_0000),
        )];
        let wrong_class = [DriverBindKey::new(
            9,
            HwMatchKey::pci(0x8086, 0x10D3, 0x0100_0000),
        )];
        let candidates = [
            DriverCandidate {
                path: "/d/wrong",
                bind_keys: &wrong_class,
            },
            DriverCandidate {
                path: "/d/right",
                bind_keys: &exact,
            },
        ];
        assert_eq!(
            resolve(&node, &candidates),
            MatchResolution::Winner {
                candidate: 1,
                priority: 1
            }
        );
    }

    #[test]
    fn class_wildcard_candidate_binds_a_concrete_device() {
        // A generic xHCI driver declares a class-wildcard bind key
        // (vendor/device 0); a concrete VL805 node (vendor 0x1106, device
        // 0x3483, class 0x0C0330) binds it, while a different-class node
        // (an AHCI controller, class 0x010601) does not.
        let xhci = [DriverBindKey::new(3, HwMatchKey::pci(0, 0, 0x0C_0330))];
        let candidates = [DriverCandidate {
            path: "/System/Drivers/bus_usb.rxe",
            bind_keys: &xhci,
        }];
        let vl805 = [HwMatchKey::pci(0x1106, 0x3483, 0x0C_0330)];
        assert_eq!(
            resolve(&vl805, &candidates),
            MatchResolution::Winner {
                candidate: 0,
                priority: 3
            }
        );
        let ahci = [HwMatchKey::pci(0x8086, 0x2922, 0x01_0601)];
        assert_eq!(resolve(&ahci, &candidates), MatchResolution::Unmatched);
    }

    #[test]
    fn no_match_is_unmatched() {
        let node = [HwMatchKey::virtio(2)];
        let table = [DriverBindKey::new(7, HwMatchKey::virtio(1))];
        let candidates = [DriverCandidate {
            path: "/d/blk",
            bind_keys: &table,
        }];
        assert_eq!(resolve(&node, &candidates), MatchResolution::Unmatched);
        assert_eq!(resolve(&node, &[]), MatchResolution::Unmatched);
        assert_eq!(resolve(&[], &candidates), MatchResolution::Unmatched);
    }

    #[test]
    fn higher_priority_candidate_wins() {
        let node = [compat(b"ns16550a")];
        let low = [DriverBindKey::new(1, compat(b"ns16550a"))];
        let high = [DriverBindKey::new(9, compat(b"ns16550a"))];
        let candidates = [
            DriverCandidate {
                path: "/d/generic",
                bind_keys: &low,
            },
            DriverCandidate {
                path: "/d/tuned",
                bind_keys: &high,
            },
        ];
        assert_eq!(
            resolve(&node, &candidates),
            MatchResolution::Winner {
                candidate: 1,
                priority: 9
            }
        );
        // Order independence: same winner with the slice reversed.
        let reversed = [candidates[1], candidates[0]];
        assert_eq!(
            resolve(&node, &reversed),
            MatchResolution::Winner {
                candidate: 0,
                priority: 9
            }
        );
    }

    #[test]
    fn unbroken_tie_is_reported() {
        let node = [HwMatchKey::virtio(16)];
        let a = [DriverBindKey::new(4, HwMatchKey::virtio(16))];
        let b = [DriverBindKey::new(4, HwMatchKey::virtio(16))];
        let candidates = [
            DriverCandidate {
                path: "/d/gpu-a",
                bind_keys: &a,
            },
            DriverCandidate {
                path: "/d/gpu-b",
                bind_keys: &b,
            },
        ];
        assert_eq!(
            resolve(&node, &candidates),
            MatchResolution::Tie { priority: 4 }
        );
    }

    #[test]
    fn tie_below_the_winner_is_broken() {
        let node = [HwMatchKey::usb(0x046D, 0xC52B, 0x0300)];
        let low_a = [DriverBindKey::new(
            2,
            HwMatchKey::usb(0x046D, 0xC52B, 0x0300),
        )];
        let low_b = [DriverBindKey::new(
            2,
            HwMatchKey::usb(0x046D, 0xC52B, 0x0300),
        )];
        let high = [DriverBindKey::new(
            3,
            HwMatchKey::usb(0x046D, 0xC52B, 0x0300),
        )];
        let candidates = [
            DriverCandidate {
                path: "/d/hid-a",
                bind_keys: &low_a,
            },
            DriverCandidate {
                path: "/d/hid-b",
                bind_keys: &low_b,
            },
            DriverCandidate {
                path: "/d/hid-vendor",
                bind_keys: &high,
            },
        ];
        assert_eq!(
            resolve(&node, &candidates),
            MatchResolution::Winner {
                candidate: 2,
                priority: 3
            }
        );
    }

    #[test]
    fn intra_candidate_tie_is_not_a_tie() {
        // The same driver matching twice at the same priority is not a
        // packaging defect — the same driver binds either way.
        let node = [compat(b"arm,pl011"), compat(b"arm,primecell")];
        let table = [
            DriverBindKey::new(6, compat(b"arm,pl011")),
            DriverBindKey::new(6, compat(b"arm,primecell")),
        ];
        let candidates = [DriverCandidate {
            path: "/d/uart",
            bind_keys: &table,
        }];
        assert_eq!(
            resolve(&node, &candidates),
            MatchResolution::Winner {
                candidate: 0,
                priority: 6
            }
        );
    }

    #[test]
    fn best_bind_priority_reports_the_maximum() {
        let node = [compat(b"a"), compat(b"b")];
        let table = [
            DriverBindKey::new(1, compat(b"a")),
            DriverBindKey::new(8, compat(b"b")),
            DriverBindKey::new(3, compat(b"a")),
        ];
        assert_eq!(best_bind_priority(&node, &table), Some(8));
        assert_eq!(best_bind_priority(&[], &table), None);
        assert_eq!(best_bind_priority(&node, &[]), None);
    }
}

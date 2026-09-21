//! The socket set's totality, which a rig's mount table is indexed by.

use tairix_util::mathf;

use super::{Mount, Side, Socket};
use crate::frame::{Body, Rotation};
use crate::joint::JointId;

#[test]
fn every_socket_has_its_own_slot() {
    // A rig holds its mounts in an array this indexes, so a collision here
    // would silently make two sockets one.
    let mut seen = [false; Socket::COUNT];
    for socket in Socket::ALL {
        let slot = socket.index();
        assert!(slot < Socket::COUNT, "{socket:?} indexes past the table");
        assert!(!seen[slot], "{socket:?} shares a slot");
        seen[slot] = true;
    }
    assert!(seen.into_iter().all(|used| used), "a slot names no socket");
}

#[test]
fn the_socket_list_is_in_index_order() {
    for (position, socket) in Socket::ALL.into_iter().enumerate() {
        assert_eq!(socket.index(), position);
    }
}

#[test]
fn a_side_points_one_way_or_the_other() {
    assert!(
        Side::Left.across() > 0.0,
        "the frame's side axis is the left"
    );
    assert!(Side::Right.across() < 0.0);
    assert!(
        mathf::fabs(Side::Left.across() + Side::Right.across()) <= f64::EPSILON,
        "the two sides must be exact opposites"
    );
    assert_eq!(Side::BOTH, [Side::Left, Side::Right]);
}

#[test]
fn a_mount_rests_square_until_it_is_oriented() {
    let joint = JointId::new(3);
    let plain = Mount::new(joint, Body::new(1.0, 2.0, 3.0));
    assert_eq!(plain.orientation, Rotation::REST);

    let angled = Rotation::new(0.2, -0.1, 0.4);
    let scabbard = plain.oriented(angled);
    assert_eq!(scabbard.orientation, angled);
    assert_eq!(scabbard.joint, joint);
    assert_eq!(scabbard.at, plain.at);
}

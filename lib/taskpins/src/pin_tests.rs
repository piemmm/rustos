extern crate std;

use super::*;
use alloc::format;
use tairix_proglib::{BundlePath, EntryId};

fn entry(id: &str) -> PinTarget {
    PinTarget::Entry(EntryId::new(id).unwrap())
}

fn bundle(path: &str) -> PinTarget {
    PinTarget::Bundle(BundlePath::new(path).unwrap())
}

#[test]
fn new_list_is_empty() {
    let list = PinList::new();
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
}

#[test]
fn pinning_appends_to_the_end() {
    let mut list = PinList::new();
    let e1 = entry("com.example.app1");
    let b1 = bundle("/Apps/App1.app");

    assert_eq!(list.pin(e1.clone()).unwrap(), 0);
    assert_eq!(list.pin(b1.clone()).unwrap(), 1);

    assert_eq!(list.len(), 2);
    assert_eq!(list.get(0), Some(&e1));
    assert_eq!(list.get(1), Some(&b1));
}

#[test]
fn pinning_a_duplicate_is_refused() {
    let mut list = PinList::new();
    let e1 = entry("com.example.app1");

    list.pin(e1.clone()).unwrap();
    assert_eq!(list.pin(e1), Err(PinError::AlreadyPinned));
}

#[test]
fn pinning_at_index_inserts_or_appends() {
    let mut list = PinList::new();
    let e1 = entry("e1");
    let e2 = entry("e2");
    let e3 = entry("e3");

    list.pin(e1.clone()).unwrap(); // [e1]
    list.pin_at(0, e2.clone()).unwrap(); // [e2, e1]
    list.pin_at(5, e3.clone()).unwrap(); // [e2, e1, e3] (clamped)

    let pins: Vec<_> = list.iter().cloned().collect();
    assert_eq!(pins, std::vec![e2, e1, e3]);
}

#[test]
fn pin_at_a_duplicate_is_refused() {
    let mut list = PinList::new();
    let e1 = entry("e1");
    let e2 = entry("e2");

    list.pin(e1.clone()).unwrap();
    assert_eq!(list.pin_at(0, e1), Err(PinError::AlreadyPinned));
    assert_eq!(list.pin_at(5, e2), Ok(()));
}

#[test]
fn unpinning_removes_by_index() {
    let mut list = PinList::new();
    let e1 = entry("e1");
    let e2 = entry("e2");

    list.pin(e1.clone()).unwrap();
    list.pin(e2.clone()).unwrap();

    assert_eq!(list.unpin(0), Some(e1));
    assert_eq!(list.len(), 1);
    assert_eq!(list.get(0), Some(&e2));

    assert_eq!(list.unpin(5), None); // Out of range fails closed
}

#[test]
fn move_pin_follows_remove_then_insert_model() {
    let mut list = PinList::new();
    let e1 = entry("e1");
    let e2 = entry("e2");
    let e3 = entry("e3");

    list.pin(e1.clone()).unwrap();
    list.pin(e2.clone()).unwrap();
    list.pin(e3.clone()).unwrap();
    // [e1, e2, e3]

    // Move e1 (0) to end (clamped to 2 after removal)
    assert!(list.move_pin(0, 5));
    let pins: Vec<_> = list.iter().cloned().collect();
    assert_eq!(pins, std::vec![e2.clone(), e3.clone(), e1.clone()]);

    // Move e1 (2) to middle (1)
    assert!(list.move_pin(2, 1));
    let pins: Vec<_> = list.iter().cloned().collect();
    assert_eq!(pins, std::vec![e2, e1, e3]);

    // Move from out of range is refused
    assert!(!list.move_pin(10, 0));
}

#[test]
fn move_pin_with_same_indices_is_no_op() {
    let mut list = PinList::new();
    let e1 = entry("e1");
    list.pin(e1.clone()).unwrap();

    assert!(list.move_pin(0, 0));
    assert_eq!(list.get(0), Some(&e1));
}

#[test]
fn list_is_capped_at_max_pins() {
    let mut list = PinList::new();
    for i in 0..MAX_PINS {
        let id = format!("com.example.app{i}");
        list.pin(entry(&id)).unwrap();
    }
    assert_eq!(list.len(), MAX_PINS);
    assert_eq!(list.pin(entry("too.many")), Err(PinError::Full));
    assert_eq!(list.pin_at(0, entry("still.too.many")), Err(PinError::Full));
}

#[test]
fn position_finds_the_index() {
    let mut list = PinList::new();
    let e1 = entry("e1");
    let e2 = entry("e2");

    list.pin(e1.clone()).unwrap();
    list.pin(e2.clone()).unwrap();

    assert_eq!(list.position(&e1), Some(0));
    assert_eq!(list.position(&e2), Some(1));
    assert_eq!(list.position(&entry("none")), None);
}

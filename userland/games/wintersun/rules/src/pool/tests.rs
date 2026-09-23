use super::Pool;

#[test]
fn a_new_pool_is_clamped_to_its_maximum() {
    assert_eq!(Pool::new(500, 100).current(), 100);
    assert_eq!(Pool::new(50, 100).current(), 50);
    assert_eq!(Pool::full(100).current(), 100);
    assert!(Pool::new(0, 100).is_empty());
}

#[test]
fn spending_is_all_or_nothing() {
    let mut pool = Pool::full(100);
    assert!(!pool.spend(101), "a cost it cannot meet is not part-paid");
    assert_eq!(pool.current(), 100);
    assert!(pool.spend(100));
    assert_eq!(pool.current(), 0);
    assert!(!pool.spend(1));
}

#[test]
fn draining_and_restoring_saturate_at_both_ends() {
    let mut pool = Pool::full(100);
    assert_eq!(pool.drain(250), 100, "it reports what it could give");
    assert_eq!(pool.current(), 0);
    assert_eq!(pool.drain(1), 0);
    assert_eq!(pool.restore(250), 100, "it reports what fitted");
    assert_eq!(pool.current(), 100);
    assert_eq!(pool.restore(1), 0);
}

#[test]
fn shrinking_the_maximum_spills_the_difference() {
    let mut pool = Pool::full(100);
    pool.set_max(40);
    assert_eq!(pool.current(), 40);
    pool.set_max(200);
    assert_eq!(pool.current(), 40, "growing the bound does not refill");
    assert_eq!(pool.max(), 200);
}

#[test]
fn a_signed_delta_heals_and_harms_by_its_sign() {
    let mut pool = Pool::new(50, 100);
    assert_eq!(pool.apply_delta(20), 20);
    assert_eq!(pool.current(), 70);
    assert_eq!(pool.apply_delta(-30), 30);
    assert_eq!(pool.current(), 40);
    assert_eq!(pool.apply_delta(0), 0);
    assert_eq!(pool.current(), 40);
}

#[test]
fn an_extreme_delta_cannot_wrap_the_pool() {
    let mut pool = Pool::new(1, u32::MAX);
    assert_eq!(pool.apply_delta(i64::MIN), 1, "it had only one to give");
    assert!(pool.is_empty());
    pool.apply_delta(i64::MAX);
    assert_eq!(pool.current(), u32::MAX);
}

#[test]
fn no_sequence_of_operations_leaves_the_pool_outside_its_bound() {
    // A deterministic walk over every operation shape, asserting the
    // invariant after each. The proptest model drives the same invariant
    // over generated sequences; this keeps a fast version of it beside the
    // type.
    let mut pool = Pool::full(1000);
    let mut rng = tairix_fuzzseed::Prng::new(0x9E37_79B9);
    for _ in 0..20_000 {
        let amount = u32::from(rng.next_u16());
        match rng.below(8) {
            0 => {
                pool.spend(amount);
            }
            1 => {
                pool.drain(amount);
            }
            2 => {
                pool.restore(amount);
            }
            3 => pool.set_max(amount),
            4 => {
                pool.apply_delta(i64::from(amount));
            }
            5 => {
                pool.apply_delta(-i64::from(amount));
            }
            6 => pool.set_max(0),
            _ => {
                pool.restore(u32::MAX);
            }
        }
        assert!(
            pool.current() <= pool.max(),
            "current {} exceeded max {}",
            pool.current(),
            pool.max()
        );
    }
}

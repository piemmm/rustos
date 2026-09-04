use super::JobDesk;

/// A desk the loop has not touched owes nobody anything.
#[test]
fn a_fresh_desk_has_no_work_and_no_answer() {
    let mut desk = JobDesk::<u32, u32>::new();
    assert!(!desk.has_work());
    assert!(!desk.in_flight());
    assert_eq!(desk.next_job(), None);
    assert_eq!(desk.collect(), None);
}

/// The first submission is takeable, so the worker is worth waking.
#[test]
fn submitting_asks_for_a_worker_and_hands_the_job_over() {
    let mut desk = JobDesk::<u32, u32>::new();
    assert!(desk.submit(7).wake);
    assert!(desk.has_work());
    assert_eq!(desk.next_job(), Some(7));
    assert!(desk.in_flight());
    assert!(!desk.has_work());
}

/// Two settles before any worker looks cost one job, not two: the second
/// submission replaces the first rather than queueing behind it.
#[test]
fn submissions_before_the_job_is_taken_coalesce_to_the_latest() {
    let mut desk = JobDesk::<u32, u32>::new();
    assert_eq!(desk.submit(1).displaced, None);
    assert_eq!(desk.submit(2).displaced, Some(1));
    assert_eq!(desk.submit(3).displaced, Some(2));
    assert_eq!(desk.next_job(), Some(3));
    assert_eq!(desk.next_job(), None);
}

/// A submission during a write is held, not dropped, and needs no wake —
/// the worker looks again the moment it has delivered.
#[test]
fn a_submission_while_in_flight_is_held_without_a_wake() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    assert_eq!(desk.next_job(), Some(1));
    assert!(!desk.submit(2).wake);
    assert_eq!(desk.next_job(), None);
    assert!(!desk.deliver(10));
    assert_eq!(desk.next_job(), Some(2));
}

/// Only one job is out at a time, so two workers cannot write concurrently.
#[test]
fn only_one_job_is_in_flight_at_a_time() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    assert_eq!(desk.next_job(), Some(1));
    let _ = desk.submit(2);
    assert_eq!(desk.next_job(), None);
}

/// The answer to a superseded job is dropped: adopting it would show a state
/// the queued job is about to replace.
#[test]
fn a_superseded_answer_is_dropped_rather_than_delivered() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    desk.next_job();
    let _ = desk.submit(2);
    assert!(!desk.deliver(10));
    assert_eq!(desk.collect(), None);
    assert_eq!(desk.next_job(), Some(2));
    assert!(desk.deliver(20));
    assert_eq!(desk.collect(), Some(20));
}

/// An answer nobody superseded is delivered and collected exactly once.
#[test]
fn an_answer_is_collected_once() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    desk.next_job();
    assert!(desk.deliver(99));
    assert_eq!(desk.collect(), Some(99));
    assert_eq!(desk.collect(), None);
}

/// Delivering frees the desk for the next job even when nothing is waiting.
#[test]
fn delivering_clears_the_in_flight_marker() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    desk.next_job();
    assert!(desk.in_flight());
    desk.deliver(1);
    assert!(!desk.in_flight());
    assert!(desk.submit(2).wake);
}

/// A stopping desk hands out nothing and accepts nothing, so a parked worker
/// leaves rather than finding fresh work on the way out.
#[test]
fn stopping_refuses_submissions_and_hands_out_no_work() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    desk.stop();
    assert!(desk.stopping());
    assert!(!desk.has_work());
    assert_eq!(desk.next_job(), None);
    let refused = desk.submit(2);
    assert!(!refused.wake);
    assert_eq!(
        refused.displaced,
        Some(2),
        "a stopping desk hands the request straight back"
    );
    assert_eq!(desk.next_job(), None);
}

/// A worker mid-write still delivers after a stop, so a published document is
/// never left half-written and its outcome is still reportable.
#[test]
fn a_job_in_flight_when_stopping_can_still_be_delivered() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    assert_eq!(desk.next_job(), Some(1));
    desk.stop();
    assert!(desk.deliver(5));
    assert_eq!(desk.collect(), Some(5));
}

/// The displaced request is handed back so a caller waiting on it can be told
/// it was superseded, rather than parked for an answer nobody will produce.
#[test]
fn a_displaced_request_is_handed_back_to_the_submitter() {
    let mut desk = JobDesk::<u32, u32>::new();
    assert_eq!(desk.submit(1).displaced, None);
    let second = desk.submit(2);
    assert_eq!(second.displaced, Some(1));
    assert!(
        second.wake,
        "nothing has taken a job, so one is still wanted"
    );
    assert_eq!(desk.next_job(), Some(2));
}

/// A job already taken is not displaceable — it is being written — so a
/// submission during one displaces nothing.
#[test]
fn a_job_in_flight_is_never_displaced() {
    let mut desk = JobDesk::<u32, u32>::new();
    let _ = desk.submit(1);
    assert_eq!(desk.next_job(), Some(1));
    assert_eq!(desk.submit(2).displaced, None);
}

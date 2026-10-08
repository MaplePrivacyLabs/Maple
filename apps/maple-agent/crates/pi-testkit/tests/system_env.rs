use std::time::Duration;

use pi_ai::env::{CancellationToken, PiEnv, SleepCancelled, SystemEnv};

#[tokio::test(flavor = "current_thread")]
async fn a_settled_timer_cannot_be_overridden_by_later_cancellation() {
    let env = SystemEnv::new();
    let signal = CancellationToken::new();
    let timer = env.sleep(1.0, Some(&signal));
    tokio::time::sleep(Duration::from_millis(20)).await;
    signal.cancel();
    assert_eq!(timer.await, Ok(()));
}

#[tokio::test(flavor = "current_thread")]
async fn early_cancellation_interrupts_a_system_timer() {
    let env = SystemEnv::new();
    let signal = CancellationToken::new();
    let timer = env.sleep(60_000.0, Some(&signal));
    signal.cancel();
    assert_eq!(timer.await, Err(SleepCancelled));
}

#[test]
fn system_randomness_obeys_number_and_uuid_shapes() {
    let env = SystemEnv::new();
    assert!((0.0..1.0).contains(&env.math_random()));
    let uuid = env.random_uuid().unwrap();
    let groups: Vec<_> = uuid.split('-').map(str::len).collect();
    assert_eq!(groups, [8, 4, 4, 4, 12]);
    assert_eq!(&uuid[14..15], "4");
    assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
}

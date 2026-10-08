use std::sync::{Arc, Mutex};

use futures_util::poll;
use pi_ai::env::{CancellationToken, PiEnv, SleepCancelled};
use pi_testkit::VirtualEnv;

#[test]
fn recorder_random_sources_share_the_same_xorshift_draw_order() {
    let env = VirtualEnv::new(1_700_000_000_000);
    assert_eq!(
        env.math_random(),
        f64::from(0x8798_5aa5_u32) / 4_294_967_296.0
    );
    let mut bytes = [0; 3];
    env.fill_random(&mut bytes).unwrap();
    assert_eq!(bytes, [0xa3, 0xc4, 0x98]);
    assert_eq!(
        env.math_random(),
        f64::from(0x703a_0788_u32) / 4_294_967_296.0
    );
}

#[test]
fn scripted_sources_are_independent_and_do_not_consume_fallbacks() {
    let env = VirtualEnv::new(123);
    env.push_uuid("collision");
    env.push_uuid("collision");
    env.push_random_number(0.5);
    env.push_random_bytes([9, 8]);
    assert_eq!(env.random_uuid().unwrap(), "collision");
    assert_eq!(env.random_uuid().unwrap(), "collision");
    assert_eq!(
        env.random_uuid().unwrap(),
        "00000001-0000-4000-8000-000000000000"
    );
    assert_eq!(
        env.random_uuid().unwrap(),
        "00000002-0000-4000-8000-000000000000"
    );
    assert_eq!(env.math_random(), 0.5);
    let mut bytes = [0; 3];
    env.fill_random(&mut bytes).unwrap();
    assert_eq!(bytes, [9, 8, 0xa5]);
    assert_eq!(
        env.math_random(),
        f64::from(0x155b_24a3_u32) / 4_294_967_296.0
    );
}

#[test]
fn independent_environments_start_with_identical_sequences() {
    let left = VirtualEnv::new(100);
    let right = VirtualEnv::new(200);
    assert_eq!(left.math_random(), right.math_random());
    assert_eq!(left.random_uuid().unwrap(), right.random_uuid().unwrap());
    left.set_now(-10);
    assert_eq!(left.now_ms(), -10);
    assert_eq!(right.now_ms(), 200);
    assert_eq!(
        left.clone().random_uuid().unwrap(),
        "00000002-0000-4000-8000-000000000000"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn time_never_advances_until_explicitly_driven() {
    let env = VirtualEnv::new(1_000);
    let mut timer = env.sleep(10.0, None);
    assert_eq!(env.pending_timers(), 1);
    for _ in 0..8 {
        tokio::task::yield_now().await;
        assert!(poll!(timer.as_mut()).is_pending());
    }
    assert_eq!(env.now_ms(), 1_000);
    assert_eq!(env.monotonic_ms(), 0);
    env.advance(9).await;
    assert!(poll!(timer.as_mut()).is_pending());
    env.advance(1).await;
    assert_eq!(timer.await, Ok(()));
    assert_eq!(env.now_ms(), 1_010);
    assert_eq!(env.pending_timers(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn set_system_time_does_not_change_an_existing_deadline() {
    let env = VirtualEnv::new(1_000);
    let mut timer = env.sleep(10.0, None);
    env.advance(4).await;
    env.set_now(-500);
    env.advance(5).await;
    assert!(poll!(timer.as_mut()).is_pending());
    env.advance(1).await;
    assert_eq!(timer.await, Ok(()));
    assert_eq!(env.now_ms(), -494);
    assert_eq!(env.monotonic_ms(), 10);
}

#[tokio::test(flavor = "current_thread")]
async fn timers_complete_in_deadline_then_registration_order() {
    let env = VirtualEnv::new(100);
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for (label, delay) in [("last", 20.0), ("first", 5.0), ("second", 5.0)] {
        let env = env.clone();
        let events = Arc::clone(&events);
        tasks.push(tokio::spawn(async move {
            env.sleep(delay, None).await.unwrap();
            events.lock().unwrap().push((label, env.now_ms()));
        }));
    }
    tokio::task::yield_now().await;
    assert_eq!(env.pending_timers(), 3);
    env.advance(30).await;
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(
        *events.lock().unwrap(),
        [("first", 105), ("second", 105), ("last", 120)]
    );
    assert_eq!(env.now_ms(), 130);
}

#[tokio::test(flavor = "current_thread")]
async fn chained_timers_are_registered_before_advancing_past_their_deadline() {
    let env = VirtualEnv::new(100);
    let worker_env = env.clone();
    let worker = tokio::spawn(async move {
        worker_env.sleep(5.0, None).await.unwrap();
        let first = worker_env.now_ms();
        worker_env.sleep(7.0, None).await.unwrap();
        (first, worker_env.now_ms())
    });
    tokio::task::yield_now().await;
    env.advance(20).await;
    assert_eq!(worker.await.unwrap(), (105, 112));
    assert_eq!(env.now_ms(), 120);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_drop_release_timer_registrations() {
    let env = VirtualEnv::default();
    let signal = CancellationToken::new();
    let timer = env.sleep(5.0, Some(&signal));
    assert_eq!(env.pending_timers(), 1);
    signal.cancel();
    assert_eq!(env.pending_timers(), 0);
    assert_eq!(timer.await, Err(SleepCancelled));
    assert_eq!(env.pending_timers(), 0);

    let timer = env.sleep(5.0, None);
    drop(timer);
    assert_eq!(env.pending_timers(), 0);
    assert_eq!(env.sleep(5.0, Some(&signal)).await, Err(SleepCancelled));
    assert_eq!(env.pending_timers(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_firing_and_after_firing_preserve_settlement() {
    let env = VirtualEnv::default();
    let signal = CancellationToken::new();
    let timer = env.sleep(5.0, Some(&signal));
    signal.cancel();
    env.advance(5).await;
    assert_eq!(timer.await, Err(SleepCancelled));

    let signal = CancellationToken::new();
    let timer = env.sleep(5.0, Some(&signal));
    env.advance(5).await;
    signal.cancel();
    assert_eq!(timer.await, Ok(()));
}

#[tokio::test(flavor = "current_thread")]
async fn an_unpolled_cancellation_settles_before_advancing_time() {
    let env = VirtualEnv::new(100);
    let worker_env = env.clone();
    let signal = CancellationToken::new();
    let worker_signal = signal.clone();
    let worker = tokio::spawn(async move {
        let result = worker_env.sleep(5.0, Some(&worker_signal)).await;
        (result, worker_env.now_ms())
    });
    tokio::task::yield_now().await;
    signal.cancel();
    env.advance(10).await;
    assert_eq!(worker.await.unwrap(), (Err(SleepCancelled), 100));
    assert_eq!(env.now_ms(), 110);
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_gates_settle_external_continuations_before_advancing_again() {
    let env = VirtualEnv::new(100);
    let worker_env = env.clone();
    let (registered, registration) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        worker_env.sleep(5.0, None).await.unwrap();
        tokio::task::yield_now().await;
        let second = worker_env.sleep(7.0, None);
        registered.send(()).unwrap();
        second.await.unwrap();
        worker_env.now_ms()
    });
    tokio::task::yield_now().await;
    env.advance(5).await;
    // The clock owns no application executor: wait for the explicit gate,
    // rather than assuming advance drained the worker's external yield.
    registration.await.unwrap();
    env.advance(7).await;
    assert_eq!(worker.await.unwrap(), 112);
}

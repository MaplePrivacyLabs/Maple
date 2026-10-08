use pi_ai::env::{CancellationToken, PiEnv};
use pi_testkit::env::VirtualEnv;

#[tokio::test(flavor = "current_thread")]
async fn next_timer_delay_observes_deadlines_without_advancing() {
    let env = VirtualEnv::new(100);
    assert_eq!(env.next_timer_delay_ms(), None);
    let late = env.sleep(20.0, None);
    let first = env.sleep(5.0, None);
    assert_eq!(env.next_timer_delay_ms(), Some(5));
    assert_eq!(env.now_ms(), 100);
    env.advance(5).await;
    assert!(first.await.is_ok());
    assert_eq!(env.next_timer_delay_ms(), Some(15));
    env.set_now(900);
    assert_eq!(env.next_timer_delay_ms(), Some(15));
    env.advance(15).await;
    assert!(late.await.is_ok());
    assert_eq!(env.next_timer_delay_ms(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn next_timer_delay_ignores_cancelled_and_dropped_timers() {
    let env = VirtualEnv::new(100);
    let signal = CancellationToken::new();
    let cancelled = env.sleep(0.0, Some(&signal));
    let active = env.sleep(7.0, None);
    assert_eq!(env.next_timer_delay_ms(), Some(0));
    signal.cancel();
    assert_eq!(env.next_timer_delay_ms(), Some(7));
    drop(active);
    assert_eq!(env.next_timer_delay_ms(), None);
    assert!(cancelled.await.is_err());
}

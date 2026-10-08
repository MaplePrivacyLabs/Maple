use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::rc::Rc;
use std::task::Poll;

use futures_util::poll;
use pi_ai::env::{CancellationToken, PiEnv, SleepCancelled};
use pi_testkit::{LocalTaskSet, VirtualEnv};

#[tokio::test(flavor = "current_thread")]
async fn wake_driven_checkpoints_finish_arbitrary_finite_yield_chains() {
    for yields in [0, 1, 3, 17, 129] {
        let env = VirtualEnv::new(100);
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut tasks = LocalTaskSet::new();
        let worker_env = env.clone();
        let worker_events = Rc::clone(&events);
        tasks.spawn(async move {
            worker_env.sleep(5.0, None).await.unwrap();
            worker_events.borrow_mut().push(worker_env.now_ms());
            for _ in 0..yields {
                tokio::task::yield_now().await;
            }
            worker_env.sleep(7.0, None).await.unwrap();
            worker_events.borrow_mut().push(worker_env.now_ms());
        });

        env.advance_with(20, &mut tasks).await;
        assert_eq!(*events.borrow(), [105, 112], "yield count {yields}");
        assert_eq!(env.now_ms(), 120);
        assert_eq!(env.pending_timers(), 0);
        assert_eq!(tasks.pending_tasks(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn checkpoints_do_not_wait_for_unready_timers_or_channels() {
    let env = VirtualEnv::default();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut tasks = LocalTaskSet::new();
    for delay in [30.0, 5.0] {
        let env = env.clone();
        let events = Rc::clone(&events);
        tasks.spawn(async move {
            env.sleep(delay, None).await.unwrap();
            events.borrow_mut().push(env.now_ms());
        });
    }
    let (send, receive) = tokio::sync::oneshot::channel();
    let channel_received = Rc::new(Cell::new(false));
    let received = Rc::clone(&channel_received);
    tasks.spawn(async move {
        receive.await.unwrap();
        received.set(true);
    });

    env.advance_with(20, &mut tasks).await;
    assert_eq!(*events.borrow(), [5]);
    assert_eq!(env.now_ms(), 20);
    assert_eq!(env.pending_timers(), 1);
    assert_eq!(tasks.pending_tasks(), 2);
    assert!(!channel_received.get());

    send.send(()).unwrap();
    tasks.checkpoint().await;
    assert!(channel_received.get());
    assert_eq!(env.now_ms(), 20);
    env.advance_with(10, &mut tasks).await;
    assert_eq!(*events.borrow(), [5, 30]);
    assert_eq!(tasks.pending_tasks(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn equal_deadlines_keep_registration_order_across_yielding_callbacks() {
    let env = VirtualEnv::default();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut tasks = LocalTaskSet::new();
    for label in ["first", "second", "third"] {
        let env = env.clone();
        let events = Rc::clone(&events);
        tasks.spawn(async move {
            env.sleep(5.0, None).await.unwrap();
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            events.borrow_mut().push((label, env.now_ms()));
        });
    }
    env.advance_with(10, &mut tasks).await;
    assert_eq!(
        *events.borrow(),
        [("first", 5), ("second", 5), ("third", 5)]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn timers_truncated_to_zero_inside_a_tick_fire_one_millisecond_later() {
    for nested_delay in [0.0, 0.5] {
        let env = VirtualEnv::default();
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut tasks = LocalTaskSet::new();
        let worker_env = env.clone();
        let worker_events = Rc::clone(&events);
        tasks.spawn(async move {
            worker_env.sleep(0.0, None).await.unwrap();
            worker_events.borrow_mut().push(worker_env.now_ms());
            worker_env.sleep(nested_delay, None).await.unwrap();
            worker_events.borrow_mut().push(worker_env.now_ms());
        });
        env.advance_with(0, &mut tasks).await;
        assert_eq!(*events.borrow(), [0]);
        assert_eq!(env.pending_timers(), 1);
        env.advance_with(1, &mut tasks).await;
        assert_eq!(*events.borrow(), [0, 1]);
        assert_eq!(tasks.pending_tasks(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_pending_future_is_only_repolled_after_its_waker_fires() {
    let polls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&polls);
    let mut tasks = LocalTaskSet::new();
    tasks.spawn(poll_fn(move |_| {
        observed.set(observed.get() + 1);
        Poll::<()>::Pending
    }));
    tasks.checkpoint().await;
    tasks.checkpoint().await;
    assert_eq!(polls.get(), 1);
    assert_eq!(tasks.pending_tasks(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_spurious_parent_poll_cannot_acknowledge_the_deferred_checkpoint() {
    let completed = Rc::new(Cell::new(false));
    let observed = Rc::clone(&completed);
    let mut tasks = LocalTaskSet::new();
    tasks.spawn(async move {
        tokio::task::yield_now().await;
        observed.set(true);
    });
    let mut checkpoint = Box::pin(tasks.checkpoint());
    assert!(poll!(checkpoint.as_mut()).is_pending());
    // No return to the runtime occurred between these polls. A plain
    // YieldNow future would become Ready here without flushing child wakes.
    assert!(poll!(checkpoint.as_mut()).is_pending());
    assert!(!completed.get());
    checkpoint.await;
    assert!(completed.get());
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_owned_tasks_releases_their_pending_timers() {
    let env = VirtualEnv::default();
    let mut tasks = LocalTaskSet::new();
    let worker_env = env.clone();
    tasks.spawn(async move {
        worker_env.sleep(30.0, None).await.unwrap();
    });
    tasks.checkpoint().await;
    assert_eq!(env.pending_timers(), 1);
    drop(tasks);
    assert_eq!(env.pending_timers(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_timer_continuations_observe_time_before_the_advance() {
    let env = VirtualEnv::new(100);
    let signal = CancellationToken::new();
    let observed = Rc::new(Cell::new(None));
    let mut tasks = LocalTaskSet::new();
    let worker_env = env.clone();
    let worker_signal = signal.clone();
    let worker_observed = Rc::clone(&observed);
    tasks.spawn(async move {
        let result = worker_env.sleep(5.0, Some(&worker_signal)).await;
        worker_observed.set(Some((result, worker_env.now_ms())));
    });
    tasks.checkpoint().await;
    signal.cancel();
    env.advance_with(10, &mut tasks).await;
    assert_eq!(observed.get(), Some((Err(SleepCancelled), 100)));
    assert_eq!(env.now_ms(), 110);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_an_advance_restores_zero_delay_registration_outside_a_tick() {
    let env = VirtualEnv::default();
    let mut tasks = LocalTaskSet::new();
    let worker_env = env.clone();
    tasks.spawn(async move {
        worker_env.sleep(5.0, None).await.unwrap();
        tokio::task::yield_now().await;
    });
    {
        let mut advance = Box::pin(env.advance_with(10, &mut tasks));
        poll_fn(|context| {
            assert!(advance.as_mut().poll(context).is_pending());
            if env.monotonic_ms() == 5 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    let timer = env.sleep(0.0, None);
    env.advance_with(0, &mut tasks).await;
    assert_eq!(timer.await, Ok(()));
    assert_eq!(env.now_ms(), 5);
}

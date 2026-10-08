//! Explicitly driven callback futures for virtual-time tests.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
struct TaskWake {
    ready: AtomicBool,
}

impl Wake for TaskWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.ready.store(true, Ordering::Release);
    }
}

struct Task<'a> {
    future: Pin<Box<dyn Future<Output = ()> + 'a>>,
    wake: Arc<TaskWake>,
}

/// Callback futures owned by a virtual-time test, in source registration order.
///
/// `spawn` queues a future without detaching it onto Tokio. `checkpoint` drives
/// only these futures until none is ready, including wakes deferred by Tokio's
/// `yield_now`. A future waiting for a timer or an external channel remains
/// pending. This is not a quiescence check for arbitrary Tokio tasks or I/O.
///
/// Checkpoints require the pinned Tokio current-thread runtime: its deferred
/// wake list is drained before the driver can resume. Dropping this set drops
/// every pending future, releasing any virtual timer registrations it owns.
#[derive(Default)]
pub struct LocalTaskSet<'a> {
    tasks: Vec<Task<'a>>,
}

impl<'a> LocalTaskSet<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(&mut self, future: impl Future<Output = ()> + 'a) {
        self.tasks.push(Task {
            future: Box::pin(future),
            wake: Arc::new(TaskWake {
                ready: AtomicBool::new(true),
            }),
        });
    }

    pub fn pending_tasks(&self) -> usize {
        self.tasks.len()
    }

    /// Finish ready continuations without advancing virtual time. Repeated
    /// wake-driven rounds handle any finite chain of yielding continuations;
    /// there is no fixed scheduler-yield count.
    pub async fn checkpoint(&mut self) {
        assert_eq!(
            tokio::runtime::Handle::current().runtime_flavor(),
            tokio::runtime::RuntimeFlavor::CurrentThread,
            "virtual task checkpoints require a current-thread Tokio runtime"
        );
        loop {
            self.poll_ready();
            deferred_wake_checkpoint().await;
            if !self.has_ready_tasks() {
                return;
            }
        }
    }

    fn has_ready_tasks(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| task.wake.ready.load(Ordering::Acquire))
    }

    fn poll_ready(&mut self) {
        while self.has_ready_tasks() {
            self.tasks.retain_mut(|task| {
                if !task.wake.ready.swap(false, Ordering::AcqRel) {
                    return true;
                }
                let waker = Waker::from(Arc::clone(&task.wake));
                task.future
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            });
        }
    }
}

#[derive(Default)]
struct CheckpointWake {
    acknowledged: AtomicBool,
    waiter: Mutex<Option<Waker>>,
}

impl Wake for CheckpointWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.acknowledged.store(true, Ordering::Release);
        let waiter = self.waiter.lock().unwrap().clone();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

async fn deferred_wake_checkpoint() {
    let marker = Arc::new(CheckpointWake::default());
    let waker = Waker::from(Arc::clone(&marker));
    let mut defer = std::pin::pin!(tokio::task::yield_now());
    let mut registered = false;
    poll_fn(|context| {
        *marker.waiter.lock().unwrap() = Some(context.waker().clone());
        if !registered {
            // Tokio 1.53.1 stores this waker in its deferred list. Poll only
            // once: an unrelated wake of our caller must not finish YieldNow
            // before the scheduler actually acknowledges the marker.
            let result = defer.as_mut().poll(&mut Context::from_waker(&waker));
            assert!(
                result.is_pending(),
                "initial Tokio yield must defer its wake"
            );
            registered = true;
        }
        if marker.acknowledged.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

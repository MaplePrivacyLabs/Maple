//! Replay the recorder's virtual timer contract against the actual test clock.

use crate::CheckResult;
use pi_ai::env::PiEnv;
use pi_testkit::{LocalTaskSet, VirtualEnv};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    operations: Vec<Operation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
enum Operation {
    Schedule(Timer),
    Advance(u64),
    SetNow(i64),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Timer {
    id: String,
    delay_ms: f64,
    #[serde(default)]
    microtasks_before_next: usize,
    next: Option<Box<Timer>>,
}

async fn schedule(timer: Timer, env: VirtualEnv, events: Arc<Mutex<Vec<Value>>>) {
    let mut timer = timer;
    loop {
        env.sleep(timer.delay_ms, None)
            .await
            .expect("unsignalled timer cannot abort");
        events
            .lock()
            .unwrap()
            .push(json!({"id": timer.id, "nowMs": env.now_ms()}));
        for _ in 0..timer.microtasks_before_next {
            tokio::task::yield_now().await;
        }
        let Some(next) = timer.next else {
            break;
        };
        timer = *next;
    }
}

pub async fn replay(epoch_ms: u64, input: &Value) -> CheckResult<Value> {
    let input: Input = serde_json::from_value(input.clone())
        .map_err(|error| format!("invalid timer input: {error}"))?;
    let env = VirtualEnv::new(i64::try_from(epoch_ms).map_err(|_| "timer epoch exceeds i64")?);
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = LocalTaskSet::new();
    for operation in input.operations {
        match operation {
            Operation::Schedule(timer) => {
                tasks.spawn(schedule(timer, env.clone(), events.clone()));
            }
            Operation::SetNow(now) => env.set_now(now),
            Operation::Advance(ms) => {
                env.advance_with(ms, &mut tasks).await;
            }
        }
        // Pi registers a setTimeout immediately, before the next DSL step.
        tasks.checkpoint().await;
    }
    let events = events.lock().unwrap().clone();
    Ok(json!({"events": events, "nowMs": env.now_ms(), "pendingTimers": env.pending_timers()}))
}

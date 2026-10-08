//! Canonical replay for selected session and compaction functions.
use crate::{CheckResult, compaction_functions, session_functions};
use pi_ai::utils::{js_json, js_value::JsValue, json_parse, uuid::UuidV7Generator};
use pi_coding_agent::config::HostConfig;
use pi_testkit::{LocalTaskSet, VirtualEnv};
use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc};

pub fn supports(id: &str) -> bool {
    session_functions::FUNCTION_IDS.contains(&id)
        || compaction_functions::FUNCTION_IDS.contains(&id)
}

async fn evaluate(id: &str, input: &JsValue, epoch_ms: i64) -> CheckResult<JsValue> {
    let env = Arc::new(VirtualEnv::new(epoch_ms));
    if session_functions::FUNCTION_IDS.contains(&id) {
        return session_functions::dispatch(
            id,
            input,
            env,
            Arc::new(HostConfig::new(
                "pi",
                ".pi",
                "/unused",
                "/workspace/project",
            )),
        );
    }
    if id != crate::retry_request_ownership::FUNCTION_ID {
        return compaction_functions::dispatch(id, input, env, Arc::new(UuidV7Generator::new()))
            .await;
    }
    // This declared fixture has one zero-delay retry. Own the operation future,
    // drain its continuations and then advance each registered virtual timer.
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    let clock = env.clone();
    let mut tasks = LocalTaskSet::new();
    tasks.spawn(async move {
        *output.borrow_mut() = Some(
            compaction_functions::dispatch(id, input, clock, Arc::new(UuidV7Generator::new()))
                .await,
        );
    });
    tasks.checkpoint().await;
    while tasks.pending_tasks() != 0 {
        let delay = env
            .next_timer_delay_ms()
            .ok_or("summary fixture stalled without a registered virtual timer")?;
        env.advance_with(delay, &mut tasks).await;
    }
    result
        .borrow_mut()
        .take()
        .ok_or("summary fixture completed without an observation")?
}

pub async fn replay(root: &Path, id: &str) -> CheckResult {
    let matrix: crate::replay::FunctionMatrix = pi_ai::utils::js_value::from_js_value(
        crate::replay::read_js_json(&root.join("scenarios/functions").join(format!("{id}.json")))?,
    )
    .map_err(|error| error.to_string())?;
    let epoch = i64::try_from(matrix.clock.epoch_ms).map_err(|_| "matrix clock exceeds i64")?;
    let ownership = id == crate::retry_request_ownership::FUNCTION_ID;
    if ownership && !crate::selection::permits_owned_summary_requests(root)? {
        return Err(
            "summary request ownership fixture lacks its narrow owner authorization".into(),
        );
    }
    let goldens =
        crate::functions::read_goldens(&root.join("corpus/functions").join(format!("{id}.jsonl")))?;
    if goldens.len() != matrix.cases.len() {
        return Err("matrix and recording case counts differ".into());
    }
    let mut failures = Vec::new();
    for (case, golden) in matrix.cases.iter().zip(goldens) {
        if case.case != golden.case || case.input != golden.input {
            return Err(format!(
                "{id}: recording changed the input/order for {}",
                case.case
            ));
        }
        let expected = golden
            .result
            .map_err(|error| format!("{id}/{}: unexpected outer error {error:?}", case.case))?;
        let actual = evaluate(id, &case.input, epoch)
            .await
            .map_err(|error| format!("{id}/{}: harness failure: {error}", case.case))?;
        // Exactly the recorder's JSON snapshot: no ID, timestamp or field erasure.
        let actual = json_parse::parse_json(&js_json::stringify(&actual))
            .map_err(|error| error.to_string())?;
        let compared = if ownership {
            crate::retry_request_ownership::compare(
                &case.input,
                &expected,
                &actual,
                crate::compare::compare_unmodified,
            )
        } else {
            crate::compare::compare_unmodified(&expected, &actual)
        };
        if let Err(error) = compared {
            failures.push(format!("{id}/{}: {error}", case.case));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

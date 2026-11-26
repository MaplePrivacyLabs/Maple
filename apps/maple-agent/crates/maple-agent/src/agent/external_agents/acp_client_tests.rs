//! The ACP client's reading of what agents send, on the shapes Grok Build
//! 1.0.92 and Antigravity 1.5.17 sent in recorded runs (paths shortened).

use super::*;

fn update(state: &mut Session, update: Value) -> Vec<AgentEvent> {
    events(decode(
        "grok",
        state,
        ServerMessage::Notification {
            method: "session/update".into(),
            params: json!({ "sessionId": "s", "update": update }),
        },
    ))
}

fn events(messages: Vec<DriverMessage>) -> Vec<AgentEvent> {
    messages
        .into_iter()
        .map(|message| match message {
            DriverMessage::Event(event) => event,
            DriverMessage::Request { request, .. } => panic!("unexpected request {request:?}"),
        })
        .collect()
}

fn ask(state: &mut Session, method: &str, params: Value) -> (Vec<AgentEvent>, AgentRequest) {
    let mut messages = decode(
        "grok",
        state,
        ServerMessage::Request {
            id: json!(7),
            method: method.into(),
            params,
        },
    );
    let Some(DriverMessage::Request { id, request }) = messages.pop() else {
        panic!("no request");
    };
    assert_eq!(id, json!(7));
    (events(messages), request)
}

#[test]
fn grok_commands_become_commands_once_their_kind_is_known() {
    let mut state = Session::default();
    // Grok names the tool first and says what kind it is later.
    assert!(
        update(
            &mut state,
            json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "title": "run_terminal_cmd",
                "rawInput": {"command": "echo maple-probe"}})
        )
        .is_empty()
    );
    let started = update(
        &mut state,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "c2", "kind": "execute",
            "title": "Execute `echo maple-probe`",
            "rawInput": {"variant": "Bash", "command": "echo maple-probe"}}),
    );
    assert_eq!(
        started,
        [AgentEvent::ItemStarted(AgentItem::CommandExecution {
            id: "c2".into(),
            command: "echo maple-probe".into(),
            exit_code: None,
            status: None,
        })]
    );
    let finished = update(
        &mut state,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "c2", "status": "completed",
            "rawOutput": {"type": "Bash", "exit_code": 0, "command": "echo maple-probe"}}),
    );
    assert_eq!(
        finished,
        [AgentEvent::ItemCompleted(AgentItem::CommandExecution {
            id: "c2".into(),
            command: "echo maple-probe".into(),
            exit_code: Some(0),
            status: Some("completed".into()),
        })]
    );
    // A repeated end says nothing new.
    assert!(
        update(
            &mut state,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "c2", "status": "completed"})
        )
        .is_empty()
    );
}

#[test]
fn antigravity_commands_read_its_own_keys() {
    let mut state = Session::default();
    update(
        &mut state,
        json!({"sessionUpdate": "tool_call", "toolCallId": "a1", "status": "in_progress",
            "kind": "execute", "title": "ls -la", "rawInput": {"CommandLine": "ls -la"}}),
    );
    let finished = update(
        &mut state,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "a1", "status": "completed",
            "kind": "execute", "rawOutput": {"output": "Created At: x\nCommand exited with code 2.\n"}}),
    );
    assert_eq!(
        finished,
        [AgentEvent::ItemCompleted(AgentItem::CommandExecution {
            id: "a1".into(),
            command: "ls -la".into(),
            exit_code: Some(2),
            status: Some("completed".into()),
        })]
    );
}

#[test]
fn only_finished_edits_count_as_changes() {
    let mut state = Session::default();
    let edit = |status: &str| {
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "kind": "edit", "status": status,
            "title": "Edit `hello.txt`",
            "content": [{"type": "diff", "path": "/p/hello.txt", "oldText": "", "newText": "hi\n"}],
            "locations": [{"path": "/p/hello.txt"}]})
    };
    assert!(update(&mut state, edit("pending")).is_empty());
    assert_eq!(
        update(&mut state, edit("completed")),
        [AgentEvent::ItemCompleted(AgentItem::FileChange {
            id: "e1".into(),
            changes: vec![FileChangeEntry {
                path: "/p/hello.txt".into(),
                kind: "update".into()
            }],
            status: Some("completed".into()),
        })]
    );
    // A new file has no old text.
    assert_eq!(
        update(
            &mut state,
            json!({"sessionUpdate": "tool_call", "toolCallId": "e2", "kind": "edit", "status": "completed",
                "content": [{"type": "diff", "path": "/p/new.txt", "newText": "x"}]})
        ),
        [AgentEvent::ItemCompleted(AgentItem::FileChange {
            id: "e2".into(),
            changes: vec![FileChangeEntry {
                path: "/p/new.txt".into(),
                kind: "add".into()
            }],
            status: Some("completed".into()),
        })]
    );
    // Antigravity names the file only in its locations.
    assert_eq!(
        update(
            &mut state,
            json!({"sessionUpdate": "tool_call", "toolCallId": "e3", "kind": "edit", "status": "completed",
                "title": "File hello.txt", "locations": [{"path": "/p/hello.txt"}]})
        ),
        [AgentEvent::ItemCompleted(AgentItem::FileChange {
            id: "e3".into(),
            changes: vec![FileChangeEntry {
                path: "/p/hello.txt".into(),
                kind: "update".into()
            }],
            status: Some("completed".into()),
        })]
    );
    // A refused edit changed nothing.
    assert!(
        update(
            &mut state,
            json!({"sessionUpdate": "tool_call", "toolCallId": "e4", "kind": "edit", "status": "failed",
                "locations": [{"path": "/p/kept.txt"}]})
        )
        .is_empty()
    );
}

#[test]
fn text_runs_split_around_tool_calls_and_replay_is_dropped() {
    let mut state = Session {
        prompt: 3,
        ..Session::default()
    };
    let chunk = |text: &str| json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}});
    let ids = |events: Vec<AgentEvent>| {
        events
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::AgentMessageDelta { item_id, .. } => Some(item_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(update(&mut state, chunk("Look"))), ["message-3-0"]);
    assert_eq!(ids(update(&mut state, chunk("ing"))), ["message-3-0"]);
    update(
        &mut state,
        json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "list_dir"}),
    );
    assert_eq!(ids(update(&mut state, chunk("Done"))), ["message-3-1"]);
    assert!(update(&mut state, chunk("")).is_empty());
    assert!(
        update(
            &mut state,
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}})
        )
        .is_empty()
    );
    assert_eq!(
        update(
            &mut state,
            json!({"sessionUpdate": "plan", "entries": [
                {"content": "Read", "status": "completed", "priority": "high"},
                {"content": "Fix", "status": "in_progress", "priority": "high"}]})
        ),
        [AgentEvent::ItemCompleted(AgentItem::TodoList {
            id: "plan".into(),
            items: vec![
                super::super::driver::TodoEntry {
                    text: "Read".into(),
                    completed: true
                },
                super::super::driver::TodoEntry {
                    text: "Fix".into(),
                    completed: false
                },
            ],
        })]
    );
    state.replaying = true;
    assert!(update(&mut state, chunk("an earlier turn")).is_empty());
    // Vendor notifications say nothing Maple shows.
    assert!(
        events(decode(
            "grok",
            &mut state,
            ServerMessage::Notification {
                method: "_x.ai/session/prompt_complete".into(),
                params: json!({}),
            },
        ))
        .is_empty()
    );
}

#[test]
fn permission_requests_are_approvals_that_never_pick_lasting_options() {
    let mut state = Session::default();
    let (events, request) = ask(
        &mut state,
        "session/request_permission",
        json!({"sessionId": "s", "toolCall": {"toolCallId": "e1", "kind": "edit", "title": "Edit `hello.txt`",
            "rawInput": {"file_path": "/p/hello.txt", "new_string": "hi"}},
            "options": [
                {"optionId": "allow-edits-session", "name": "Allow edits this session", "kind": "allow_always"},
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}]}),
    );
    assert!(events.is_empty());
    let AgentRequest::Approval {
        item_id,
        tool_name,
        arguments,
        prompt,
    } = request
    else {
        panic!("expected an approval");
    };
    assert_eq!(item_id, "e1");
    assert_eq!(tool_name, "grok edit");
    assert_eq!(arguments["file_path"], "/p/hello.txt");
    assert!(!arguments.contains_key("path"));
    assert_eq!(prompt.as_deref(), Some("Edit `hello.txt`"));
    let waiting = state.waiting.get("7").unwrap().clone();
    assert_eq!(
        answer(
            &waiting,
            AgentReply::Decision(AgentPermissionDecision::AllowOnce)
        ),
        json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}})
    );
    assert_eq!(
        answer(
            &waiting,
            AgentReply::Decision(AgentPermissionDecision::Cancel)
        ),
        json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}})
    );
    // With only a lasting allow on offer, Maple allows nothing.
    let lasting = Waiting::Permission {
        options: vec![json!({"optionId": "always", "kind": "allow_always"})],
    };
    assert_eq!(
        answer(
            &lasting,
            AgentReply::Decision(AgentPermissionDecision::AllowOnce)
        ),
        json!({"outcome": {"outcome": "cancelled"}})
    );
    // Antigravity's request carries its call: the command starts showing.
    let (events, _) = ask(
        &mut state,
        "session/request_permission",
        json!({"toolCall": {"sessionUpdate": "tool_call", "toolCallId": "a1", "status": "pending",
            "kind": "execute", "title": "ls -la", "rawInput": {"CommandLine": "ls -la"}},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]}),
    );
    assert!(matches!(
        &events[..],
        [AgentEvent::ItemStarted(AgentItem::CommandExecution { command, .. })] if command == "ls -la"
    ));
}

#[test]
fn a_permission_request_that_offers_choices_is_a_question() {
    let mut state = Session::default();
    let (_, request) = ask(
        &mut state,
        "session/request_permission",
        json!({"toolCall": {"toolCallId": "p", "title": "Which plan?"},
            "options": [
                {"optionId": "a", "name": "Plan A", "kind": "allow_once"},
                {"optionId": "b", "name": "Plan B", "kind": "allow_once"},
                {"optionId": "no", "name": "Neither", "kind": "reject_once"}]}),
    );
    let AgentRequest::UserInput { questions } = request else {
        panic!("expected a question");
    };
    assert_eq!(questions[0].id, "choice");
    assert_eq!(questions[0].question, "Which plan?");
    assert_eq!(questions[0].options[1].label, "Plan B");
    let waiting = state.waiting.get("7").unwrap().clone();
    assert_eq!(
        answer(
            &waiting,
            AgentReply::Answers(r#"{"answers":{"choice":{"answers":["plan b"]}}}"#.into())
        ),
        json!({"outcome": {"outcome": "selected", "optionId": "b"}})
    );
    assert_eq!(
        answer(&waiting, AgentReply::Answers(String::new())),
        json!({"outcome": {"outcome": "cancelled"}})
    );
}

#[test]
fn antigravity_forms_become_questions_and_answers_their_fields() {
    let schema = json!({"type": "object", "properties": {
        "q1": {"type": "string", "title": "Do you prefer red or blue?",
            "oneOf": [{"const": "1", "title": "Red"}, {"const": "2", "title": "Blue"}]},
        "q1_other": {"type": "string", "title": "Other",
            "description": "Optional free-form answer to: Do you prefer red or blue?"},
        "q10": {"type": "number", "title": "How many?"},
        "q2": {"type": "array", "title": "Which days?", "items": {"enum": ["Mon", "Tue"]}}}});
    let mut state = Session::default();
    let (_, request) = ask(
        &mut state,
        "elicitation/create",
        json!({"sessionId": "s", "toolCallId": "q", "mode": "form",
            "message": "The agent has a question.", "requestedSchema": schema}),
    );
    let AgentRequest::UserInput { questions } = request else {
        panic!("expected questions");
    };
    let ids = questions
        .iter()
        .map(|question| question.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["q1", "q2", "q10"]);
    assert_eq!(questions[0].question, "Do you prefer red or blue?");
    assert_eq!(questions[0].options[1].label, "Blue");
    assert!(questions[1].multi_select);
    assert!(questions[2].options.is_empty());

    let form = |answers: &str| form_response(&schema, answers);
    assert_eq!(
        form(
            r#"{"answers":{"q1":{"answers":["blue"]},"q2":{"answers":["Tue","mon"]},"q10":{"answers":["3"]}}}"#
        ),
        json!({"action": "accept", "content": {"q1": "2", "q2": ["Tue", "Mon"], "q10": 3.0}})
    );
    // Words that name no choice go to the free-text twin.
    assert_eq!(
        form(r#"{"answers":{"q1":{"answers":["green"]}}}"#),
        json!({"action": "accept", "content": {"q1_other": "green"}})
    );
    assert_eq!(form(r#"{"answers":{}}"#), json!({"action": "decline"}));
    assert_eq!(form(""), json!({"action": "cancel"}));

    // A form in another mode is not Maple's to answer.
    let (_, request) = ask(
        &mut state,
        "elicitation/create",
        json!({"mode": "url", "url": "https://example.com"}),
    );
    assert!(matches!(request, AgentRequest::Unknown { .. }));
}

#[test]
fn vendor_requests_are_unknown() {
    let mut state = Session::default();
    let (_, request) = ask(
        &mut state,
        "_x.ai/ask_user_question",
        json!({"questions": [{"question": "Which?", "options": [{"label": "A"}]}]}),
    );
    assert_eq!(
        request,
        AgentRequest::Unknown {
            method: "_x.ai/ask_user_question".into()
        }
    );
}

#[test]
fn options_turn_ends_and_failures_read_as_agents_send_them() {
    let options = config_options(&json!({"configOptions": [
        {"id": "model", "category": "model", "type": "select", "currentValue": "grok-4.7",
            "options": [{"value": "grok-4.7", "name": "Grok 4.7"}, {"value": "grok-4.6", "name": "Grok 4.6"}]},
        {"id": "reasoning_effort", "category": "thought_level", "currentValue": "high",
            "options": [{"group": "Levels", "name": "Levels", "options": [{"value": "low"}, {"value": "high"}]}]}]}))
    .unwrap();
    assert_eq!(options[0].values, ["grok-4.7", "grok-4.6"]);
    assert_eq!(options[1].category.as_deref(), Some("thought_level"));
    assert_eq!(options[1].values, ["low", "high"]);
    assert!(config_options(&json!({})).is_none());

    let grok = super::super::catalog::entry("grok").unwrap();
    let end = |stop: Value| turn_end(grok, Ok(json!({ "stopReason": stop })));
    assert_eq!(end(json!("end_turn")), (TurnOutcome::Completed, None));
    assert_eq!(end(json!("cancelled")), (TurnOutcome::Cancelled, None));
    assert_eq!(end(json!("max_tokens")).0, TurnOutcome::Completed);
    assert!(end(json!("max_tokens")).1.is_some());
    assert_eq!(
        end(json!("rate_limit")),
        (
            TurnOutcome::Failed,
            Some("The agent stopped: rate_limit.".into())
        )
    );
    let signed_out = RpcFailure {
        code: Some(-32000),
        message: "Authentication required".into(),
    };
    assert_eq!(
        turn_end(grok, Err(signed_out.clone())),
        (TurnOutcome::Failed, Some(grok.text.sign_in_hint.clone()))
    );
    assert_eq!(
        describe_failure(grok, "session/new", &signed_out),
        grok.text.sign_in_hint
    );
    assert_eq!(
        describe_failure(
            grok,
            "session/new",
            &RpcFailure {
                code: Some(-32603),
                message: "Internal error".into()
            }
        ),
        "session/new failed: Internal error"
    );
}

#[test]
fn capabilities_are_read_under_either_spelling() {
    let antigravity = json!({"loadSession": true, "load_session": true,
        "session_capabilities": {"resume": {}, "load": {}}});
    assert!(session_capability(&antigravity, "resume"));
    assert!(flag(&antigravity, "loadSession"));
    let grok = json!({"loadSession": true, "sessionCapabilities": {"list": {}, "resume": {}, "close": {}}});
    assert!(session_capability(&grok, "resume"));
    assert!(!session_capability(
        &json!({"sessionCapabilities": {}}),
        "resume"
    ));
}

#[test]
fn an_approval_names_the_command_or_file_however_the_agent_spells_it() {
    let arguments = |tool_call: Value| {
        let mut state = Session::default();
        let (_, request) = ask(
            &mut state,
            "session/request_permission",
            json!({"sessionId": "s", "toolCall": tool_call,
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}),
        );
        let AgentRequest::Approval { arguments, .. } = request else {
            panic!("expected an approval");
        };
        arguments
    };
    // Antigravity's own spellings, as recorded.
    let command = arguments(
        json!({"toolCallId": "c", "kind": "execute", "title": "Read maple-live.txt",
        "rawInput": {"CommandLine": "cat maple-live.txt", "WaitMsBeforeAsync": 5000}}),
    );
    assert_eq!(command["command"], "cat maple-live.txt");
    assert_eq!(command["CommandLine"], "cat maple-live.txt");
    let edit = arguments(json!({"toolCallId": "e", "kind": "edit", "title": "Edit",
        "rawInput": {"Path": "/p/maple-live.txt", "Content": "ready", "Overwrite": true}}));
    assert_eq!(edit["path"], "/p/maple-live.txt");
    assert!(!edit.contains_key("command"));
    // A call with no input is named by its location, never by its title.
    let located = arguments(
        json!({"toolCallId": "l", "kind": "delete", "title": "rm -rf /",
        "locations": [{"path": "/p/old.txt"}]}),
    );
    assert_eq!(located["path"], "/p/old.txt");
    let untitled = arguments(json!({"toolCallId": "t", "kind": "execute", "title": "ls"}));
    assert!(untitled.is_empty());
}

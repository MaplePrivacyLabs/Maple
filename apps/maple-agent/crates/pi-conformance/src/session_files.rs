//! Input-only filesystem observations through the actual SessionManager.
use crate::CheckResult;
use pi_ai::{
    types::{JsObject, JsString, JsValue},
    utils::{js_json::stringify, js_value::to_js_value, json_parse::parse_json},
};
use pi_coding_agent::{
    config::HostConfig,
    core::{
        messages::raw_message,
        session_manager::{NewSessionOptions, SessionError, SessionManager},
    },
};
use pi_testkit::VirtualEnv;
use std::{
    fs,
    io::ErrorKind,
    path::Path,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

enum Operation {
    AppendMessage(JsObject),
    AppendModelChange {
        provider: JsString,
        model_id: JsString,
    },
}

struct FileCase {
    case: JsString,
    create: bool,
    content: Option<JsString>,
    operations: Vec<Operation>,
}

fn required_string<'a>(value: &'a JsValue, key: &str) -> CheckResult<&'a JsString> {
    value
        .get(key)
        .and_then(JsValue::as_js_str)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn fixture(input: &JsValue) -> CheckResult<FileCase> {
    if input.as_object().is_none() {
        return Err("sessionFile must be an object".into());
    }
    let case = required_string(input, "case")?.clone();
    let create = match input.get("mode") {
        None => false,
        Some(value) if value.as_str() == Some("create") => true,
        Some(_) => return Err("sessionFile.mode must be absent or \"create\"".into()),
    };
    let content = input
        .get("content")
        .map(|value| {
            value
                .as_js_str()
                .cloned()
                .ok_or_else(|| "sessionFile.content must be a string".to_owned())
        })
        .transpose()?;
    let operations = input
        .get("operations")
        .and_then(JsValue::as_array)
        .ok_or_else(|| "sessionFile.operations must be an array".to_owned())?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let parse = || -> CheckResult<Operation> {
                match required_string(value, "operation")?.as_str() {
                    Some("appendMessage") => Ok(Operation::AppendMessage(
                        value
                            .get("message")
                            .and_then(JsValue::as_object)
                            .cloned()
                            .ok_or_else(|| "message must be an object".to_owned())?,
                    )),
                    Some("appendModelChange") => Ok(Operation::AppendModelChange {
                        provider: required_string(value, "provider")?.clone(),
                        model_id: required_string(value, "modelId")?.clone(),
                    }),
                    _ => Err("operation must be appendMessage or appendModelChange".into()),
                }
            };
            parse().map_err(|error| format!("sessionFile.operations[{index}]: {error}"))
        })
        .collect::<CheckResult<Vec<_>>>()?;
    Ok(FileCase {
        case,
        create,
        content,
        operations,
    })
}

fn obj<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    JsObject::from(fields).into()
}

fn js(value: &impl serde::Serialize) -> CheckResult<JsValue> {
    to_js_value(value).map_err(|error| format!("serialize session observation: {error}"))
}

fn exists(file: &str) -> CheckResult<bool> {
    Path::new(file)
        .try_exists()
        .map_err(|error| format!("inspect session file {file}: {error}"))
}

fn bytes(file: &str) -> CheckResult<JsValue> {
    match fs::read(file) {
        // Node's readFileSync(file, "utf8") replaces invalid UTF-8 sequences.
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned().into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(JsValue::Null),
        Err(error) => Err(format!("read session file {file}: {error}")),
    }
}

fn snapshot(value: &JsValue) -> CheckResult<JsValue> {
    parse_json(&stringify(value)).map_err(|error| format!("snapshot session observation: {error}"))
}

fn error(error: &SessionError, file: &str) -> JsValue {
    // Only the helper-owned filename is normalized. Preserve all other UTF-16
    // units, including lone surrogates in a source error message.
    let message = error.message.as_utf16();
    let needle: Vec<_> = file.encode_utf16().collect();
    let replacement: Vec<_> = "<SESSION_FILE>".encode_utf16().collect();
    let mut normalized = Vec::new();
    let mut offset = 0;
    while offset < message.len() {
        if !needle.is_empty() && message[offset..].starts_with(&needle) {
            normalized.extend_from_slice(&replacement);
            offset += needle.len();
        } else {
            normalized.push(message[offset]);
            offset += 1;
        }
    }
    obj([
        ("error", JsString::from_utf16(normalized).into()),
        ("errorClass", error.name.into()),
    ])
}

fn readback(manager: &SessionManager) -> CheckResult<JsValue> {
    snapshot(&obj([
        ("header", js(&manager.get_header())?),
        ("entries", js(&manager.get_entries())?),
        ("context", js(&manager.build_session_context())?),
    ]))
}

fn observe(manager: &SessionManager, file: &str, result: Option<JsValue>) -> CheckResult<JsValue> {
    let mut value = JsObject::new();
    if let Some(result) = result {
        value.insert("result", result);
    }
    value.insert("bytes", bytes(file)?);
    value.insert("header", js(&manager.get_header())?);
    value.insert("entries", js(&manager.get_entries())?);
    value.insert("leafId", js(&manager.get_leaf_id())?);
    value.insert("context", js(&manager.build_session_context())?);
    snapshot(&value.into())
}

enum ObservationError {
    Session(SessionError),
    Harness(String),
}

impl From<SessionError> for ObservationError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<String> for ObservationError {
    fn from(error: String) -> Self {
        Self::Harness(error)
    }
}

/// Each call owns its temporary directory and fresh deterministic environment.
/// Product errors inside the source catch are observations; fixture/setup errors
/// remain harness failures rather than being compared as product behavior.
pub async fn observe_file(input: &JsValue, now: i64, cwd: &str) -> CheckResult<JsValue> {
    let item = fixture(input)?;
    let directory = tempfile::tempdir()
        .map_err(|error| format!("create session fixture directory: {error}"))?;
    let dir = directory
        .path()
        .to_str()
        .ok_or_else(|| "session fixture directory is not valid UTF-8".to_owned())?;
    let mut file = directory
        .path()
        .join("session.jsonl")
        .to_str()
        .ok_or_else(|| "session fixture filename is not valid UTF-8".to_owned())?
        .to_owned();
    if let Some(content) = &item.content {
        // Node's writeFileSync converts an input string to UTF-8 at this boundary.
        fs::write(&file, content.to_string_lossy())
            .map_err(|error| format!("write session fixture {file}: {error}"))?;
    }
    let env = Arc::new(VirtualEnv::new(now));
    let config = Arc::new(HostConfig::new("pi", ".pi", directory.path(), cwd));
    let mut steps = Vec::new();
    let operation = (|| -> Result<JsValue, ObservationError> {
        let mut manager = if item.create {
            SessionManager::create(
                cwd,
                Some(dir),
                Some(NewSessionOptions {
                    id: Some("barrier-session".into()),
                    parent_session: None,
                }),
                env.clone(),
                config.clone(),
            )?
        } else {
            SessionManager::open(&file, Some(dir), Some(cwd), env.clone(), config.clone())?
        };
        // These mutations survive a later product failure, as in the TS helper.
        file = manager
            .get_session_file()
            .ok_or_else(|| "file-backed session returned no filename".to_owned())?;
        steps.push(observe(&manager, &file, None)?);
        for operation in &item.operations {
            let id = match operation {
                Operation::AppendMessage(message) => {
                    manager.append_message(raw_message(message.clone()))?
                }
                Operation::AppendModelChange { provider, model_id } => {
                    manager.append_model_change(provider.clone(), model_id.clone())?
                }
            };
            steps.push(observe(&manager, &file, Some(id.into()))?);
        }
        if exists(&file)? {
            let reopened =
                SessionManager::open(&file, Some(dir), Some(cwd), env.clone(), config.clone())?;
            Ok(readback(&reopened)?)
        } else {
            Ok(JsValue::Null)
        }
    })();
    let read_back = match operation {
        Ok(readback) => readback,
        Err(ObservationError::Session(cause)) => {
            let observation = error(&cause, &file);
            steps.push(observation.clone());
            observation
        }
        Err(ObservationError::Harness(cause)) => return Err(cause),
    };
    let mut listing = Vec::new();
    if exists(&file)? {
        let elapsed = Duration::from_millis(now.unsigned_abs());
        let timestamp = if now >= 0 {
            UNIX_EPOCH.checked_add(elapsed)
        } else {
            UNIX_EPOCH.checked_sub(elapsed)
        }
        .ok_or_else(|| {
            format!("session fixture epoch {now} is outside the host timestamp range")
        })?;
        fs::File::open(&file)
            .and_then(|file| {
                file.set_times(
                    fs::FileTimes::new()
                        .set_accessed(timestamp)
                        .set_modified(timestamp),
                )
            })
            .map_err(|error| format!("set session fixture timestamp for {file}: {error}"))?;
        // listAll is outside the source helper's catch, so its failures propagate.
        let infos = SessionManager::list_all(Some(dir), None, None, &config)
            .await
            .map_err(|error| format!("list session fixtures in {dir}: {error}"))?;
        for info in &infos {
            let JsValue::Object(mut raw) = js(info)? else {
                return Err("serialized session listing entry must be an object".into());
            };
            raw.remove("path");
            listing.push(JsValue::Object(raw));
        }
    }
    snapshot(&obj([
        ("case", item.case.into()),
        ("steps", steps.into()),
        ("finalBytes", bytes(&file)?),
        ("readBack", read_back),
        ("listing", listing.into()),
    ]))
}

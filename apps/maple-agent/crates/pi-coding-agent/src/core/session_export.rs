//! Current-branch JSONL export from `core/session-export.ts`.
use super::session_manager::{CURRENT_SESSION_VERSION, SessionManager, SessionResult};
use crate::utils::{
    dates::iso_timestamp,
    paths::{PathInputOptions, resolve_path},
};
use pi_ai::{
    types::{JsObject, JsString, JsValue},
    utils::js_json::stringify,
};
use std::{fs, path::Path};
pub type TrailingEntries<'a> = dyn FnMut(Option<&JsString>, &JsString) -> Vec<JsValue> + 'a;
pub fn serialize_session_branch(
    manager: &SessionManager,
    mut trailing: Option<&mut TrailingEntries<'_>>,
) -> String {
    let timestamp = iso_timestamp(manager.env.now_ms());
    let header = JsObject::from([
        ("type", "session".into()),
        ("version", f64::from(CURRENT_SESSION_VERSION).into()),
        ("id", manager.get_session_id().into()),
        ("timestamp", timestamp.clone().into()),
        ("cwd", manager.get_cwd().into()),
    ]);
    let mut entries = vec![JsValue::Object(header)];
    let mut parent = None;
    for entry in manager.get_branch(None) {
        let mut raw = entry.snapshot();
        raw.insert(
            "parentId",
            parent.clone().map(JsValue::String).unwrap_or(JsValue::Null),
        );
        parent = Some(entry.id());
        entries.push(raw.into());
    }
    if let Some(trailing) = trailing.as_mut() {
        entries.extend(trailing(parent.as_ref(), &timestamp));
    }
    let mut result = entries.iter().map(stringify).collect::<Vec<_>>().join("\n");
    result.push('\n');
    result
}
pub fn export_session_to_jsonl(
    manager: &SessionManager,
    output_path: Option<&str>,
    trailing: Option<&mut TrailingEntries<'_>>,
) -> SessionResult<String> {
    let default_path = output_path.is_none().then(|| {
        format!(
            "session-{}.jsonl",
            iso_timestamp(manager.env.now_ms())
                .to_string_lossy()
                .replace([':', '.'], "-")
        )
    });
    let path = resolve_path(
        output_path
            .or(default_path.as_deref())
            .expect("provided or generated export path"),
        &manager.config.process_cwd.to_string_lossy(),
        &PathInputOptions {
            home_dir: Some(manager.config.home_dir.to_string_lossy().into()),
            ..Default::default()
        },
    )?;
    if let Some(dir) = Path::new(&path).parent()
        && !dir.exists()
    {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, serialize_session_branch(manager, trailing))?;
    Ok(path)
}

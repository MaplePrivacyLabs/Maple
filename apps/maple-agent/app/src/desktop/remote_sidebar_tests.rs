//! Account-owned nickname hydration does not use the process-global settings cache.
use super::*;
use crate::settings::RemoteSidebarSettings;
use std::collections::HashMap;

fn snapshot(name: &str) -> HashMap<String, RemoteSidebarSettings> {
    HashMap::from([(
        "east".into(),
        RemoteSidebarSettings {
            display_name: name.into(),
            pinned_tasks: vec!["persisted-pin".into()],
        },
    )])
}

fn shell(
    cx: &mut gpui::TestAppContext,
    settings: &parking_lot::ReentrantMutexGuard<'_, ()>,
) -> Entity<MapleApp> {
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
    tests::project_shell_with_backend(cx, backend, settings)
}

#[gpui::test]
fn controller_nicknames_hydrate_desktop_and_already_open_settings(cx: &mut gpui::TestAppContext) {
    let settings_lock = crate::settings::SETTINGS_IO_LOCK.lock();
    let app = shell(cx, &settings_lock);
    app.update(cx, |app, cx| {
        app.show_settings(Section::Remote, cx);
        let Screen::Settings(screen) = &app.screen else {
            panic!("expected Settings");
        };
        let screen = screen.clone();
        let stale_close = screen.read(cx).settings_snapshot();
        let owner = app.local_chat.as_ref().unwrap().entity_id();
        app.apply_remote_sidebars(app.store_load_epoch, "user", owner, snapshot("Office"), cx);
        assert_eq!(app.remote_computers["east"].name, "Office");
        assert_eq!(
            app.settings.remote_sidebars["east"].pinned_tasks,
            ["persisted-pin"]
        );
        assert_eq!(
            screen.read(cx).settings_snapshot().remote_sidebars["east"].display_name,
            "Office"
        );
        app.apply_refreshed_password_settings(crate::settings::AppSettings::default(), cx);
        assert_eq!(app.settings.remote_sidebars["east"].display_name, "Office");
        app.close_settings(stale_close, cx);
        assert_eq!(app.settings.remote_sidebars["east"].display_name, "Office");
        app.show_settings(Section::Remote, cx);
        let Screen::Settings(reopened) = &app.screen else {
            panic!("expected reopened Settings");
        };
        assert_eq!(
            reopened.read(cx).settings_snapshot().remote_sidebars["east"].display_name,
            "Office"
        );
    });
}

#[gpui::test]
fn nickname_response_is_fenced_by_account_store_and_exact_local_chat(
    cx: &mut gpui::TestAppContext,
) {
    let settings_lock = crate::settings::SETTINGS_IO_LOCK.lock();
    let app = shell(cx, &settings_lock);
    app.update(cx, |app, cx| {
        let owner = app.local_chat.as_ref().unwrap().entity_id();
        let epoch = app.store_load_epoch;
        app.apply_remote_sidebars(
            epoch.wrapping_add(1),
            "user",
            owner,
            snapshot("Wrong epoch"),
            cx,
        );
        assert!(app.settings.remote_sidebars.is_empty());
        app.apply_remote_sidebars(epoch, "other-account", owner, snapshot("Other account"), cx);
        assert!(app.settings.remote_sidebars.is_empty());
        app.store_ready = false;
        app.apply_remote_sidebars(epoch, "user", owner, snapshot("Locked"), cx);
        assert!(app.settings.remote_sidebars.is_empty());
        app.store_ready = true;
        // Even a return to the same account must reject a response admitted by
        // its previous chat attachment, before sign-out/re-authentication.
        let replacement = app.remote_computers["east"].chat.clone().unwrap();
        app.local_chat = Some(replacement.clone());
        app.apply_remote_sidebars(epoch, "user", owner, snapshot("Retired attachment"), cx);
        assert!(app.settings.remote_sidebars.is_empty());
        app.apply_remote_sidebars(
            epoch,
            "user",
            replacement.entity_id(),
            snapshot("Current"),
            cx,
        );
        assert_eq!(app.settings.remote_sidebars["east"].display_name, "Current");
    });
}

#[gpui::test]
fn slow_nickname_load_preserves_a_local_edit_and_owners_task_pins(cx: &mut gpui::TestAppContext) {
    let settings_lock = crate::settings::SETTINGS_IO_LOCK.lock();
    let app = shell(cx, &settings_lock);
    app.update(cx, |app, cx| {
        app.settings.remote_sidebars = HashMap::from([(
            "east".into(),
            RemoteSidebarSettings {
                display_name: "New local nickname".into(),
                ..Default::default()
            },
        )]);
        let owner = app.local_chat.as_ref().unwrap().entity_id();
        app.apply_remote_sidebars(
            app.store_load_epoch,
            "user",
            owner,
            snapshot("Older nickname"),
            cx,
        );
        assert_eq!(
            app.settings.remote_sidebars["east"].display_name,
            "New local nickname"
        );
        assert_eq!(
            app.settings.remote_sidebars["east"].pinned_tasks,
            ["persisted-pin"]
        );
        assert_eq!(app.remote_computers["east"].name, "New local nickname");
    });
}

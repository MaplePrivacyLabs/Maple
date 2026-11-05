//! The application menu bar. Every item dispatches the same action a
//! shortcut does, so a menu entry, its keyboard hint, and the Settings
//! shortcuts page cannot disagree. The Edit menu binds the platform's
//! standard actions so text fields and the transcript get Cut, Copy,
//! Paste, and Select All from the bar and from Services. macOS also
//! gets Hide Maple, Hide Others, and Show All.

use gpui::{App, Menu, MenuItem, OsAction, SystemMenuType};

use super::chat;
use super::i18n::{tr, tr_args};
use super::text_input;

/// Install the menu bar. gpui shows it on macOS; other platforms ignore
/// it, so the call is unconditional.
pub fn install(cx: &mut App) {
    cx.set_menus(app_menus(cfg!(target_os = "macos")));
}

fn app_menus(macos: bool) -> Vec<Menu> {
    vec![
        Menu {
            name: crate::profile::DISPLAY_NAME.into(),
            items: maple_menu_items(macos),
            disabled: false,
        },
        Menu {
            name: tr("native-edit"),
            items: vec![
                MenuItem::os_action(tr("native-undo"), text_input::Undo, OsAction::Undo),
                MenuItem::os_action(tr("native-redo"), text_input::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action(tr("native-cut"), text_input::Cut, OsAction::Cut),
                MenuItem::os_action(tr("native-copy"), text_input::Copy, OsAction::Copy),
                MenuItem::os_action(tr("native-paste"), text_input::Paste, OsAction::Paste),
                MenuItem::os_action(
                    tr("native-select-all"),
                    text_input::SelectAll,
                    OsAction::SelectAll,
                ),
            ],
            disabled: false,
        },
        Menu {
            name: tr("native-task"),
            items: vec![
                MenuItem::action(tr("native-new-task"), chat::NewTask),
                MenuItem::action(tr("native-search-tasks"), chat::FocusSearch),
                MenuItem::separator(),
                MenuItem::action(tr("native-next-task"), chat::NextTask),
                MenuItem::action(tr("native-previous-task"), chat::PreviousTask),
                MenuItem::separator(),
                MenuItem::action(tr("native-choose-project"), chat::ChooseProject),
            ],
            disabled: false,
        },
        Menu {
            name: tr("native-view"),
            items: vec![
                MenuItem::action(tr("native-toggle-sidebar"), chat::ToggleSidebar),
                MenuItem::action(tr("native-show-archived"), chat::ToggleArchived),
            ],
            disabled: false,
        },
    ]
}

fn maple_menu_items(macos: bool) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::action(
            tr_args(
                "native-about",
                [("app", crate::profile::DISPLAY_NAME.into())],
            ),
            crate::update::AboutAgent,
        ),
        MenuItem::action(tr("native-check-updates"), crate::update::CheckForUpdates),
        MenuItem::separator(),
        MenuItem::action(tr("native-settings"), chat::OpenAppSettings),
        MenuItem::separator(),
        MenuItem::os_submenu(tr("native-services"), SystemMenuType::Services),
        MenuItem::separator(),
    ];
    if macos {
        items.extend([
            MenuItem::action(
                tr_args(
                    "native-hide",
                    [("app", crate::profile::DISPLAY_NAME.into())],
                ),
                crate::desktop::Hide,
            ),
            MenuItem::action(tr("native-hide-others"), crate::desktop::HideOthers),
            MenuItem::action(tr("native-show-all"), crate::desktop::ShowAll),
            MenuItem::separator(),
        ]);
    }
    items.push(MenuItem::action(
        tr_args(
            "native-quit",
            [("app", crate::profile::DISPLAY_NAME.into())],
        ),
        crate::desktop::QuitApp,
    ));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maple_action_names(macos: bool) -> Vec<String> {
        maple_menu_items(macos)
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rebuilding_menus_changes_labels_but_preserves_platform_actions() {
        let _locale = super::super::i18n::TestLocalizationGuard::enable();
        let en = app_menus(true);
        for (locale, edit, new_task) in
            [("es", "Editar", "Nueva tarea"), ("en", "Edit", "New Task")]
        {
            super::super::i18n::apply_preference(Some(locale));
            let translated = app_menus(true);
            assert_eq!(translated[1].name.as_ref(), edit);
            match &translated[2].items[0] {
                MenuItem::Action { name, action, .. } => {
                    assert_eq!(name.as_ref(), new_task);
                    assert!(action.as_any().is::<chat::NewTask>());
                }
                _ => panic!("New Task remains an action"),
            }
            for (before, after) in en.iter().zip(&translated) {
                assert_eq!(before.items.len(), after.items.len());
                for (before, after) in before.items.iter().zip(&after.items) {
                    if let (
                        MenuItem::Action { action: a, .. },
                        MenuItem::Action { action: b, .. },
                    ) = (before, after)
                    {
                        assert_eq!(a.name(), b.name());
                    }
                }
            }
        }
    }

    #[test]
    fn macos_application_menu_has_standard_hide_commands() {
        assert_eq!(
            maple_action_names(true),
            vec![
                format!("About {}", crate::profile::DISPLAY_NAME),
                "Check for Updates…".to_string(),
                "Settings…".to_string(),
                format!("Hide {}", crate::profile::DISPLAY_NAME),
                "Hide Others".to_string(),
                "Show All".to_string(),
                format!("Quit {}", crate::profile::DISPLAY_NAME),
            ]
        );
    }

    #[test]
    fn other_platforms_omit_hide_commands() {
        assert_eq!(
            maple_action_names(false),
            vec![
                format!("About {}", crate::profile::DISPLAY_NAME),
                "Check for Updates…".to_string(),
                "Settings…".to_string(),
                format!("Quit {}", crate::profile::DISPLAY_NAME)
            ]
        );
    }
}

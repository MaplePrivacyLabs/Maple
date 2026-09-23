//! The application menu bar. Every item dispatches the same action a
//! shortcut does, so a menu entry, its keyboard hint, and the Settings
//! shortcuts page cannot disagree. The Edit menu binds the platform's
//! standard actions so text fields and the transcript get Cut, Copy,
//! Paste, and Select All from the bar and from Services. macOS also
//! gets Hide Maple, Hide Others, and Show All.

use gpui::{App, Menu, MenuItem, OsAction, SystemMenuType};

use super::chat;
use super::text_input;

/// Install the menu bar. gpui shows it on macOS; other platforms ignore
/// it, so the call is unconditional.
pub fn install(cx: &mut App) {
    cx.set_menus(app_menus(cfg!(target_os = "macos")));
}

fn app_menus(macos: bool) -> Vec<Menu> {
    vec![
        Menu {
            name: "Maple".into(),
            items: maple_menu_items(macos),
            disabled: false,
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", text_input::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", text_input::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", text_input::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", text_input::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", text_input::Paste, OsAction::Paste),
                MenuItem::os_action("Select All", text_input::SelectAll, OsAction::SelectAll),
            ],
            disabled: false,
        },
        Menu {
            name: "Task".into(),
            items: vec![
                MenuItem::action("New Task", chat::NewTask),
                MenuItem::action("Search Tasks", chat::FocusSearch),
                MenuItem::separator(),
                MenuItem::action("Next Task", chat::NextTask),
                MenuItem::action("Previous Task", chat::PreviousTask),
                MenuItem::separator(),
                MenuItem::action("Choose Project…", chat::ChooseProject),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Toggle Sidebar", chat::ToggleSidebar),
                MenuItem::action("Show Archived Tasks", chat::ToggleArchived),
            ],
            disabled: false,
        },
    ]
}

fn maple_menu_items(macos: bool) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::action("Settings…", chat::OpenAppSettings),
        MenuItem::separator(),
        MenuItem::os_submenu("Services", SystemMenuType::Services),
        MenuItem::separator(),
    ];
    if macos {
        items.extend([
            MenuItem::action("Hide Maple", crate::desktop::Hide),
            MenuItem::action("Hide Others", crate::desktop::HideOthers),
            MenuItem::action("Show All", crate::desktop::ShowAll),
            MenuItem::separator(),
        ]);
    }
    items.push(MenuItem::action("Quit Maple", crate::desktop::QuitApp));
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
    fn macos_application_menu_has_standard_hide_commands() {
        assert_eq!(
            maple_action_names(true),
            [
                "Settings…",
                "Hide Maple",
                "Hide Others",
                "Show All",
                "Quit Maple",
            ]
        );
    }

    #[test]
    fn other_platforms_omit_hide_commands() {
        assert_eq!(maple_action_names(false), ["Settings…", "Quit Maple"]);
    }
}

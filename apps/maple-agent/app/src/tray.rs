//! Stock status-icon backend; no service supervisor or custom native tray code.
use crate::ui::i18n::{current_locale, tr};
use std::cell::RefCell;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem},
};

pub enum Action {
    Open,
    Quit,
    CheckForUpdates,
}
pub struct StatusIcon {
    tray: TrayIcon,
    open: MenuItem,
    check: MenuItem,
    quit: MenuItem,
    presentation: RefCell<Option<(fluent_templates::LanguageIdentifier, bool, bool)>>,
}
impl StatusIcon {
    pub fn new() -> Result<(Self, tokio::sync::mpsc::Receiver<Action>), String> {
        let menu = Menu::new();
        let open = MenuItem::with_id("maple-open", tr("native-open-maple").as_ref(), true, None);
        let check = MenuItem::with_id(
            "maple-updates",
            tr("native-check-updates").as_ref(),
            true,
            None,
        );
        let quit = MenuItem::with_id("maple-quit", tr("native-quit-maple").as_ref(), true, None);
        menu.append_items(&[&open, &check, &quit])
            .map_err(|_| "Could not create the Maple tray menu")?;
        let image = image::load_from_memory(include_bytes!("../packaging/maple-agent.png"))
            .map_err(|_| "Could not load the Maple tray icon")?
            .resize(32, 32, image::imageops::FilterType::Lanczos3)
            .into_rgba8();
        let icon =
            Icon::from_rgba(image.into_raw(), 32, 32).map_err(|_| "Invalid Maple tray icon")?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_tooltip(tr("native-remote-off").as_ref())
            .build()
            .map_err(|_| "The desktop tray is unavailable")?;
        tray.set_visible(false)
            .map_err(|_| "Could not configure the Maple tray")?;
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = match event.id.as_ref() {
                "maple-open" => Action::Open,
                "maple-quit" => Action::Quit,
                "maple-updates" => Action::CheckForUpdates,
                _ => return,
            };
            let _ = sender.try_send(action);
        }));
        Ok((
            Self {
                tray,
                open,
                check,
                quit,
                presentation: RefCell::new(None),
            },
            receiver,
        ))
    }
    pub fn set_state(&self, remote_enabled: bool, store_resident: bool) {
        let next = (current_locale(), remote_enabled, store_resident);
        let mut previous = self.presentation.borrow_mut();
        if previous.as_ref() == Some(&next) {
            return;
        }
        if previous.as_ref().map(|state| &state.0) != Some(&next.0) {
            self.open.set_text(tr("native-open-maple").as_ref());
            self.check.set_text(tr("native-check-updates").as_ref());
            self.quit.set_text(tr("native-quit-maple").as_ref());
        }
        let tooltip = tr(if remote_enabled {
            "native-remote-on"
        } else if store_resident {
            "native-store-resident"
        } else {
            "native-remote-off"
        });
        let _ = self.tray.set_tooltip(Some(tooltip.as_ref()));
        let _ = self.tray.set_visible(remote_enabled || store_resident);
        *previous = Some(next);
    }
}

/// Creating an SNI item does not mean the desktop actually displays a tray.
#[cfg(target_os = "linux")]
pub async fn usable() -> bool {
    let check = async {
        let connection = zbus::Connection::session().await.ok()?;
        let watcher = zbus::Proxy::new(
            &connection,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .await
        .ok()?;
        watcher
            .get_property::<bool>("IsStatusNotifierHostRegistered")
            .await
            .ok()
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), check)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

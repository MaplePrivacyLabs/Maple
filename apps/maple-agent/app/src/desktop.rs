//! The gpui desktop window: login/chat routing, window state, and the
//! event pump that forwards agent service events into the active chat
//! screen. Compiled only with the `desktop` feature.

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Entity, Pixels, Render, Window, WindowBounds, WindowOptions, actions,
    div, prelude::*, px, size,
};

actions!(maple_app, [QuitApp]);

use crate::backend::AgentBackend;
use crate::ui;
use crate::ui::chat::{ChatScreen, LoggedOut};
use crate::ui::login::{LoginScreen, LoginSucceeded};
use crate::ui::settings::{
    AccountDeleted, Section, SettingsClosed, SettingsScreen, ShortcutSettingsChange,
    ShortcutSettingsRequested, SignOutRequested,
};
use crate::ui::titlebar::TitleBar;

enum Screen {
    /// Construction placeholder. `run` replaces it before the first paint
    /// with the chat screen (saved sign-in) or the login form.
    Restoring,
    Login(Entity<LoginScreen>),
    Chat(Entity<ChatScreen>),
    Settings(Entity<SettingsScreen>),
}

struct MapleApp {
    backend: Arc<AgentBackend>,
    screen: Screen,
    user_id: Option<String>,
    /// The chat screen is parked while settings is open so Back returns to
    /// it with its state intact.
    parked_chat: Option<Entity<ChatScreen>>,
    settings: crate::settings::AppSettings,
    /// The complete live shortcut map. Settings mutations prepare a full
    /// replacement before this runtime clears GPUI's process-wide bindings.
    shortcuts: crate::shortcuts::ShortcutRuntime,
    /// Created once; re-creating it per render leaked an entity per frame.
    titlebar: Entity<TitleBar>,
}

impl MapleApp {
    /// Forward a batch of backend service events to the chat screen when
    /// one exists. One batch is one render, however many events arrived.
    fn handle_service_events(
        &mut self,
        events: Vec<maple_agent::agent::AgentServiceEvent>,
        cx: &mut Context<Self>,
    ) {
        if let Screen::Chat(chat) = &self.screen {
            chat.update(cx, |chat, cx| chat.handle_service_events(events, cx));
        }
    }

    /// Show a task, for a click on its notification. Only the chat screen
    /// can: login has no tasks, and settings keeps its parked chat as is.
    fn open_task(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Screen::Chat(chat) = &self.screen {
            chat.update(cx, |chat, cx| chat.select_session(session_id, cx));
        }
    }

    /// A newer release exists: tell the chat screen so it shows the banner.
    /// Without a chat screen (login in progress) nothing is done here; the
    /// result stays in `update::available()` and seeds the next chat.
    fn set_update(&mut self, info: crate::update::UpdateInfo, cx: &mut Context<Self>) {
        let chat = match &self.screen {
            Screen::Chat(chat) => Some(chat.clone()),
            _ => self.parked_chat.clone(),
        };
        if let Some(chat) = chat {
            chat.update(cx, |chat, cx| chat.set_update(info, cx));
        }
        cx.notify();
    }

    /// Tear down the chat screen and return to a fresh login form. A
    /// no-op when the login form is already showing, so a sign-out that
    /// is reported twice (Settings button, then the backend) keeps the
    /// form the user may already be typing in.
    fn show_login(&mut self, cx: &mut Context<Self>) {
        self.user_id = None;
        self.parked_chat = None;
        if matches!(self.screen, Screen::Login(_)) {
            return;
        }
        let backend = self.backend.clone();
        let login = cx.new(|cx| LoginScreen::new(backend, cx));
        self.subscribe_login(&login, cx);
        self.screen = Screen::Login(login);
        cx.notify();
    }

    /// Wire the login form: a successful sign-in opens the chat screen.
    fn subscribe_login(&mut self, login: &Entity<LoginScreen>, cx: &mut Context<Self>) {
        cx.subscribe(
            login,
            |app: &mut MapleApp, _emitter, event: &LoginSucceeded, cx| {
                app.open_chat(event.0.clone(), cx);
            },
        )
        .detach();
    }

    /// Show the chat screen for a signed-in account and wire its events.
    fn open_chat(&mut self, user_id: String, cx: &mut Context<Self>) {
        log::debug!(
            "startup: chat screen open at {} ms",
            crate::startup_elapsed()
        );
        let backend = self.backend.clone();
        let chat = cx.new(|cx| ChatScreen::new(backend, user_id.clone(), cx));
        // The release check may have finished while the login screen was
        // up; the banner must not be lost with it.
        if let Some(info) = crate::update::available() {
            chat.update(cx, |chat, cx| chat.set_update(info.clone(), cx));
        }
        self.user_id = Some(user_id);
        self.parked_chat = None;
        self.screen = Screen::Chat(chat.clone());
        self.subscribe_chat(&chat, cx);
        cx.notify();
    }

    /// Park the chat screen and show settings.
    fn show_settings(&mut self, section: Section, cx: &mut Context<Self>) {
        let Screen::Chat(chat) = &self.screen else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone().unwrap_or_default();
        let settings = self.settings.clone();
        let shortcut_snapshot = self.shortcuts.snapshot();
        let screen = cx.new(|cx| {
            SettingsScreen::new(backend, user_id, settings, shortcut_snapshot, section, cx)
        });
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, _emitter, event: &SettingsClosed, cx| {
                app.close_settings(event.0.clone(), cx);
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, emitter, event: &ShortcutSettingsRequested, cx| {
                let result = app.apply_shortcut_change(&event.0, cx);
                let shortcut_overrides = app.settings.shortcut_overrides.clone();
                let snapshot = app.shortcuts.snapshot();
                emitter.update(cx, |screen, cx| {
                    screen.apply_shortcut_result(shortcut_overrides, snapshot, result, cx);
                });
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, _emitter, _event: &SignOutRequested, cx| {
                if let Some(chat) = app.parked_chat.take() {
                    chat.update(cx, |chat, cx| chat.sign_out(cx));
                }
                app.show_login(cx);
            },
        )
        .detach();
        cx.subscribe(
            &screen,
            |app: &mut MapleApp, _emitter, _event: &AccountDeleted, cx| {
                // The runtime stopped and the credentials went before the
                // event; the parked chat has nothing left to sign out of.
                app.parked_chat = None;
                app.show_login(cx);
            },
        )
        .detach();
        self.parked_chat = Some(chat.clone());
        self.screen = Screen::Settings(screen);
        cx.notify();
    }

    /// Wire per-chat events: sign-out and the settings gear.
    fn subscribe_chat(&mut self, chat: &Entity<ChatScreen>, cx: &mut Context<Self>) {
        cx.subscribe(
            chat,
            |app: &mut MapleApp, _emitter, _event: &LoggedOut, cx| {
                app.show_login(cx);
            },
        )
        .detach();
        cx.subscribe(
            chat,
            |app: &mut MapleApp, _emitter, _event: &ui::chat::OpenSettings, cx| {
                app.show_settings(Section::General, cx);
            },
        )
        .detach();
        cx.subscribe(
            chat,
            |app: &mut MapleApp, _emitter, event: &ui::settings::OpenSettingsSection, cx| {
                app.show_settings(event.0, cx);
            },
        )
        .detach();
    }

    /// Restore the parked chat screen, applying any changed defaults.
    fn close_settings(&mut self, updated: crate::settings::AppSettings, cx: &mut Context<Self>) {
        self.settings = updated;
        ui::theme::apply_preference(ui::theme::Preference::parse(&self.settings.theme), cx);
        ui::typography::apply(
            ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family),
            self.settings.chat_font_size,
        );
        if let Some(chat) = self.parked_chat.take() {
            let settings = self.settings.clone();
            chat.update(cx, |chat, cx| chat.apply_defaults(&settings, cx));
            self.screen = Screen::Chat(chat);
        }
        cx.notify();
    }

    fn apply_shortcut_change(
        &mut self,
        change: &ShortcutSettingsChange,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut overrides = self.settings.shortcut_overrides.clone();
        match change {
            ShortcutSettingsChange::Set {
                slot_id,
                sequence,
                disable_conflicts,
            } => {
                for conflict in disable_conflicts {
                    if conflict != slot_id {
                        overrides.insert(conflict.clone(), None);
                    }
                }
                overrides.insert(slot_id.clone(), Some(sequence.clone()));
            }
            ShortcutSettingsChange::Disable { slot_id } => {
                overrides.insert(slot_id.clone(), None);
            }
            ShortcutSettingsChange::Reset { slot_id } => {
                overrides.remove(slot_id);
            }
            ShortcutSettingsChange::ResetAll => overrides.clear(),
        }

        self.shortcuts.replace(&overrides, cx)?;
        self.settings.shortcut_overrides = overrides.clone();
        crate::settings::update_settings_in_background(move |settings| {
            settings.shortcut_overrides = overrides;
        });
        Ok(())
    }
}

impl Render for MapleApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        static FIRST_RENDER: std::sync::Once = std::sync::Once::new();
        FIRST_RENDER.call_once(|| {
            log::debug!("startup: first render at {} ms", crate::startup_elapsed());
        });
        remember_window_state(window);
        if ui::theme::resolve(window.appearance()) {
            // Every view reads the palette in render; make them all redo it.
            cx.refresh_windows();
        }
        let titlebar = self.titlebar.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(crate::assets::FONT_BODY)
            // Chrome default 14 px (`text_sm`). Chat reading size is set
            // on transcript/composer via `typography::chat_reading`.
            .text_sm()
            .child(titlebar)
            .child(match &self.screen {
                Screen::Restoring => restoring_view().into_any_element(),
                Screen::Login(login) => login.clone().into_any_element(),
                Screen::Chat(chat) => chat.clone().into_any_element(),
                Screen::Settings(screen) => screen.clone().into_any_element(),
            })
    }
}

/// Shown only if a paint happens before `run` picks the first screen,
/// which places it in the same event-loop turn as the window open.
fn restoring_view() -> gpui::Div {
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .flex()
        .justify_center()
        .items_center()
        .bg(gpui::rgb(ui::theme::bg_app()))
        .child(ui::titlebar::drag_strip())
        .child(
            div()
                .text_sm()
                .text_color(gpui::rgb(ui::theme::text_secondary()))
                .child("Signing in…"),
        )
}

/// Last seen window geometry, read back when the app quits.
static WINDOW_STATE: std::sync::Mutex<Option<crate::settings::WindowState>> =
    std::sync::Mutex::new(None);

/// Runs on every root render, which includes every resize and maximize.
fn remember_window_state(window: &Window) {
    let Some(state) = window_state_for(window.window_bounds()) else {
        return;
    };
    if let Ok(mut slot) = WINDOW_STATE.lock() {
        *slot = Some(state);
    }
}

/// The state worth saving for `bounds`. Fullscreen yields `None`: its
/// bounds are the monitor, and saving them as "maximized" made the next
/// launch open maximized instead of at the last windowed size.
fn window_state_for(bounds: WindowBounds) -> Option<crate::settings::WindowState> {
    let (bounds, maximized) = match bounds {
        WindowBounds::Windowed(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) => (bounds, true),
        WindowBounds::Fullscreen(_) => return None,
    };
    Some(crate::settings::WindowState {
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
    })
}

/// Write the last seen window state to settings.json.
fn persist_window_state() {
    let state = WINDOW_STATE.lock().ok().and_then(|slot| *slot);
    if let Some(state) = state {
        crate::settings::save_window_state(state);
    }
}

/// Every key binding the window uses. Tests register the same set, so
/// what they exercise is what ships.
#[cfg(test)]
pub(crate) fn register_key_bindings(cx: &mut App) {
    crate::keymap::bootstrap(cx);
}

pub fn run() {
    crate::init_logging(crate::LogOutput::FileAndStderr);
    log::debug!("startup: logging ready at {} ms", crate::startup_elapsed());

    // One read of settings.json for the whole startup: the backend, the
    // theme, the window geometry, and the root view all take it from here.
    // Name the resolved file so a launcher with its own XDG_CONFIG_HOME
    // makes itself visible: settings that look unsaved usually live in a
    // different root than the one this launch reads.
    let startup_settings = crate::settings::load_settings();
    log::debug!(
        "startup: settings loaded from {} at {} ms",
        crate::backend::app_config_root()
            .join("settings.json")
            .display(),
        crate::startup_elapsed()
    );
    let backend = Arc::new(
        AgentBackend::new(
            crate::configured_api_url(),
            startup_settings.effective_harness_instructions(),
        )
        .expect("failed to initialize agent backend"),
    );
    log::debug!("startup: backend ready at {} ms", crate::startup_elapsed());

    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            log::debug!("startup: gpui app ready at {} ms", crate::startup_elapsed());
            if let Err(error) = cx.text_system().add_fonts(
                crate::assets::FONTS
                    .iter()
                    .map(|bytes| std::borrow::Cow::Borrowed(*bytes))
                    .collect(),
            ) {
                log::warn!("failed to register bundled fonts: {error}");
            }
            ui::spell::preload();
            cx.on_action(|_: &QuitApp, cx| cx.quit());
            let shortcut_runtime = crate::shortcuts::ShortcutRuntime::bootstrap(
                &startup_settings.shortcut_overrides,
                cx,
            );
            ui::theme::apply_preference(ui::theme::Preference::parse(&startup_settings.theme), cx);
            ui::typography::apply(
                ui::typography::ChatFontFamily::parse(&startup_settings.chat_font_family),
                startup_settings.chat_font_size,
            );
            ui::menus::install(cx);
            // A click on a notification brings the app forward and shows
            // the task it was about (tags are `task:<session id>`; see
            // `ChatScreen::notify_desktop`).
            cx.on_system_notification_response(|response, cx| {
                cx.activate(true);
                let session_id = response.tag.strip_prefix("task:").map(str::to_owned);
                for window in cx.windows() {
                    window
                        .update(cx, |root, window, cx| {
                            window.activate_window();
                            if let (Some(session_id), Ok(app)) =
                                (session_id.as_deref(), root.downcast::<MapleApp>())
                            {
                                app.update(cx, |app, cx| app.open_task(session_id, cx));
                            }
                        })
                        .ok();
                }
            });
            cx.set_reduce_motion(startup_settings.reduce_motion);
            let saved = startup_settings
                .window
                .map(crate::settings::WindowState::clamped);
            let window_size = saved
                .map(|state| size(px(state.width), px(state.height)))
                .unwrap_or_else(|| size(px(1280.), px(860.)));
            let bounds: Bounds<Pixels> = Bounds::centered(None, window_size, cx);
            let window_bounds = if saved.is_some_and(|state| state.maximized) {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            };
            cx.on_app_quit(|_cx| async {
                persist_window_state();
            })
            .detach();
            let root_backend = backend.clone();
            let root_settings = startup_settings.clone();
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(window_bounds),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some(ui::titlebar::WINDOW_TITLE.into()),
                            // macOS: the bar is transparent and the app's
                            // top row stands in for it; see ui::titlebar.
                            appears_transparent: ui::titlebar::TRANSPARENT_TITLEBAR,
                            traffic_light_position: ui::titlebar::TRANSPARENT_TITLEBAR
                                .then_some(ui::titlebar::TRAFFIC_LIGHT_POSITION),
                        }),
                        app_owns_titlebar_drag: ui::titlebar::TRANSPARENT_TITLEBAR,
                        ..Default::default()
                    },
                    move |_, cx| {
                        cx.new(move |cx| MapleApp {
                            backend: root_backend,
                            screen: Screen::Restoring,
                            user_id: None,
                            parked_chat: None,
                            settings: root_settings,
                            shortcuts: shortcut_runtime,
                            titlebar: cx.new(|_| TitleBar::new(ui::titlebar::WINDOW_TITLE)),
                        })
                    },
                )
                .expect("failed to open main window");
            log::debug!("startup: window open at {} ms", crate::startup_elapsed());
            // Saved credentials are trusted at once: the chat screen opens
            // with the account's local task list while the server validates
            // the credentials in the background. Only a definitive rejection
            // returns to the login form; offline the local history stays
            // readable and the runtime start reports the connection error.
            {
                let root_window = window;
                match backend.saved_user_id() {
                    Some(user_id) => {
                        let restore = backend.restore_in_background();
                        root_window
                            .update(cx, |app: &mut MapleApp, _window, cx| {
                                app.open_chat(user_id, cx);
                            })
                            .ok();
                        cx.spawn(async move |cx| {
                            let outcome = restore.await.ok();
                            log::debug!(
                                "startup: credential validation done at {} ms ({outcome:?})",
                                crate::startup_elapsed()
                            );
                            if outcome == Some(crate::backend::RestoreOutcome::Rejected) {
                                root_window
                                    .update(cx, |app: &mut MapleApp, _window, cx| {
                                        app.show_login(cx);
                                    })
                                    .ok();
                            }
                        })
                        .detach();
                    }
                    None => {
                        root_window
                            .update(cx, |app: &mut MapleApp, _window, cx| app.show_login(cx))
                            .ok();
                    }
                }
            }
            // Ask for a newer release off the UI thread; the banner shows
            // in the chat when one exists.
            if crate::update::enabled() {
                let check = backend.spawn(crate::update::check());
                let root_window = window;
                cx.spawn(async move |cx| {
                    if let Ok(Some(info)) = check.await {
                        root_window
                            .update(cx, |app: &mut MapleApp, _window, cx| {
                                app.set_update(info, cx);
                            })
                            .ok();
                    }
                })
                .detach();
            }
            let root = window
                .update(cx, |_, window, cx| {
                    // The only window: closing it from the window manager
                    // ends the app the same way the title bar button does.
                    window.on_window_should_close(cx, |_window, cx| {
                        persist_window_state();
                        cx.quit();
                        true
                    });
                    // A system theme change must reach every view.
                    window
                        .observe_window_appearance(|_window, cx| cx.refresh_windows())
                        .detach();
                    cx.entity()
                })
                .expect("root entity");

            // The event pump runs once for the whole process and routes events to
            // whichever screen is active. It exits when the root entity is gone.
            let (spawn_backend, take_backend) = (backend.clone(), backend.clone());
            let rx = spawn_backend.spawn(async move { take_backend.take_events().await });
            cx.spawn(async move |cx| {
                let Some(mut rx) = rx.await.ok().flatten() else {
                    return;
                };
                while let Some(event) = rx.recv().await {
                    // Drain whatever else is queued so a burst of streaming
                    // chunks costs one update and one render, not one each.
                    let mut batch = vec![event];
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                        if batch.len() >= 256 {
                            break;
                        }
                    }
                    // This gpui's entity update is infallible; the pump ends
                    // with the channel instead.
                    root.update(cx, |app, cx| app.handle_service_events(batch, cx));
                }
            })
            .detach();

            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(gpui::point(px(0.), px(0.)), size(px(width), px(height)))
    }

    #[test]
    fn fullscreen_does_not_replace_the_windowed_state() {
        let windowed = window_state_for(WindowBounds::Windowed(bounds(1280., 860.))).unwrap();
        assert_eq!((windowed.width, windowed.height), (1280., 860.));
        assert!(!windowed.maximized);

        let maximized = window_state_for(WindowBounds::Maximized(bounds(1280., 860.))).unwrap();
        assert!(maximized.maximized);

        assert!(window_state_for(WindowBounds::Fullscreen(bounds(3840., 2160.))).is_none());
    }
}

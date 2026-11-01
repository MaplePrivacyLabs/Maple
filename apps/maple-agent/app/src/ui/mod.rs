//! gpui frontend for Maple: login, chat, shared widgets, theme.

pub(crate) mod application_vim;
pub mod chat;
pub mod context_ring;
pub mod decorations;
pub(crate) mod i18n;
pub mod icons;
pub mod login;
pub mod markdown;
pub mod menus;
pub mod motion;
pub mod plan_gate;
pub(crate) mod popup;
pub mod rich_text;
pub mod scrollbar;
pub mod secret_input;
pub mod settings;
pub mod spell;
pub mod store_gate;
pub mod store_repair;
pub mod task;
pub mod text_input;
pub mod theme;
pub mod titlebar;
pub mod typography;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod update_confirmation;
pub mod widgets;

pub mod remote;

use gpui::{Context, Div, div, prelude::*, px};

use super::{GeneralTarget, LanguageRestartRequested, SettingsScreen, SettingsTarget};
use crate::ui::{i18n::tr, icons::icon, theme, widgets};

impl SettingsScreen {
    pub(super) fn language_restart_visible(&self) -> bool {
        crate::ui::i18n::enabled()
            && self.language_restart_pending
            && !self.language_restart_dismissed
    }

    pub(super) fn restart_for_language(&mut self, cx: &mut Context<Self>) {
        if self.language_restart_visible() {
            cx.emit(LanguageRestartRequested);
        }
    }

    pub(super) fn defer_language_restart(&mut self, cx: &mut Context<Self>) {
        self.language_restart_dismissed = true;
        self.reconcile_application_vim_target();
        cx.notify();
    }

    pub(super) fn language_restart_notice(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        widgets::card_row()
            .id("language-restart-notice")
            .debug_selector(|| "language-restart-notice".into())
            .role(gpui::Role::Status)
            .aria_label(tr("settings-language-restart-title"))
            .border_color(gpui::rgb(theme::accent()))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(icon("circle-alert", px(18.), theme::accent()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(tr("settings-language-restart-title")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(gpui::rgb(theme::text_muted()))
                                    .child(tr("settings-language-restart-detail")),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        self.application_target(
                            || SettingsTarget::General(GeneralTarget::LanguageRestartLater),
                            widgets::secondary_button("language-restart-later")
                                .debug_selector(|| "language-restart-later".into())
                                .aria_label(tr("settings-language-restart-later"))
                                .tab_index(3)
                                .child(tr("settings-language-restart-later"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.defer_language_restart(cx)),
                                )
                                .on_key_down(cx.listener(
                                    |this, event: &gpui::KeyDownEvent, _, cx| {
                                        if activates(event) {
                                            this.defer_language_restart(cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                )),
                        ),
                    )
                    .child(
                        self.application_target(
                            || SettingsTarget::General(GeneralTarget::LanguageRestartNow),
                            widgets::primary_button("language-restart-now")
                                .debug_selector(|| "language-restart-now".into())
                                .aria_label(tr("settings-language-restart-now"))
                                .tab_index(4)
                                .child(tr("settings-language-restart-now"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.restart_for_language(cx)),
                                )
                                .on_key_down(cx.listener(
                                    |this, event: &gpui::KeyDownEvent, _, cx| {
                                        if activates(event) {
                                            this.restart_for_language(cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                )),
                        ),
                    ),
            )
    }
}

fn activates(event: &gpui::KeyDownEvent) -> bool {
    !event.keystroke.modifiers.modified()
        && matches!(event.keystroke.key.as_str(), "enter" | "space" | " ")
}

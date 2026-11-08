//! General settings presentation; persisted values and element IDs stay stable.
use super::*;
use crate::ui::i18n::tr;

impl SettingsScreen {
    pub(super) fn render_general(
        &self,
        pane: gpui::Stateful<Div>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        pane.child(section_title(&tr("settings-defaults")))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::Appearance),
                self.setting_menu_row(
                    &tr("settings-appearance"),
                    &tr("settings-appearance-description"),
                    SettingMenu::Appearance,
                    window,
                    cx,
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::Language),
                self.setting_menu_row(
                    &crate::ui::i18n::tr("settings-language"),
                    &crate::ui::i18n::tr("settings-language-description"),
                    SettingMenu::Language,
                    window,
                    cx,
                ),
            ))
            .child(section_title(&tr("settings-chat-appearance")))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ChatFont),
                self.setting_menu_row(
                    &tr("settings-chat-font"),
                    &font_note(crate::ui::typography::ChatFontFamily::parse(
                        &self.settings.chat_font_family,
                    )),
                    SettingMenu::ChatFont,
                    window,
                    cx,
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ChatSize),
                self.setting_menu_row(
                    &tr("settings-text-size"),
                    &tr("settings-text-size-description"),
                    SettingMenu::ChatSize,
                    window,
                    cx,
                ),
            ))
            .child(self.chat_appearance_preview())
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ToolDetails),
                localized_toggle_row(
                    "show-tool-call-details",
                    &tr("settings-tool-details"),
                    &tr("settings-tool-details-description"),
                    self.settings.tool_details,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_tool_details(cx);
                    }),
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::Notifications),
                localized_toggle_row(
                    "desktop-notifications",
                    &tr("settings-notifications"),
                    &tr("settings-notifications-description"),
                    self.settings.desktop_notifications,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_desktop_notifications(cx);
                    }),
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::PasswordNotificationDetails),
                localized_toggle_row(
                    "show-task-details-in-password-mode",
                    &tr("settings-password-details"),
                    &tr("settings-password-details-description"),
                    self.settings.password_notification_details,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_password_notification_details(cx);
                    }),
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ReduceMotion),
                localized_toggle_row(
                    "reduce-motion",
                    &tr("settings-reduce-motion"),
                    &tr("settings-reduce-motion-description"),
                    self.settings.reduce_motion,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_reduce_motion(cx);
                    }),
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ToolSummaries),
                localized_toggle_row(
                    "summarize-tool-calls",
                    &tr("settings-tool-summaries"),
                    &tr("settings-tool-summaries-description"),
                    self.settings.tool_summaries,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_tool_summaries(cx);
                    }),
                ),
            ))
            .child(section_title(&tr("settings-editing")))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ComposerVim),
                localized_toggle_row(
                    "vim-mode-in-composer",
                    &tr("settings-composer-vim"),
                    &tr("settings-composer-vim-description"),
                    self.settings.composer_vim_enabled,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_composer_vim(cx);
                    }),
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::ApplicationVim),
                localized_toggle_row(
                    "vim-navigation-across-the-app",
                    &tr("settings-application-vim"),
                    &tr("settings-application-vim-description"),
                    self.settings.application_vim_enabled,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_application_vim(cx);
                    }),
                ),
            ))
            .child(section_title(&tr("settings-voice")))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::Voice),
                self.setting_menu_row(
                    &tr("settings-speech-voice"),
                    &tr("settings-speech-voice-description"),
                    SettingMenu::Voice,
                    window,
                    cx,
                ),
            ))
            .child(self.application_target(
                || SettingsTarget::General(GeneralTarget::SpeechSpeed),
                self.setting_menu_row(
                    &tr("settings-speech-speed"),
                    &tr("settings-speech-speed-description"),
                    SettingMenu::SpeechSpeed,
                    window,
                    cx,
                ),
            ))
    }
    fn chat_appearance_preview(&self) -> Div {
        let family = crate::ui::typography::ChatFontFamily::parse(&self.settings.chat_font_family);
        let size = crate::ui::typography::clamp_chat_font_size(self.settings.chat_font_size);
        let speaker = |name: gpui::SharedString| {
            div()
                .text_xs()
                .font_family(crate::assets::FONT_BODY)
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(gpui::rgb(theme::text_muted()))
                .child(name)
        };
        // The settings pane and user bubbles both sit on elevated fills in
        // dark mode. Recess a real chat well (`bg_app`) so assistant text
        // and the user bubble separate from the card, matching the transcript.
        widgets::card_row()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .font_family(crate::assets::FONT_BODY)
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::rgb(theme::text_muted()))
                    .flex()
                    .flex_wrap()
                    .child(tr("settings-preview"))
                    .child(format!(" · {} · {size} px", font_label(family))),
            )
            .child(
                div()
                    .w_full()
                    .rounded(theme::RADIUS_MD)
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .bg(gpui::rgb(if theme::is_light() {
                        theme::bg_sidebar()
                    } else {
                        theme::bg_app()
                    }))
                    .px_3()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        crate::ui::typography::chat_reading(div())
                            .max_w(gpui::relative(0.82))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(speaker(tr("settings-preview-assistant")))
                            .child(tr("settings-preview-text"))
                            .child(
                                div()
                                    .font_weight(crate::ui::typography::emphasis_weight())
                                    .child(tr("settings-preview-bold")),
                            ),
                    )
                    .child(
                        div().w_full().flex().justify_end().child(
                            crate::ui::typography::chat_reading(div())
                                .px_3()
                                .py_2()
                                .rounded(theme::RADIUS_MD)
                                .bg(gpui::rgb(theme::bg_user_bubble()))
                                .border_1()
                                .border_color(gpui::rgb(theme::user_bubble_border()))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_color(gpui::rgb(theme::text_primary()))
                                .child(speaker(tr("settings-preview-you")))
                                .child(tr("settings-preview-reply")),
                        ),
                    ),
            )
    }
}

fn localized_toggle_row(
    slug: &'static str,
    title: &str,
    description: &str,
    on: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    widgets::card_row()
        .id(gpui::SharedString::from(format!("setting-{slug}")))
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .hover(|style| style.cursor_pointer())
        .on_click(on_click)
        .child(setting_copy(title, description))
        .child(widgets::switch(
            gpui::SharedString::from(format!("setting-switch-{slug}")),
            on,
        ))
}
pub(super) fn theme_label(value: theme::Preference) -> gpui::SharedString {
    match value {
        theme::Preference::System => tr("settings-theme-label-system"),
        theme::Preference::Dark => tr("settings-theme-label-dark"),
        theme::Preference::Light => tr("settings-theme-label-light"),
    }
}
pub(super) fn font_label(value: crate::ui::typography::ChatFontFamily) -> gpui::SharedString {
    match value {
        crate::ui::typography::ChatFontFamily::System => tr("settings-font-label-system"),
        crate::ui::typography::ChatFontFamily::Manrope => tr("settings-font-label-manrope"),
        crate::ui::typography::ChatFontFamily::Geist => tr("settings-font-label-geist"),
        crate::ui::typography::ChatFontFamily::Serif => tr("settings-font-label-serif"),
    }
}
pub(super) fn font_note(value: crate::ui::typography::ChatFontFamily) -> gpui::SharedString {
    match value {
        crate::ui::typography::ChatFontFamily::System => tr("settings-font-note-system"),
        crate::ui::typography::ChatFontFamily::Manrope => tr("settings-font-note-manrope"),
        crate::ui::typography::ChatFontFamily::Geist => tr("settings-font-note-geist"),
        crate::ui::typography::ChatFontFamily::Serif => tr("settings-font-note-serif"),
    }
}
pub(super) fn voice_label(id: &str) -> String {
    match id {
        "neutral_female" => tr("settings-voice-neutral-female").to_string(),
        "neutral_male" => tr("settings-voice-neutral-male").to_string(),
        "casual_female" => tr("settings-voice-casual-female").to_string(),
        "casual_male" => tr("settings-voice-casual-male").to_string(),
        "cheerful_female" => tr("settings-voice-cheerful-female").to_string(),
        "ar_male" => tr("settings-voice-ar-male").to_string(),
        "de_female" => tr("settings-voice-de-female").to_string(),
        "de_male" => tr("settings-voice-de-male").to_string(),
        "es_female" => tr("settings-voice-es-female").to_string(),
        "es_male" => tr("settings-voice-es-male").to_string(),
        "fr_female" => tr("settings-voice-fr-female").to_string(),
        "fr_male" => tr("settings-voice-fr-male").to_string(),
        "hi_female" => tr("settings-voice-hi-female").to_string(),
        "hi_male" => tr("settings-voice-hi-male").to_string(),
        "it_female" => tr("settings-voice-it-female").to_string(),
        "it_male" => tr("settings-voice-it-male").to_string(),
        "nl_female" => tr("settings-voice-nl-female").to_string(),
        "nl_male" => tr("settings-voice-nl-male").to_string(),
        "pt_female" => tr("settings-voice-pt-female").to_string(),
        "pt_male" => tr("settings-voice-pt-male").to_string(),
        _ => id.to_owned(),
    }
}

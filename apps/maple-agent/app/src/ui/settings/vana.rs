//! Vana's local MCP setup. Vana owns consent; Maple only displays public
//! endpoint and definition metadata, never OAuth credentials.

use gpui::{Context, Div, FontWeight, div, prelude::*, px};

use super::{SettingsScreen, SettingsTarget};
use crate::ui::{icons::icon, theme, widgets};
use maple_agent::store::records::{McpDefinition, McpTarget};

fn chip(label: &'static str) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .text_xs()
        .text_color(gpui::rgb(theme::text_secondary()))
        .child(label)
}

fn vana_wordmark() -> gpui::Svg {
    gpui::svg()
        .path("icons/vana-mark.svg")
        .w(px(100.))
        .h(px(100. * 478. / 1716.))
        .text_color(gpui::rgb(theme::text_primary()))
}

impl SettingsScreen {
    /// A curated card identifies the exact saved Vana MCP name. Other MCP
    /// servers, including user-provided lookalike descriptions, stay custom.
    pub(super) fn vana_server(&self) -> Option<&McpDefinition> {
        self.mcp_servers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|server| server.name.eq_ignore_ascii_case("vana"))
    }

    pub(super) fn open_vana_details(&mut self, cx: &mut Context<Self>) {
        if self.vana_connecting {
            return;
        }
        self.vana_details_open = !self.vana_details_open;
        if self.vana_details_open
            && let Some(McpDefinition {
                target: McpTarget::StreamableHttp { url },
                ..
            }) = self.vana_server()
            && validate_local_endpoint(url).is_ok()
        {
            let endpoint = url.clone();
            self.vana_endpoint
                .update(cx, |input, cx| input.set_text(&endpoint, cx));
        }
        self.mcp_editor = None;
        self.vana_notice = None;
        self.reconcile_application_vim_target();
        cx.notify();
    }

    pub(super) fn render_vana_catalog_card(&self, cx: &mut Context<Self>) -> Div {
        let server = self.vana_server();
        let (status, status_color) = match (self.mcp_servers.is_some(), server) {
            (false, _) => ("Loading…", theme::text_muted()),
            (_, Some(server)) if server.enabled => {
                ("Default on for new chats", theme::status_success())
            }
            (_, Some(_)) => ("Configured · choose per chat", theme::text_muted()),
            (_, None) => ("Not connected", theme::text_muted()),
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .rounded(theme::RADIUS_LG)
            .bg(gpui::rgb(theme::bg_sidebar_card()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(icon("puzzle", px(14.), theme::text_muted()))
                            .child("FEATURED INTEGRATION"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .text_xs()
                            .text_color(gpui::rgb(status_color))
                            .child(div().size(px(6.)).rounded_full().bg(gpui::rgb(status_color)))
                            .child(status),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_5()
                    .child(
                        div()
                            .flex_none()
                            .w(px(136.))
                            .h(px(88.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_MD)
                            .bg(gpui::rgb(theme::bg_elevated()))
                            .border_1()
                            .border_color(gpui::rgb(theme::border_subtle()))
                            .child(vana_wordmark()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(gpui::rgb(theme::text_heading()))
                                    .child("Your context. Your permissions."),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(gpui::rgb(theme::text_secondary()))
                                    .child("Bring context from your personal Vana vault into Maple. Get answers that understand your interests, goals, and routines."),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1p5()
                            .child(chip("Local connection"))
                            .child(chip("Vana permissions"))
                            .child(chip("MCP")),
                    )
                    .child(self.application_target(
                        || SettingsTarget::VanaDetails,
                        widgets::secondary_button("vana-catalog-details")
                            .tab_index(0)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.open_vana_details(cx);
                            }))
                            .child(if server.is_some() { "Manage Vana" } else { "Connect Vana" })
                            .child(icon("chevron-right", px(14.), theme::text_primary())),
                    )),
            )
    }

    pub(super) fn render_vana_details(&self, cx: &mut Context<Self>) -> Div {
        let server = self.vana_server();
        let mut content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(theme::RADIUS_LG)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .child(vana_wordmark())
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(gpui::rgb(theme::text_heading()))
                                    .child("Connect your local vault"),
                            ),
                    )
                    .child(
                        widgets::icon_button(
                            "vana-details-close",
                            "x",
                            "Close Vana details",
                            px(16.),
                            theme::text_muted(),
                        )
                        .tab_index(0)
                        .when(self.vana_connecting, |button| button.opacity(0.5))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.open_vana_details(cx);
                        })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child("Keep Vana Desktop open on this computer. In Vana, go to Settings → Server → Connect apps and copy the Local MCP endpoint."),
            );
        if self.vana_local_available {
            content = content.child(
                self.application_target(
                    || SettingsTarget::VanaEndpoint,
                    div()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(gpui::rgb(theme::text_primary()))
                                .child("Local MCP endpoint"),
                        )
                        .child(if self.vana_connecting {
                            widgets::input_frame()
                                .text_sm()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child("Waiting for Vana authorization…")
                        } else {
                            widgets::input_frame()
                                .text_sm()
                                .child(self.vana_endpoint.clone())
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(
                                    "Paste the complete address from Vana. Its port can change.",
                                ),
                        ),
                ),
            );
        } else {
            content = content.child(
                div()
                    .p_3()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::rgb(theme::bg_sidebar()))
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child("Choose This computer before connecting Vana. A local vault cannot be connected from a remote computer."),
            );
        }
        content = content
            .child(vault_row(
                "shield-check",
                "Choose what Maple can read in Vana",
                "Vana will show the requested access. Select the sources you want to share, then choose Approve and return.",
            ))
            .child(vault_row(
                "lock",
                "A direct connection on this computer",
                "MCP reads use the local endpoint and work with Vana’s remote tunnel off. Account and authorization services may still require internet access.",
            ))
            .child(
                div()
                    .p_3()
                    .rounded(theme::RADIUS_SM)
                    .bg(gpui::rgb(theme::bg_sidebar()))
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child("Choose Vana in each chat’s Integrations menu. To revoke access, open Vana Settings → Server → Connected clients and select Disconnect."),
            );
        if let Some(notice) = &self.vana_notice {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(notice.clone()),
            );
        }
        let mut actions = div().flex().items_center().gap_2();
        if let Some(server) = server {
            let name = server.name.clone();
            actions = actions.child(
                self.application_target(
                    || SettingsTarget::VanaToggle,
                    widgets::secondary_button("vana-default-toggle")
                        .tab_index(0)
                        .when(self.mcp_saving || self.vana_connecting, |button| {
                            button.opacity(0.5)
                        })
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            if !this.mcp_saving {
                                this.toggle_mcp_server(&name, cx);
                            }
                        }))
                        .child(if server.enabled {
                            "Turn off for new chats"
                        } else {
                            "Use by default in new chats"
                        }),
                ),
            );
        }
        actions = actions.child(
            self.application_target(
                || SettingsTarget::VanaConnect,
                widgets::primary_button("vana-connect")
                    .tab_index(0)
                    .when(
                        self.vana_connecting || self.mcp_saving || !self.vana_local_available,
                        |button| button.opacity(0.5),
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.start_vana_connection(cx);
                    }))
                    .child(if self.vana_connecting {
                        "Connecting…"
                    } else if server.is_some() {
                        "Reconnect Vana"
                    } else {
                        "Connect Vana"
                    }),
            ),
        );
        content.child(actions)
    }

    pub(super) fn start_vana_connection(&mut self, cx: &mut Context<Self>) {
        if self.vana_connecting {
            return;
        }
        if !self.vana_local_available {
            self.vana_notice = Some("Choose This computer before connecting Vana.".to_string());
            cx.notify();
            return;
        }
        if self.mcp_saving {
            self.vana_notice =
                Some("Wait for the current MCP change to finish, then connect Vana.".to_string());
            cx.notify();
            return;
        }
        let endpoint = self.vana_endpoint.read(cx).text().trim().to_string();
        if let Err(message) = validate_local_endpoint(&endpoint) {
            self.vana_notice = Some(message.to_string());
            cx.notify();
            return;
        }
        self.vana_connecting = true;
        self.vana_notice = Some("Your browser will open Vana. Choose the sources to share and approve the connection there.".to_string());
        cx.notify();
        let backend = self.integrations_backend.clone();
        let user_id = self.user_id.clone();
        self.call(
            async move { backend.connect_vana_local(&user_id, endpoint).await },
            cx,
            |this, result, cx| {
                this.vana_connecting = false;
                match result {
                    Ok(servers) => {
                        this.mcp_servers = Some(servers);
                        this.vana_notice = Some("Vana is configured. Choose it in your chat’s Integrations menu when you want to use the access you approved.".to_string());
                    }
                    Err(message) => this.vana_notice = Some(message),
                }
                this.reconcile_application_vim_target();
                cx.notify();
            },
        );
    }
}

/// A UI hint only. The host repeats endpoint validation before making requests.
fn validate_local_endpoint(endpoint: &str) -> Result<(), &'static str> {
    const MESSAGE: &str = "Paste Vana’s Local MCP endpoint: http://127.0.0.1:PORT/mcp.";
    let url = reqwest::Url::parse(endpoint).map_err(|_| MESSAGE)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.path() != "/mcp"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MESSAGE);
    }
    Ok(())
}

fn vault_row(mark: &'static str, title: &'static str, description: &'static str) -> Div {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_none()
                .size(px(32.))
                .rounded(theme::RADIUS_SM)
                .bg(gpui::rgb(theme::bg_sidebar_card()))
                .flex()
                .items_center()
                .justify_center()
                .child(icon(mark, px(18.), theme::text_secondary())),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(description),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::validate_local_endpoint;

    #[test]
    fn local_endpoint_keeps_the_copied_port_and_rejects_remote_or_credential_urls() {
        assert!(validate_local_endpoint("http://127.0.0.1:47321/mcp").is_ok());
        assert!(validate_local_endpoint("http://127.0.0.1:57123/mcp").is_ok());
        for endpoint in [
            "https://0x123.server.vana.org/mcp",
            "http://example.com:47321/mcp",
            "http://localhost:47321/mcp",
            "http://127.0.0.1/mcp",
            "http://127.0.0.1:0/mcp",
            "http://user:secret@127.0.0.1:47321/mcp",
            "http://127.0.0.1:47321/mcp?token=secret",
            "http://127.0.0.1:47321/mcp#fragment",
            "http://127.0.0.1:47321/other",
        ] {
            assert!(validate_local_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }
}

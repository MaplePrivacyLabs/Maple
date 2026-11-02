//! Maple Work is not available on the Free plan. A signed-in account that
//! the billing service reports as Free sees this dialog in place of its
//! tasks. It sends the user to the website to upgrade, checks the plan
//! again, and signs out. The desktop decides when it shows.

use gpui::{Context, EventEmitter, FocusHandle, Render, SharedString, Window, div, prelude::*, px};

use super::{theme, widgets};

pub const TITLE: &str = "Maple Work isn't available on the Free plan";
const BODY: &str = "Free use isn't available at this time. Upgrade your plan on the Maple \
                    website, then check again.";
const GUEST_BODY: &str = "On the website, choose Log in as Anonymous and enter this Account ID \
                          and your password. Anonymous accounts pay with Bitcoin.";
pub const STILL_FREE: &str = "Your account is still on the Free plan.";
pub const CHECK_FAILED: &str = "Couldn't check your plan. Try again.";

/// What the gate asks of the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanGateEvent {
    CheckAgain,
    SignOut,
}

pub struct PlanGate {
    /// Set for an anonymous account: it has no email and signs in on the
    /// website with this id.
    guest_account_id: Option<SharedString>,
    checking: bool,
    signing_out: bool,
    notice: Option<SharedString>,
    focus: FocusHandle,
    focus_pending: bool,
}

impl EventEmitter<PlanGateEvent> for PlanGate {}

impl PlanGate {
    pub fn new(guest_account_id: Option<String>, cx: &mut Context<Self>) -> Self {
        Self {
            guest_account_id: guest_account_id.map(Into::into),
            checking: false,
            signing_out: false,
            notice: None,
            focus: cx.focus_handle(),
            focus_pending: true,
        }
    }

    pub fn set_checking(&mut self, cx: &mut Context<Self>) {
        self.checking = true;
        self.notice = None;
        cx.notify();
    }

    /// The plan is unchanged. `notice` answers a Check again.
    pub fn finish_check(&mut self, notice: Option<&'static str>, cx: &mut Context<Self>) {
        self.checking = false;
        if notice.is_some() {
            self.notice = notice.map(Into::into);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn guest_account_id(&self) -> Option<&str> {
        self.guest_account_id.as_deref()
    }

    pub fn sign_out_failed(&mut self, cx: &mut Context<Self>) {
        self.signing_out = false;
        cx.notify();
    }

    fn open_website(&mut self, cx: &mut Context<Self>) {
        self.notice = Some(
            match crate::backend::open_in_browser(crate::billing::PRICING_URL, "pricing") {
                Ok(()) => "Opened the pricing page in your browser.".into(),
                Err(message) => message.into(),
            },
        );
        cx.notify();
    }

    fn render_guest_id(&self, id: SharedString, cx: &Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(GUEST_BODY),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("plan-gate-account-id")
                            .debug_selector(|| "plan-gate-account-id".to_string())
                            .text_xs()
                            .font_family(crate::assets::FONT_MONO)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(id.clone()),
                    )
                    .child(widgets::copy_button(
                        "plan-gate-copy-id",
                        id,
                        None,
                        Some(cx.entity_id()),
                    )),
            )
    }
}

impl Render for PlanGate {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        let busy = self.checking || self.signing_out;
        let width = (window.viewport_size().width - px(48.)).clamp(px(0.), px(500.));
        let button =
            |button: gpui::Stateful<gpui::Div>, selector: &'static str, label: &'static str| {
                button
                    .debug_selector(move || selector.to_string())
                    .aria_label(label)
                    .py_1p5()
                    .when(busy, |button| button.opacity(0.6))
                    .child(label)
            };
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgb(theme::bg_app()))
            .child(crate::ui::titlebar::drag_strip())
            .child(
                div()
                    .id("plan-gate")
                    .debug_selector(|| "plan-gate".to_string())
                    .role(gpui::Role::Dialog)
                    .aria_label(TITLE)
                    .track_focus(&self.focus)
                    .w(width)
                    .p_5()
                    .rounded(theme::RADIUS_XL)
                    .shadow_lg()
                    .bg(gpui::rgb(theme::bg_elevated()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border()))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(TITLE),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(BODY),
                    )
                    .when_some(self.guest_account_id.clone(), |card, id| {
                        card.child(self.render_guest_id(id, cx))
                    })
                    .when_some(self.notice.clone(), |card, notice| {
                        card.child(
                            widgets::banner(theme::bg_sidebar_pill())
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(notice),
                        )
                    })
                    .child(
                        // Sign out stays apart from the plan actions; on a
                        // narrow window the two groups wrap rather than spill.
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .mt_1()
                            .child(
                                button(
                                    widgets::ghost_button("plan-gate-sign-out"),
                                    "plan-gate-sign-out",
                                    if self.signing_out {
                                        "Signing out…"
                                    } else {
                                        "Sign out"
                                    },
                                )
                                .when(!busy, |button| {
                                    button.on_click(cx.listener(|this, _, _, cx| {
                                        this.signing_out = true;
                                        this.notice = None;
                                        cx.emit(PlanGateEvent::SignOut);
                                        cx.notify();
                                    }))
                                }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        button(
                                            widgets::secondary_button("plan-gate-check"),
                                            "plan-gate-check",
                                            if self.checking {
                                                "Checking…"
                                            } else {
                                                "Check again"
                                            },
                                        )
                                        .when(
                                            !busy,
                                            |button| {
                                                button.on_click(cx.listener(|_, _, _, cx| {
                                                    cx.emit(PlanGateEvent::CheckAgain);
                                                }))
                                            },
                                        ),
                                    )
                                    .child(
                                        button(
                                            widgets::primary_button("plan-gate-website"),
                                            "plan-gate-website",
                                            "Upgrade on the website",
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.open_website(cx)),
                                        ),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, Modifiers};

    struct Host {
        gate: Entity<PlanGate>,
        events: Vec<PlanGateEvent>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(self.gate.clone())
        }
    }

    fn show<'a>(
        guest: Option<&str>,
        cx: &'a mut gpui::TestAppContext,
    ) -> (Entity<Host>, &'a mut gpui::VisualTestContext) {
        let guest = guest.map(str::to_string);
        let (host, cx) = cx.add_window_view(move |_, cx| {
            let gate = cx.new(|cx| PlanGate::new(guest, cx));
            cx.subscribe(&gate, |host: &mut Host, _, event: &PlanGateEvent, _| {
                host.events.push(*event)
            })
            .detach();
            Host {
                gate,
                events: Vec::new(),
            }
        });
        cx.simulate_resize(gpui::size(px(900.), px(700.)));
        (host, cx)
    }

    fn click(selector: &'static str, cx: &mut gpui::VisualTestContext) {
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_guest_gets_the_account_id_to_sign_in_on_the_website(cx: &mut gpui::TestAppContext) {
        let (_host, cx) = show(Some("guest-account-id"), cx);
        assert!(cx.debug_bounds("plan-gate-account-id").is_some());
        for selector in ["plan-gate-website", "plan-gate-check", "plan-gate-sign-out"] {
            assert!(cx.debug_bounds(selector).is_some(), "{selector}");
        }
    }

    #[gpui::test]
    fn an_email_account_gets_no_account_id(cx: &mut gpui::TestAppContext) {
        let (_host, cx) = show(None, cx);
        assert!(cx.debug_bounds("plan-gate").is_some());
        assert!(cx.debug_bounds("plan-gate-account-id").is_none());
    }

    #[gpui::test]
    fn the_actions_stay_inside_the_dialog(cx: &mut gpui::TestAppContext) {
        let (_host, cx) = show(Some("guest-account-id"), cx);
        for (width, height) in [(1280., 800.), (640., 600.), (360., 600.)] {
            cx.simulate_resize(gpui::size(px(width), px(height)));
            let card = cx.debug_bounds("plan-gate").unwrap();
            assert!(
                card.left() >= px(0.) && card.right() <= px(width),
                "{card:?}"
            );
            for selector in ["plan-gate-sign-out", "plan-gate-check", "plan-gate-website"] {
                let action = cx.debug_bounds(selector).unwrap();
                assert!(
                    action.left() >= card.left() && action.right() <= card.right(),
                    "{selector} at {width}: {action:?} outside {card:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn check_again_and_sign_out_go_to_the_desktop_once(cx: &mut gpui::TestAppContext) {
        let (host, cx) = show(None, cx);
        click("plan-gate-check", cx);
        let gate = host.read_with(cx, |host, _| host.gate.clone());
        gate.update(cx, |gate, cx| gate.set_checking(cx));
        cx.run_until_parked();
        // A check in flight ignores more presses.
        click("plan-gate-check", cx);
        click("plan-gate-sign-out", cx);
        gate.update(cx, |gate, cx| gate.finish_check(Some(STILL_FREE), cx));
        cx.run_until_parked();
        click("plan-gate-sign-out", cx);
        click("plan-gate-sign-out", cx);
        host.read_with(cx, |host, _| {
            assert_eq!(
                host.events,
                [PlanGateEvent::CheckAgain, PlanGateEvent::SignOut]
            );
        });
    }
}

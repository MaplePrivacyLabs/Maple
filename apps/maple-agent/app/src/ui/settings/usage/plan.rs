//! What the plan card adds for accounts with the full page: credits used of
//! the allowance, the team's split, and where the pace leads. The web's
//! `UsagePlanCard.tsx`, set inside the plan card the Usage page shows every
//! account.

use super::*;

impl SettingsScreen {
    /// Lines under the plan bar: `4,158 of 7,000 plan credits used` with
    /// what's left or the team's split, then where the pace leads. `None`
    /// until the billing status arrives, or when the plan has no allowance.
    pub(in super::super) fn usage_plan_details(&self) -> Option<Div> {
        let data = self.usage.data.as_ref()?;
        let plan = &data.plan;
        if plan.allowance <= 0.0 {
            return None;
        }
        let team = plan.kind == PlanKind::Team;
        let exhausted = plan.used >= plan.allowance;
        let amount = self.messages.get(
            if team {
                "settings-usage-team-amount"
            } else {
                "settings-usage-plan-amount"
            },
            [
                ("used", fmt_count(plan.used).into()),
                ("allowance", fmt_count(plan.allowance).into()),
            ],
        );
        let aside: Option<SharedString> = match (team, plan.team_you) {
            (true, Some(you)) => {
                let you = you.min(plan.used);
                Some(
                    format!(
                        "{} · {}",
                        self.messages.get(
                            "settings-usage-team-you",
                            [("credits", fmt_count(you).into())],
                        ),
                        self.messages.get(
                            "settings-usage-team-rest",
                            [("credits", fmt_count((plan.used - you).max(0.0)).into())],
                        ),
                    )
                    .into(),
                )
            }
            (true, None) => None,
            // The pace sentence already says the plan ran out.
            (false, _) if exhausted => None,
            (false, _) => Some(self.messages.get(
                "settings-usage-left",
                [("left", fmt_count(plan.allowance - plan.used).into())],
            )),
        };
        Some(
            div()
                .debug_selector(|| "usage-plan-details".into())
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_between()
                        .gap_x_4()
                        .gap_y_1()
                        .child(
                            div()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(gpui::text!(id = "usage-plan-amount", amount.to_string())),
                        )
                        .children(aside.map(|aside| {
                            div()
                                .text_color(gpui::rgb(theme::text_muted()))
                                .child(aside)
                        })),
                )
                .children(self.pace_text(plan).map(|text| {
                    div()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(gpui::text!(id = "usage-pace", text.to_string()))
                }))
                .when(team, |details| {
                    details.child(
                        div()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(tr("settings-usage-team-footer")),
                    )
                }),
        )
    }

    /// Where the pace leads: when the plan runs out, or where it ends up.
    /// `None` while less than a credit a day has been used, when the bar
    /// says enough and "About 0 credits a day" would read wrong.
    pub(super) fn pace_text(&self, plan: &PlanData) -> Option<SharedString> {
        let progress = plan.progress.as_ref()?;
        if progress.pace < 0.5 && !progress.exhausted {
            return None;
        }
        let reset = plan
            .window
            .as_ref()
            .map(|window| self.short_date(model::local_date(window.end)))
            .unwrap_or_default();
        let team = plan.kind == PlanKind::Team;
        let pace = fmt_count(progress.pace);
        let pace_count = progress.pace.round() as i64;
        let balance = self
            .billing
            .status
            .as_ref()
            .and_then(|status| status.api_credit_balance)
            .unwrap_or(0);
        Some(if progress.exhausted {
            self.messages.get(
                if team {
                    "settings-usage-pace-team-exhausted"
                } else if balance > 0 {
                    "settings-usage-pace-exhausted-extra"
                } else {
                    "settings-usage-pace-exhausted"
                },
                [("reset", reset.to_string().into())],
            )
        } else if let Some(run_out) = progress.run_out {
            self.messages.get(
                if team {
                    "settings-usage-pace-team-runs-out"
                } else if balance > 0 {
                    "settings-usage-pace-runs-out-extra"
                } else {
                    "settings-usage-pace-runs-out"
                },
                [
                    ("pace", pace.into()),
                    ("count", pace_count.into()),
                    ("allowance", fmt_count(progress.allowance).into()),
                    ("date", self.short_date(run_out).to_string().into()),
                ],
            )
        } else {
            self.messages.get(
                "settings-usage-pace-on-track",
                [
                    ("pace", pace.into()),
                    ("count", pace_count.into()),
                    ("projected", fmt_count(progress.projected).into()),
                    ("allowance", fmt_count(progress.allowance).into()),
                    ("reset", reset.to_string().into()),
                ],
            )
        })
    }
}

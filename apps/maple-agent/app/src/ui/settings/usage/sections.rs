//! The page's later sections: the last twelve months, recent requests,
//! current model prices and the notes on what the numbers mean. The web's
//! `UsageSections.tsx`.

use super::*;

impl SettingsScreen {
    pub(super) fn render_usage_months(&self, months: &[MonthBucket]) -> Div {
        let used: Vec<Origin> = Origin::ALL
            .into_iter()
            .filter(|origin| {
                months
                    .iter()
                    .any(|month| month.origins[origin.index()] > 0.0)
            })
            .collect();
        let peak = months.iter().map(MonthBucket::credits).fold(0.0, f64::max);
        let scale = model::nice_scale(peak.max(1.0) * 1.04);
        let columns = months
            .iter()
            .map(|month| chart::Column {
                segments: used
                    .iter()
                    .map(|origin| (palette::origin(*origin), month.origins[origin.index()]))
                    .collect(),
                total: month.credits(),
                tick: month_short(month.month),
            })
            .collect();
        let data = self.usage.data.clone();
        let tip: Rc<dyn Fn(usize) -> Tip> = Rc::new(move |index| month_tip(data.as_deref(), index));
        let figure = chart::bars(
            chart::Bars {
                id: "usage-months",
                label: tr("settings-usage-months-label"),
                columns,
                axis: scale
                    .ticks
                    .iter()
                    .map(|tick| fmt_compact(*tick).into())
                    .collect(),
                scale,
                height: px(170.),
                max_bar_width: px(34.),
                label_every: 1,
                reference: None,
                marker: None,
                span: None,
            },
            tip,
        );
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(card_heading(
                tr("settings-usage-months-title"),
                tr("settings-usage-months-description"),
            ))
            .child(
                widgets::card_row()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(figure)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x_4()
                            .gap_y_1()
                            .children(used.iter().map(|origin| {
                                legend_item(palette::origin(*origin), origin_name(*origin))
                            })),
                    ),
            )
    }

    pub(super) fn render_usage_requests(&self, data: &UsageData, cx: &mut Context<Self>) -> Div {
        let requests = &self.usage.requests;
        let kind = data.plan.kind;
        let heading = card_heading(
            tr("settings-usage-requests-title"),
            tr("settings-usage-requests-description"),
        );
        let mut section = div().flex().flex_col().gap_3().child(heading);
        if !requests.loaded {
            return section.child(if requests.failed && !requests.loading {
                self.retry_block(
                    "usage-requests-error",
                    tr("settings-usage-requests-error"),
                    9,
                    |this, cx| this.load_usage_requests(true, cx),
                    cx,
                )
            } else {
                loading_block(
                    "usage-requests-loading",
                    tr("settings-usage-requests-loading"),
                )
            });
        }
        if requests.failed && !requests.loading {
            // A reload failed; the rows below are from an earlier one.
            section = section.child(self.retry_block(
                "usage-requests-error",
                tr("settings-usage-requests-error"),
                9,
                |this, cx| this.load_usage_requests(true, cx),
                cx,
            ));
        }
        if requests.rows.is_empty() {
            return section.child(empty_note(tr("settings-usage-requests-empty")));
        }
        // `None` means there is no model chart (another split or no range).
        // Some(empty) means a model chart is drawn and every row is folded.
        let named_models: Option<Vec<&str>> = match &data.range {
            Some(range) if self.usage.split == Split::Model => Some(
                range
                    .chart
                    .series
                    .iter()
                    .filter_map(|series| series.model.as_deref())
                    .collect(),
            ),
            _ => None,
        };
        let now = chrono::Local::now();
        let header = table_row(
            [
                (tr("settings-usage-col-time"), Column::Time),
                (tr("settings-usage-col-model"), Column::Name),
                (tr("settings-usage-col-source"), Column::Source),
                (tr("settings-usage-col-input"), Column::Number),
                (tr("settings-usage-col-cached"), Column::Number),
                (tr("settings-usage-col-output"), Column::Number),
                (tr("settings-usage-col-credits"), Column::Number),
            ]
            .into_iter()
            .map(|(label, column)| (column, header_cell(label))),
        );
        let mut rows: Vec<gpui::AnyElement> = vec![header.into_any_element()];
        rows.extend(
            requests
                .rows
                .iter()
                .zip(&requests.times)
                .map(|(request, time)| {
                    let origin = Origin::of(&request.source, request.client_app.as_deref());
                    let source: SharedString = match origin {
                        Origin::Api => tr("settings-usage-source-api-key"),
                        origin if request.source == "api" => self.messages.get(
                            "settings-usage-source-with-key",
                            [("app", origin_name(origin).to_string().into())],
                        ),
                        origin => origin_name(origin),
                    };
                    let label = model::model_label(&request.model, &requests.labels);
                    let color = if let Some(models) = named_models.as_deref() {
                        palette::model_among(&request.model, models, &data.model_ranking)
                    } else {
                        palette::model(&request.model, &data.model_ranking)
                    };
                    let extra = request.pool == "extra" && kind == PlanKind::Credits;
                    table_row([
                        (
                            Column::Time,
                            div()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(self.request_time(*time, now))
                                .into_any_element(),
                        ),
                        (
                            Column::Name,
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(chart::swatch(color, px(8.)))
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(gpui::rgb(theme::text_primary()))
                                        .child(label),
                                )
                                .into_any_element(),
                        ),
                        (
                            Column::Source,
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(7.))
                                        .rounded_full()
                                        .bg(gpui::rgb(palette::origin(origin))),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(gpui::rgb(theme::text_secondary()))
                                        .child(source),
                                )
                                .into_any_element(),
                        ),
                        (
                            Column::Number,
                            number_cell(fmt_count(request.uncached_input_tokens as f64)),
                        ),
                        (
                            Column::Number,
                            number_cell(
                                request
                                    .cached_input_tokens
                                    .map(|cached| fmt_count(cached as f64))
                                    .unwrap_or_else(|| "—".into()),
                            ),
                        ),
                        (
                            Column::Number,
                            number_cell(fmt_count(request.output_tokens as f64)),
                        ),
                        (
                            Column::Number,
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_1p5()
                                .when(extra, |cell| {
                                    cell.child(
                                        div()
                                            .px_1p5()
                                            .rounded(px(4.))
                                            .text_size(px(10.))
                                            .bg(gpui::rgb(theme::accent_container()))
                                            .text_color(gpui::rgb(theme::text_primary()))
                                            .child(tr("settings-usage-paid-extra")),
                                    )
                                })
                                .child(number_cell(fmt_count(request.credits)))
                                .into_any_element(),
                        ),
                    ])
                    .into_any_element()
                }),
        );
        section = section.child(widgets::card_stack(rows));
        if requests.more_failed {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::status_error()))
                    .child(tr("settings-usage-requests-more-error")),
            );
        }
        if requests.next_cursor.is_some() {
            let loading = requests.loading;
            section = section.child(
                div().flex().justify_center().child(
                    self.application_target(
                        || SettingsTarget::Billing(BillingTarget::Usage(UsageTarget::LoadMore)),
                        widgets::ghost_button("usage-load-more")
                            .debug_selector(|| "usage-load-more".into())
                            .tab_index(9)
                            .py_1p5()
                            .when(loading, |button| button.opacity(0.6))
                            .when(loading, |button| {
                                button.child(spinner(
                                    "usage-load-more-spinner",
                                    px(12.),
                                    theme::text_muted(),
                                ))
                            })
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.load_usage_requests(false, cx);
                            }))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                if super::super::activates(event) {
                                    cx.stop_propagation();
                                    this.load_usage_requests(false, cx);
                                }
                            }))
                            .child(tr("settings-usage-load-more")),
                    ),
                ),
            );
        }
        section
    }

    pub(super) fn render_usage_prices(&self, data: &UsageData, cx: &mut Context<Self>) -> Div {
        let heading = card_heading(
            tr("settings-usage-prices-title"),
            tr("settings-usage-prices-description"),
        );
        let section = div().flex().flex_col().gap_3().child(heading);
        let Some(prices) = &self.usage.prices else {
            return section.child(if self.usage.prices_failed && !self.usage.prices_loading {
                self.retry_block(
                    "usage-prices-error",
                    tr("settings-usage-prices-error"),
                    10,
                    Self::load_usage_prices,
                    cx,
                )
            } else {
                loading_block("usage-prices-loading", tr("settings-usage-prices-loading"))
            });
        };
        let header = table_row(
            [
                (tr("settings-usage-col-model"), Column::Name),
                (tr("settings-usage-col-input"), Column::Price),
                (tr("settings-usage-col-cached"), Column::Price),
                (tr("settings-usage-col-output"), Column::Price),
            ]
            .into_iter()
            .map(|(label, column)| (column, header_cell(label))),
        );
        let mut rows: Vec<gpui::AnyElement> = vec![header.into_any_element()];
        rows.extend(prices.models.iter().map(|price| {
            let name = if price.display_name.trim().is_empty() {
                price.id.clone()
            } else {
                price.display_name.clone()
            };
            table_row([
                (
                    Column::Name,
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(chart::swatch(
                            palette::model(&price.id, &data.model_ranking),
                            px(8.),
                        ))
                        .child(
                            div()
                                .truncate()
                                .text_color(gpui::rgb(theme::text_primary()))
                                .child(name),
                        )
                        .when(price.price_may_vary, |cell| {
                            cell.child(div().text_color(gpui::rgb(theme::text_muted())).child("*"))
                        })
                        .into_any_element(),
                ),
                (Column::Price, number_cell(fmt_price(price.input))),
                (
                    Column::Price,
                    match price.cached_input {
                        Some(cached) => number_cell(fmt_price(cached)),
                        None => div()
                            .text_right()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(tr("settings-usage-price-same-as-input"))
                            .into_any_element(),
                    },
                ),
                (Column::Price, number_cell(fmt_price(price.output))),
            ])
            .into_any_element()
        }));
        rows.extend(data.unpriced.iter().map(|(model, label)| {
            table_row([
                (
                    Column::Name,
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(chart::swatch(
                            palette::model(model, &data.model_ranking),
                            px(8.),
                        ))
                        .child(
                            div()
                                .truncate()
                                .text_color(gpui::rgb(theme::text_secondary()))
                                .child(label.clone()),
                        )
                        .into_any_element(),
                ),
                (Column::Price, number_cell("—".into())),
                (Column::Price, number_cell("—".into())),
                (Column::Price, number_cell("—".into())),
            ])
            .into_any_element()
        }));
        let mut notes = vec![tr("settings-usage-prices-note")];
        if !data.unpriced.is_empty() {
            notes.push(tr("settings-usage-prices-unlisted"));
        }
        if prices.models.iter().any(|price| price.price_may_vary) {
            notes.push(tr("settings-usage-prices-vary"));
        }
        section.child(widgets::card_stack(rows)).child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .children(notes.into_iter().map(|note| div().child(note))),
        )
    }

    pub(super) fn render_usage_about(&self, data: &UsageData) -> Div {
        let status = self.billing.status.as_ref();
        let allowance = data.plan.allowance;
        let credits_body: SharedString = match data.plan.kind {
            PlanKind::Team => self.messages.get(
                "settings-usage-about-credits-team",
                [("allowance", fmt_count(allowance).into())],
            ),
            _ => match status
                .and_then(|status| status.product_name.clone())
                .filter(|name| !name.trim().is_empty())
            {
                Some(plan) => self.messages.get(
                    "settings-usage-about-credits-plan",
                    [
                        ("plan", plan.into()),
                        ("allowance", fmt_count(allowance).into()),
                    ],
                ),
                None => self.messages.get(
                    "settings-usage-about-credits-unnamed",
                    [("allowance", fmt_count(allowance).into())],
                ),
            },
        };
        let notes: [(SharedString, SharedString); 9] = [
            (
                tr("settings-usage-about-account-title"),
                tr("settings-usage-about-account"),
            ),
            (
                tr("settings-usage-about-apps-title"),
                tr("settings-usage-about-apps"),
            ),
            (tr("settings-usage-about-credits-title"), credits_body),
            (
                tr("settings-usage-about-background-title"),
                tr("settings-usage-about-background"),
            ),
            (
                tr("settings-usage-about-auto-title"),
                tr("settings-usage-about-auto"),
            ),
            (
                tr("settings-usage-about-tokens-title"),
                tr("settings-usage-about-tokens"),
            ),
            (
                tr("settings-usage-about-prices-title"),
                tr("settings-usage-about-prices"),
            ),
            (
                tr("settings-usage-about-free-title"),
                tr("settings-usage-about-free"),
            ),
            (
                tr("settings-usage-about-zone-title"),
                self.messages.get(
                    "settings-usage-about-zone",
                    [("zone", self.usage.tz.clone().into())],
                ),
            ),
        ];
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(subsection_title(&tr("settings-usage-about-title")))
            .child(widgets::card_stack(
                notes
                    .into_iter()
                    .map(|(title, body)| super::super::setting_copy(title, body).into_any_element())
                    .collect::<Vec<gpui::AnyElement>>(),
            ))
    }

    /// An error line with a Try again button that runs `retry`.
    pub(super) fn retry_block(
        &self,
        id: &'static str,
        text: SharedString,
        tab_index: isize,
        retry: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_3()
            .rounded(theme::RADIUS_MD)
            .border_1()
            .border_color(gpui::rgb(theme::border_subtle()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(icon("circle-alert", px(14.), theme::status_error()))
                    .child(gpui::text!(id = id, text.to_string())),
            )
            .child(
                widgets::secondary_button(SharedString::from(format!("{id}-retry")))
                    .debug_selector(move || format!("{id}-retry"))
                    .flex_none()
                    .py_1()
                    .tab_index(tab_index)
                    .on_click(cx.listener(move |this, _event, _window, cx| retry(this, cx)))
                    .on_key_down(Self::on_press_key(retry, cx))
                    .child(tr("settings-usage-retry")),
            )
    }
}

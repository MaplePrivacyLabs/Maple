//! Usage history: the range picker, the range's totals over its daily
//! chart, and the breakdowns by model and by app. The web's
//! `UsageExplorer.tsx`.

use super::*;

impl SettingsScreen {
    pub(super) fn render_usage_range(
        &self,
        data: &UsageData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let today = data.today;
        let range_label = self.usage_range_label(self.usage.range, today);
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .child(subsection_title(&tr("settings-usage-history-title")))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .whitespace_nowrap()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(gpui::text!(
                                id = "usage-range-dates",
                                range_label.to_string()
                            )),
                    )
                    .child(self.range_picker(today, window, cx)),
            );
        let section = div().flex().flex_col().gap_3().child(header).child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(tr("settings-usage-scope")),
        );
        let Some(range) = &data.range else {
            return section.child(if self.usage.range_failed() {
                self.retry_block(
                    "usage-range-error",
                    tr("settings-usage-range-error"),
                    1,
                    Self::load_usage_summary,
                    cx,
                )
            } else {
                loading_block("usage-range-loading", tr("settings-usage-loading"))
            });
        };
        section.child(
            div()
                .flex()
                .flex_col()
                .gap_6()
                .when(self.usage.summary_loading, |body| body.opacity(0.6))
                .when(self.usage.range_failed(), |body| {
                    body.child(self.retry_block(
                        "usage-range-error",
                        tr("settings-usage-range-error"),
                        1,
                        Self::load_usage_summary,
                        cx,
                    ))
                })
                .child(self.daily_chart(data, range, cx))
                .child(self.models_table(range))
                .child(self.apps_list(range)),
        )
    }

    pub(super) fn range_picker(
        &self,
        today: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let menu = SettingMenu::UsageRange;
        let dropdown = self.popup.is_open(&menu).then(|| {
            let mut dropdown = crate::ui::popup::Menu::new("setting-menu-usage-range", px(260.))
                .label(tr("settings-usage-range-picker"))
                .max_height(px(360.))
                .application_vim(self.settings.application_vim_enabled);
            for (index, option) in self.usage_range_options().into_iter().enumerate() {
                if index == 2 {
                    dropdown = dropdown
                        .separator()
                        .header(tr("settings-usage-range-previous"));
                }
                dropdown = dropdown.item(
                    crate::ui::popup::MenuItem::new(
                        super::super::option_id(menu, index),
                        option.label,
                        move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                            this.pick_setting_option(menu, index, cx);
                        },
                    )
                    .current(option.current),
                );
            }
            self.popup
                .render(dropdown, crate::ui::popup::Placement::BelowEnd, window, cx)
        });
        let value = self.usage_range_name(self.usage.range, today);
        let button = widgets::secondary_button("setting-value-usage-range")
            .debug_selector(|| "setting-value-usage-range".into())
            .aria_label(tr("settings-usage-range-picker"))
            .aria_value(value.to_string())
            .tab_index(1)
            .py_1p5()
            .gap_1p5()
            .child(icon("list-filter", px(12.), theme::text_secondary()))
            .child(value)
            .child(icon("chevron-down", px(12.), theme::text_muted()));
        div().relative().flex_none().child(
            self.application_target(
                || SettingsTarget::Billing(BillingTarget::Usage(UsageTarget::Range)),
                div()
                    .child(self.popup.trigger(menu, button, cx, move |this, _, cx| {
                        this.toggle_setting_menu(menu, cx)
                    }))
                    .children(dropdown),
            ),
        )
    }

    /// The range in four numbers, above its chart.
    pub(super) fn range_totals(&self, range: &RangeData) -> Div {
        let counts = &range.summary.counts;
        let active = self.messages.get(
            "settings-usage-tile-active-value",
            [
                ("active", range.active_days.to_string().into()),
                ("days", range.dates.len().to_string().into()),
            ],
        );
        let totals: [(&'static str, SharedString, String); 4] = [
            (
                "usage-total-credits",
                tr("settings-usage-tile-credits"),
                fmt_count(counts.credits),
            ),
            (
                "usage-total-requests",
                tr("settings-usage-tile-requests"),
                fmt_count(counts.requests as f64),
            ),
            (
                "usage-total-active",
                tr("settings-usage-tile-active"),
                active.to_string(),
            ),
            (
                "usage-total-tokens",
                tr("settings-usage-metric-tokens"),
                fmt_compact(counts.tokens() as f64),
            ),
        ];
        div()
            .flex()
            .flex_wrap()
            .gap_x_10()
            .gap_y_3()
            .children(totals.into_iter().map(|(id, label, value)| {
                div()
                    .debug_selector(move || id.to_string())
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(20.))
                            .line_height(px(28.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(gpui::text!(id = id, value)),
                    )
            }))
    }

    pub(super) fn daily_chart(
        &self,
        data: &UsageData,
        range: &RangeData,
        cx: &mut Context<Self>,
    ) -> Div {
        let kind = data.plan.kind;
        let metric = self.usage.metric;
        let split = self.usage.split;
        let chart_data = &range.chart;
        let metric_pills = segmented(
            "usage-metric",
            tr("settings-usage-show"),
            Metric::ALL
                .into_iter()
                .enumerate()
                .map(|(index, option)| {
                    let pill = segment_pill(
                        SharedString::from(format!("usage-metric-{}", metric_slug(option))),
                        metric_name(option),
                        option == metric,
                        2 + index as isize,
                        move |this, cx| this.choose_usage_metric(option, cx),
                        cx,
                    );
                    self.application_target(
                        || {
                            SettingsTarget::Billing(BillingTarget::Usage(UsageTarget::Metric(
                                option,
                            )))
                        },
                        pill,
                    )
                })
                .collect(),
        );
        let splits = Split::options(metric, kind);
        let split_pills = (splits.len() > 1).then(|| {
            segmented(
                "usage-split",
                tr("settings-usage-split-by"),
                splits
                    .into_iter()
                    .enumerate()
                    .map(|(index, option)| {
                        let pill = segment_pill(
                            SharedString::from(format!("usage-split-{}", split_slug(option))),
                            split_name(option),
                            option == split,
                            5 + index as isize,
                            move |this, cx| this.choose_usage_split(option, cx),
                            cx,
                        );
                        self.application_target(
                            || {
                                SettingsTarget::Billing(BillingTarget::Usage(UsageTarget::Split(
                                    option,
                                )))
                            },
                            pill,
                        )
                    })
                    .collect(),
            )
        });
        let series_colors: Vec<u32> = chart_data
            .series
            .iter()
            .map(|series| palette::series(series.key))
            .collect();
        let columns = chart_data
            .days
            .iter()
            .map(|day| chart::Column {
                segments: series_colors
                    .iter()
                    .copied()
                    .zip(day.values.iter().copied())
                    .collect(),
                total: day.total,
                tick: self.short_date(day.date),
            })
            .collect();
        let data_rc = self.usage.data.clone();
        let tip_metric = metric;
        let tip: Rc<dyn Fn(usize) -> Tip> =
            Rc::new(move |index| day_tip(data_rc.as_deref(), index, tip_metric));
        let marker = range
            .reset_index
            .map(|index| (index, tr("settings-usage-marker-reset")));
        let span = (kind == PlanKind::Credits && metric == Metric::Credits)
            .then_some(range.extra_span)
            .flatten()
            .map(|(from, to)| {
                (
                    from,
                    to,
                    palette::series(SeriesKey::Extra),
                    tr("settings-usage-extra-span"),
                )
            });
        let figure = chart::bars(
            chart::Bars {
                id: "usage-daily",
                label: self.messages.get(
                    "settings-usage-daily-label",
                    [
                        ("title", daily_title(metric).to_string().into()),
                        ("split", split_word(split).to_string().into()),
                    ],
                ),
                columns,
                axis: chart_data
                    .scale
                    .ticks
                    .iter()
                    .map(|tick| fmt_compact(*tick).into())
                    .collect(),
                scale: chart_data.scale.clone(),
                height: px(220.),
                max_bar_width: px(26.),
                label_every: chart_data.days.len().div_ceil(8),
                reference: None,
                marker,
                span,
            },
            tip,
        );
        let legend = if chart_data.series.is_empty() {
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(tr("settings-usage-no-usage"))
        } else {
            div()
                .flex()
                .flex_wrap()
                .gap_x_4()
                .gap_y_1()
                .children(chart_data.series.iter().map(|series| {
                    legend_item(
                        palette::series(series.key),
                        series_name(series.key, series.label.as_deref()),
                    )
                }))
        };
        widgets::card_row()
            .p_5()
            .flex()
            .flex_col()
            .gap_5()
            .child(self.range_totals(range))
            .child(
                div()
                    .pt_5()
                    .border_t_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(daily_title(metric)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .child(metric_pills)
                            .children(split_pills),
                    ),
            )
            .child(figure)
            .child(legend)
    }

    /// Credits by the app that sent each request.
    pub(super) fn apps_list(&self, range: &RangeData) -> Div {
        let summary = &range.summary;
        let total = summary.counts.credits;
        let peak = summary.origins.iter().copied().fold(0.0, f64::max);
        let header = table_row(
            [
                (tr("settings-usage-col-app"), Column::Name),
                (tr("settings-usage-col-credits"), Column::Number),
                (tr("settings-usage-col-share"), Column::Share),
            ]
            .into_iter()
            .map(|(label, column)| (column, header_cell(label))),
        );
        let rows = Origin::ALL
            .into_iter()
            .filter(|origin| summary.origins[origin.index()] > 0.0)
            .map(|origin| {
                let value = summary.origins[origin.index()];
                let color = palette::origin(origin);
                table_row([
                    (
                        Column::Name,
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(chart::swatch(color, px(10.)))
                            .child(
                                div()
                                    .text_color(gpui::rgb(theme::text_primary()))
                                    .child(origin_name(origin)),
                            )
                            .into_any_element(),
                    ),
                    (Column::Number, number_cell(fmt_count(value))),
                    (Column::Share, share_cell(value, peak, total, color)),
                ])
                .into_any_element()
            });
        let mut list: Vec<gpui::AnyElement> = vec![header.into_any_element()];
        list.extend(rows);
        let mut section = div().flex().flex_col().gap_3().child(card_heading(
            tr("settings-usage-apps-title"),
            tr("settings-usage-apps-description"),
        ));
        if total <= 0.0 {
            return section.child(empty_note(tr("settings-usage-no-usage")));
        }
        section = section.child(widgets::card_stack(list));
        if summary.api > 0.0 {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(self.messages.get(
                        "settings-usage-apps-api-share",
                        [(
                            "percent",
                            format!("{}%", percent(summary.api, total)).into(),
                        )],
                    )),
            );
        }
        section
    }

    pub(super) fn models_table(&self, range: &RangeData) -> Div {
        let models = &range.summary.by_model;
        let total = range.summary.counts.credits;
        let peak = models
            .iter()
            .map(|model| model.counts.credits)
            .fold(0.0, f64::max);
        let header = table_row(
            [
                (tr("settings-usage-col-model"), Column::Name),
                (tr("settings-usage-col-requests"), Column::Number),
                (tr("settings-usage-col-input"), Column::Number),
                (tr("settings-usage-col-cached"), Column::Number),
                (tr("settings-usage-col-output"), Column::Number),
                (tr("settings-usage-col-credits"), Column::Number),
                (tr("settings-usage-col-share"), Column::Share),
            ]
            .into_iter()
            .map(|(label, column)| (column, header_cell(label))),
        );
        let peers: Vec<&str> = range
            .chart
            .series
            .iter()
            .filter_map(|series| series.model.as_deref())
            .collect();
        let rows = models.iter().map(|model| {
            let color = palette::model_table_color(&model.model, &peers, self.usage.split);
            let (mix, more) = &model.mix;
            // One app needs no percentage: `Work`, not `100% Work`.
            let mut mix: Vec<String> = match mix.as_slice() {
                [(_, origin)] => vec![origin_name(*origin).to_string()],
                mix => mix
                    .iter()
                    .map(|(pct, origin)| {
                        self.messages
                            .get(
                                "settings-usage-origin-share",
                                [
                                    ("percent", format!("{pct}%").into()),
                                    ("origin", origin_name(*origin).to_string().into()),
                                ],
                            )
                            .to_string()
                    })
                    .collect(),
            };
            if *more > 0 {
                mix.push(
                    self.messages
                        .get(
                            "settings-usage-origin-more",
                            [("count", (*more as i64).into())],
                        )
                        .to_string(),
                );
            }
            let name = div()
                .flex()
                .items_start()
                .gap_2()
                .min_w_0()
                .child(div().mt(px(4.)).child(chart::swatch(color, px(10.))))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_color(gpui::rgb(theme::text_primary()))
                                .child(model.label.clone()),
                        )
                        .when(!mix.is_empty(), |cell| {
                            cell.child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(gpui::rgb(theme::text_muted()))
                                    .child(mix.join(" · ")),
                            )
                        }),
                );
            let cached = if model.counts.cached_reported == 0 {
                "—".to_string()
            } else {
                fmt_compact(model.counts.cached as f64)
            };
            table_row([
                (Column::Name, name.into_any_element()),
                (
                    Column::Number,
                    number_cell(fmt_count(model.counts.requests as f64)),
                ),
                (
                    Column::Number,
                    number_cell(fmt_compact(model.counts.input as f64)),
                ),
                (Column::Number, number_cell(cached)),
                (
                    Column::Number,
                    number_cell(fmt_compact(model.counts.output as f64)),
                ),
                (Column::Number, number_cell(fmt_count(model.counts.credits))),
                (
                    Column::Share,
                    share_cell(model.counts.credits, peak, total, color),
                ),
            ])
            .into_any_element()
        });
        let mut list: Vec<gpui::AnyElement> = vec![header.into_any_element()];
        list.extend(rows);
        let body = if models.is_empty() {
            empty_note(tr("settings-usage-no-usage"))
        } else {
            widgets::card_stack(list)
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(card_heading(
                tr("settings-usage-models-title"),
                tr("settings-usage-models-description"),
            ))
            .child(body)
    }
}

//! Immediate, local send feedback, separate from the host's authoritative
//! transcript. A preview never becomes a saved message or a retry request.

use super::*;

pub(super) struct SendPreview {
    pub session: Option<String>,
    text: SharedString,
    images: Vec<(SharedString, Option<Arc<gpui::Image>>)>,
    show_body: bool,
    acknowledged_run: Option<String>,
    projected_runs: HashSet<String>,
    baseline_user_rows: HashSet<String>,
    snapshot_projected: bool,
}

impl SendPreview {
    pub(super) fn new(session: Option<String>, text: &str, images: &[DraftImage]) -> Self {
        Self {
            session,
            text: text.to_string().into(),
            images: images
                .iter()
                .map(|image| (image.name.clone().into(), image.thumbnail.clone()))
                .collect(),
            show_body: true,
            acknowledged_run: None,
            projected_runs: HashSet::new(),
            baseline_user_rows: HashSet::new(),
            snapshot_projected: false,
        }
    }

    pub(super) fn status_only(mut self) -> Self {
        self.show_body = false;
        self
    }

    pub(super) fn capture_timeline(mut self, timeline: &[AgentTimelineItem]) -> Self {
        self.baseline_user_rows = timeline
            .iter()
            .filter(|item| is_user_message(item))
            .map(|item| item.id.clone())
            .collect();
        self
    }

    pub(super) fn admitting(&self) -> bool {
        self.acknowledged_run.is_none()
    }

    pub(super) fn body_visible(&self) -> bool {
        self.show_body
            && !self.snapshot_projected
            && match &self.acknowledged_run {
                // Before the reply identifies the run, yield the body to real
                // user rows without declaring delivery or discarding the preview.
                None => self.projected_runs.is_empty(),
                Some(run) => !self.projected_runs.contains(run),
            }
    }

    fn settled(&self) -> bool {
        self.acknowledged_run.as_ref().is_some_and(|run| {
            !self.show_body || self.snapshot_projected || self.projected_runs.contains(run)
        })
    }
}

fn is_user_message(item: &AgentTimelineItem) -> bool {
    item.item_type == "message" && item.role.as_deref() == Some("user")
}

impl ChatScreen {
    fn current_send_preview(&self) -> Option<&SendPreview> {
        if self.selected_session.is_none() && self.pending_first_send.is_some() {
            return self.first_send_preview.as_ref();
        }
        let session = self.selected_session.as_deref()?;
        // There can be an acknowledged body waiting for projection plus a
        // queued/steering admission. Keep the body visible during that call.
        self.sending_previews
            .values()
            .filter(|preview| preview.session.as_deref() == Some(session))
            .find(|preview| preview.body_visible())
            .or_else(|| {
                self.sending_previews.values().find(|preview| {
                    preview.session.as_deref() == Some(session) && preview.admitting()
                })
            })
    }

    pub(super) fn send_pending(&self) -> bool {
        if self.selected_session.is_none() && self.pending_first_send.is_some() {
            return self.first_send_preview.is_some();
        }
        self.selected_session.as_deref().is_some_and(|session| {
            self.sending_previews
                .values()
                .any(|preview| preview.session.as_deref() == Some(session) && preview.admitting())
        })
    }

    pub(super) fn has_send_feedback(&self) -> bool {
        self.current_send_preview().is_some()
    }

    pub(super) fn shows_empty_state(&self) -> bool {
        self.timeline.is_empty()
            && self.current_send_preview().is_none()
            && !self.awaiting_first_token
    }

    /// Admission and projection travel independently. The reply identifies a
    /// run but never invents its user row; keep feedback until both arrive.
    pub(super) fn acknowledge_send_preview(&mut self, id: &str, run: &str) {
        if self.finished_runs.iter().any(|finished| finished == run) {
            self.sending_previews.remove(id);
            return;
        }
        if let Some(preview) = self.sending_previews.get_mut(id) {
            preview.acknowledged_run = Some(run.to_owned());
        }
        self.sending_previews
            .retain(|_, preview| !preview.settled());
    }

    pub(super) fn observe_send_projection(
        &mut self,
        session: &str,
        run: Option<&str>,
        item: &AgentTimelineItem,
    ) {
        let Some(run) = run.filter(|_| is_user_message(item)) else {
            return;
        };
        for preview in self
            .sending_previews
            .values_mut()
            .filter(|preview| preview.session.as_deref() == Some(session) && preview.show_body)
        {
            // Bound pre-ack observations from other controllers. Once the
            // response identifies our run, unrelated activity cannot retire it.
            if preview.baseline_user_rows.contains(&item.id) {
                continue;
            }
            if preview
                .acknowledged_run
                .as_deref()
                .is_some_and(|expected| expected != run)
            {
                continue;
            }
            if preview.projected_runs.len() < 8 || preview.acknowledged_run.as_deref() == Some(run)
            {
                preview.projected_runs.insert(run.to_owned());
            }
        }
        self.sending_previews
            .retain(|_, preview| !preview.settled());
    }

    pub(super) fn finish_send_preview(&mut self, session: &str, run: &str) {
        self.sending_previews.retain(|_, preview| {
            preview.session.as_deref() != Some(session)
                || preview.acknowledged_run.as_deref() != Some(run)
        });
    }

    pub(super) fn finish_send(
        &mut self,
        send_id: String,
        send: UnresolvedSend,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let session = send.session.clone();
        match result {
            Ok(run_id) => {
                self.acknowledge_send_preview(&send_id, &run_id);
                // A short run can finish before admission returns.
                if !self.finished_runs.contains(&run_id) {
                    self.active_runs.insert(session, run_id);
                    self.sync_sidebar(cx);
                }
            }
            Err(message) => {
                self.sending_previews.remove(&send_id);
                let uncertain = message.starts_with("Send outcome unknown");
                self.unresolved_sends.insert(send_id.clone(), send);
                if self.is_selected(&session) {
                    self.awaiting_first_token = false;
                }
                if uncertain {
                    self.notice = Some("Delivery is uncertain. Your message and images are retained while Maple checks the host; nothing will be resent.".into());
                    self.check_unresolved_sends(cx);
                } else {
                    if self.is_selected(&session) {
                        self.push_local_error("Send failed", &message, cx);
                    }
                    self.apply_send_receipt(
                        &send_id,
                        Some(crate::host::receipts::Outcome::Finished(Err(message))),
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// A reload/reconnect can contain the canonical user row without replaying
    /// its event. Row identity (never text) detects new authoritative content.
    /// This only retires presentation; receipts remain delivery authority.
    pub(super) fn reconcile_send_snapshot(
        &mut self,
        session: &str,
        timeline: &[AgentTimelineItem],
    ) {
        for preview in self
            .sending_previews
            .values_mut()
            .filter(|preview| preview.session.as_deref() == Some(session) && preview.show_body)
        {
            if timeline
                .iter()
                .any(|item| is_user_message(item) && !preview.baseline_user_rows.contains(&item.id))
            {
                preview.snapshot_projected = true;
            }
        }
        self.sending_previews
            .retain(|_, preview| !preview.settled());
    }

    pub(super) fn render_send_preview(&self) -> Option<gpui::Stateful<Div>> {
        let preview = self.current_send_preview()?;
        Some(
            div()
                .id("pending-send")
                .debug_selector(|| "pending-send".to_string())
                .role(gpui::Role::Status)
                .aria_label(if self.send_pending() {
                    "Sending message"
                } else {
                    "Waiting for response"
                })
                .flex_none()
                .w_full()
                .max_w(CONTENT_WIDTH)
                .mx_auto()
                .px_6()
                .pb_2()
                .flex()
                .flex_col()
                .items_end()
                .gap_1()
                .when(preview.body_visible(), |row| {
                    row.child(
                        crate::ui::typography::chat_reading(div())
                            .id("pending-send-body")
                            .debug_selector(|| "pending-send-body".to_string())
                            .max_w(gpui::relative(0.75))
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            // A long message scrolls inside its bubble; the
                            // pane must not scroll the transcript as well.
                            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                            .px_4()
                            .py_2()
                            .rounded(theme::RADIUS_MD)
                            .bg(gpui::rgb(theme::bg_user_bubble()))
                            .border_1()
                            .border_color(gpui::rgb(theme::user_bubble_border()))
                            .text_color(gpui::rgb(theme::text_primary()))
                            .when(!preview.images.is_empty(), |body| {
                                body.child(div().flex().flex_wrap().gap_2().mb_1().children(
                                    preview.images.iter().map(|(name, image)| {
                                        match image {
                                            Some(image) => div().child(
                                                gpui::img(gpui::ImageSource::Image(image.clone()))
                                                    .size_16()
                                                    .rounded(theme::RADIUS_SM),
                                            ),
                                            None => div().text_xs().child(name.clone()),
                                        }
                                    }),
                                ))
                            })
                            .child(preview.text.clone()),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(spinner(
                            "pending-send-spinner",
                            px(12.),
                            theme::text_muted(),
                        ))
                        .child(if self.send_pending() {
                            "Sending…"
                        } else {
                            "Waiting for response…"
                        }),
                ),
        )
    }
}

//! Immediate, local send feedback, separate from the host's authoritative
//! transcript. A preview never becomes a saved message or a retry request.

use super::*;

pub(super) struct SendPreview {
    pub session: Option<String>,
    text: SharedString,
    attachments: Vec<PreviewAttachment>,
    show_body: bool,
    acknowledged_run: Option<String>,
    projected_runs: HashSet<String>,
    baseline_user_rows: HashSet<String>,
    snapshot_projected: bool,
    phase: SendPhase,
    /// When the preview was made: its bubble's entrance starts here.
    shown: std::time::Instant,
}

struct PreviewAttachment {
    name: SharedString,
    mime: String,
    size: u64,
    /// The whole picture, as the sent message's thumbnail shows it.
    image: Option<Arc<gpui::Image>>,
}

enum SendPhase {
    Uploading,
    Prepared(Box<uploads::PreparedSend>),
    Admitting,
}

impl SendPreview {
    pub(super) fn new(
        session: Option<String>,
        text: &str,
        attachments: &[DraftAttachment],
    ) -> Self {
        Self {
            session,
            text: text.to_string().into(),
            attachments: attachments
                .iter()
                .map(|attachment| PreviewAttachment {
                    name: attachment.name.clone().into(),
                    mime: attachment.mime_type.clone(),
                    size: attachment.size,
                    image: attachment.picture.clone(),
                })
                .collect(),
            show_body: true,
            acknowledged_run: None,
            projected_runs: HashSet::new(),
            baseline_user_rows: HashSet::new(),
            snapshot_projected: false,
            phase: SendPhase::Admitting,
            shown: motion::now(),
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
        matches!(self.phase, SendPhase::Admitting) && self.acknowledged_run.is_none()
    }

    pub(super) fn uploading(mut self) -> Self {
        self.phase = SendPhase::Uploading;
        self
    }

    pub(super) fn prepare(&mut self, send: uploads::PreparedSend) {
        self.phase = SendPhase::Prepared(Box::new(send));
    }

    pub(super) fn prepared(&self) -> Option<&uploads::PreparedSend> {
        match &self.phase {
            SendPhase::Prepared(send) => Some(send),
            _ => None,
        }
    }

    pub(super) fn take_prepared(&mut self) -> Option<uploads::PreparedSend> {
        if !matches!(self.phase, SendPhase::Prepared(_)) {
            return None;
        }
        match std::mem::replace(&mut self.phase, SendPhase::Admitting) {
            SendPhase::Prepared(send) => Some(*send),
            _ => unreachable!(),
        }
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
        let mut handed_off = false;
        let mut pictures = Vec::new();
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
            let showing = preview.body_visible();
            if preview.projected_runs.len() < 8 || preview.acknowledged_run.as_deref() == Some(run)
            {
                preview.projected_runs.insert(run.to_owned());
            }
            if showing && !preview.body_visible() {
                handed_off = true;
                pictures = preview
                    .attachments
                    .iter()
                    .filter_map(|attachment| attachment.image.clone())
                    .collect();
            }
        }
        // The row takes the place of the message already on screen; it
        // must not enter a second time. The transcript files a row under
        // its view key, which a real host's rows have and which differs
        // from the id the event names, so hand off under that key.
        if handed_off {
            let key = crate::backend::timeline_view_key(item).unwrap_or_else(|| item.id.clone());
            self.row_motion.hand_off(key);
            if self.is_selected(session) {
                self.keep_sent_pictures(item, pictures);
            }
        }
        self.sending_previews
            .retain(|_, preview| !preview.settled());
    }

    /// The row that takes over from the message being sent shows the same
    /// pictures from its first frame, instead of file names until the
    /// stored pictures load. Its image attachments are the drafts' in
    /// order; if they do not line up, the row loads its pictures as usual.
    fn keep_sent_pictures(&mut self, item: &AgentTimelineItem, pictures: Vec<Arc<gpui::Image>>) {
        let sent: Vec<&str> = transcript::image_attachment_refs(item)
            .map(|(id, _)| id)
            .collect();
        if sent.len() != pictures.len() {
            return;
        }
        for (id, picture) in sent.into_iter().zip(pictures) {
            if !self.attachment_images.contains_key(id) {
                self.attachment_previews.insert(id.to_owned(), picture);
            }
        }
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
        result: Result<String, UiFailure>,
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
                let raw = message.raw_message().map(str::to_owned);
                let uncertain = raw
                    .as_deref()
                    .is_none_or(|raw| raw.starts_with("Send outcome unknown"));
                self.unresolved_sends.insert(send_id.clone(), send);
                if self.is_selected(&session) {
                    self.awaiting_first_token = false;
                }
                if uncertain {
                    self.notice = Some(UiText::key("chat-send-uncertain"));
                    self.check_unresolved_sends(cx);
                } else {
                    let raw = raw.expect("definite failure retains its original diagnostic");
                    // `message` still carries semantic presentation for the UI-only error renderer.
                    if self.is_selected(&session) {
                        self.push_local_error("Send failed", &raw, message.into(), cx);
                    }
                    self.apply_send_receipt(
                        &send_id,
                        Some(crate::host::receipts::Outcome::Finished(Err(raw))),
                        cx,
                    );
                }
            }
        }
        self.dispatch_prepared_uploads(cx);
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

    /// The message being sent, under the transcript. Its bubble rises in
    /// from the moment it shows; the status line under it fades in once,
    /// in the slot the waiting dots use, so the real row that takes over
    /// lands where the bubble was.
    pub(super) fn render_send_preview(
        &self,
        window: &mut Window,
        cx: &gpui::App,
    ) -> Option<gpui::Stateful<Div>> {
        self.row_motion.preview_body.set(None);
        let preview = self.current_send_preview()?;
        let body_visible = preview.body_visible();
        let entrance = motion::progress(
            preview.shown,
            motion::ENTER,
            motion::now(),
            cx.reduce_motion(),
        );
        if body_visible && entrance < 1. {
            window.request_animation_frame();
        }
        let body_bounds = self.row_motion.preview_body.clone();
        let row = div()
            .on_children_prepainted(move |bounds, _, _| {
                body_bounds.set(bounds.first().copied().filter(|_| body_visible));
            })
            .id("pending-send")
            .debug_selector(|| "pending-send".to_string())
            .role(gpui::Role::Status)
            .aria_label(if self.send_pending() {
                tr("chat-sending-message")
            } else {
                tr("chat-waiting-response")
            })
            .flex_none()
            .w_full()
            .max_w(CONTENT_WIDTH)
            .mx_auto()
            .px_6()
            .flex()
            .flex_col()
            .items_end()
            .gap(transcript::PREVIEW_GAP)
            .when(body_visible, |row| {
                // The same pieces in the same places as the row that takes
                // over: the attachments in their own row, the text in a
                // bubble under it, and no bubble without text.
                let bubble = (!preview.text.trim().is_empty()).then(|| {
                    crate::ui::typography::chat_reading(div())
                        .id("pending-send-body")
                        .debug_selector(|| "pending-send-body".to_string())
                        .max_w(gpui::relative(0.75))
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        // A long message scrolls inside its bubble; the
                        // pane must not scroll the transcript as well.
                        .on_scroll(|_, _, cx| cx.stop_propagation())
                        .px_4()
                        .py_2()
                        .rounded(theme::RADIUS_MD)
                        .bg(gpui::rgb(theme::bg_user_bubble()))
                        .border_1()
                        .border_color(gpui::rgb(theme::user_bubble_border()))
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(preview.text.clone())
                });
                let body = div()
                    .debug_selector(|| "pending-send-message".to_string())
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_0p5()
                    .when(!preview.attachments.is_empty(), |body| {
                        body.child(
                            transcript::attachment_row().children(
                                preview.attachments.iter().enumerate().map(
                                    |(index, attachment)| match &attachment.image {
                                        Some(image) => {
                                            transcript::attachment_thumbnail(image.clone())
                                                .debug_selector(move || {
                                                    format!("pending-send-thumbnail-{index}")
                                                })
                                        }
                                        None => transcript::attachment_chip(
                                            attachment.name.clone(),
                                            attachments::cached_attachment_detail(
                                                &attachment.mime,
                                                attachment.size,
                                                &self.messages,
                                            ),
                                        ),
                                    },
                                ),
                            ),
                        )
                    })
                    .children(bubble);
                row.child(if entrance < 1. {
                    motion::arrive(body, entrance).into_any_element()
                } else {
                    body.into_any_element()
                })
            })
            .child(motion::fade_in(
                div()
                    .debug_selector(|| "pending-send-status".to_string())
                    .h(transcript::WAITING_ROW_HEIGHT)
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
                    .child(
                        self.selected_upload()
                            .map(|(_, upload)| upload.label.clone())
                            .unwrap_or_else(|| {
                                if self.send_pending() {
                                    UiText::key("chat-sending")
                                } else {
                                    UiText::key("chat-waiting-response-progress")
                                }
                            }),
                    ),
                "pending-send-status-reveal",
            ));
        Some(row)
    }
}

#[cfg(test)]
mod localization_tests {
    use super::*;

    #[test]
    fn attachment_preview_reformats_without_changing_payload() {
        let _locale = crate::ui::i18n::TestLocalizationGuard::enable();
        let messages = MessageCache::default();
        let mut draft = DraftAttachment::preparing(7, "User file.txt".into());
        draft.mime_type = "text/plain".into();
        draft.size = 42;
        let preview = SendPreview::new(Some("task-id".into()), "User message", &[draft]);
        for (locale, label) in [
            ("en", "Text · 42 B"),
            ("es", "Texto · 42 B"),
            ("en", "Text · 42 B"),
        ] {
            crate::ui::i18n::apply_preference(Some(locale));
            let attachment = &preview.attachments[0];
            assert_eq!(
                attachments::cached_attachment_detail(&attachment.mime, attachment.size, &messages)
                    .as_ref(),
                label
            );
            assert_eq!(attachment.name.as_ref(), "User file.txt");
            assert_eq!(preview.text.as_ref(), "User message");
            assert_eq!(preview.session.as_deref(), Some("task-id"));
        }
    }
}

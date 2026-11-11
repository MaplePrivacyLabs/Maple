//! The composer previews and changes the runtime's persisted thinking level.

use super::*;
use crate::ui::i18n::tr;

/// A model's available levels can be reused, but its selected level belongs to
/// a particular task (or to the new-task default). Screens themselves belong
/// to one account and one computer, so no cache is shared across either boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ThinkingKey {
    session: Option<String>,
    model: String,
}

#[derive(Default)]
pub(super) struct ThinkingCache {
    entries: std::collections::VecDeque<CachedThinking>,
}

struct CachedThinking {
    key: ThinkingKey,
    state: AgentThinkingState,
    /// A pending pick previews its choice but can restore the confirmed value.
    /// A pushed update replaces this entry, so a late failure cannot undo it.
    previous: Option<AgentThinkingState>,
}

impl ThinkingCache {
    const CAPACITY: usize = 64;

    fn get(&self, key: &ThinkingKey) -> Option<AgentThinkingState> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| entry.state.clone())
    }

    fn insert(&mut self, key: ThinkingKey, state: AgentThinkingState) {
        self.remove(&key);
        self.entries.push_back(CachedThinking {
            key,
            state,
            previous: None,
        });
        if self.entries.len() > Self::CAPACITY {
            self.entries.pop_front();
        }
    }

    fn remove(&mut self, key: &ThinkingKey) {
        self.entries.retain(|entry| &entry.key != key);
    }

    fn begin_pick(&mut self, key: ThinkingKey, state: AgentThinkingState, level: ThinkingLevel) {
        let previous = state.clone();
        self.insert(
            key,
            AgentThinkingState {
                current: level,
                ..state
            },
        );
        if let Some(entry) = self.entries.back_mut() {
            entry.previous = Some(previous);
        }
    }

    fn finish_pick(
        &mut self,
        key: &ThinkingKey,
        confirmed: Option<AgentThinkingState>,
    ) -> Option<AgentThinkingState> {
        let entry = self.entries.iter_mut().find(|entry| &entry.key == key)?;
        let previous = entry.previous.take()?;
        entry.state = confirmed.unwrap_or(previous);
        Some(entry.state.clone())
    }

    /// New Task remembers one model and level, so a pick there, or of a
    /// model there, changes what a new task starts at on every model.
    pub(super) fn invalidate_defaults(&mut self) {
        self.entries.retain(|entry| entry.key.session.is_some());
    }

    fn invalidate_session(&mut self, session: &str) {
        self.entries
            .retain(|entry| entry.key.session.as_deref() != Some(session));
    }
}

pub(super) fn thinking_label(level: ThinkingLevel) -> SharedString {
    tr(match level {
        ThinkingLevel::Off => "chat-thinking-off",
        ThinkingLevel::Minimal => "chat-thinking-minimal",
        ThinkingLevel::Low => "chat-thinking-low",
        ThinkingLevel::Medium => "chat-thinking-medium",
        ThinkingLevel::High => "chat-thinking-high",
        ThinkingLevel::Xhigh => "chat-thinking-xhigh",
        ThinkingLevel::Max => "chat-thinking-max",
    })
}

impl ChatScreen {
    /// The level a pick is still waiting to send for the selected task and
    /// model, while an earlier pick saves.
    pub(super) fn queued_thinking(&self) -> Option<ThinkingLevel> {
        let (key, level) = self.thinking_queued.as_ref()?;
        (Some(key) == self.thinking_key().as_ref()).then_some(*level)
    }

    fn thinking_key(&self) -> Option<ThinkingKey> {
        Some(ThinkingKey {
            session: self.selected_session.clone(),
            model: self.selected_model.clone()?,
        })
    }

    /// Drop responses for a task or model that is no longer selected, or
    /// for a read that started before the latest pick.
    pub(super) fn refresh_thinking(&mut self, cx: &mut Context<Self>) {
        self.thinking_generation += 1;
        self.thinking_loading = false;
        let generation = self.thinking_generation;
        let key = self.thinking_key();
        self.thinking = key.as_ref().and_then(|key| self.thinking_cache.get(key));
        let Some(key) = key else {
            return;
        };
        // Do not let a read race an optimistic pick. Completion revalidates
        // whichever task is then selected, including navigation during a save.
        if self.thinking_saving {
            return;
        }
        self.thinking_loading = true;
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let request = key.clone();
        self.call(
            async move {
                backend
                    .thinking_state(&user_id, request.session.as_deref(), &request.model)
                    .await
            },
            cx,
            move |this, result, cx| {
                if this.thinking_generation == generation {
                    this.thinking_loading = false;
                    match result {
                        Ok(state) => {
                            this.thinking_cache.insert(key, state.clone());
                            this.thinking = Some(state);
                        }
                        Err(error) => log::warn!("Failed to load thinking controls: {error}"),
                    }
                    cx.notify();
                }
            },
        );
    }

    /// A task's level changed, here or on another surface. It is the
    /// task's own: New Task's previews stand.
    pub(super) fn apply_thinking_changed(&mut self, session: &str, state: AgentThinkingState) {
        self.thinking_cache.invalidate_session(session);
        if self.selected_session.as_deref() == Some(session) {
            self.thinking_generation += 1;
            self.thinking_loading = false;
            if let Some(key) = self.thinking_key() {
                self.thinking_cache.insert(key, state.clone());
            }
            self.thinking = Some(state);
        }
    }

    /// Save `level` for the selected task, or for New Task, which
    /// remembers it with its model.
    pub(super) fn pick_thinking(&mut self, level: ThinkingLevel, cx: &mut Context<Self>) {
        if !self
            .thinking
            .as_ref()
            .is_some_and(|state| state.levels.contains(&level))
        {
            return;
        }
        if let Some(key) = self.thinking_key() {
            self.save_thinking(key, level, cx);
        }
    }

    /// Save `level` for `key`. A pick made while another saves waits for
    /// it, and only the latest waiting pick (one for all tasks and models)
    /// goes out, so a slider swept across its stops saves once more at
    /// most. It goes out for the task and model it was made on, even when
    /// those are no longer selected.
    fn save_thinking(&mut self, key: ThinkingKey, level: ThinkingLevel, cx: &mut Context<Self>) {
        if self.thinking_saving {
            self.thinking_queued = Some((key, level));
            cx.notify();
            return;
        }
        self.thinking_queued = None;
        let selected = self.thinking_key().as_ref() == Some(&key);
        let state = if selected {
            self.thinking.clone()
        } else {
            self.thinking_cache.get(&key)
        };
        if state
            .as_ref()
            .is_some_and(|state| state.current == level || !state.levels.contains(&level))
        {
            return;
        }
        self.thinking_generation += 1;
        self.thinking_loading = false;
        self.thinking_saving = true;
        // A pushed update may have dropped the levels a waiting pick was
        // made against; it still goes out, without a preview, and the
        // runtime clamps it.
        if let Some(state) = state {
            self.thinking_cache.begin_pick(key.clone(), state, level);
            if selected {
                self.thinking = self.thinking_cache.get(&key);
            }
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let request = key.clone();
        self.call_semantic(
            async move {
                backend
                    .set_thinking_level(&user_id, request.session.as_deref(), &request.model, level)
                    .await
            },
            cx,
            move |this, result, cx| this.thinking_saved(key, result, cx),
        );
        cx.notify();
    }

    /// A save for `key` finished: keep what the runtime applied, or roll
    /// back, then send the pick that waited for it.
    fn thinking_saved(
        &mut self,
        key: ThinkingKey,
        result: Result<AgentThinkingState, crate::ui::task::Failure>,
        cx: &mut Context<Self>,
    ) {
        self.thinking_saving = false;
        match result {
            Ok(state) => {
                let applied = self.thinking_cache.finish_pick(&key, Some(state));
                if key.session.is_none() {
                    self.thinking_cache.invalidate_defaults();
                }
                if let Some(state) = applied {
                    self.thinking_cache.insert(key, state);
                }
            }
            Err(error) => {
                self.thinking_cache.finish_pick(&key, None);
                // A pick waiting to replace this one for the same task and
                // model is the one to report on.
                if self
                    .thinking_queued
                    .as_ref()
                    .is_none_or(|(queued, _)| queued != &key)
                {
                    self.notice = Some(error.context("chat-thinking-save-failed"));
                }
            }
        }
        self.refresh_thinking(cx);
        if let Some((key, level)) = self.thinking_queued.take() {
            self.save_thinking(key, level, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: Option<&str>, model: &str) -> ThinkingKey {
        ThinkingKey {
            session: session.map(str::to_string),
            model: model.into(),
        }
    }

    fn state(current: ThinkingLevel) -> AgentThinkingState {
        AgentThinkingState {
            levels: vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High],
            current,
            recommended: Some(ThinkingLevel::Low),
        }
    }

    #[test]
    fn thinking_cache_keeps_task_model_and_computer_defaults_separate() {
        let mut cache = ThinkingCache::default();
        cache.insert(key(None, "model-a"), state(ThinkingLevel::Low));
        cache.insert(key(Some("task"), "model-a"), state(ThinkingLevel::High));
        assert_eq!(
            cache.get(&key(None, "model-a")),
            Some(state(ThinkingLevel::Low))
        );
        assert_eq!(
            cache.get(&key(Some("task"), "model-a")),
            Some(state(ThinkingLevel::High))
        );
        assert!(cache.get(&key(Some("other-task"), "model-a")).is_none());
        assert!(cache.get(&key(Some("task"), "model-b")).is_none());
        assert!(
            ThinkingCache::default()
                .get(&key(None, "model-a"))
                .is_none()
        );
        cache.invalidate_defaults();
        assert!(cache.get(&key(None, "model-a")).is_none());
        assert!(cache.get(&key(Some("task"), "model-a")).is_some());
    }

    #[test]
    fn thinking_cache_is_bounded_and_refreshes_existing_entries() {
        let mut cache = ThinkingCache::default();
        for index in 0..ThinkingCache::CAPACITY {
            cache.insert(
                key(Some(&index.to_string()), "model"),
                state(ThinkingLevel::Low),
            );
        }
        cache.insert(key(Some("0"), "model"), state(ThinkingLevel::High));
        cache.insert(key(Some("newest"), "model"), state(ThinkingLevel::Low));
        assert_eq!(cache.entries.len(), ThinkingCache::CAPACITY);
        assert!(cache.get(&key(Some("1"), "model")).is_none());
        assert_eq!(
            cache.get(&key(Some("0"), "model")),
            Some(state(ThinkingLevel::High))
        );
    }

    #[test]
    fn thinking_pick_previews_immediately_and_rolls_back_after_failure() {
        let mut cache = ThinkingCache::default();
        let key = key(Some("task"), "model");
        cache.begin_pick(key.clone(), state(ThinkingLevel::Low), ThinkingLevel::High);
        assert_eq!(cache.get(&key), Some(state(ThinkingLevel::High)));
        assert_eq!(
            cache.finish_pick(&key, None),
            Some(state(ThinkingLevel::Low))
        );
        assert_eq!(cache.get(&key), Some(state(ThinkingLevel::Low)));
    }

    #[test]
    fn thinking_pick_uses_the_authoritative_clamp_and_levels() {
        let mut cache = ThinkingCache::default();
        let key = key(Some("task"), "model");
        cache.begin_pick(key.clone(), state(ThinkingLevel::Low), ThinkingLevel::High);
        let confirmed = AgentThinkingState {
            levels: vec![ThinkingLevel::Off, ThinkingLevel::Low],
            current: ThinkingLevel::Low,
            recommended: Some(ThinkingLevel::Low),
        };
        assert_eq!(
            cache.finish_pick(&key, Some(confirmed.clone())),
            Some(confirmed.clone())
        );
        assert_eq!(cache.get(&key), Some(confirmed));
    }

    #[test]
    fn thinking_pushed_update_survives_a_late_pick_response_or_failure() {
        for response in [None, Some(state(ThinkingLevel::High))] {
            let mut cache = ThinkingCache::default();
            let key = key(Some("task"), "model");
            cache.begin_pick(key.clone(), state(ThinkingLevel::Low), ThinkingLevel::High);
            cache.insert(key.clone(), state(ThinkingLevel::Off));
            assert!(cache.finish_pick(&key, response).is_none());
            assert_eq!(cache.get(&key), Some(state(ThinkingLevel::Off)));
        }
    }

    #[gpui::test]
    fn thinking_refresh_keeps_cached_options_without_reusing_another_models_levels(
        cx: &mut gpui::TestAppContext,
    ) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .expect("backend"),
        );
        let chat = cx.new(|cx| ChatScreen::new_inner_for_test(backend, "user".into(), cx));
        chat.update(cx, |this, cx| {
            this.selected_model = Some("model-a".into());
            this.thinking_cache
                .insert(key(None, "model-a"), state(ThinkingLevel::Low));
            this.refresh_thinking(cx);
            assert_eq!(this.thinking, Some(state(ThinkingLevel::Low)));
            this.refresh_thinking(cx);
            assert_eq!(this.thinking, Some(state(ThinkingLevel::Low)));
            this.selected_model = Some("model-b".into());
            this.refresh_thinking(cx);
            assert!(
                this.thinking.is_none(),
                "uncached models cannot borrow capabilities"
            );
            this.selected_model = Some("model-a".into());
            this.refresh_thinking(cx);
            assert_eq!(this.thinking, Some(state(ThinkingLevel::Low)));
        });
    }

    /// A pick that waited behind a save still goes out for the model it
    /// was made on after another model is selected, even when a pushed
    /// update dropped that model's levels meanwhile, and the save it
    /// replaces fails without a notice.
    #[gpui::test]
    fn a_waiting_pick_saves_for_its_own_model_after_a_switch(cx: &mut gpui::TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        for evicted in [false, true] {
            let backend = Arc::new(
                crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                    .expect("backend"),
            );
            let chat = cx.new(|cx| ChatScreen::new_inner_for_test(backend, "user".into(), cx));
            chat.update(cx, |this, cx| {
                let model_a = key(None, "model-a");
                this.selected_session = None;
                this.selected_model = Some("model-a".into());
                // High saving for model A, as `save_thinking` leaves it.
                this.thinking_cache.begin_pick(
                    model_a.clone(),
                    state(ThinkingLevel::Low),
                    ThinkingLevel::High,
                );
                this.thinking = this.thinking_cache.get(&model_a);
                this.thinking_saving = true;
                this.pick_thinking(ThinkingLevel::Off, cx);
                assert_eq!(this.queued_thinking(), Some(ThinkingLevel::Off));
                this.selected_model = Some("model-b".into());
                if evicted {
                    // Another pick on New Task landed meanwhile.
                    this.thinking_cache.invalidate_defaults();
                }
                this.refresh_thinking(cx);
                this.thinking_saved(model_a.clone(), Err("offline".to_string().into()), cx);
                assert!(this.notice.is_none(), "the waiting pick replaces it");
                assert!(
                    this.thinking_saving,
                    "the waiting pick goes out ({evicted})"
                );
                assert_eq!(
                    this.thinking_cache.get(&model_a).map(|state| state.current),
                    (!evicted).then_some(ThinkingLevel::Off),
                    "it previews where the levels are known"
                );
                assert!(this.thinking.is_none(), "model B keeps its own levels");
            });
        }
    }

    /// A pick on New Task replaces the one model and level it remembers,
    /// so every model's New Task preview is read again; a task's pick is
    /// its own and leaves them.
    #[gpui::test]
    fn only_a_new_task_pick_drops_the_new_task_previews(cx: &mut gpui::TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .expect("backend"),
        );
        let chat = cx.new(|cx| ChatScreen::new_inner_for_test(backend, "user".into(), cx));
        chat.update(cx, |this, cx| {
            let preview = key(None, "model-b");
            for (picked, kept) in [
                (key(Some("task"), "model-a"), true),
                (key(None, "model-a"), false),
            ] {
                this.thinking_cache
                    .insert(preview.clone(), state(ThinkingLevel::High));
                this.thinking_cache.begin_pick(
                    picked.clone(),
                    state(ThinkingLevel::Low),
                    ThinkingLevel::High,
                );
                this.thinking_saving = true;
                this.thinking_saved(picked.clone(), Ok(state(ThinkingLevel::High)), cx);
                assert_eq!(
                    this.thinking_cache.get(&preview).is_some(),
                    kept,
                    "{picked:?}"
                );
                assert_eq!(
                    this.thinking_cache.get(&picked),
                    Some(state(ThinkingLevel::High))
                );
            }
        });
    }
}

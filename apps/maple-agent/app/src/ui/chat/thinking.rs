//! The composer previews and changes the runtime's persisted thinking level.

use super::*;
use crate::ui::i18n::tr;

/// A model's available levels can be reused, but its selected level belongs to
/// a particular task (or to the new-task default). Screens themselves belong
/// to one account and one computer, so no cache is shared across either boundary.
#[derive(Clone, PartialEq, Eq)]
struct ThinkingKey {
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

    /// A task pick also changes the account default. Other tasks keep their
    /// own levels, while default previews must be revalidated for each model.
    fn invalidate_defaults(&mut self) {
        self.entries.retain(|entry| entry.key.session.is_some());
    }

    fn invalidate_session(&mut self, session: &str) {
        self.entries
            .retain(|entry| entry.key.session.as_deref() != Some(session));
    }
}

fn thinking_label(level: ThinkingLevel) -> SharedString {
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
    pub(super) fn thinking_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("thinking-menu", px(220.))
            .label(tr("chat-thinking-title"))
            .application_vim(self.application_vim_enabled);
        if let Some(state) = &self.thinking {
            if state.levels == [ThinkingLevel::Off] {
                return menu.header(tr("chat-thinking-unsupported"));
            }
            menu = menu.items(state.levels.iter().copied().map(|level| {
                MenuItem::new(
                    SharedString::from(format!("thinking-{}", level.as_str())),
                    thinking_label(level),
                    move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                        this.pick_thinking(level, cx);
                    },
                )
                .current(state.current == level)
                .enabled(!self.thinking_saving)
            }));
        } else {
            menu = menu.header(if self.thinking_loading {
                tr("chat-thinking-loading")
            } else if self.thinking_saving {
                tr("chat-thinking-saving")
            } else if self.selected_model.is_some() {
                tr("chat-thinking-unavailable")
            } else {
                tr("chat-thinking-choose-model")
            });
            if self.selected_model.is_some() && !self.thinking_loading && !self.thinking_saving {
                menu = menu.item(MenuItem::new(
                    "retry-thinking",
                    tr("chat-thinking-retry"),
                    |this: &mut Self, _, cx| {
                        this.refresh_thinking(cx);
                        cx.notify();
                    },
                ));
            }
        }
        menu
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

    pub(super) fn apply_thinking_changed(
        &mut self,
        session: &str,
        state: AgentThinkingState,
        cx: &mut Context<Self>,
    ) {
        self.thinking_cache.invalidate_defaults();
        self.thinking_cache.invalidate_session(session);
        if self.selected_session.as_deref() == Some(session) {
            self.thinking_generation += 1;
            self.thinking_loading = false;
            if let Some(key) = self.thinking_key() {
                self.thinking_cache.insert(key, state.clone());
            }
            self.thinking = Some(state);
        } else if self.selected_session.is_none() {
            self.refresh_thinking(cx);
        }
    }

    fn pick_thinking(&mut self, level: ThinkingLevel, cx: &mut Context<Self>) {
        self.popup.close(cx);
        if self.thinking_saving
            || !self
                .thinking
                .as_ref()
                .is_some_and(|state| state.levels.contains(&level))
        {
            return;
        }
        let Some(key) = self.thinking_key() else {
            return;
        };
        self.thinking_generation += 1;
        self.thinking_loading = false;
        self.thinking_saving = true;
        if let Some(state) = self.thinking.take() {
            self.thinking_cache.begin_pick(key.clone(), state, level);
            self.thinking = self.thinking_cache.get(&key);
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
            move |this, result, cx| {
                this.thinking_saving = false;
                match result {
                    Ok(state) => {
                        let applied = this.thinking_cache.finish_pick(&key, Some(state));
                        this.thinking_cache.invalidate_defaults();
                        if let Some(state) = applied {
                            this.thinking_cache.insert(key.clone(), state.clone());
                            // The runtime persisted this clamped level as the
                            // new-task default. Other models fetch their own
                            // clamp, never borrowing these capabilities.
                            this.thinking_cache.insert(
                                ThinkingKey {
                                    session: None,
                                    model: key.model,
                                },
                                state,
                            );
                        }
                    }
                    Err(error) => {
                        this.thinking_cache.finish_pick(&key, None);
                        this.notice = Some(error.context("chat-thinking-save-failed"));
                    }
                }
                this.refresh_thinking(cx);
                cx.notify();
            },
        );
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
}

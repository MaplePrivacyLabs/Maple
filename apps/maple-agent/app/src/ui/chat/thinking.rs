//! The composer previews and changes the runtime's persisted thinking level.
//!
//! A level is on screen as soon as its task or model is: a model's levels
//! and recommended level come from the catalog the model list was built
//! from, a task's own level from its summary, and a new task's from New
//! Task. The runtime's read then confirms it, and is the only source when
//! those cannot say.

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
            // New Task's entries stay: there is one per model, and one may
            // hold the remembered level, which nothing else here records. A
            // task's level comes back with its summary.
            match self
                .entries
                .iter()
                .position(|entry| entry.key.session.is_some())
            {
                Some(oldest) => self.entries.remove(oldest),
                None => self.entries.pop_front(),
            };
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

    /// New Task's level for `model`: the one read or picked there, else
    /// the model's recommendation, once the level New Task remembers is
    /// known not to be this model's.
    fn expected_new_task_thinking(&self, model: &str) -> Option<AgentThinkingState> {
        let key = ThinkingKey {
            session: None,
            model: model.to_string(),
        };
        self.thinking_cache.get(&key).or_else(|| {
            let remembered = self.default_model.as_deref() == Some(model);
            if remembered && !self.new_task_level_known {
                return None;
            }
            maple_agent::agent::catalog_thinking_state(self.thinking_catalog.as_ref()?, model, None)
        })
    }

    /// What the runtime will answer for `key`, as far as this screen
    /// already holds it, following `AgentRuntimeHandle::thinking_state`:
    /// - a task on its own model is at the level its summary records;
    /// - a task that has not run yet starts where New Task is;
    /// - a task looking at another model gets that model's recommendation,
    ///   as its switch will.
    ///
    /// `None` leaves the level out until the read returns: no catalog, a
    /// model it does not list, or a started task whose host sent no level.
    fn expected_thinking(&self, key: &ThinkingKey) -> Option<AgentThinkingState> {
        let Some(session) = &key.session else {
            return self.expected_new_task_thinking(&key.model);
        };
        let task = self.sessions.iter().find(|task| &task.id == session)?;
        let catalog = self.thinking_catalog.as_ref()?;
        if task
            .model
            .as_deref()
            .is_some_and(|model| model != key.model)
        {
            return maple_agent::agent::catalog_thinking_state(catalog, &key.model, None);
        }
        match task.thinking_level {
            Some(level) => {
                maple_agent::agent::catalog_thinking_state(catalog, &key.model, Some(level))
            }
            None if task.message_count == 0 => self.expected_new_task_thinking(&key.model),
            None => None,
        }
    }

    /// Show the selected task and model's level, from what this screen
    /// holds, and have the runtime confirm it. Responses for a task or
    /// model that is no longer selected, or for a read that started before
    /// the latest pick, are dropped.
    pub(super) fn refresh_thinking(&mut self, cx: &mut Context<Self>) {
        self.thinking_generation += 1;
        self.thinking_loading = false;
        let generation = self.thinking_generation;
        let key = self.thinking_key();
        if key != self.thinking_shown_for {
            self.thinking_epoch = self.thinking_epoch.wrapping_add(1);
            self.thinking_shown_for = key.clone();
        }
        self.thinking = key.as_ref().and_then(|key| {
            self.thinking_cache
                .get(key)
                .or_else(|| self.expected_thinking(key))
        });
        if key.is_none() {
            return;
        }
        // Do not let a read race an optimistic pick. Completion revalidates
        // whichever task is then selected, including navigation during a save.
        if self.thinking_saving {
            return;
        }
        self.thinking_loading = true;
        // Opening a task selects New Task's model and then the task's in
        // one update. Reading after the update asks once, for the last.
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| this.read_thinking(generation, cx))
                .ok();
        });
    }

    /// The runtime's read for the selection `generation` was shown for,
    /// unless another selection or a pick has replaced it.
    fn read_thinking(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.thinking_generation != generation {
            return;
        }
        let Some(key) = self.thinking_key() else {
            return;
        };
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
                            this.note_new_task_level(&key);
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

    /// `key`'s level came from the runtime. For New Task on the default
    /// model, that is the level New Task remembers.
    fn note_new_task_level(&mut self, key: &ThinkingKey) {
        if key.session.is_none() && self.default_model.as_deref() == Some(key.model.as_str()) {
            self.new_task_level_known = true;
        }
    }

    /// Read New Task's level for the default model before New Task is
    /// opened, so it is there the first time, as a task's is. The models
    /// arrive while a task may already be on screen.
    pub(super) fn read_new_task_thinking(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.default_model.clone() else {
            return;
        };
        let key = ThinkingKey {
            session: None,
            model,
        };
        // The selected key has its own read.
        if self.new_task_level_known || self.thinking_key().as_ref() == Some(&key) {
            return;
        }
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let request = key.clone();
        self.call(
            async move { backend.thinking_state(&user_id, None, &request.model).await },
            cx,
            move |this, result, _cx| {
                // A pick or a read made meanwhile is newer than this answer.
                let current = this.default_model.as_deref() == Some(key.model.as_str());
                if !current || this.new_task_level_known {
                    return;
                }
                match result {
                    Ok(state) => {
                        this.new_task_level_known = true;
                        if this.thinking_cache.get(&key).is_none() {
                            this.thinking_cache.insert(key, state);
                        }
                    }
                    Err(error) => log::warn!("Failed to load New Task's thinking level: {error}"),
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
                    // New Task now remembers this model and level, and
                    // every other model starts at its recommendation.
                    self.new_task_level_known = true;
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

    fn catalog() -> maple_sdk::ModelCatalogResponse {
        serde_json::from_value(serde_json::json!({"object":"list","data":[
            {"id":"model-a","reasoning":{"mandatory":true,"supported_efforts":["max","high","low"],"default_effort":"max"}},
            {"id":"model-b","reasoning":{"supported_efforts":["high","low"],"default_effort":"low"}}
        ],"aliases":[]}))
        .unwrap()
    }

    fn task(
        id: &str,
        model: &str,
        level: Option<ThinkingLevel>,
        messages: usize,
    ) -> AgentSessionSummary {
        AgentSessionSummary {
            id: id.into(),
            title: id.into(),
            project_root: "/tmp/project".into(),
            created_ms: 0,
            updated_ms: 0,
            message_count: messages,
            model: Some(model.into()),
            thinking_level: level,
            web_enabled: true,
            permission_mode: Default::default(),
            state: AgentTaskState::Active,
            acp: false,
        }
    }

    /// No runtime answers in these tests (the backend is unreachable), so
    /// whatever `thinking` holds is what the screen shows before a read.
    fn screen(cx: &mut gpui::TestAppContext) -> gpui::Entity<ChatScreen> {
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .expect("backend"),
        );
        let chat = cx.new(|cx| ChatScreen::new_inner_for_test(backend, "user".into(), cx));
        chat.update(cx, |this, _| {
            this.thinking_catalog = Some(catalog());
            this.default_model = Some("model-a".into());
            this.sessions = vec![
                task("started", "model-a", Some(ThinkingLevel::Low), 2),
                task("created", "model-a", None, 0),
                task("older-host", "model-a", None, 2),
            ];
        });
        chat
    }

    fn select(
        this: &mut ChatScreen,
        session: Option<&str>,
        model: &str,
        cx: &mut Context<ChatScreen>,
    ) {
        this.selected_session = session.map(str::to_string);
        this.selected_model = Some(model.into());
        this.refresh_thinking(cx);
    }

    /// A task that ran shows the level its summary records, with its
    /// model's levels from the catalog, the moment it is selected, and
    /// another model it looks at shows the level a switch gives it.
    #[gpui::test]
    fn a_selected_task_shows_its_level_before_the_runtime_answers(cx: &mut gpui::TestAppContext) {
        use ThinkingLevel::{High, Low, Max};
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            select(this, Some("started"), "model-a", cx);
            assert_eq!(
                this.thinking,
                Some(AgentThinkingState {
                    levels: vec![Low, High, Max],
                    current: Low,
                    recommended: Some(Max),
                })
            );
            assert_eq!(this.effort_chosen(), Some(Low), "the chip names it");
            select(this, Some("started"), "model-b", cx);
            assert_eq!(
                this.thinking.as_ref().map(|state| state.current),
                Some(Low),
                "model B's recommendation, not the task's level on model A"
            );
            assert_eq!(
                this.thinking.as_ref().map(|state| state.levels.clone()),
                Some(vec![ThinkingLevel::Off, Low, High])
            );
            // A started task whose host sent no level waits for the read,
            // rather than showing New Task's.
            select(this, Some("older-host"), "model-a", cx);
            assert!(this.thinking.is_none());
            // A model the catalog does not list waits too.
            select(this, Some("started"), "unlisted", cx);
            assert!(this.thinking.is_none());
        });
    }

    /// New Task shows the remembered level once it is known and never a
    /// guess before, another model there starts at its recommendation, and
    /// the task a first message creates keeps what New Task showed.
    #[gpui::test]
    fn new_task_and_the_task_it_creates_show_one_level(cx: &mut gpui::TestAppContext) {
        use ThinkingLevel::{High, Low, Max};
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            select(this, None, "model-a", cx);
            assert!(
                this.thinking.is_none(),
                "the level remembered with the default model is not known yet"
            );
            select(this, None, "model-b", cx);
            assert_eq!(
                this.thinking.as_ref().map(|state| state.current),
                Some(Low),
                "only the default model can have a remembered level"
            );

            // The runtime's read: High was picked with the default model.
            let remembered = AgentThinkingState {
                levels: vec![Low, High, Max],
                current: High,
                recommended: Some(Max),
            };
            this.thinking_cache
                .insert(key(None, "model-a"), remembered.clone());
            this.new_task_level_known = true;
            select(this, None, "model-a", cx);
            assert_eq!(this.thinking, Some(remembered.clone()));

            // The first message creates the task.
            select(this, Some("created"), "model-a", cx);
            assert_eq!(this.thinking, Some(remembered));

            // Picking another model on New Task forgets the remembered
            // level, so both models then start at their recommendations.
            this.selected_session = None;
            this.pick_model("model-b".into(), cx);
            assert_eq!(this.thinking.as_ref().map(|state| state.current), Some(Low));
            this.pick_model("model-a".into(), cx);
            assert_eq!(this.thinking.as_ref().map(|state| state.current), Some(Max));
        });
    }

    /// The navigation itself: opening a task, going back to New Task, and
    /// a first message turning New Task into a task each leave the chip
    /// its level in the same update, with no read answered.
    #[gpui::test]
    fn navigation_never_leaves_the_chip_without_its_level(cx: &mut gpui::TestAppContext) {
        use ThinkingLevel::{High, Low, Max};
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.project_root = Some("/tmp/project".into());
            this.models = vec!["model-a".into(), "model-b".into()];
            // High was picked on New Task with the default model, whose
            // recommendation is Max.
            this.thinking_cache.insert(
                key(None, "model-a"),
                AgentThinkingState {
                    levels: vec![Low, High, Max],
                    current: High,
                    recommended: Some(Max),
                },
            );
            this.new_task_level_known = true;
            this.new_session(cx);
            assert_eq!(this.effort_chosen(), Some(High));

            // A task that ran on model B at High, whose recommendation is Low.
            let opened = task("opened", "model-b", Some(High), 2);
            this.upsert_session(opened.clone(), cx);
            this.set_active_session(opened, Vec::new(), HashMap::new(), cx);
            assert_eq!(this.selected_model.as_deref(), Some("model-b"));
            assert_eq!(this.effort_chosen(), Some(High));

            this.new_session(cx);
            assert_eq!(this.selected_model.as_deref(), Some("model-a"));
            assert_eq!(this.effort_chosen(), Some(High));

            let generation = this.selection_generation;
            this.finish_new_session(task("first", "model-a", None, 0), generation, cx);
            assert_eq!(this.selected_session.as_deref(), Some("first"));
            assert_eq!(this.effort_chosen(), Some(High));
        });
    }

    /// The chip's level morphs when the task and model on screen change
    /// their level, and swaps in when another task or model is selected.
    #[gpui::test]
    fn the_level_belongs_to_one_task_and_model_at_a_time(cx: &mut gpui::TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            select(this, Some("started"), "model-a", cx);
            let shown = this.thinking_epoch;
            this.refresh_thinking(cx);
            let mut state = this.thinking.clone().unwrap();
            state.current = ThinkingLevel::High;
            this.apply_thinking_changed("started", state);
            assert_eq!(this.thinking_epoch, shown, "the same task and model");
            select(this, Some("started"), "model-b", cx);
            assert_ne!(this.thinking_epoch, shown);
            let shown = this.thinking_epoch;
            select(this, Some("created"), "model-b", cx);
            assert_ne!(this.thinking_epoch, shown);
        });
    }

    /// New Task's levels outlast a long walk through tasks: the remembered
    /// level is recorded nowhere else on the screen.
    #[test]
    fn thinking_cache_evicts_tasks_before_new_task() {
        let mut cache = ThinkingCache::default();
        cache.insert(key(None, "model-a"), state(ThinkingLevel::High));
        for index in 0..=ThinkingCache::CAPACITY {
            cache.insert(
                key(Some(&index.to_string()), "model-a"),
                state(ThinkingLevel::Low),
            );
        }
        assert_eq!(cache.entries.len(), ThinkingCache::CAPACITY);
        assert_eq!(
            cache.get(&key(None, "model-a")),
            Some(state(ThinkingLevel::High))
        );
        assert!(cache.get(&key(Some("0"), "model-a")).is_none());
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

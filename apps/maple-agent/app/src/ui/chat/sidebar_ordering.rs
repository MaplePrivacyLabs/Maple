//! Account-owned sidebar order and direct manipulation. The same identities
//! are used by each grouping; dragging never mutates a task's owner or state.
use super::*;

impl ReorderRow {
    pub(super) fn target(
        &self,
        drag: &SidebarDrag,
        grouping: SidebarGroup,
    ) -> Option<SidebarDragItem> {
        match &drag.item {
            SidebarDragItem::Project(source) => self
                .project
                .as_ref()
                .filter(|target| {
                    grouping != SidebarGroup::Connection || source.computer == target.computer
                })
                .cloned()
                .map(SidebarDragItem::Project),
            SidebarDragItem::Connection(_) => {
                self.connection.clone().map(SidebarDragItem::Connection)
            }
            SidebarDragItem::Task(_) if self.task_group == drag.group => {
                self.task.clone().map(SidebarDragItem::Task)
            }
            _ => None,
        }
    }
}

impl Sidebar {
    pub(super) fn connection_grouping_available(&self) -> bool {
        self.has_added_computer()
    }

    pub(super) fn load_order(&mut self, cx: &mut Context<Self>) {
        if self.order_loaded || self.order_loading {
            return;
        }
        self.order_loading = true;
        let backend = self.backend.clone();
        let user = self.user_id.clone();
        self.call(
            async move { backend.get_sidebar_order(&user).await },
            cx,
            |this, result, cx| {
                this.order_loading = false;
                match result {
                    Ok(order) => {
                        this.order =
                            crate::settings::install_sidebar_order(this.user_id.clone(), order);
                        this.order_loaded = true;
                        this.rebuild_order_ranks();
                        this.rebuild_sections();
                        this.refresh_task_order(cx);
                    }
                    Err(message) => cx.emit(SidebarEvent::Notice(message.into())),
                }
                cx.notify();
            },
        );
    }

    pub(super) fn save_order(&self, cx: &mut Context<Self>) {
        if !self.order_loaded {
            return;
        }
        let saved = crate::settings::save_sidebar_order(
            self.backend.clone(),
            self.user_id.clone(),
            self.order.clone(),
        );
        self.call(
            async move {
                saved
                    .await
                    .map_err(|_| "Could not save sidebar order".to_string())?
            },
            cx,
            |_, result, cx| {
                if let Err(message) = result {
                    cx.emit(SidebarEvent::Notice(message.into()));
                }
            },
        );
    }

    pub(super) fn forget_task_order(&mut self, ids: &[String], cx: &mut Context<Self>) {
        if !self.order_loaded {
            return;
        }
        self.sync_cached_order();
        let computer = self.backend.remote_host();
        let keep =
            |key: &SidebarItemKey| key.computer.as_deref() != computer || !ids.contains(&key.id);
        let before = self.order.tasks.len() + self.order.pinned_tasks.len();
        self.order.tasks.retain(keep);
        self.order.pinned_tasks.retain(keep);
        if self.order.tasks.len() + self.order.pinned_tasks.len() != before {
            self.rebuild_order_ranks();
            self.save_order(cx);
        }
    }

    pub(super) fn rebuild_order_ranks(&mut self) {
        self.task_ranks.clear();
        for (rank, task) in self.order.tasks.iter().enumerate() {
            self.task_ranks
                .entry(task.computer.clone())
                .or_default()
                .insert(task.id.clone(), rank);
        }
        self.pinned_task_ranks.clear();
        for (rank, task) in self.order.pinned_tasks.iter().enumerate() {
            self.pinned_task_ranks
                .entry(task.computer.clone())
                .or_default()
                .insert(task.id.clone(), rank);
        }
        // Older saved orders had one rank list. Keep their pinned placement
        // until the user arranges Pinned independently for the first time.
        for (rank, task) in self.order.tasks.iter().enumerate() {
            self.pinned_task_ranks
                .entry(task.computer.clone())
                .or_default()
                .entry(task.id.clone())
                .or_insert(self.order.pinned_tasks.len() + rank);
        }
        self.project_ranks.clear();
        for (rank, project) in self.order.projects.iter().enumerate() {
            self.project_ranks
                .entry(project.computer.clone())
                .or_default()
                .insert(project.id.clone(), rank);
        }
    }

    pub(super) fn task_order_key(
        &self,
        computer: &Option<String>,
        id: &str,
        created: i64,
        updated: i64,
    ) -> (u8, i64) {
        if self.view.sort == SidebarSort::Updated {
            return (0, updated.saturating_neg());
        }
        catalog::manual_order_key(
            self.task_ranks
                .get(computer)
                .and_then(|ranks| ranks.get(id))
                .copied(),
            created,
            self.order.created_before_ms,
        )
    }

    pub(super) fn local_manual_order(
        &self,
        computer: &Option<String>,
    ) -> Option<catalog::ManualOrder> {
        self.manual_order_for(computer, false)
    }

    pub(super) fn local_pinned_manual_order(
        &self,
        computer: &Option<String>,
    ) -> Option<catalog::ManualOrder> {
        self.manual_order_for(computer, true)
    }

    pub(super) fn pinned_task_order_key(
        &self,
        computer: &Option<String>,
        id: &str,
        created: i64,
        updated: i64,
    ) -> (u8, i64) {
        if self.view.sort == SidebarSort::Updated {
            return (0, updated.saturating_neg());
        }
        catalog::manual_order_key(
            self.pinned_task_ranks
                .get(computer)
                .and_then(|ranks| ranks.get(id))
                .copied(),
            created,
            self.order.created_before_ms,
        )
    }

    fn manual_order_for(
        &self,
        computer: &Option<String>,
        pinned: bool,
    ) -> Option<catalog::ManualOrder> {
        (self.view.sort == SidebarSort::Manual && self.order_loaded).then(|| {
            let mut seen = HashSet::new();
            let preferred = if pinned {
                self.order.pinned_tasks.as_slice()
            } else {
                &[]
            };
            catalog::ManualOrder {
                cutoff: self.order.created_before_ms,
                tasks: preferred
                    .iter()
                    .chain(&self.order.tasks)
                    .filter(|task| &task.computer == computer && seen.insert(task.id.clone()))
                    .map(|task| task.id.clone())
                    .collect(),
            }
        })
    }

    pub(super) fn project_order_rank(&self, target: &ProjectTarget) -> usize {
        self.project_ranks
            .get(&target.computer)
            .and_then(|ranks| ranks.get(&target.path))
            .copied()
            .unwrap_or(usize::MAX)
    }

    fn sync_cached_order(&mut self) {
        if let Some(order) = crate::settings::cached_sidebar_order(&self.user_id)
            && self.order != order
        {
            if self.view.sort == SidebarSort::Manual
                && (self.order.tasks != order.tasks
                    || self.order.pinned_tasks != order.pinned_tasks
                    || self.order.created_before_ms != order.created_before_ms)
            {
                self.order_query_dirty = true;
            }
            self.order = order;
            self.rebuild_order_ranks();
        }
    }

    pub(super) fn sync_container_order(&mut self) {
        if !self.order_loaded {
            return;
        }
        self.sync_cached_order();
        let mut changed = false;
        let mut known = self.order.projects.iter().cloned().collect::<HashSet<_>>();
        let mut bytes = serde_json::to_vec(&self.order).map_or(usize::MAX, |value| value.len());
        let projects = self
            .project_roots
            .iter()
            .map(|root| root.target.clone())
            .chain(
                self.computer_project_roots
                    .iter()
                    .flatten()
                    .map(|root| root.target.clone()),
            )
            .collect::<Vec<_>>();
        for target in projects {
            let key = SidebarItemKey {
                computer: target.computer,
                id: target.path,
            };
            let extra =
                serde_json::to_vec(&key).map_or(usize::MAX, |value| value.len().saturating_add(1));
            if !known.contains(&key)
                && self.order.projects.len() < 8192
                && bytes.saturating_add(extra) <= 2 * 1024 * 1024
            {
                bytes += extra;
                known.insert(key.clone());
                self.order.projects.push(key);
                changed = true;
            }
        }
        for computer in self.computers.iter() {
            let extra = serde_json::to_vec(&computer.key)
                .map_or(usize::MAX, |value| value.len().saturating_add(1));
            if !self.order.connections.contains(&computer.key)
                && self.order.connections.len() < 256
                && bytes.saturating_add(extra) <= 2 * 1024 * 1024
            {
                bytes += extra;
                self.order.connections.push(computer.key.clone());
                changed = true;
            }
        }
        if changed {
            self.rebuild_order_ranks();
            // The settings writer owns retries and the Quit barrier, even if
            // this projection is replaced before its acknowledgement arrives.
            drop(crate::settings::save_sidebar_order(
                self.backend.clone(),
                self.user_id.clone(),
                self.order.clone(),
            ));
        }
    }

    pub(crate) fn set_view(&mut self, mut view: SidebarView, cx: &mut Context<Self>) {
        self.sync_cached_order();
        // Changing task order must preserve a saved Connection grouping while
        // the desktop is still loading its paired-computer inventory.
        if view.group != self.view.group
            && view.group == SidebarGroup::Connection
            && !self.connection_grouping_available()
        {
            view.group = SidebarGroup::Project;
        }
        if self.view == view {
            return;
        }
        if view.sort == SidebarSort::Manual && !self.order_loaded {
            self.load_order(cx);
            return;
        }
        if view.sort == SidebarSort::Manual && self.order.created_before_ms == 0 {
            let previous = self.order.clone();
            self.seed_task_order();
            if !order_fits(&self.order) {
                self.order = previous;
                self.rebuild_order_ranks();
                cx.emit(SidebarEvent::Notice(
                    "This sidebar order is too large to save".into(),
                ));
                return;
            }
            self.save_order(cx);
        }
        let sort_changed = self.view.sort != view.sort;
        self.view = view;
        crate::settings::update_settings_in_background(move |settings| {
            settings.sidebar_layout = view
        });
        if sort_changed {
            self.refresh_task_order(cx);
        }
        self.ensure_computer_pages(cx);
        self.rebuild_sections();
        self.list.remeasure_items(0..self.entries.len());
        cx.notify();
    }

    pub(super) fn can_reorder(&self) -> bool {
        self.order_loaded && self.filter.is_empty() && self.rename.is_none()
    }

    fn task_group(
        &self,
        target: ProjectTarget,
        id: &str,
        state: AgentTaskState,
        pinned: bool,
    ) -> TaskGroup {
        if state == AgentTaskState::Archived {
            TaskGroup::Archived
        } else if pinned {
            TaskGroup::Pinned
        } else {
            match self.view.group {
                SidebarGroup::Project | SidebarGroup::Connection => TaskGroup::Project(target),
                SidebarGroup::Status => {
                    let running = if target.computer.as_deref() == self.backend.remote_host() {
                        self.running.contains(id)
                    } else {
                        self.computers
                            .iter()
                            .find(|computer| computer.key == target.computer)
                            .is_some_and(|computer| computer.running.contains(id))
                    };
                    TaskGroup::Status(state == AgentTaskState::Settled && !running)
                }
                SidebarGroup::List => TaskGroup::List,
            }
        }
    }

    pub(super) fn rebuild_reorder_rows(&mut self) {
        let mut rows = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let mut row = ReorderRow::default();
            match *entry {
                SidebarEntry::SectionLabel(SidebarSection::Project(index))
                | SidebarEntry::ProjectEmpty(index) => {
                    row.project = self
                        .project_roots
                        .get(index)
                        .map(|root| root.target.clone());
                }
                SidebarEntry::ComputerProject(index) => {
                    row.project = self
                        .computer_projects
                        .get(index)
                        .map(|root| root.target.clone());
                }
                SidebarEntry::Computer(index) => {
                    if self.view.group == SidebarGroup::Connection {
                        row.connection = self
                            .computers
                            .get(index)
                            .map(|computer| computer.key.clone());
                    }
                }
                SidebarEntry::Task(task) => {
                    if let Some(session) = self.sessions.get(task.session) {
                        let target = self.project_target(&session.project_root);
                        let key = SidebarItemKey {
                            computer: target.computer.clone(),
                            id: session.id.clone(),
                        };
                        row.task_group = Some(self.task_group(
                            target.clone(),
                            &session.id,
                            session.state,
                            task.pinned,
                        ));
                        row.task = Some(key);
                        if matches!(
                            self.view.group,
                            SidebarGroup::Project | SidebarGroup::Connection
                        ) && !task.archived
                            && !task.pinned
                        {
                            row.project = Some(target);
                        }
                    }
                }
                SidebarEntry::OtherTask(index) => {
                    if let Some(task) = self.other_tasks.get(index) {
                        let target = ProjectTarget {
                            computer: task.request.computer.clone(),
                            path: task.session.project_root.clone(),
                        };
                        row.task_group = Some(self.task_group(
                            target.clone(),
                            &task.session.id,
                            task.session.state,
                            task.pinned,
                        ));
                        row.task = Some(SidebarItemKey {
                            computer: target.computer.clone(),
                            id: task.session.id.clone(),
                        });
                        if matches!(
                            self.view.group,
                            SidebarGroup::Project | SidebarGroup::Connection
                        ) && !task.archived
                            && !task.pinned
                        {
                            row.project = Some(target);
                        }
                    }
                }
                SidebarEntry::ComputerMore(index) => {
                    if let Some((computer, root)) = self.computer_more_scope(index) {
                        row.project = root.map(|path| ProjectTarget {
                            computer: computer.clone(),
                            path,
                        });
                        if self.view.group == SidebarGroup::Connection {
                            row.connection = Some(computer);
                        }
                    }
                }
                _ => {}
            }
            if self.view.group == SidebarGroup::Connection && row.connection.is_none() {
                row.connection = row.project.as_ref().map(|project| project.computer.clone());
            }
            rows.push(row);
        }
        self.drag_ranges.clear();
        for (index, row) in rows.iter().enumerate() {
            let items = row
                .project
                .clone()
                .map(SidebarDragItem::Project)
                .into_iter()
                .chain(row.connection.clone().map(SidebarDragItem::Connection))
                .chain(row.task.clone().map(SidebarDragItem::Task));
            for item in items {
                self.drag_ranges
                    .entry(item)
                    .and_modify(|range| range.1 = index)
                    .or_insert((index, index));
            }
        }
        self.reorder_rows = rows;
    }

    pub(super) fn drag_for_row(&self, row: usize, sidebar: gpui::EntityId) -> Option<SidebarDrag> {
        if !self.can_reorder() {
            return None;
        }
        let item = self.entries.get(row)?;
        let (item, name, icon) = match *item {
            SidebarEntry::SectionLabel(SidebarSection::Project(index))
                if self.project_filter.is_none() =>
            {
                let root = self.project_roots.get(index)?;
                (
                    SidebarDragItem::Project(root.target.clone()),
                    root.name.clone(),
                    "folder",
                )
            }
            SidebarEntry::ComputerProject(index) if self.project_filter.is_none() => {
                let root = self.computer_projects.get(index)?;
                (
                    SidebarDragItem::Project(root.target.clone()),
                    root.name.clone(),
                    "folder",
                )
            }
            SidebarEntry::Computer(index)
                if self.view.group == SidebarGroup::Connection && self.project_filter.is_none() =>
            {
                let computer = self.computers.get(index)?;
                (
                    SidebarDragItem::Connection(computer.key.clone()),
                    computer.label.clone(),
                    "monitor",
                )
            }
            SidebarEntry::Task(task) => {
                let session = self.sessions.get(task.session)?;
                (
                    SidebarDragItem::Task(self.reorder_rows.get(row)?.task.clone()?),
                    session.title.clone().into(),
                    "square-pen",
                )
            }
            SidebarEntry::OtherTask(index) => {
                let task = self.other_tasks.get(index)?;
                (
                    SidebarDragItem::Task(self.reorder_rows.get(row)?.task.clone()?),
                    task.session.title.clone().into(),
                    "square-pen",
                )
            }
            _ => return None,
        };
        Some(SidebarDrag {
            sidebar,
            item,
            group: self.reorder_rows.get(row)?.task_group.clone(),
            name,
            icon,
        })
    }

    fn drag_sequence(&self, drag: &SidebarDrag) -> Vec<SidebarDragItem> {
        let mut seen = HashSet::new();
        self.reorder_rows
            .iter()
            .filter_map(|row| row.target(drag, self.view.group))
            .filter(|item| seen.insert(item.clone()))
            .collect()
    }

    pub(super) fn set_drop_target(&mut self, next: Option<SidebarDrop>, cx: &mut Context<Self>) {
        if self.drop_target != next {
            self.drop_target = next;
            cx.notify();
        }
    }

    pub(super) fn update_drop_target(
        &mut self,
        row: usize,
        position: gpui::Point<gpui::Pixels>,
        bounds: gpui::Bounds<gpui::Pixels>,
        drag: &SidebarDrag,
        cx: &mut Context<Self>,
    ) {
        let next = (|| {
            if drag.sidebar != cx.entity_id() || !self.can_reorder() || !bounds.contains(&position)
            {
                return None;
            }
            let target = self.reorder_rows.get(row)?.target(drag, self.view.group)?;
            let (first, _) = *self.drag_ranges.get(&target)?;
            let after = if row == first {
                position.y >= bounds.center().y
            } else {
                true
            };
            let sequence = self.drag_sequence(drag);
            let from = sequence.iter().position(|item| item == &drag.item)?;
            let to = sequence.iter().position(|item| item == &target)? + usize::from(after);
            if to == from || to == from + 1 {
                return None;
            }
            // The trailing edge of one item and leading edge of the next are
            // the same slot. Give that slot one target and one line position.
            let (target, after) = match sequence.get(to) {
                Some(next) => (next.clone(), false),
                None => (sequence.last()?.clone(), true),
            };
            let (first, last) = *self.drag_ranges.get(&target)?;
            Some(SidebarDrop {
                target,
                after,
                hover_row: row,
                line_row: if after { last } else { first },
            })
        })();
        self.set_drop_target(next, cx);
    }

    pub(super) fn finish_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging || self.drop_target.is_some() {
            self.dragging = false;
            self.drop_target = None;
            cx.notify();
        }
    }

    pub(in crate::ui::chat) fn cancel_sidebar_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.dragging {
            return false;
        }
        cx.stop_active_drag(window);
        self.finish_sidebar_drag(cx);
        true
    }

    fn loaded_task_order_seeds(&self) -> HashMap<SidebarItemKey, (i64, i64, bool)> {
        let computer = self.backend.remote_host().map(str::to_owned);
        let mut loaded = HashMap::<SidebarItemKey, (i64, i64, bool)>::new();
        for (key, created, updated, pinned) in self.group_pages.task_order_seeds() {
            loaded
                .entry(key)
                .and_modify(|(_, known, _)| *known = (*known).max(updated))
                .or_insert((created, updated, pinned));
        }
        for host in self.computers.iter() {
            for task in &host.sessions {
                if task.message_count > 0 || host.running.contains(&task.id) {
                    loaded
                        .entry(SidebarItemKey {
                            computer: host.key.clone(),
                            id: task.id.clone(),
                        })
                        .and_modify(|(_, known, _)| *known = (*known).max(task.updated_ms))
                        .or_insert((
                            task.created_ms,
                            task.updated_ms,
                            host.pinned.contains(&task.id),
                        ));
                }
            }
        }
        for task in self.sessions.iter() {
            if task.message_count > 0 || self.running.contains(&task.id) {
                loaded
                    .entry(SidebarItemKey {
                        computer: computer.clone(),
                        id: task.id.clone(),
                    })
                    .and_modify(|(_, known, _)| *known = (*known).max(task.updated_ms))
                    .or_insert((
                        task.created_ms,
                        task.updated_ms,
                        self.pinned_tasks.contains(&task.id),
                    ));
            }
        }
        for (key, (_, _, pinned)) in &mut loaded {
            *pinned = if key.computer == computer {
                self.pinned_tasks.contains(&key.id)
            } else {
                self.computers
                    .iter()
                    .find(|host| host.key == key.computer)
                    .is_some_and(|host| host.pinned.contains(&key.id))
            };
        }
        loaded
    }

    fn seed_task_order(&mut self) {
        let mut loaded = self
            .loaded_task_order_seeds()
            .into_iter()
            .collect::<Vec<_>>();
        loaded.sort_by(|(a, (_, updated_a, _)), (b, (_, updated_b, _))| {
            updated_b
                .cmp(updated_a)
                .then_with(|| a.computer.cmp(&b.computer))
                .then_with(|| a.id.cmp(&b.id))
        });
        let visible = loaded
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let seen = visible.iter().collect::<HashSet<_>>();
        let remaining = self
            .order
            .tasks
            .iter()
            .filter(|key| !seen.contains(key))
            .cloned()
            .collect::<Vec<_>>();
        self.order.tasks = visible.into_iter().chain(remaining).collect();
        self.order.pinned_tasks = loaded
            .into_iter()
            .filter(|(_, (_, _, pinned))| *pinned)
            .map(|(key, _)| key)
            .collect();
        self.order.created_before_ms = crate::settings::timestamp_ms();
        self.rebuild_order_ranks();
    }

    fn materialize_loaded_task_order(&mut self) {
        let known = self.order.tasks.iter().collect::<HashSet<_>>();
        let mut missing = self
            .loaded_task_order_seeds()
            .into_iter()
            .filter(|(key, _)| !known.contains(key))
            .collect::<Vec<_>>();
        missing.sort_by(|(a, (created_a, _, _)), (b, (created_b, _, _))| {
            catalog::manual_order_key(None, *created_a, self.order.created_before_ms)
                .cmp(&catalog::manual_order_key(
                    None,
                    *created_b,
                    self.order.created_before_ms,
                ))
                .then_with(|| a.computer.cmp(&b.computer))
                .then_with(|| a.id.cmp(&b.id))
        });
        let split =
            missing.partition_point(|(_, (created, _, _))| *created > self.order.created_before_ms);
        let older = missing.split_off(split);
        self.order.tasks = missing
            .into_iter()
            .map(|(key, _)| key)
            .chain(std::mem::take(&mut self.order.tasks))
            .chain(older.into_iter().map(|(key, _)| key))
            .collect();
    }

    pub(super) fn commit_reorder(
        &mut self,
        drag: &SidebarDrag,
        target: &SidebarDragItem,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.can_reorder() {
            return;
        }
        self.sync_cached_order();
        let sequence = self.drag_sequence(drag);
        let Some(from) = sequence.iter().position(|item| item == &drag.item) else {
            return;
        };
        let Some(to) = sequence.iter().position(|item| item == target) else {
            return;
        };
        let insertion = to + usize::from(after);
        if insertion == from || insertion == from + 1 {
            return;
        }
        let previous_order = self.order.clone();
        let previous_view = self.view;
        let task_move = matches!(drag.item, SidebarDragItem::Task(_));
        if task_move && self.order.created_before_ms == 0 {
            self.seed_task_order();
        }
        if task_move && drag.group != Some(TaskGroup::Pinned) {
            self.materialize_loaded_task_order();
        }
        match (&drag.item, target) {
            (SidebarDragItem::Project(source), SidebarDragItem::Project(target)) => {
                let source = SidebarItemKey {
                    computer: source.computer.clone(),
                    id: source.path.clone(),
                };
                let target = SidebarItemKey {
                    computer: target.computer.clone(),
                    id: target.path.clone(),
                };
                move_item(&mut self.order.projects, &source, &target, after);
            }
            (SidebarDragItem::Connection(source), SidebarDragItem::Connection(target)) => {
                move_item(&mut self.order.connections, source, target, after)
            }
            (SidebarDragItem::Task(source), SidebarDragItem::Task(target)) => {
                let visible = sequence
                    .iter()
                    .filter_map(|item| match item {
                        SidebarDragItem::Task(key) => Some(key.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let saved = if drag.group == Some(TaskGroup::Pinned) {
                    &mut self.order.pinned_tasks
                } else {
                    &mut self.order.tasks
                };
                include_unranked_tasks(saved, &visible);
                // Moving one row must not swap the other members of its
                // visible group through unrelated projects' global slots.
                move_item(saved, source, target, after);
                self.view.sort = SidebarSort::Manual;
            }
            _ => return,
        }
        if !order_fits(&self.order) {
            self.order = previous_order;
            self.view = previous_view;
            self.rebuild_order_ranks();
            cx.emit(SidebarEvent::Notice(
                "This sidebar order is too large to save".into(),
            ));
            return;
        }
        // Every accepted move refreshes the display indexes, including project
        // moves invoked by menus and drag-and-drop.
        self.rebuild_order_ranks();
        if self.view != previous_view {
            let view = self.view;
            crate::settings::update_settings_in_background(move |settings| {
                settings.sidebar_layout = view
            });
        }
        self.save_order(cx);
        self.close_popups(cx);
        self.rebuild_sections();
        if task_move {
            self.refresh_task_order(cx);
        }
        cx.notify();
    }

    pub(super) fn project_move_available(&self, target: &ProjectTarget, forward: bool) -> bool {
        let mut seen = HashSet::new();
        let projects = self
            .reorder_rows
            .iter()
            .filter_map(|row| row.project.as_ref())
            .filter(|project| {
                self.view.group != SidebarGroup::Connection || project.computer == target.computer
            })
            .filter(|project| seen.insert((*project).clone()))
            .collect::<Vec<_>>();
        projects
            .iter()
            .position(|project| *project == target)
            .is_some_and(|position| {
                if forward {
                    position + 1 < projects.len()
                } else {
                    position > 0
                }
            })
    }

    pub(super) fn move_project(
        &mut self,
        target: &ProjectTarget,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let drag = SidebarDrag {
            sidebar: cx.entity_id(),
            item: SidebarDragItem::Project(target.clone()),
            group: None,
            name: self.project_name(target).into(),
            icon: "folder",
        };
        let sequence = self.drag_sequence(&drag);
        let Some(position) = sequence.iter().position(|item| item == &drag.item) else {
            return;
        };
        let adjacent = if forward {
            sequence.get(position + 1)
        } else {
            position
                .checked_sub(1)
                .and_then(|index| sequence.get(index))
        };
        if let Some(adjacent) = adjacent {
            self.commit_reorder(&drag, adjacent, forward, cx);
        }
    }
}

fn move_item<T: PartialEq>(items: &mut Vec<T>, source: &T, target: &T, after: bool) {
    let Some(from) = items.iter().position(|item| item == source) else {
        return;
    };
    let Some(to) = items.iter().position(|item| item == target) else {
        return;
    };
    let to = to + usize::from(after);
    let to = to - usize::from(from < to);
    if from != to {
        let item = items.remove(from);
        items.insert(to, item);
    }
}

fn order_fits(order: &SidebarOrder) -> bool {
    order.tasks.len() <= 8192
        && order.pinned_tasks.len() <= 8192
        && order.projects.len() <= 8192
        && order.connections.len() <= 256
        && serde_json::to_vec(order).is_ok_and(|json| json.len() <= 2 * 1024 * 1024)
}

/// Materialize new rows in their current relative positions before moving one.
/// Already ranked rows retain their positions, including hidden group members.
fn include_unranked_tasks(saved: &mut Vec<SidebarItemKey>, visible: &[SidebarItemKey]) {
    for (index, key) in visible.iter().enumerate() {
        if saved.contains(key) {
            continue;
        }
        let next = visible[index + 1..]
            .iter()
            .find_map(|next| saved.iter().position(|saved| saved == next));
        let previous = visible[..index]
            .iter()
            .rev()
            .find_map(|previous| saved.iter().position(|saved| saved == previous));
        let at = next
            .or_else(|| previous.map(|previous| previous + 1))
            .unwrap_or(0);
        saved.insert(at, key.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> SidebarItemKey {
        SidebarItemKey {
            computer: None,
            id: id.into(),
        }
    }

    fn task(id: &str, root: &str, updated_ms: i64) -> AgentSessionSummary {
        AgentSessionSummary {
            id: id.into(),
            title: id.into(),
            project_root: root.into(),
            created_ms: 1,
            updated_ms,
            message_count: 1,
            model: None,
            web_enabled: true,
            state: AgentTaskState::Active,
            acp: false,
        }
    }

    #[test]
    fn moving_one_group_member_preserves_all_other_tasks_relative_order() {
        let mut saved = ["a1", "other1", "a2", "other2", "a3"].map(key).to_vec();
        include_unranked_tasks(&mut saved, &[key("a1"), key("a2"), key("a3")]);
        move_item(&mut saved, &key("a3"), &key("a1"), false);
        assert_eq!(saved, ["a3", "a1", "other1", "a2", "other2"].map(key));
    }

    #[test]
    fn moving_a_new_task_keeps_unranked_neighbors_and_hidden_tasks_in_order() {
        let mut saved = ["old1", "hidden", "old2"].map(key).to_vec();
        include_unranked_tasks(
            &mut saved,
            &[key("new2"), key("new1"), key("old1"), key("old2")],
        );
        move_item(&mut saved, &key("new2"), &key("old2"), true);
        assert_eq!(saved, ["new1", "old1", "hidden", "old2", "new2"].map(key));
    }

    #[gpui::test]
    fn pinned_drag_preserves_the_tasks_ordinary_slots(cx: &mut gpui::TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.update(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView {
                group: SidebarGroup::List,
                sort: SidebarSort::Manual,
            };
            sidebar.order.created_before_ms = 100;
            sidebar.order.tasks = ["a", "b", "c", "d"].map(key).to_vec();
            sidebar.pinned_tasks = vec!["a".into(), "c".into()];
            sidebar.rebuild_order_ranks();
            sidebar.set_sessions(
                vec![
                    task("a", "/a", 4),
                    task("b", "/a", 3),
                    task("c", "/b", 2),
                    task("d", "/b", 1),
                ],
                cx,
            );
            let ordinary = sidebar.order.tasks.clone();
            let drag = SidebarDrag {
                sidebar: cx.entity_id(),
                item: SidebarDragItem::Task(key("c")),
                group: Some(TaskGroup::Pinned),
                name: "c".into(),
                icon: "square-pen",
            };
            sidebar.commit_reorder(&drag, &SidebarDragItem::Task(key("a")), false, cx);
            assert_eq!(sidebar.order.tasks, ordinary);
            assert_eq!(sidebar.order.pinned_tasks, [key("c"), key("a")]);
            assert!(
                sidebar.pinned_task_order_key(&None, "c", 1, 2)
                    < sidebar.pinned_task_order_key(&None, "a", 1, 4)
            );
            sidebar.archived_expanded = true;
            for task in &mut sidebar.sessions {
                if task.id == "a" || task.id == "c" {
                    task.state = AgentTaskState::Archived;
                }
            }
            sidebar.rebuild_sections();
            let archived = sidebar
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    SidebarEntry::Task(row) if row.archived => {
                        Some(sidebar.sessions[row.session].id.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(archived, ["a", "c"]);
            for task in &mut sidebar.sessions {
                task.state = AgentTaskState::Active;
            }
            sidebar.pinned_tasks.clear();
            sidebar.rebuild_sections();
            let unpinned = sidebar
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    SidebarEntry::Task(row) => Some(sidebar.sessions[row.session].id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(unpinned, ["a", "b", "c", "d"]);
            // Deleting a local task releases its saved slots, but a task with
            // the same identifier on another computer remains independent.
            let remote = SidebarItemKey {
                computer: Some("remote".into()),
                id: "c".into(),
            };
            sidebar.order.tasks.push(remote.clone());
            sidebar.save_order(cx);
            sidebar.forget_tasks(&["c".into()], cx);
            assert_eq!(sidebar.order.tasks, [key("a"), key("b"), key("d"), remote]);
            assert_eq!(sidebar.order.pinned_tasks, [key("a")]);
        });
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
    }

    #[gpui::test]
    fn initial_manual_order_includes_tasks_in_folded_projects(cx: &mut gpui::TestAppContext) {
        let _guard = crate::settings::SETTINGS_IO_LOCK.lock();
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
        let chat = cx.new(|cx| ChatScreen::new_without_start(backend, "user".into(), cx));
        let sidebar = chat.update(cx, |chat, _| chat.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reset_persisted_for_test();
            sidebar.view.group = SidebarGroup::Project;
            sidebar.collapsed_projects.insert(ProjectTarget {
                computer: None,
                path: "/folded".into(),
            });
            sidebar.set_sessions(
                vec![
                    task("older", "/folded", 10),
                    task("newer", "/folded", 30),
                    task("visible", "/open", 20),
                ],
                cx,
            );
            sidebar.seed_task_order();
            assert_eq!(
                sidebar.order.tasks,
                [key("newer"), key("visible"), key("older")]
            );

            sidebar.reset_persisted_for_test();
            sidebar.view = SidebarView {
                group: SidebarGroup::Project,
                sort: SidebarSort::Manual,
            };
            sidebar.order.created_before_ms = 100;
            sidebar.order.tasks = [key("old-a"), key("old-b")].to_vec();
            sidebar.collapsed_projects.insert(ProjectTarget {
                computer: None,
                path: "/b".into(),
            });
            let mut new_a = task("new-a", "/a", 300);
            new_a.created_ms = 300;
            let mut new_b = task("new-b", "/b", 200);
            new_b.created_ms = 200;
            sidebar.rebuild_order_ranks();
            sidebar.set_sessions(
                vec![
                    new_a,
                    new_b,
                    task("old-a", "/a", 50),
                    task("old-b", "/b", 40),
                ],
                cx,
            );
            let drag = SidebarDrag {
                sidebar: cx.entity_id(),
                item: SidebarDragItem::Task(key("old-a")),
                group: Some(TaskGroup::Project(ProjectTarget {
                    computer: None,
                    path: "/a".into(),
                })),
                name: "old-a".into(),
                icon: "square-pen",
            };
            sidebar.commit_reorder(&drag, &SidebarDragItem::Task(key("new-a")), false, cx);
            assert_eq!(
                sidebar.order.tasks,
                [key("old-a"), key("new-a"), key("new-b"), key("old-b")]
            );
        });
        crate::settings::install_test_settings(crate::settings::AppSettings::default());
    }
}

//! Merge independently paged task states without displaying rows beyond an
//! unread state's frontier. Cursors and summaries stay on the viewing desktop.
use super::*;

pub(super) type Position = ((u8, i64), Option<String>, String);

pub(super) struct Stream<T> {
    pub(super) source: T,
    pub(super) end: Option<Position>,
    pub(super) complete: bool,
}

pub(super) struct Prefix<T> {
    boundary: Option<Position>,
    unknown: bool,
    exact: bool,
    pub(super) next: Vec<T>,
}

impl<T> Prefix<T> {
    pub(super) fn new(streams: impl IntoIterator<Item = Stream<T>>, exact: bool) -> Self {
        let mut result = Self {
            boundary: None,
            unknown: false,
            exact,
            next: Vec::new(),
        };
        for stream in streams {
            if stream.complete {
                continue;
            }
            let Some(end) = stream.end else {
                if !result.unknown {
                    result.next.clear();
                    result.unknown = true;
                }
                result.next.push(stream.source);
                continue;
            };
            if result.unknown {
                continue;
            }
            let ordering = result.boundary.as_ref().map(|boundary| {
                if exact {
                    end.cmp(boundary)
                } else {
                    end.0.cmp(&boundary.0)
                }
            });
            match ordering {
                None | Some(std::cmp::Ordering::Less) => {
                    result.boundary = Some(end);
                    result.next.clear();
                    result.next.push(stream.source);
                }
                Some(std::cmp::Ordering::Equal) => result.next.push(stream.source),
                Some(std::cmp::Ordering::Greater) => {}
            }
        }
        result
    }

    pub(super) fn allows(&self, position: &Position) -> bool {
        if self.unknown {
            return false;
        }
        self.boundary.as_ref().is_none_or(|boundary| {
            if self.exact {
                position <= boundary
            } else {
                // Protocol seven orders timestamp ties by its private row id.
                // Drain the boundary timestamp before exposing its UUID order.
                position.0 < boundary.0
            }
        })
    }
}

impl Sidebar {
    pub(super) fn task_prefix_position(
        &self,
        computer: &Option<String>,
        id: &str,
        created: i64,
        updated: i64,
    ) -> Position {
        (
            self.task_order_key(computer, id, created, updated),
            computer.clone(),
            id.to_owned(),
        )
    }

    pub(super) fn local_task_prefix(&self) -> Option<Prefix<paging::PageKind>> {
        if !self.pages.enabled || !self.filter.is_empty() || self.view.group == SidebarGroup::Status
        {
            return None;
        }
        let computer = self.backend.remote_host().map(str::to_owned);
        Some(Prefix::new(
            [PageKind::Active, PageKind::Settled].map(|kind| Stream {
                source: kind,
                end: self
                    .sessions
                    .iter()
                    .filter(|task| self.pages.kind_contains(kind, &task.id))
                    .map(|task| {
                        self.task_prefix_position(
                            &computer,
                            &task.id,
                            task.created_ms,
                            task.updated_ms,
                        )
                    })
                    .max(),
                complete: self.pages.kind_complete(kind),
            }),
            self.view.sort == SidebarSort::Manual,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(updated: i64, id: &str) -> Position {
        ((0, -updated), None, id.into())
    }

    #[test]
    fn a_merged_page_holds_rows_behind_the_earliest_unread_state() {
        let prefix = Prefix::new(
            [
                Stream {
                    source: "active",
                    end: Some(position(10, "a")),
                    complete: false,
                },
                Stream {
                    source: "settled",
                    end: Some(position(100, "s")),
                    complete: false,
                },
            ],
            false,
        );
        assert!(prefix.allows(&position(110, "new")));
        assert!(!prefix.allows(&position(90, "older-loaded")));
        assert_eq!(prefix.next, ["settled"]);
    }

    #[test]
    fn exhausting_a_state_releases_the_other_states_loaded_prefix() {
        let prefix = Prefix::new(
            [
                Stream {
                    source: "active",
                    end: Some(position(10, "a")),
                    complete: false,
                },
                Stream {
                    source: "settled",
                    end: Some(position(100, "s")),
                    complete: true,
                },
            ],
            false,
        );
        assert!(prefix.allows(&position(90, "older-loaded")));
        assert_eq!(prefix.next, ["active"]);
    }

    #[test]
    fn timestamp_ties_wait_until_their_host_cursor_is_drained() {
        let prefix = Prefix::new(
            [Stream {
                source: "active",
                end: Some(position(100, "z")),
                complete: false,
            }],
            false,
        );
        assert!(!prefix.allows(&position(100, "a")));
        assert!(!prefix.allows(&position(100, "z")));
        assert!(prefix.allows(&position(101, "a")));
    }

    #[test]
    fn exact_manual_frontiers_include_the_computer_and_task_tiebreakers() {
        let boundary = ((1, 2), Some("b".into()), "b".into());
        let prefix = Prefix::new(
            [Stream {
                source: "b",
                end: Some(boundary),
                complete: false,
            }],
            true,
        );
        assert!(prefix.allows(&((1, 2), Some("a".into()), "z".into())));
        assert!(prefix.allows(&((1, 2), Some("b".into()), "a".into())));
        assert!(!prefix.allows(&((1, 2), Some("b".into()), "c".into())));
    }

    #[test]
    fn manual_positions_are_exact_and_unknown_streams_advance_first() {
        let prefix = Prefix::new(
            [Stream {
                source: "active",
                end: Some(position(100, "b")),
                complete: false,
            }],
            true,
        );
        assert!(prefix.allows(&position(100, "a")));
        assert!(prefix.allows(&position(100, "b")));
        assert!(!prefix.allows(&position(100, "c")));
        let unknown = Prefix::new(
            [
                Stream {
                    source: "active",
                    end: Some(position(100, "a")),
                    complete: false,
                },
                Stream {
                    source: "settled",
                    end: None,
                    complete: false,
                },
            ],
            true,
        );
        assert_eq!(unknown.next, ["settled"]);
        assert!(!unknown.allows(&position(101, "a")));
    }
}

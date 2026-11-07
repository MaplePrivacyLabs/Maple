//! Run a backend future and apply its result to a view.
//!
//! Screens do the same dance for every backend call: spawn the future on
//! the backend runtime, await the join handle on the UI executor, then
//! update the entity with the result. This is that dance, written once.

use std::future::Future;

use gpui::{Context, Task};

use crate::backend::{AgentBackend, HostFailure};

/// Keep a bridging task alive on its view. Finished bridges are pruned
/// first, so the vector stays bounded by the number of calls in flight
/// rather than growing forever. Dropping a live task would cancel it,
/// which is why nothing here evicts by age.
pub fn retain(tasks: &std::cell::RefCell<Vec<Task<()>>>, bridge: Task<()>) {
    let mut tasks = tasks.borrow_mut();
    if tasks.len() >= 16 {
        tasks.retain(|task| !task.is_ready());
    }
    tasks.push(bridge);
}

/// Run `future` on the backend runtime and hand its result to `then` on
/// the UI thread. A cancelled or panicked task becomes an `Err` so the
/// view always leaves its busy state.
///
/// Returns the bridging task instead of detaching it: the backend's
/// sender holds the task's waker, and this gpui revision asserts a task
/// is only dropped by the thread that spawned it. Retaining the task on
/// the owning view (see [`retain`]) keeps the final drop on the UI (or
/// test) thread.
pub fn call<V, T, F>(
    backend: &AgentBackend,
    future: F,
    cx: &mut Context<V>,
    then: impl FnOnce(&mut V, Result<T, String>, &mut Context<V>) + 'static,
) -> Task<()>
where
    V: 'static,
    T: Send + 'static,
    F: Future<Output = Result<T, String>> + Send + 'static,
{
    let task = backend.spawn(future);
    cx.spawn(async move |this, cx| {
        let result = task.await.unwrap_or_else(|error| {
            log::debug!("backend task failed: {error:?}");
            Err("The task was cancelled".to_string())
        });
        this.update(cx, |this, cx| then(this, result, cx)).ok();
    })
}

/// Local task completion retains cancellation separately from backend diagnostics.
/// Only the UI thread turns this data into locale-dependent presentation.
#[derive(Debug)]
pub(crate) enum Failure {
    Cancelled(&'static str),
    Backend(String),
    Host(HostFailure),
    Local(&'static str, &'static str),
    Context {
        key: &'static str,
        args: Vec<(&'static str, String)>,
        inner: Box<Failure>,
        fallback: Option<String>,
    },
}

impl Failure {
    /// Worker-side context remains data until the UI receives the result.
    pub(crate) fn with_context(
        self,
        key: &'static str,
        args: Vec<(&'static str, String)>,
        fallback: Option<String>,
    ) -> Self {
        Self::Context {
            key,
            args,
            inner: Box::new(self),
            fallback,
        }
    }
    /// Diagnostics are never localized or used to infer a first-party code.
    pub(crate) fn raw_message(&self) -> Option<&str> {
        match self {
            Self::Backend(message) => Some(message),
            Self::Host(failure) => Some(&failure.message),
            Self::Context { fallback, .. } => fallback.as_deref(),
            Self::Local(_, fallback) => Some(fallback),
            Self::Cancelled(_) => None,
        }
    }

    pub(crate) fn context(self, key: &'static str) -> super::i18n::UiText {
        match self {
            Self::Cancelled(cancelled) => super::i18n::UiText::key(cancelled),
            Self::Backend(error) => super::i18n::UiText::context(key, "error", error.into()),
            failure => super::i18n::UiText::context(key, "error", failure.into()),
        }
    }
}

impl From<Failure> for super::i18n::UiText {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Cancelled(key) => Self::key(key),
            Failure::Backend(error) => error.into(),
            Failure::Host(failure) => super::host_errors::failure_text(&failure),
            Failure::Local(key, _) => Self::key(key),
            Failure::Context {
                key, args, inner, ..
            } => Self::context_args(
                key,
                args.into_iter().map(|(name, value)| (name, value.into())),
                "error",
                (*inner).into(),
            ),
        }
    }
}

impl From<HostFailure> for Failure {
    fn from(failure: HostFailure) -> Self {
        Self::Host(failure)
    }
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Self::Backend(error)
    }
}

impl From<&str> for Failure {
    fn from(error: &str) -> Self {
        Self::Backend(error.to_owned())
    }
}

pub(crate) fn completion<T, E: Into<Failure>>(
    result: Result<Result<T, E>, tokio::task::JoinError>,
    cancelled: &'static str,
) -> Result<T, Failure> {
    match result {
        Ok(result) => result.map_err(Into::into),
        Err(error) => {
            log::debug!("backend task failed: {error:?}");
            Err(Failure::Cancelled(cancelled))
        }
    }
}

/// Semantic counterpart to `call` for UI consumers. Legacy callers that use
/// raw errors for recovery retain their existing String contract.
pub(crate) fn call_semantic<V, T, E, F>(
    backend: &AgentBackend,
    future: F,
    cancelled: &'static str,
    cx: &mut Context<V>,
    then: impl FnOnce(&mut V, Result<T, Failure>, &mut Context<V>) + 'static,
) -> Task<()>
where
    V: 'static,
    T: Send + 'static,
    E: Into<Failure> + Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    let task = backend.spawn(future);
    cx.spawn(async move |this, cx| {
        let result = completion(task.await, cancelled);
        this.update(cx, |this, cx| then(this, result, cx)).ok();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::i18n::{TestLocalizationGuard, UiText, apply_preference};

    #[test]
    fn semantic_completion_keeps_cancellation_and_context_behavior() {
        fn assert_send<T: Send>() {}
        assert_send::<Failure>();
        let _locale = TestLocalizationGuard::enable();
        let result: Result<(), Failure> = Err(Failure::Cancelled("chat-task-cancelled"));
        let notice = completion(Ok(result), "chat-agent-task-cancelled")
            .unwrap_err()
            .context("chat-attachment-failed");
        for (locale, expected) in [
            ("en", "The task was cancelled"),
            ("es", "La tarea se canceló"),
            ("en", "The task was cancelled"),
        ] {
            apply_preference(Some(locale));
            assert_eq!(notice.resolve().as_ref(), expected);
        }
    }

    #[tokio::test]
    async fn cancellation_reformats_but_backend_diagnostic_stays_raw() {
        let _locale = TestLocalizationGuard::enable();
        let task = tokio::spawn(std::future::pending::<Result<(), String>>());
        task.abort();
        let cancelled: UiText = completion(task.await, "chat-task-cancelled")
            .unwrap_err()
            .into();
        let backend: UiText =
            completion::<(), String>(Ok(Err("Provider diagnostic".into())), "chat-task-cancelled")
                .unwrap_err()
                .into();
        for (locale, expected) in [
            ("en", "The task was cancelled"),
            ("es", "La tarea se canceló"),
            ("en", "The task was cancelled"),
        ] {
            apply_preference(Some(locale));
            assert_eq!(cancelled.resolve().as_ref(), expected);
            assert_eq!(backend.resolve().as_ref(), "Provider diagnostic");
        }
    }
}

#[cfg(test)]
mod presented_tests {
    use super::*;
    use crate::ui::i18n::{TestLocalizationGuard, UiText, apply_preference};

    #[test]
    fn typed_host_failure_crosses_worker_and_nested_context_keeps_metadata() {
        let _locale = TestLocalizationGuard::enable();
        let failure = std::thread::spawn(|| {
            Failure::from(HostFailure::new("host-disconnected", "raw diagnostic")).with_context(
                "chat-upload-confirm-failed",
                vec![],
                Some("original contextual diagnostic".into()),
            )
        })
        .join()
        .unwrap();
        assert_eq!(
            failure.raw_message(),
            Some("original contextual diagnostic")
        );
        let notice: UiText = failure.into();
        for locale in ["en", "es", "en"] {
            apply_preference(Some(locale));
            assert!(
                notice
                    .resolve()
                    .contains(crate::ui::i18n::tr("runtime-error-host-disconnected").as_ref())
            );
            assert!(!notice.resolve().contains("raw diagnostic"));
        }
    }
}

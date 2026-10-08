//! Ordered session-resource cleanup registry from Pi v1.0.4.
use crate::types::CallbackError;
use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::{Arc, Mutex, OnceLock},
};

pub type SessionResourceCleanup =
    Arc<dyn Fn(Option<&str>) -> Result<(), CallbackError> + Send + Sync>;
pub type UnregisterSessionResource = Box<dyn Fn() + Send + Sync>;
#[derive(Default)]
struct RegistryState {
    next: u64,
    cleanups: BTreeMap<u64, SessionResourceCleanup>,
}
/// Instance form supports runtime isolation while the module functions retain Pi's global registry.
#[derive(Clone, Default)]
pub struct SessionResourceRegistry(Arc<Mutex<RegistryState>>);
impl SessionResourceRegistry {
    pub fn register(&self, cleanup: SessionResourceCleanup) -> UnregisterSessionResource {
        let mut state = self
            .0
            .lock()
            .expect("session resource registry lock poisoned");
        if !state
            .cleanups
            .values()
            .any(|registered| Arc::ptr_eq(registered, &cleanup))
        {
            let id = state.next;
            state.next = state
                .next
                .checked_add(1)
                .expect("session cleanup registration counter exhausted");
            state.cleanups.insert(id, cleanup.clone());
        }
        drop(state);
        let registry = self.clone();
        Box::new(move || {
            registry
                .0
                .lock()
                .expect("session resource registry lock poisoned")
                .cleanups
                .retain(|_, registered| !Arc::ptr_eq(registered, &cleanup));
        })
    }
    pub fn cleanup(&self, session_id: Option<&str>) -> Result<(), SessionResourceCleanupError> {
        let mut errors = Vec::new();
        let mut cursor = None;
        loop {
            let next = {
                let state = self
                    .0
                    .lock()
                    .expect("session resource registry lock poisoned");
                match cursor {
                    None => state.cleanups.first_key_value(),
                    Some(id) => state
                        .cleanups
                        .range((std::ops::Bound::Excluded(id), std::ops::Bound::Unbounded))
                        .next(),
                }
                .map(|(id, cleanup)| (*id, cleanup.clone()))
            };
            let Some((id, cleanup)) = next else {
                break;
            };
            cursor = Some(id);
            if let Err(error) = cleanup(session_id) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SessionResourceCleanupError { errors })
        }
    }
}
#[derive(Debug)]
pub struct SessionResourceCleanupError {
    pub errors: Vec<CallbackError>,
}
impl fmt::Display for SessionResourceCleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Failed to cleanup session resources")
    }
}
impl Error for SessionResourceCleanupError {}
fn registry() -> &'static SessionResourceRegistry {
    static REGISTRY: OnceLock<SessionResourceRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SessionResourceRegistry::default)
}
pub fn register_session_resource_cleanup(
    cleanup: SessionResourceCleanup,
) -> UnregisterSessionResource {
    registry().register(cleanup)
}
pub fn cleanup_session_resources(
    session_id: Option<&str>,
) -> Result<(), SessionResourceCleanupError> {
    registry().cleanup(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_set_order_identity_and_explicit_unregister() {
        let registry = SessionResourceRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let first_calls = calls.clone();
        let first: SessionResourceCleanup = Arc::new(move |session| {
            first_calls
                .lock()
                .unwrap()
                .push(format!("first:{}", session.unwrap_or("all")));
            Ok(())
        });
        let remove_first = registry.register(first.clone());
        let remove_duplicate = registry.register(first);
        let second_calls = calls.clone();
        let _remove_second = registry.register(Arc::new(move |session| {
            second_calls
                .lock()
                .unwrap()
                .push(format!("second:{}", session.unwrap_or("all")));
            Ok(())
        }));
        registry.cleanup(Some("session")).unwrap();
        assert_eq!(*calls.lock().unwrap(), ["first:session", "second:session"]);
        remove_duplicate();
        remove_first();
        registry.cleanup(None).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["first:session", "second:session", "second:all"]
        );
    }

    #[test]
    fn attempts_every_cleanup_and_aggregates_errors_in_order() {
        let registry = SessionResourceRegistry::default();
        let removals = ["first", "second"].map(|message| {
            registry.register(Arc::new(move |_| {
                Err(Arc::new(std::io::Error::other(message)))
            }))
        });
        let error = registry.cleanup(None).unwrap_err();
        assert_eq!(error.to_string(), "Failed to cleanup session resources");
        assert_eq!(
            error
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        for remove in removals {
            remove();
        }
        registry.cleanup(None).unwrap();
    }

    #[test]
    fn visits_additions_and_skips_deleted_callbacks_during_iteration() {
        let registry = SessionResourceRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let remove_second: Arc<Mutex<Option<UnregisterSessionResource>>> = Arc::default();
        let registry_for_first = registry.clone();
        let calls_for_first = calls.clone();
        let remove_for_first = remove_second.clone();
        let calls_for_third = calls.clone();
        let third: SessionResourceCleanup = Arc::new(move |_| {
            calls_for_third.lock().unwrap().push("third");
            Ok(())
        });
        let _remove_first = registry.register(Arc::new(move |_| {
            calls_for_first.lock().unwrap().push("first");
            remove_for_first.lock().unwrap().as_ref().unwrap()();
            let _remove_third = registry_for_first.register(third.clone());
            Ok(())
        }));
        let calls_for_second = calls.clone();
        *remove_second.lock().unwrap() = Some(registry.register(Arc::new(move |_| {
            calls_for_second.lock().unwrap().push("second");
            Ok(())
        })));
        registry.cleanup(None).unwrap();
        assert_eq!(*calls.lock().unwrap(), ["first", "third"]);
    }
}

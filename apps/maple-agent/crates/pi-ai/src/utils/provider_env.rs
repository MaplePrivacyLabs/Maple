//! Pi v1.0.4 provider environment lookup, excluding Bun's sandbox fallback.
use crate::types::ProviderEnv;
pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    get_provider_env_value_with(name, env, |key| std::env::var(key).ok())
}
/// Explicit process lookup keeps scoped tests independent of process-global mutation.
pub fn get_provider_env_value_with(
    name: &str,
    env: Option<&ProviderEnv>,
    process: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    env.and_then(|env| env.get(name))
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| process(name).filter(|v| !v.is_empty()))
}

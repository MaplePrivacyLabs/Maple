//! The upstream default stream slot is intentionally global; serialize only its tests.
pub static DEFAULT_STREAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

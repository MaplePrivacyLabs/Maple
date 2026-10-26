//! App entry points share package validation with bounded core subprocesses.

#[cfg(all(target_os = "linux", feature = "desktop"))]
pub use maple_agent::self_launch::child_command;
pub use maple_agent::self_launch::command;
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub use maple_agent::self_launch::executable;

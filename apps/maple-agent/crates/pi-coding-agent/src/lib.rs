pub mod config;
pub mod core {
    pub mod compaction {
        pub mod branch_summarization;
        #[allow(clippy::module_inception)] // Preserve the selected upstream module path.
        pub mod compaction;
        pub mod utils;
    }
    pub mod defaults;
    pub mod messages;
    pub mod session_cwd;
    pub mod session_export;
    pub mod session_manager;
    pub mod usage_totals;
}
pub mod utils {
    pub mod dates;
    pub mod paths;
    pub mod text;
}

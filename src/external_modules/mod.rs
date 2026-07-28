pub mod entities;
pub mod events;
pub mod manager;
pub mod manifest;
pub mod process;
pub mod protocol;
pub mod source_inspection;
pub mod state;

pub const MAX_ENABLED_MODULES: usize = 32;
pub const MAX_COMMANDS_PER_MODULE: usize = 32;
pub const MODULE_DIR_NAME: &str = "lavis/modules";

pub const MODULES_CLI_USAGE: &str =
    "lavis modules [validate <path>|enable <id>|disable <id>|status]";

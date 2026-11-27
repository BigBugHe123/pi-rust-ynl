//! MCP configuration, operations, authentication, and UI state.

#[path = "mcp/actions.rs"]
pub(crate) mod actions;
#[path = "mcp/config_edit.rs"]
pub(crate) mod config_edit;
#[path = "mcp/config_read.rs"]
pub(crate) mod config_read;
#[path = "mcp/test.rs"]
pub(crate) mod test;
#[path = "mcp/ui_state.rs"]
pub(crate) mod ui_state;

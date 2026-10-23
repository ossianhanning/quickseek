//! Quickseek's MCP server pieces: the tools ([`tools`]) and registering the
//! server with AI clients ([`register`]), shared by `quickseek-mcp`, the CLI,
//! the GUI and the installer.

pub mod charts;
pub mod guide;
pub mod register;
pub mod tools;

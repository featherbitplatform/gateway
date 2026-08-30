//! Model Context Protocol server for AI agents.
//!
//! Layout: [`auth`] (bearer tokens → scope), [`tools`] (the typed tool
//! functions over [`crate::state::SharedState`]), [`docs`] (documentation
//! pages embedded in the binary), [`prompts`] (precompiled debugging and
//! authoring prompts). Everything here compiles in every build — the Admin
//! API's `/api/mcp/prompts` uses the renderer even without a transport. Only
//! [`server`] (the `rmcp` adapter and the mounted Streamable HTTP service)
//! sits behind the `mcp` cargo feature.

pub mod auth;

// wired to the rmcp server in Task 9; the registry is exercised by tests until then
#[allow(dead_code)]
pub mod tools;

// resources are served by the rmcp server in Task 9
#[allow(dead_code)]
pub mod docs;

// served by the rmcp server (Task 9) and /api/mcp/prompts (Task 10)
#[allow(dead_code)]
pub mod prompts;

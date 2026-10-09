//! Setup: JSONC marker-block surgical merge, user-level Zed config,
//! per-project config, and the scan for tasks that reuse the Xcode task
//! labels. See `docs/design/dap-proxy.md` §6.1.

pub mod build_server;
pub mod jsonc;
pub mod project;
pub mod task_collisions;
pub mod user;

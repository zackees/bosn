//! The crate's integration tests, linked as one binary (bosn#503).
//!
//! Each former `tests/<name>.rs` target is a module here, so an edit relinks
//! one test binary instead of one per file. Run one module with
//! `cargo test -p bosn-service --test integration -- <module>::`.

mod act_cli;
mod ci_agent_live;
mod ci_checkout_live;
mod ci_cli;
mod ci_live;
mod compose_cli;
mod daemon_cli;
mod hermes_mcp;
mod managed_retention_ci_docker;
mod managed_retention_docker;
mod managed_retention_paths_docker;
mod mcp_setup_ensure_docker;
mod setup_cli;
mod setup_ensure_docker;
mod setup_lifecycle_docker;
mod setup_remote_https;
mod setup_task_docker;
mod stack_task_docker;
mod support;
mod unmanaged_apply;
mod unmanaged_census_cli;

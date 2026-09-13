//! jay — terminal project management tool, git-like (one `.nest/` per folder).
//!
//! A task is a work contract: whoever works on it (human or agent) fills in
//! every field and reports through the standard sections when done.
//!
//! ## Modules
//! - [model] — domain types: `Task`, `Milestone`, `KnowledgeEntry`, status/priority.
//! - [project] — project resolution, init and config (the `.nest/` folder).
//! - [tasks] — plain-text persistence (one TOML file per task).
//! - [knowledge] — project knowledge base (decisions, status, notes).
//! - [diag] — data-integrity diagnostics (doctor) and explicit repair.
//! - [service] — shared task mutation service (lock, atomic writes, patch/report/complete).
//! - [cli] — quick terminal commands.
//! - [mcp] — MCP server (the orchestrator agent's interface).
//! - [gitflow] — optional branch/PR automation.

pub mod cli;
pub mod diag;
pub mod forgejo;
pub mod gitflow;
pub mod knowledge;
pub mod mcp;
pub mod model;
pub mod project;
pub mod service;
pub mod tasks;

//! Reopens the coding-agent conversations in the panes the Rex server restores after it restarts,
//! and carries dispatched worker results back to the orchestrator.

pub mod cli;
mod daemon;
mod event;
mod gate;
mod install;
mod proc;
mod report;
mod restore;
mod rex;
mod runtime;
mod status;
mod store;
mod task;

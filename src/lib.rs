//! Restores Rex sessions, the coding-agent conversations in them, and dispatched worker results
//! after the Rex server restarts.

pub mod cli;
mod daemon;
mod gate;
mod install;
mod kqueue;
mod layout;
mod proc;
mod report;
mod restore;
mod rex;
mod runtime;
mod status;
mod store;
mod task;

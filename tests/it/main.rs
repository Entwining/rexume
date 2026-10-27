#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a scenario harness panics where a step fails; the panic is the report"
)]

// The bench reads memory as macOS accounts it.
#[cfg(target_os = "macos")]
mod bench;
mod cli;
mod common;
mod gate;
mod install;
mod relay;
mod restore;
mod schema;
mod stop;
mod structure;
mod task;
mod upgrade;
mod wait;

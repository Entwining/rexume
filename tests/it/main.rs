#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a scenario harness panics where a step fails; the panic is the report"
)]

mod bench;
mod cli;
mod common;
mod gate;
mod install;
mod restore;
mod schema;
mod stop;
mod structure;
mod task;
mod upgrade;
mod wait;

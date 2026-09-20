#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Binary-only deps (`src/bin/bot.rs`) — referenced so the library crate does not
// warn under `unused_crate_dependencies` when those crates are package deps.
use clap as _;
use tracing_subscriber as _;

pub mod amms;
pub mod arbitrage;
pub mod execution;
pub mod metrics;
pub mod notify;
pub mod ops;
pub mod rpc_pipeline;
pub mod rpc_probe;
pub mod rpc_rate_pressure;
pub mod service;
pub mod signing;
pub mod state_space;

//! High-level arbitrage monitoring primitives.

pub mod data;
pub mod error;
pub mod graph;
pub mod monitor;
pub mod optimizer;
pub mod pathfinder;

pub use monitor::{ArbitrageMonitor, MonitorConfig, OpportunisticScanResult};
pub use optimizer::{OptimizationConfig, OptimizationResult, PathOptimizer};
pub use pathfinder::{ArbitragePath, PathHop};

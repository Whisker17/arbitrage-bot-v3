//! Prometheus metrics facade for the multi-protocol bot (WHI-532).
//!
//! Instrumentation sites call typed helpers in [`record`]; the `metrics::` macros
//! stay confined to this module so label spelling is exhaustively testable.

mod exporter;
mod names;
mod record;

pub use exporter::{
    build_handle_without_listener, build_recorder, install_recorder, parse_metrics_bind,
    render_with_local, with_local_recorder, BLOCK_TO_SUBMIT_BUCKETS, DEFAULT_METRICS_BIND,
    MetricsBindError, PREFLIGHT_BUCKETS, STAGE_BUCKETS,
};
pub use names::{ALL_METRIC_NAMES, *};
pub use record::*;

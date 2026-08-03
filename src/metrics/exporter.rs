//! Prometheus scrape exporter install + bind guards (WHI-532).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::thread;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder};

use super::names::{
    BLOCK_TO_SUBMIT_DURATION_SECONDS, HTTP_TIP_WAIT_DURATION_SECONDS,
    PIPELINE_STAGE_DURATION_SECONDS, PREFLIGHT_DURATION_SECONDS,
};

/// Default scrape bind. Loopback only, by construction.
pub const DEFAULT_METRICS_BIND: &str = "127.0.0.1:9464";

/// Block → submit end-to-end. Mantle block time ≈ 2s, so the whole budget must
/// fit inside one block: resolution is deliberately dense from 250ms to 3s.
pub const BLOCK_TO_SUBMIT_BUCKETS: [f64; 17] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 5.0,
    10.0, 30.0,
];

/// Individual stages are sub-millisecond to low-millisecond; shift the range down.
pub const STAGE_BUCKETS: [f64; 15] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0,
    5.0,
];

/// Preflight is a single RPC round trip; range is network-shaped.
pub const PREFLIGHT_BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.2, 0.35, 0.5, 0.75, 1.0, 2.5, 5.0,
];

/// HTTP tip catch-up wait (WHI-792). Sub-block, dense under the default 800ms deadline.
pub const HTTP_TIP_WAIT_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.2, 0.35, 0.5, 0.75, 1.0, 2.0,
];

#[derive(Debug, thiserror::Error)]
pub enum MetricsBindError {
    #[error("invalid metrics bind address {0:?}: {1}")]
    Parse(String, std::net::AddrParseError),
    #[error(
        "refusing to bind the unauthenticated /metrics endpoint to non-loopback address {0}; \
set BOT_METRICS_ALLOW_PUBLIC_BIND=1 only behind an authenticating reverse proxy"
    )]
    NonLoopback(SocketAddr),
    #[error("metrics recorder install failed: {0}")]
    Install(String),
}

/// Parse + fail closed on a non-loopback bind unless explicitly overridden.
pub fn parse_metrics_bind(
    raw: Option<&str>,
    allow_public: bool,
) -> Result<SocketAddr, MetricsBindError> {
    let text = raw.unwrap_or(DEFAULT_METRICS_BIND);
    let addr: SocketAddr = text
        .parse()
        .map_err(|e| MetricsBindError::Parse(text.to_string(), e))?;
    if !allow_public && !is_loopback(addr.ip()) {
        return Err(MetricsBindError::NonLoopback(addr));
    }
    Ok(addr)
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4 == Ipv4Addr::LOCALHOST || v4.is_loopback(),
        IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST || v6.is_loopback(),
    }
}

fn buckets_builder() -> Result<PrometheusBuilder, MetricsBindError> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(BLOCK_TO_SUBMIT_DURATION_SECONDS.to_string()),
            &BLOCK_TO_SUBMIT_BUCKETS,
        )
        .map_err(|e| MetricsBindError::Install(e.to_string()))?
        .set_buckets_for_metric(
            Matcher::Full(PIPELINE_STAGE_DURATION_SECONDS.to_string()),
            &STAGE_BUCKETS,
        )
        .map_err(|e| MetricsBindError::Install(e.to_string()))?
        .set_buckets_for_metric(
            Matcher::Full(PREFLIGHT_DURATION_SECONDS.to_string()),
            &PREFLIGHT_BUCKETS,
        )
        .map_err(|e| MetricsBindError::Install(e.to_string()))?
        .set_buckets_for_metric(
            Matcher::Full(HTTP_TIP_WAIT_DURATION_SECONDS.to_string()),
            &HTTP_TIP_WAIT_BUCKETS,
        )
        .map_err(|e| MetricsBindError::Install(e.to_string()))
}

/// Build a recorder with the same bucket config but **no** listener.
///
/// Used by unit/integration tests via [`with_local_recorder`].
pub fn build_recorder() -> PrometheusRecorder {
    buckets_builder()
        .expect("bucket config is static and non-empty")
        .build_recorder()
}

/// Run `f` with a process-thread-local recorder so tests do not need the global
/// metrics installer (and do not race other tests for it).
pub fn with_local_recorder<T>(recorder: &PrometheusRecorder, f: impl FnOnce() -> T) -> T {
    metrics::with_local_recorder(recorder, f)
}

/// Build a local recorder, run `f` against it, and return the rendered registry.
pub fn render_with_local<F: FnOnce()>(f: F) -> String {
    let recorder = build_recorder();
    let handle = recorder.handle();
    with_local_recorder(&recorder, f);
    handle.render()
}

/// Install the global recorder with pinned buckets (no HTTP listener).
///
/// Used by `--metrics-dump` and tests that need the process-global recorder.
/// Idempotent-safe: a second call returns `Err` rather than panicking.
pub fn build_handle_without_listener() -> Result<PrometheusHandle, MetricsBindError> {
    install_global(None)
}

/// Install the global recorder with pinned buckets and start the scrape listener.
///
/// Idempotent-safe: a second call returns `Err` rather than panicking.
pub fn install_recorder(bind: SocketAddr) -> Result<PrometheusHandle, MetricsBindError> {
    install_global(Some(bind))
}

fn install_global(bind: Option<SocketAddr>) -> Result<PrometheusHandle, MetricsBindError> {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.get().is_some() {
        return Err(MetricsBindError::Install(
            "metrics recorder already installed".into(),
        ));
    }

    let builder = buckets_builder()?;
    let handle = match bind {
        None => builder
            .install_recorder()
            .map_err(|e| MetricsBindError::Install(e.to_string()))?,
        Some(addr) => {
            let builder = builder.with_http_listener(addr);
            // Mirror PrometheusBuilder::install, but keep the handle for dump/tests.
            let recorder = if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let (recorder, exporter) = {
                    let _g = rt.enter();
                    builder
                        .build()
                        .map_err(|e| MetricsBindError::Install(e.to_string()))?
                };
                rt.spawn(exporter);
                recorder
            } else {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| MetricsBindError::Install(e.to_string()))?;
                let (recorder, exporter) = {
                    let _g = runtime.enter();
                    builder
                        .build()
                        .map_err(|e| MetricsBindError::Install(e.to_string()))?
                };
                thread::Builder::new()
                    .name("metrics-exporter-prometheus".into())
                    .spawn(move || {
                        let _ = runtime.block_on(exporter);
                    })
                    .map_err(|e| MetricsBindError::Install(e.to_string()))?;
                recorder
            };
            let handle = recorder.handle();
            metrics::set_global_recorder(recorder)
                .map_err(|e| MetricsBindError::Install(e.to_string()))?;
            handle
        }
    };

    let _ = INSTALLED.set(());
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_bind_defaults_to_loopback() {
        assert_eq!(
            parse_metrics_bind(None, false).unwrap().to_string(),
            "127.0.0.1:9464"
        );
    }

    #[test]
    fn non_loopback_bind_is_rejected_without_override() {
        let err = parse_metrics_bind(Some("0.0.0.0:9464"), false).unwrap_err();
        assert!(err.to_string().contains("non-loopback"), "err={err}");
    }

    #[test]
    fn non_loopback_bind_is_allowed_with_explicit_override() {
        assert!(parse_metrics_bind(Some("0.0.0.0:9464"), true).is_ok());
    }
}

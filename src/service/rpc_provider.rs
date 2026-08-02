//! Production RPC provider construction (WHI-786).
//!
//! The multi-protocol bot used to call bare `ProviderBuilder::connect_http` /
//! `connect_ws` while the crate's own tests layered `ThrottleLayer` +
//! `RetryBackoffLayer`. Sustained state-sync against public Mantle endpoints
//! then aborted on transient 429 / 503 responses. This module is the single
//! construction path for production HTTP (and, for request/response only, WS)
//! so future binaries cannot reintroduce an unlayered provider by accident.
//!
//! ## HTTP vs WebSocket
//!
//! * **HTTP** — request-heavy (state sync, log ranges, eth_call). Gets throttle,
//!   retry-with-backoff, and a per-request timeout.
//! * **WS** — used by the bot for a long-lived `newHeads` subscription plus the
//!   occasional tip refresh. Subscriptions are not rate-sensitive the way batch
//!   HTTP is, so throttle is omitted (it would only add head latency). Retry +
//!   per-request timeout still apply to request/response traffic on the socket.
//!
//! ## Defaults
//!
//! Defaults match the values the crate already uses in integration tests
//! (`ThrottleLayer::new(250)`, `RetryBackoffLayer::new(5, 200, 330)`) plus a
//! 30s per-request timeout:
//!
//! * **250 RPS** (`ThrottleLayer` units are requests/sec, burst 1) — caps the
//!   hammering that produced 429s on a 21-pool Agni-V3 sync while still letting
//!   multi-pool state sync finish in seconds, not minutes. Public Mantle
//!   endpoints soft-limit well below this; retries absorb the overflow.
//! * **5 retries / 200 ms initial backoff / 330 CU/s** — alloy's Alchemy-style
//!   defaults already proven in this crate's live-RPC unit tests.
//! * **30 s request timeout** — long enough for a slow `eth_getLogs` batch
//!   under load, short enough that a hung socket fails closed instead of
//!   looking like a deadlock (the bug report sat silent for 15 minutes).

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use alloy::{
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::{
        layers::{
            RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy, ThrottleLayer,
        },
        ws::WsConnect,
        TransportError, TransportErrorKind, TransportFut,
    },
};
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use eyre::{Result, WrapErr};
use tower::{Layer, Service};
use tracing::warn;
use url::Url;

/// Default max requests per second for the HTTP throttle layer.
pub const DEFAULT_HTTP_THROTTLE_RPS: u32 = 250;
/// Default max retries for rate-limit / transient transport errors.
pub const DEFAULT_RETRY_MAX: u32 = 5;
/// Default initial backoff between retries, in milliseconds.
pub const DEFAULT_RETRY_INITIAL_BACKOFF_MS: u64 = 200;
/// Default compute-units-per-second budget fed into alloy's retry pacing.
pub const DEFAULT_RETRY_CUPS: u64 = 330;
/// Default per-request timeout in milliseconds.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Env: HTTP throttle requests-per-second (`ThrottleLayer` units).
pub const ENV_HTTP_THROTTLE_RPS: &str = "RPC_HTTP_THROTTLE_RPS";
/// Env: max retries for transient errors.
pub const ENV_RETRY_MAX: &str = "RPC_HTTP_RETRY_MAX";
/// Env: initial retry backoff in milliseconds.
pub const ENV_RETRY_INITIAL_BACKOFF_MS: &str = "RPC_HTTP_RETRY_INITIAL_BACKOFF_MS";
/// Env: compute-units-per-second for alloy retry pacing.
pub const ENV_RETRY_CUPS: &str = "RPC_HTTP_RETRY_CUPS";
/// Env: per-request timeout in milliseconds.
pub const ENV_REQUEST_TIMEOUT_MS: &str = "RPC_HTTP_REQUEST_TIMEOUT_MS";

/// Tunable knobs for production RPC construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcProviderConfig {
    /// HTTP throttle in requests per second (`ThrottleLayer::new`). Must be > 0.
    pub throttle_rps: u32,
    /// Max retries for transient / rate-limit errors.
    pub max_retries: u32,
    /// Initial backoff in milliseconds.
    pub initial_backoff_ms: u64,
    /// Compute units per second (alloy retry pacing).
    pub compute_units_per_second: u64,
    /// Per-request timeout.
    pub request_timeout: Duration,
}

impl Default for RpcProviderConfig {
    fn default() -> Self {
        Self {
            throttle_rps: DEFAULT_HTTP_THROTTLE_RPS,
            max_retries: DEFAULT_RETRY_MAX,
            initial_backoff_ms: DEFAULT_RETRY_INITIAL_BACKOFF_MS,
            compute_units_per_second: DEFAULT_RETRY_CUPS,
            request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
        }
    }
}

impl RpcProviderConfig {
    /// Load from process env, falling back to [`Self::default`] per field.
    ///
    /// Invalid values (non-numeric, zero throttle, zero timeout) fall back to
    /// the default and emit a warn so misconfiguration is visible.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            throttle_rps: read_u32_env(ENV_HTTP_THROTTLE_RPS, defaults.throttle_rps, 1),
            max_retries: read_u32_env(ENV_RETRY_MAX, defaults.max_retries, 0),
            initial_backoff_ms: read_u64_env(
                ENV_RETRY_INITIAL_BACKOFF_MS,
                defaults.initial_backoff_ms,
                0,
            ),
            compute_units_per_second: read_u64_env(
                ENV_RETRY_CUPS,
                defaults.compute_units_per_second,
                1,
            ),
            request_timeout: Duration::from_millis(read_u64_env(
                ENV_REQUEST_TIMEOUT_MS,
                defaults.request_timeout.as_millis() as u64,
                1,
            )),
        }
    }

    /// Retry layer with the observing policy that logs + metrics on each retry.
    pub fn retry_layer(&self) -> RetryBackoffLayer<ObservingRetryPolicy> {
        RetryBackoffLayer::new_with_policy(
            self.max_retries,
            self.initial_backoff_ms,
            self.compute_units_per_second,
            ObservingRetryPolicy::default(),
        )
    }

    /// Per-request timeout layer.
    pub fn timeout_layer(&self) -> RequestTimeoutLayer {
        RequestTimeoutLayer::new(self.request_timeout)
    }

    /// HTTP-only throttle layer. Panics if `throttle_rps == 0` (guarded by
    /// [`Self::from_env`] / defaults).
    pub fn throttle_layer(&self) -> ThrottleLayer {
        ThrottleLayer::new(self.throttle_rps.max(1))
    }
}

fn read_u32_env(key: &str, default: u32, min: u32) -> u32 {
    match std::env::var(key) {
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(v) if v >= min => v,
            Ok(v) => {
                warn!(
                    target: "service.rpc",
                    key,
                    value = v,
                    min,
                    default,
                    "RPC env value below minimum; using default"
                );
                default
            }
            Err(_) => {
                warn!(
                    target: "service.rpc",
                    key,
                    raw = %raw,
                    default,
                    "RPC env value not a u32; using default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

fn read_u64_env(key: &str, default: u64, min: u64) -> u64 {
    match std::env::var(key) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(v) if v >= min => v,
            Ok(v) => {
                warn!(
                    target: "service.rpc",
                    key,
                    value = v,
                    min,
                    default,
                    "RPC env value below minimum; using default"
                );
                default
            }
            Err(_) => {
                warn!(
                    target: "service.rpc",
                    key,
                    raw = %raw,
                    default,
                    "RPC env value not a u64; using default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

/// Retry policy that extends alloy's [`RateLimitRetryPolicy`] with Mantle
/// provider shapes and emits warn + metric on every retry decision.
///
/// Built-in classification already covers:
/// * HTTP 429 / 503 transport errors
/// * JSON-RPC `-32016` with `"rate limit"` in the message
///
/// This policy additionally treats as retryable:
/// * JSON-RPC `-32011` (`"no backends available for method"`) even when the
///   body arrives as an `ErrorResp` rather than HTTP 503
/// * connection-reset / broken-pipe style transport messages
///
/// Alloy's `RetryBackoffService` only logs at `trace`; this policy is the
/// seam that surfaces retries at `warn` with an attempt ordinal and bumps
/// `arbbot_rpc_retries_total`.
#[derive(Debug, Clone, Default)]
pub struct ObservingRetryPolicy {
    /// Monotonic attempt counter across all in-flight requests on this policy
    /// instance. Used for log correlation; Prometheus uses a separate counter.
    attempts: Arc<AtomicU32>,
}

impl ObservingRetryPolicy {
    /// Number of times [`RetryPolicy::should_retry`] has returned `true`.
    pub fn attempt_count(&self) -> u32 {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl RetryPolicy for ObservingRetryPolicy {
    fn should_retry(&self, error: &TransportError) -> bool {
        let base = RateLimitRetryPolicy::default();
        let retry = base.should_retry(error) || is_mantle_transient(error);
        if retry {
            let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
            let class = classify_retry_error(error);
            warn!(
                target: "service.rpc",
                attempt,
                error_class = class,
                error = %error,
                "retrying RPC request after transient error"
            );
            crate::metrics::record_rpc_retry(class);
        }
        retry
    }

    fn backoff_hint(&self, error: &TransportError) -> Option<Duration> {
        RateLimitRetryPolicy::default()
            .backoff_hint(error)
            .or_else(|| mantle_backoff_hint(error))
    }
}

/// Transient Mantle / public-RPC shapes not covered by alloy's default policy
/// when they arrive as JSON-RPC `ErrorResp` (HTTP 200 with error body) rather
/// than as HTTP 429/503 transport errors.
pub fn is_mantle_transient(error: &TransportError) -> bool {
    match error {
        TransportError::ErrorResp(payload) => {
            if payload.code == -32011 {
                return true;
            }
            let msg = payload.message.to_ascii_lowercase();
            msg.contains("no backends available")
                || msg.contains("backend unavailable")
                || msg.contains("temporarily unavailable")
        }
        TransportError::Transport(TransportErrorKind::Custom(err)) => {
            let msg = err.to_string();
            // Our own per-request timeout is a hard deadline — do not retry it
            // (retrying would multiply the hang budget by max_retries).
            if msg.contains("RPC request timed out after") {
                return false;
            }
            let msg = msg.to_ascii_lowercase();
            msg.contains("connection reset")
                || msg.contains("broken pipe")
                || msg.contains("connection refused")
                || msg.contains("error sending request")
                || msg.contains("tcp connect error")
        }
        TransportError::Transport(TransportErrorKind::HttpError(http)) => {
            // Alloy already covers 429/503; keep other 5xx as retryable for
            // flaky public endpoints (502/504) without retrying 4xx.
            matches!(http.status, 502 | 504)
        }
        _ => false,
    }
}

fn mantle_backoff_hint(error: &TransportError) -> Option<Duration> {
    // No provider-specific hint beyond alloy's parse of "try again in Nms".
    let _ = error;
    None
}

/// Stable label values for `arbbot_rpc_retries_total{error_class=...}`.
pub fn classify_retry_error(error: &TransportError) -> &'static str {
    match error {
        TransportError::Transport(TransportErrorKind::HttpError(http)) if http.status == 429 => {
            "http_429"
        }
        TransportError::Transport(TransportErrorKind::HttpError(http)) if http.status == 503 => {
            "http_503"
        }
        TransportError::Transport(TransportErrorKind::HttpError(_)) => "http_5xx",
        TransportError::ErrorResp(payload) if payload.code == -32016 => "rpc_rate_limit",
        TransportError::ErrorResp(payload) if payload.code == -32011 => "rpc_no_backend",
        TransportError::ErrorResp(_) => "rpc_error",
        TransportError::Transport(TransportErrorKind::Custom(_)) => "transport_custom",
        _ => "other",
    }
}

/// Tower layer that aborts a single RPC call after `timeout`.
///
/// Alloy has no first-class timeout layer; without this a hung socket blocks
/// the bot indefinitely. The error message is stable so callers / tests can
/// match on it (`RPC request timed out`).
#[derive(Debug, Clone)]
pub struct RequestTimeoutLayer {
    timeout: Duration,
}

impl RequestTimeoutLayer {
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Service produced by [`RequestTimeoutLayer`].
#[derive(Debug, Clone)]
pub struct RequestTimeoutService<S> {
    inner: S,
    timeout: Duration,
}

impl<S> Layer<S> for RequestTimeoutLayer {
    type Service = RequestTimeoutService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTimeoutService {
            inner,
            timeout: self.timeout,
        }
    }
}

impl<S> Service<RequestPacket> for RequestTimeoutService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Send
        + 'static
        + Clone,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let timeout = self.timeout;
        let mut inner = self.inner.clone();
        Box::pin(async move {
            match tokio::time::timeout(timeout, inner.call(request)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(timeout_error(timeout)),
            }
        })
    }
}

/// Named timeout error used by [`RequestTimeoutLayer`].
pub fn timeout_error(timeout: Duration) -> TransportError {
    TransportErrorKind::custom_str(&format!(
        "RPC request timed out after {}ms",
        timeout.as_millis()
    ))
}

/// True when `error` is the named per-request timeout from this module.
pub fn is_request_timeout_error(error: &TransportError) -> bool {
    match error {
        TransportError::Transport(TransportErrorKind::Custom(err)) => {
            err.to_string().contains("RPC request timed out after")
        }
        _ => false,
    }
}

/// Build a production HTTP provider with throttle + retry + timeout.
///
/// This is the only construction path the multi-protocol bot should use for
/// HTTP. Layers (outermost first): timeout → retry → throttle → HTTP transport.
///
/// Returns a [`DynProvider`] so callers do not depend on the exact fill-stack
/// type produced by `ProviderBuilder`.
pub fn connect_http_provider(
    http_endpoint: &str,
    config: &RpcProviderConfig,
) -> Result<DynProvider> {
    let url = Url::parse(http_endpoint)
        .wrap_err_with(|| format!("parse HTTP endpoint: {http_endpoint}"))?;

    // Layer order: first added is outermost (see ClientBuilder::layer docs).
    // Outermost timeout bounds the whole retry budget; retry sits outside
    // throttle so a rate-limited call re-enters the limiter on each attempt.
    let client = ClientBuilder::default()
        .layer(config.timeout_layer())
        .layer(config.retry_layer())
        .layer(config.throttle_layer())
        .http(url);

    Ok(ProviderBuilder::new().connect_client(client).erased())
}

/// Build a production WS provider with retry + timeout (no throttle).
///
/// See module docs for why throttle is intentionally omitted on WS.
pub async fn connect_ws_provider(
    ws_endpoint: &str,
    config: &RpcProviderConfig,
) -> Result<DynProvider> {
    let client = ClientBuilder::default()
        .layer(config.timeout_layer())
        .layer(config.retry_layer())
        .ws(WsConnect::new(ws_endpoint.to_string()))
        .await
        .wrap_err_with(|| format!("connect WS provider: {ws_endpoint}"))?;

    Ok(ProviderBuilder::new().connect_client(client).erased())
}

/// Build a provider over an arbitrary layered transport (tests).
///
/// Applies the same timeout + observing-retry stack used in production, but
/// skips throttle so unit tests do not sleep on RPS pacing.
pub fn connect_layered_mock_provider<T>(
    transport: T,
    config: &RpcProviderConfig,
    is_local: bool,
) -> DynProvider
where
    T: alloy::transports::IntoBoxTransport + Clone,
{
    let client = ClientBuilder::default()
        .layer(config.timeout_layer())
        .layer(config.retry_layer())
        .transport(transport, is_local);
    ProviderBuilder::new().connect_client(client).erased()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::{
        providers::Provider,
        transports::mock::{Asserter, MockTransport},
    };
    use alloy_json_rpc::ErrorPayload;
    use std::borrow::Cow;
    use std::sync::Mutex;
    use tower::Service;

    fn error_payload(code: i64, message: &'static str) -> ErrorPayload {
        ErrorPayload {
            code,
            message: Cow::Borrowed(message),
            data: None,
        }
    }

    fn base_policy() -> RateLimitRetryPolicy {
        RateLimitRetryPolicy::default()
    }

    #[test]
    fn rate_limit_32016_is_retryable_via_builtin() {
        let err = TransportError::ErrorResp(error_payload(
            -32016,
            "rate limit exceeded, please try it later.",
        ));
        assert!(base_policy().should_retry(&err));
        assert!(ObservingRetryPolicy::default().should_retry(&err));
    }

    #[test]
    fn http_429_and_503_are_retryable_via_builtin() {
        let e429 = TransportErrorKind::http_error(
            429,
            r#"{"code":-32016,"message":"rate limit exceeded, please try it later."}"#.into(),
        );
        let e503 = TransportErrorKind::http_error(
            503,
            r#"{"code":-32011,"message":"no backends available for method"}"#.into(),
        );
        assert!(base_policy().should_retry(&e429));
        assert!(base_policy().should_retry(&e503));
        assert!(ObservingRetryPolicy::default().should_retry(&e429));
        assert!(ObservingRetryPolicy::default().should_retry(&e503));
    }

    #[test]
    fn rpc_32011_error_resp_is_retryable_via_extension() {
        let err = TransportError::ErrorResp(error_payload(
            -32011,
            "no backends available for method",
        ));
        // Builtin does NOT cover -32011 as ErrorResp (only message keywords).
        assert!(
            !base_policy().should_retry(&err),
            "precondition: alloy base policy must not already cover -32011 ErrorResp"
        );
        assert!(ObservingRetryPolicy::default().should_retry(&err));
        assert_eq!(classify_retry_error(&err), "rpc_no_backend");
    }

    #[test]
    fn connection_reset_is_retryable_via_extension() {
        let err = TransportErrorKind::custom_str("connection reset by peer");
        assert!(!base_policy().should_retry(&err));
        assert!(ObservingRetryPolicy::default().should_retry(&err));
    }

    #[test]
    fn permanent_errors_are_not_retryable() {
        let err = TransportError::ErrorResp(error_payload(-32602, "invalid params"));
        assert!(!ObservingRetryPolicy::default().should_retry(&err));
    }

    #[test]
    fn timeout_error_is_named_and_detectable() {
        let err = timeout_error(Duration::from_millis(50));
        assert!(is_request_timeout_error(&err));
        assert!(err.to_string().contains("RPC request timed out after 50ms"));
    }

    #[test]
    fn default_config_matches_crate_test_values() {
        let c = RpcProviderConfig::default();
        assert_eq!(c.throttle_rps, 250);
        assert_eq!(c.max_retries, 5);
        assert_eq!(c.initial_backoff_ms, 200);
        assert_eq!(c.compute_units_per_second, 330);
        assert_eq!(c.request_timeout, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn mock_rate_limit_then_success_completes() {
        let asserter = Asserter::new();
        // Two rate-limit failures, then success — mirrors the live 429 path.
        asserter.push_failure(error_payload(
            -32016,
            "rate limit exceeded, please try it later.",
        ));
        asserter.push_failure(error_payload(
            -32016,
            "rate limit exceeded, please try it later.",
        ));
        asserter.push_success(&1u64);

        let mut config = RpcProviderConfig::default();
        config.initial_backoff_ms = 1; // keep the test fast
        config.max_retries = 5;

        let provider =
            connect_layered_mock_provider(MockTransport::new(asserter.clone()), &config, true);

        let n = provider.get_block_number().await.expect("should succeed after retries");
        assert_eq!(n, 1);
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn mock_no_backend_then_success_completes() {
        let asserter = Asserter::new();
        asserter.push_failure(error_payload(-32011, "no backends available for method"));
        asserter.push_failure(error_payload(-32011, "no backends available for method"));
        asserter.push_success(&42u64);

        let mut config = RpcProviderConfig::default();
        config.initial_backoff_ms = 1;
        config.max_retries = 5;

        let provider =
            connect_layered_mock_provider(MockTransport::new(asserter.clone()), &config, true);

        let n = provider.get_block_number().await.expect("should succeed after -32011 retries");
        assert_eq!(n, 42);
    }

    /// Transport that never resolves — used to assert the timeout layer.
    #[derive(Clone, Debug, Default)]
    struct HangTransport;

    impl Service<RequestPacket> for HangTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: RequestPacket) -> Self::Future {
            Box::pin(async {
                // Park forever; the timeout layer must cut this short.
                std::future::pending::<()>().await;
                unreachable!()
            })
        }
    }

    #[tokio::test]
    async fn hang_transport_times_out_with_named_error() {
        let mut config = RpcProviderConfig::default();
        // Short real-time deadline so the test stays bounded without test-util
        // paused-time (not enabled on the package tokio feature set).
        config.request_timeout = Duration::from_millis(80);
        config.initial_backoff_ms = 1;
        config.max_retries = 5; // timeout itself is not retryable

        let provider = connect_layered_mock_provider(HangTransport, &config, true);

        let started = std::time::Instant::now();
        let err = provider
            .get_block_number()
            .await
            .expect_err("hang must time out");
        let elapsed = started.elapsed();

        let msg = err.to_string();
        assert!(
            msg.contains("RPC request timed out after"),
            "expected named timeout error, got: {msg}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout must complete in wall-clock bound, took {elapsed:?}"
        );
    }

    #[test]
    fn observing_policy_emits_retry_metric_and_attempt_count() {
        let rendered = crate::metrics::render_with_local(|| {
            crate::metrics::describe_all();
            let policy = ObservingRetryPolicy::default();
            let err = TransportError::ErrorResp(error_payload(
                -32016,
                "rate limit exceeded, please try it later.",
            ));
            assert!(policy.should_retry(&err));
            assert_eq!(policy.attempt_count(), 1);
            assert!(policy.should_retry(&err));
            assert_eq!(policy.attempt_count(), 2);
        });
        assert!(
            rendered.contains("arbbot_rpc_retries_total"),
            "expected rpc retry metric in scrape:\n{rendered}"
        );
        assert!(
            rendered.contains("rpc_rate_limit"),
            "expected error_class label in scrape:\n{rendered}"
        );
    }

    #[test]
    fn named_timeout_is_not_retryable() {
        let err = timeout_error(Duration::from_millis(50));
        assert!(!ObservingRetryPolicy::default().should_retry(&err));
    }

    // Serialize env mutation — other tests may also touch process env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_reads_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(ENV_HTTP_THROTTLE_RPS, "40");
        std::env::set_var(ENV_RETRY_MAX, "8");
        std::env::set_var(ENV_RETRY_INITIAL_BACKOFF_MS, "250");
        std::env::set_var(ENV_RETRY_CUPS, "500");
        std::env::set_var(ENV_REQUEST_TIMEOUT_MS, "15000");

        let c = RpcProviderConfig::from_env();
        assert_eq!(c.throttle_rps, 40);
        assert_eq!(c.max_retries, 8);
        assert_eq!(c.initial_backoff_ms, 250);
        assert_eq!(c.compute_units_per_second, 500);
        assert_eq!(c.request_timeout, Duration::from_millis(15_000));

        std::env::remove_var(ENV_HTTP_THROTTLE_RPS);
        std::env::remove_var(ENV_RETRY_MAX);
        std::env::remove_var(ENV_RETRY_INITIAL_BACKOFF_MS);
        std::env::remove_var(ENV_RETRY_CUPS);
        std::env::remove_var(ENV_REQUEST_TIMEOUT_MS);
    }
}

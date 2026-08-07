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
//! * **8 RPS** HTTP throttle (`ThrottleLayer` units are requests/sec, burst 1)
//!   — WHI-862 measured **8** as sustainable on Mantle public RPC for a
//!   **59-pool** universe. The earlier 250 default relied on retries to absorb
//!   overflow; at **137 pools** that overflow became fatal (`CreateContractSizeLimit`
//!   after a 429 storm — WHI-921). Carry the measured 8 as the default; scale
//!   with [`recommended_throttle_rps`] for larger universes and set
//!   `RPC_HTTP_THROTTLE_RPS` explicitly in ops.
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
        layers::{RateLimitRetryPolicy, RetryPolicy, ThrottleLayer},
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
///
/// WHI-862 measured 8 RPS at 59 pools on Mantle public RPC; WHI-921 carries
/// that value forward as the production default (was 250).
pub const DEFAULT_HTTP_THROTTLE_RPS: u32 = 8;
/// WHI-862 reference universe size used by [`recommended_throttle_rps`].
pub const THROTTLE_REF_POOL_COUNT: u32 = 59;
/// WHI-862 measured RPS at [`THROTTLE_REF_POOL_COUNT`] pools.
pub const THROTTLE_REF_RPS: u32 = 8;

/// Recommended HTTP throttle RPS for a universe of `pool_count` pools.
///
/// Scales inversely from WHI-862's measured 8 RPS @ 59 pools. Floor **4** so a
/// large universe still makes progress; ceiling **16** so small fixtures do not
/// silently re-adopt a 250-class hammer.
///
/// | pools | recommended |
/// |------:|------------:|
/// |    30 |          15 |
/// |    59 |           8 |
/// |   137 |           4 |
pub fn recommended_throttle_rps(pool_count: usize) -> u32 {
    if pool_count == 0 {
        return THROTTLE_REF_RPS;
    }
    let scaled = (THROTTLE_REF_RPS as u64)
        .saturating_mul(THROTTLE_REF_POOL_COUNT as u64)
        / pool_count as u64;
    (scaled as u32).clamp(4, THROTTLE_REF_RPS.saturating_mul(2))
}
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

    /// Retry layer that classifies Mantle transients and emits warn + metric
    /// with a **per-request** attempt ordinal.
    pub fn retry_layer(&self) -> ObservingRetryBackoffLayer {
        ObservingRetryBackoffLayer::new(
            self.max_retries,
            self.initial_backoff_ms,
            self.compute_units_per_second,
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
/// provider shapes. Side-effect free — logging and metrics live in
/// [`ObservingRetryBackoffService`] so the attempt ordinal is per-request.
///
/// Built-in classification already covers:
/// * HTTP 429 / 503 transport errors
/// * JSON-RPC `-32016` with `"rate limit"` in the message
///
/// This policy additionally treats as retryable:
/// * JSON-RPC `-32011` (`"no backends available for method"`) even when the
///   body arrives as an `ErrorResp` rather than HTTP 503
/// * connection-reset / broken-pipe style transport messages
#[derive(Debug, Clone, Default)]
pub struct ObservingRetryPolicy;

impl RetryPolicy for ObservingRetryPolicy {
    fn should_retry(&self, error: &TransportError) -> bool {
        RateLimitRetryPolicy::default().should_retry(error) || is_mantle_transient(error)
    }

    fn backoff_hint(&self, error: &TransportError) -> Option<Duration> {
        RateLimitRetryPolicy::default().backoff_hint(error)
    }
}

/// Alloy-compatible retry layer that logs each retry at `warn` with the
/// **per-request** attempt count and increments `arbbot_rpc_retries_total`.
///
/// Alloy's stock [`RetryBackoffLayer`] only traces retries; this is the
/// production seam for observability (WHI-786 / WHI-532).
#[derive(Debug, Clone)]
pub struct ObservingRetryBackoffLayer {
    max_retries: u32,
    initial_backoff_ms: u64,
    compute_units_per_second: u64,
    policy: ObservingRetryPolicy,
}

impl ObservingRetryBackoffLayer {
    pub const fn new(
        max_retries: u32,
        initial_backoff_ms: u64,
        compute_units_per_second: u64,
    ) -> Self {
        Self {
            max_retries,
            initial_backoff_ms,
            compute_units_per_second,
            policy: ObservingRetryPolicy,
        }
    }
}

impl<S> Layer<S> for ObservingRetryBackoffLayer {
    type Service = ObservingRetryBackoffService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ObservingRetryBackoffService {
            inner,
            policy: self.policy.clone(),
            max_retries: self.max_retries,
            initial_backoff_ms: self.initial_backoff_ms,
            compute_units_per_second: self.compute_units_per_second,
            requests_enqueued: Arc::new(AtomicU32::new(0)),
        }
    }
}

/// Service produced by [`ObservingRetryBackoffLayer`].
#[derive(Debug, Clone)]
pub struct ObservingRetryBackoffService<S> {
    inner: S,
    policy: ObservingRetryPolicy,
    max_retries: u32,
    initial_backoff_ms: u64,
    compute_units_per_second: u64,
    requests_enqueued: Arc<AtomicU32>,
}

impl<S> Service<RequestPacket> for ObservingRetryBackoffService<S>
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
        let inner = self.inner.clone();
        let this = self.clone();
        let mut inner = std::mem::replace(&mut self.inner, inner);
        Box::pin(async move {
            // Mirror alloy's queue-aware CU pacing so concurrent batch sync
            // does not stampede a public endpoint after a 429.
            let ahead_in_queue = this.requests_enqueued.fetch_add(1, Ordering::SeqCst) as u64;
            let mut attempt: u32 = 0;
            loop {
                let err;
                let res = inner.call(request.clone()).await;
                match res {
                    Ok(res) => {
                        if let Some(e) = res.as_error() {
                            err = TransportError::ErrorResp(e.clone());
                        } else {
                            this.requests_enqueued.fetch_sub(1, Ordering::SeqCst);
                            return Ok(res);
                        }
                    }
                    Err(e) => err = e,
                }

                if !this.policy.should_retry(&err) {
                    this.requests_enqueued.fetch_sub(1, Ordering::SeqCst);
                    return Err(err);
                }

                attempt += 1;
                if attempt > this.max_retries {
                    this.requests_enqueued.fetch_sub(1, Ordering::SeqCst);
                    return Err(TransportErrorKind::custom_str(&format!(
                        "Max retries exceeded {err}"
                    )));
                }

                let class = classify_retry_error(&err);
                // WHI-921: feed the Moe CREATE-size discriminator so under-429
                // pressure we slow down instead of fanning out batch splits.
                if class == "http_429" || class == "rpc_rate_limit" {
                    crate::metrics::note_rpc_rate_limit();
                }
                warn!(
                    target: "service.rpc",
                    attempt,
                    max_retries = this.max_retries,
                    error_class = class,
                    error = %err,
                    "retrying RPC request after transient error"
                );
                crate::metrics::record_rpc_retry(class);

                let next_backoff = this
                    .policy
                    .backoff_hint(&err)
                    .unwrap_or_else(|| Duration::from_millis(this.initial_backoff_ms));
                let queued = this.requests_enqueued.load(Ordering::SeqCst) as u64;
                // Same 20 CU average alloy uses for Alchemy-style pacing.
                let avg_cost = 20u64;
                let capacity = this
                    .compute_units_per_second
                    .saturating_div(avg_cost)
                    .max(1);
                let budget_secs = if queued > capacity {
                    queued.min(ahead_in_queue).saturating_div(capacity)
                } else {
                    0
                };
                tokio::time::sleep(next_backoff + Duration::from_secs(budget_secs)).await;
            }
        })
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
/// HTTP. Layers (outermost first): retry → timeout → throttle → HTTP transport.
///
/// Returns a [`DynProvider`] so callers do not depend on the exact fill-stack
/// type produced by `ProviderBuilder`.
pub fn connect_http_provider(
    http_endpoint: &str,
    config: &RpcProviderConfig,
) -> Result<DynProvider> {
    let url = Url::parse(http_endpoint)
        .wrap_err_with(|| format!("parse HTTP endpoint: {http_endpoint}"))?;

    // Layer order: first added is outermost (ClientBuilder::layer docs).
    // Retry outermost so each attempt re-enters the per-attempt timeout and
    // the throttle; a hung single attempt dies at `request_timeout`, then
    // (only if the error is retryable) the next attempt starts a fresh clock.
    let client = ClientBuilder::default()
        .layer(config.retry_layer())
        .layer(config.timeout_layer())
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
        .layer(config.retry_layer())
        .layer(config.timeout_layer())
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
        .layer(config.retry_layer())
        .layer(config.timeout_layer())
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
        // WHI-921: default is WHI-862's measured 8 RPS (was 250).
        assert_eq!(c.throttle_rps, 8);
        assert_eq!(c.throttle_rps, DEFAULT_HTTP_THROTTLE_RPS);
        assert_eq!(c.max_retries, 5);
        assert_eq!(c.initial_backoff_ms, 200);
        assert_eq!(c.compute_units_per_second, 330);
        assert_eq!(c.request_timeout, Duration::from_secs(30));
    }

    #[test]
    fn recommended_throttle_scales_from_whi862_reference() {
        assert_eq!(recommended_throttle_rps(0), 8);
        assert_eq!(recommended_throttle_rps(59), 8);
        // 8 * 59 / 137 ≈ 3.4 → floor 4
        assert_eq!(recommended_throttle_rps(137), 4);
        // 8 * 59 / 30 ≈ 15.7 → 15, within ceiling 16
        assert_eq!(recommended_throttle_rps(30), 15);
        // Tiny universes clamp to ceiling 16
        assert_eq!(recommended_throttle_rps(1), 16);
    }

    #[tokio::test]
    async fn mock_rate_limit_then_success_completes() {
        let asserter = Asserter::new();
        // Two rate-limit failures, then success — JSON-RPC -32016 path.
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

    /// HTTP 429 transport errors (status, not JSON-RPC ErrorResp) — the shape
    /// observed live: `HTTP error 429: {"code":-32016,...}`.
    #[tokio::test]
    async fn mock_http_429_then_success_completes() {
        let body = r#"{"code":-32016,"message":"rate limit exceeded, please try it later."}"#;
        let transport = SequenceTransport::new(vec![
            Err(TransportErrorKind::http_error(429, body.into())),
            Err(TransportErrorKind::http_error(429, body.into())),
            Ok(success_block_number(9)),
        ]);

        let mut config = RpcProviderConfig::default();
        config.initial_backoff_ms = 1;
        config.max_retries = 5;

        let provider = connect_layered_mock_provider(transport, &config, true);
        let n = provider
            .get_block_number()
            .await
            .expect("HTTP 429 must be retried to success");
        assert_eq!(n, 9);
    }

    #[tokio::test]
    async fn mock_http_503_no_backend_then_success_completes() {
        let body = r#"{"code":-32011,"message":"no backends available for method"}"#;
        let transport = SequenceTransport::new(vec![
            Err(TransportErrorKind::http_error(503, body.into())),
            Ok(success_block_number(11)),
        ]);

        let mut config = RpcProviderConfig::default();
        config.initial_backoff_ms = 1;
        config.max_retries = 5;

        let provider = connect_layered_mock_provider(transport, &config, true);
        let n = provider
            .get_block_number()
            .await
            .expect("HTTP 503 -32011 must be retried to success");
        assert_eq!(n, 11);
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

    fn success_block_number(n: u64) -> ResponsePacket {
        use alloy_json_rpc::{Id, Response, ResponsePayload};
        let value = serde_json::to_string(&n).unwrap();
        ResponsePacket::Single(Response {
            id: Id::Number(1),
            payload: ResponsePayload::Success(
                serde_json::value::RawValue::from_string(value).unwrap(),
            ),
        })
    }

    /// Transport that returns a scripted sequence of Ok/Err results (for
    /// HTTP-status error shapes Asserter cannot produce).
    #[derive(Clone, Debug)]
    struct SequenceTransport {
        queue: Arc<std::sync::Mutex<std::collections::VecDeque<Result<ResponsePacket, TransportError>>>>,
    }

    impl SequenceTransport {
        fn new(items: Vec<Result<ResponsePacket, TransportError>>) -> Self {
            Self {
                queue: Arc::new(std::sync::Mutex::new(items.into())),
            }
        }
    }

    impl Service<RequestPacket> for SequenceTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: RequestPacket) -> Self::Future {
            let queue = self.queue.clone();
            Box::pin(async move {
                let next = queue
                    .lock()
                    .expect("sequence queue")
                    .pop_front()
                    .ok_or_else(|| {
                        TransportErrorKind::custom_str("sequence transport exhausted")
                    })?;
                next
            })
        }
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
    fn observing_retry_layer_emits_metric_on_success_after_failures() {
        let asserter = Asserter::new();
        asserter.push_failure(error_payload(
            -32016,
            "rate limit exceeded, please try it later.",
        ));
        asserter.push_success(&3u64);

        let mut config = RpcProviderConfig::default();
        config.initial_backoff_ms = 1;

        let rendered = crate::metrics::render_with_local(|| {
            crate::metrics::describe_all();
            let provider =
                connect_layered_mock_provider(MockTransport::new(asserter.clone()), &config, true);
            // Dedicated runtime so we stay outside any ambient tokio test handle
            // and can drive the layered provider under the local metrics recorder.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            rt.block_on(async {
                let n = provider
                    .get_block_number()
                    .await
                    .expect("success after one retry");
                assert_eq!(n, 3);
            });
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

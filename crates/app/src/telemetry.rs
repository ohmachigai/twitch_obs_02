use metrics::{describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{
    BuildError as PrometheusBuildError, PrometheusBuilder, PrometheusHandle,
};
use std::{
    fmt as stdfmt,
    sync::{Mutex, OnceLock},
    time::Instant,
};
use tracing_subscriber::{
    fmt::{self as tracing_fmt, time::UtcTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

use twi_overlay_util::{AppConfig, Environment};

#[derive(Debug)]
pub enum TelemetryError {
    Tracing(tracing_subscriber::util::TryInitError),
    Metrics(PrometheusBuildError),
}

impl stdfmt::Display for TelemetryError {
    fn fmt(&self, f: &mut stdfmt::Formatter<'_>) -> stdfmt::Result {
        match self {
            Self::Tracing(err) => write!(f, "failed to initialize tracing: {err}"),
            Self::Metrics(err) => write!(f, "failed to initialize prometheus recorder: {err}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<tracing_subscriber::util::TryInitError> for TelemetryError {
    fn from(value: tracing_subscriber::util::TryInitError) -> Self {
        Self::Tracing(value)
    }
}

impl From<PrometheusBuildError> for TelemetryError {
    fn from(value: PrometheusBuildError) -> Self {
        Self::Metrics(value)
    }
}

static TRACING_INIT: OnceLock<()> = OnceLock::new();
static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static METRICS_INSTALL_GUARD: OnceLock<Mutex<()>> = OnceLock::new();
static START_TIME: OnceLock<Instant> = OnceLock::new();

const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

fn build_git_sha() -> &'static str {
    option_env!("GIT_SHA").unwrap_or("unknown")
}

pub fn init_tracing(config: &AppConfig) -> Result<(), TelemetryError> {
    if TRACING_INIT.get().is_some() {
        return Ok(());
    }

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    match config.environment {
        Environment::Development | Environment::Test => {
            let fmt_layer = tracing_fmt::layer()
                .with_target(false)
                .with_level(true)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_timer(UtcTime::rfc_3339())
                .event_format(tracing_fmt::format().pretty());

            tracing_subscriber::registry()
                .with(env_filter.clone())
                .with(fmt_layer)
                .try_init()
                .map_err(TelemetryError::Tracing)?;
        }
        Environment::Production => {
            let fmt_layer = tracing_fmt::layer()
                .with_target(false)
                .with_level(true)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_timer(UtcTime::rfc_3339())
                .json();

            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .try_init()
                .map_err(TelemetryError::Tracing)?;
        }
    }

    TRACING_INIT.set(()).ok();
    tracing::info!(stage = "telemetry", env = %config.environment.as_str(), version = BUILD_VERSION, git_sha = build_git_sha(), "tracing initialized");
    Ok(())
}

pub fn init_metrics() -> Result<PrometheusHandle, TelemetryError> {
    if let Some(handle) = METRICS_HANDLE.get() {
        return Ok(handle.clone());
    }

    let guard = METRICS_INSTALL_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("metrics install guard poisoned");

    if let Some(handle) = METRICS_HANDLE.get() {
        drop(guard);
        return Ok(handle.clone());
    }

    let handle = PrometheusBuilder::new().install_recorder()?;
    METRICS_HANDLE.set(handle.clone()).ok();
    drop(guard);

    describe_gauge!("app_build_info", "Build metadata for the running binary");
    describe_gauge!("app_uptime_seconds", "Seconds since the process started");
    describe_counter!(
        "eventsub_ingress_total",
        "Count of EventSub webhook requests processed, labelled by message type"
    );
    describe_counter!(
        "eventsub_invalid_signature_total",
        "Count of EventSub webhook requests rejected due to invalid signatures"
    );
    describe_histogram!(
        "webhook_ack_latency_seconds",
        "Latency in seconds to acknowledge EventSub webhook requests"
    );
    describe_counter!(
        "policy_commands_total",
        "Count of commands emitted by the policy engine labelled by kind"
    );
    describe_counter!(
        "api_state_requests_total",
        "Count of state API requests, labelled by result"
    );
    describe_counter!(
        "db_ttl_deleted_total",
        "Count of rows deleted by TTL sweeps, labelled by table"
    );
    describe_histogram!(
        "db_checkpoint_seconds",
        "Duration of WAL checkpoint operations in seconds"
    );
    describe_counter!(
        "db_busy_total",
        "Number of SQLite busy conditions encountered by maintenance tasks, labelled by operation"
    );
    START_TIME.get_or_init(Instant::now);

    Ok(handle)
}
pub fn render_metrics(handle: &PrometheusHandle) -> String {
    let mut body = handle.render();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }

    body.push_str("# TYPE app_build_info gauge\n");
    body.push_str(&format!(
        "app_build_info{{version=\"{}\",git=\"{}\"}} 1\n",
        BUILD_VERSION,
        build_git_sha()
    ));

    let uptime = START_TIME
        .get()
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or_default();
    body.push_str("# TYPE app_uptime_seconds gauge\n");
    body.push_str(&format!("app_uptime_seconds {}\n", uptime));

    body
}

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{sse::Sse, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;
use twi_overlay_storage::Database;

use crate::tap::{parse_stage_list, tap_keep_alive, tap_stream, TapFilter, TapHub};
use crate::{telemetry, webhook};

#[derive(Clone)]
pub struct AppState {
    metrics: PrometheusHandle,
    tap: TapHub,
    storage: Database,
    webhook_secret: Arc<[u8]>,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl AppState {
    pub fn new(
        metrics: PrometheusHandle,
        tap: TapHub,
        storage: Database,
        webhook_secret: Arc<[u8]>,
    ) -> Self {
        Self {
            metrics,
            tap,
            storage,
            webhook_secret,
            clock: Arc::new(Utc::now),
        }
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub fn metrics(&self) -> &PrometheusHandle {
        &self.metrics
    }

    pub fn tap(&self) -> &TapHub {
        &self.tap
    }

    pub fn storage(&self) -> &Database {
        &self.storage
    }

    pub fn webhook_secret(&self) -> Arc<[u8]> {
        self.webhook_secret.clone()
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
}

pub fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/_debug/tap", get(debug_tap))
        .route("/eventsub/webhook", post(webhook::handle))
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = telemetry::render_metrics(state.metrics());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(body))
        .unwrap()
}

#[derive(Debug, Deserialize)]
struct TapQuery {
    #[serde(default)]
    s: Option<String>,
}

async fn debug_tap(
    State(state): State<AppState>,
    Query(query): Query<TapQuery>,
) -> Result<
    Sse<impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, serde_json::Error>>>,
    (StatusCode, String),
> {
    let stages = parse_stage_list(query.s).map_err(|err| (StatusCode::BAD_REQUEST, err))?;
    let filter = TapFilter::from_stages(stages);
    let stream = tap_stream(state.tap().clone(), filter);

    Ok(Sse::new(stream).keep_alive(tap_keep_alive()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tokio::time::{self, Duration};
    use tower::ServiceExt;

    use crate::tap::StageEvent;

    async fn setup_state() -> AppState {
        let metrics = telemetry::init_metrics().expect("metrics init");
        let tap = TapHub::new();

        let database = Database::connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect");
        database.run_migrations().await.expect("migrations");

        let secret: Arc<[u8]> = Arc::from(b"test-secret".to_vec().into_boxed_slice());
        AppState::new(metrics, tap, database, secret)
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = app_router(setup_state().await);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_exports_build_info() {
        let app = app_router(setup_state().await);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let collected = response
            .into_body()
            .collect()
            .await
            .expect("body should read");
        let body = String::from_utf8(collected.to_bytes().to_vec()).expect("utf-8");
        assert!(body.contains("app_build_info"));
        assert!(body.contains("app_uptime_seconds"));
    }

    #[tokio::test]
    async fn tap_stream_emits_events() {
        let state = setup_state().await;
        let tap = state.tap().clone();
        let app = app_router(state);

        let request = Request::builder()
            .uri("/_debug/tap")
            .body(Body::empty())
            .unwrap();

        let publish = tokio::spawn(async move {
            time::sleep(Duration::from_millis(25)).await;
            tap.publish(StageEvent::mock("test.event"));
        });

        let mut response = app.oneshot(request).await.expect("handler should respond");

        let frame = time::timeout(Duration::from_secs(1), response.body_mut().frame())
            .await
            .expect("stream produced chunk")
            .expect("chunk ok")
            .expect("chunk available");

        let data = match frame.into_data() {
            Ok(data) => data,
            Err(_) => panic!("expected data frame"),
        };
        let text = String::from_utf8(data.to_vec()).expect("utf-8");
        assert!(text.contains("data:"));
        assert!(text.contains("\"stage\":\"sse\""));

        publish.await.expect("publish task");
    }
}

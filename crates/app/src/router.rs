use axum::{http::StatusCode, routing::get, Router};

pub fn app_router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::app_router;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt; // for `oneshot`

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = app_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("handler should respond");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }
}

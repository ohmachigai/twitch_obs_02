use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ProblemDetails {
    #[serde(rename = "type")]
    problem_type: &'static str,
    title: &'static str,
    detail: String,
}

pub struct ProblemResponse {
    status: StatusCode,
    body: ProblemDetails,
}

impl ProblemResponse {
    pub fn new<S: Into<String>>(status: StatusCode, problem_type: &'static str, detail: S) -> Self {
        Self {
            status,
            body: ProblemDetails {
                problem_type,
                title: status.canonical_reason().unwrap_or("error"),
                detail: detail.into(),
            },
        }
    }
}

impl IntoResponse for ProblemResponse {
    fn into_response(self) -> Response {
        let mut response = Json(self.body).into_response();
        *response.status_mut() = self.status;
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

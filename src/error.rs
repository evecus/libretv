//! 统一错误类型与响应构造。前端 client-api.ts 依赖 `{ "error": "..." }` 的 JSON 错误体
//! 以及 401/503 状态码来触发登录框，这里的契约不能改。

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

pub type ApiResult<T = Response> = Result<T, ApiError>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    TooManyRequests(String),
    #[error("{0}")]
    BadGateway(String),
    #[error("{0}")]
    ServiceUnavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn bad_gateway(err: impl std::fmt::Display) -> Self {
        Self::BadGateway(err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(json!({ "error": self.to_string() }))).into_response()
    }
}

impl From<url::ParseError> for ApiError {
    fn from(e: url::ParseError) -> Self {
        Self::BadRequest(format!("无效的 URL: {e}"))
    }
}

pub fn json_ok(v: Value) -> Response {
    Json(v).into_response()
}

/// NDJSON 流式响应（/api/search?stream=1、/api/quality?stream=1、/api/live/probe?stream=1）
pub fn ndjson_response<S, E>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    E: Into<axum::BoxError> + Send + 'static,
{
    let body = axum::body::Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

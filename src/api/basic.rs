//! /api/status 与 /api/auth（POST 登录 / GET 会话状态 / DELETE 登出）

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, HeaderValue},
    response::Response,
};
use serde_json::{json, Value};

use crate::{
    auth::{check_password, guard, logout_cookie_header, session_cookie_header, session_from_cookie},
    error::{json_ok, ApiError, ApiResult},
    state::AppState,
    util::client_ip,
};

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let required = state.cfg.password_configured();
    let verified = required && session_from_cookie(&state.cfg, &headers);
    json_ok(json!({
        "passwordRequired": required,
        "verified": verified,
        "version": state.cfg.version,
        "defaultSources": state.cfg.default_sources,
        "defaultLiveSources": state.cfg.default_live_sources,
        "defaultSubscriptions": state.cfg.default_subscriptions,
        "defaultRecommendSource": state.cfg.default_recommend_source,
        "defaultImageMode": state.cfg.default_image_mode,
    }))
}

pub async fn login(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    if !state.cfg.password_configured() {
        return Err(ApiError::ServiceUnavailable(
            "服务器未设置 PASSWORD 环境变量，请联系管理员配置".into(),
        ));
    }
    let ip = client_ip(&headers, None);
    if !state.limiter.check(&ip) {
        return Err(ApiError::TooManyRequests("尝试次数过多，请 10 分钟后再试".into()));
    }
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("请求格式错误".into()))?;
    let password = parsed.get("password").and_then(Value::as_str).unwrap_or("");
    if !check_password(&state.cfg, password) {
        return Err(ApiError::Unauthorized("密码错误".into()));
    }
    state.limiter.clear(&ip);
    let (cookie, _) = session_cookie_header(&state, &headers);
    let mut resp = json_ok(json!({ "success": true }));
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    Ok(resp)
}

pub async fn auth_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let verified = state.cfg.password_configured() && session_from_cookie(&state.cfg, &headers);
    json_ok(json!({ "success": true, "verified": verified }))
}

pub async fn logout() -> Response {
    let mut resp = json_ok(json!({ "success": true }));
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(logout_cookie_header()),
    );
    resp
}

/// 供其他模块复用：检查请求是否已登录（未登录返回标准 401/503 错误）
pub fn ensure_login(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    guard(state, headers)?;
    Ok(())
}

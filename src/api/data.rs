//! /api/data：服务端持久化数据接口。
//!
//! 契约（前端 server-data.ts / remote-table.ts 消费）：
//!   GET    /api/data                → { "sections": { history: {key:value,...}, ... } }
//!   PUT    /api/data/{section}      → body { "items": { key: value, ... } } 批量 upsert
//!   DELETE /api/data/{section}      → body 可空（清空整段）或 { "keys": [...] } 按 key 删除
//!
//! 所有接口需登录；分区名走白名单；响应一律 no-store。
//! 缓存类数据不经过这里——它们只存在于内存 TtlCache / 浏览器。

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue},
    response::Response,
};
use serde_json::{json, Map, Value};

use crate::{
    error::{json_ok, ApiError, ApiResult},
    state::AppState,
    store::SECTIONS,
};

fn check_section(s: &str) -> ApiResult<()> {
    if SECTIONS.contains(&s) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(format!("未知数据分区: {}", s.chars().take(40).collect::<String>())))
    }
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    resp
}

/// 全量拉取（前端启动时一次请求完成 hydrate）
pub async fn all(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    Ok(no_store(json_ok(state.store.all()?)))
}

/// 批量 upsert
pub async fn upsert(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(section): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    check_section(&section)?;
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("请求格式错误".into()))?;
    let items: &Map<String, Value> = parsed
        .get("items")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::BadRequest("缺少 items 对象".into()))?;
    let n = state.store.upsert(&section, items)?;
    Ok(no_store(json_ok(json!({ "ok": true, "count": n }))))
}

/// 按 key 删除；无请求体时清空整段
pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(section): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    check_section(&section)?;
    let keys: Option<Vec<String>> = if body.is_empty() {
        None
    } else {
        let parsed: Value = serde_json::from_slice(&body)
            .map_err(|_| ApiError::BadRequest("请求格式错误".into()))?;
        let list = parsed
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::BadRequest("缺少 keys 数组".into()))?;
        Some(
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    };
    let n = state.store.delete(&section, keys.as_deref())?;
    Ok(no_store(json_ok(json!({ "ok": true, "count": n }))))
}

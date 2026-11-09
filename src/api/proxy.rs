//! /api/proxy 与 /api/live/stream：流式透传 + m3u8 重写 + SSRF 防护。
//!
//! 直播流使用独立路径：只给「等响应头」设超时，body 不做总限时，
//! 否则 HTTP-FLV 的单一无限长连接会在数秒后被切断。客户端断开时 Body 被 drop，
//! reqwest 的响应流随之释放，上游连接自动关闭。

use std::{collections::HashMap, time::Duration};

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use futures_util::TryStreamExt;
use serde_json::json;
use url::Url;

use crate::{
    error::{ApiError, ApiResult},
    m3u8::{is_m3u8, rewrite_m3u8, LIVE_STREAM_BASE, PROXY_BASE},
    net::FetchOptions,
    state::AppState,
    util::decode_component,
};

fn is_douban_host(host: &str) -> bool {
    let h = host.to_lowercase();
    h == "douban.com" || h.ends_with(".douban.com") || h == "doubanio.com" || h.ends_with(".doubanio.com")
}

/// 未登录时允许代理的公开图片域（豆瓣封面需要 Referer 伪装）
fn is_anonymous_image_host(host: &str) -> bool {
    let h = host.to_lowercase();
    is_douban_host(&h)
        || h == "doubanio.viki.moe"
        || h.ends_with(".doubanio.viki.moe")
        || h == "bgm.tv"
        || h.ends_with(".bgm.tv")
}

fn looks_like_image_url(target: &str) -> bool {
    Url::parse(target)
        .ok()
        .and_then(|u| u.host_str().map(|h| is_anonymous_image_host(h)))
        .unwrap_or(false)
}

fn host_of(url: &Url) -> String {
    url.host_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']').to_lowercase()
}

fn upstream_options(state: &AppState, target: &Url, req_headers: &HeaderMap) -> FetchOptions {
    let mut opts = FetchOptions::default()
        .with_timeout(state.cfg.request_timeout.as_millis() as u64)
        .header("user-agent", &state.cfg.user_agent);
    if is_douban_host(&host_of(target)) {
        opts = opts.header("referer", "https://movie.douban.com/");
    }
    if let Some(r) = req_headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        opts = opts.header("range", r);
    }
    opts
}

fn streaming_response(resp: reqwest::Response, headers: HeaderMap, status: StatusCode) -> Response {
    let stream = resp
        .bytes_stream()
        .map_err(|e| std::io::Error::other(e.to_string()));
    let mut builder = Response::builder().status(status);
    for (k, v) in headers.iter() {
        builder = builder.header(k.clone(), v.clone());
    }
    builder.body(Body::from_stream(stream)).unwrap()
}

/// 通用代理（点播 / 图片 / 分片 / key）
pub async fn proxy(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let target = q.get("url").cloned().ok_or_else(|| ApiError::BadRequest("缺少 url 参数".into()))?;
    handle_proxy(state, req_headers, target).await
}

/// 旧路径形式 /api/proxy/<encoded>（兼容已缓存的播放列表）
pub async fn proxy_path(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Response> {
    let raw = params.get("url").cloned().unwrap_or_default();
    handle_proxy(state, req_headers, decode_component(&raw)).await
}

async fn handle_proxy(state: AppState, req_headers: HeaderMap, target: String) -> ApiResult<Response> {
    if crate::api::basic::ensure_login(&state, &req_headers).is_err()
        && !looks_like_image_url(&target)
    {
        return Err(ApiError::Unauthorized("未登录".into()));
    }
    let parsed = Url::parse(&target).map_err(|_| ApiError::BadRequest("无效的 URL".into()))?;
    state.upstream.check_url(&target, false).await?;

    let opts = upstream_options(&state, &parsed, &req_headers);
    let result = state
        .upstream
        .fetch_with_retries(&target, &opts, state.cfg.max_retries)
        .await?;
    let crate::net::FetchResult { status, headers: resp_headers, final_url, response: resp } = result;
    let content_type = content_type_of(&resp_headers);

    if is_m3u8(&content_type, &final_url) {
        let bytes = tokio::time::timeout(state.cfg.request_timeout, resp.bytes())
            .await
            .map_err(|_| ApiError::BadGateway("读取 m3u8 超时".into()))?
            .map_err(|e| ApiError::bad_gateway(e))?;
        let text = crate::net::decode_body(&bytes);
        return Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(rewrite_m3u8(&text, &final_url, PROXY_BASE)))
            .unwrap());
    }

    let mut out = HeaderMap::new();
    for name in ["content-type", "accept-ranges", "content-range", "etag", "last-modified"] {
        if let Some(v) = resp_headers.get(name) {
            out.insert(header::HeaderName::from_bytes(name.as_bytes()).unwrap(), v.clone());
        }
    }
    // reqwest 已自动解压，绝不转发 content-length / content-encoding
    let cache = if content_type.starts_with("image/") {
        "public, max-age=2592000"
    } else {
        "public, max-age=3600"
    };
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    Ok(streaming_response(resp, out, status))
}

fn content_type_of(h: &HeaderMap) -> String {
    h.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// 直播流专用长连接代理
pub async fn live_stream(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let target = q.get("url").cloned().ok_or_else(|| ApiError::BadRequest("缺少 url 参数".into()))?;
    handle_live_stream(state, req_headers, target).await
}

pub async fn live_stream_path(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Response> {
    let raw = params.get("url").cloned().unwrap_or_default();
    handle_live_stream(state, req_headers, decode_component(&raw)).await
}

async fn handle_live_stream(
    state: AppState,
    req_headers: HeaderMap,
    target: String,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &req_headers)?;

    let parsed = Url::parse(&target).map_err(|_| ApiError::Forbidden("无效的直播地址".into()))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ApiError::Forbidden("直播地址仅支持 http/https 协议".into()));
    }
    state.upstream.check_url(&target, state.cfg.live_allow_private).await?;

    let opts = FetchOptions::default()
        .with_timeout(15_000)
        .allow_private(state.cfg.live_allow_private)
        .header("user-agent", &state.cfg.user_agent);
    let result = state.upstream.fetch(&target, &opts).await?;
    let crate::net::FetchResult { status, headers: resp_headers, final_url, response: resp } = result;
    let content_type = content_type_of(&resp_headers);

    // 非 2xx：透传原始状态码（区分 token 过期 403 与源瞬断）
    if !status.is_success() {
        return Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .header("X-Live-Upstream-Status", status.as_str())
            .body(Body::from(json!({ "error": format!("直播流上游返回 {status}") }).to_string()))
            .unwrap());
    }

    if is_m3u8(&content_type, &final_url) {
        let bytes = tokio::time::timeout(Duration::from_secs(15), resp.bytes())
            .await
            .map_err(|_| ApiError::BadGateway("读取直播 m3u8 超时".into()))?
            .map_err(|e| ApiError::bad_gateway(e))?;
        let text = crate::net::decode_body(&bytes);
        return Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(rewrite_m3u8(&text, &final_url, LIVE_STREAM_BASE)))
            .unwrap());
    }

    // FLV / TS：纯流式透传，禁止缓冲与缓存
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(if content_type.is_empty() { "video/mp2t" } else { &content_type })
            .unwrap_or(HeaderValue::from_static("video/mp2t")),
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store, no-transform"));
    out.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    Ok(streaming_response(resp, out, status))
}

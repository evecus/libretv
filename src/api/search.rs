//! /api/search（含 NDJSON 逐源流）、/api/detail、/api/source/test

use std::{collections::HashMap, time::Duration};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use bytes::Bytes as BufBytes;
use futures_util::{future::join_all, stream::FuturesUnordered, StreamExt};
use serde_json::{json, Value};

use crate::{
    cms::{aggregate, cms_options, parse_detail, parse_detail_page_html, parse_search_list},
    config::Config,
    error::{json_ok, ndjson_response, ApiError, ApiResult},
    state::AppState,
    util::{encode_component, now_ms, s},
};

fn cms_opts(cfg: &Config) -> crate::net::FetchOptions {
    cms_options(cfg).with_timeout(8000)
}

async fn fetch_page(state: &AppState, base: &str, wd: &str, page: u32) -> ApiResult<Value> {
    let api = format!("{base}?ac=videolist&wd={}&pg={page}", encode_component(wd));
    state.upstream.json(&api, &cms_opts(&state.cfg)).await
}

/// 单源搜索：第一页读 pagecount → 并行抓后续页 → 整源受死线约束。
/// 死线到点会 drop 整个 future，reqwest 在途连接随之关闭（等效原版 AbortController）。
async fn search_source(state: AppState, source: Value, wd: String) -> Value {
    let start = now_ms();
    let key = s(&source, "key").unwrap_or("").to_string();
    let finish = |ok: bool, list: Value, error: Option<String>, timed_out: bool| {
        let mut o = json!({
            "sourceKey": key,
            "ok": ok,
            "list": list,
            "ms": now_ms() - start,
        });
        if let Some(e) = error {
            o["error"] = Value::String(e);
        }
        if timed_out {
            o["timedOut"] = Value::Bool(true);
        }
        o
    };

    let base = s(&source, "url").unwrap_or("").trim().trim_end_matches('/').to_string();
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return finish(false, json!([]), Some("无效的源地址".into()), false);
    }
    if let Err(e) = state.upstream.check_url(&base, false).await {
        return finish(false, json!([]), Some(e.to_string()), false);
    }

    let fut = async {
        let first = fetch_page(&state, &base, &wd, 1).await?;
        let mut list = parse_search_list(&first, &source)
            .map_err(ApiError::BadGateway)?;
        let raw = first
            .get("pagecount")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|x| x.parse().ok())))
            .unwrap_or(1)
            .max(1) as u32;
        let pages = raw.min(state.cfg.search_max_pages);
        if pages > 1 {
            let extras = join_all((2..=pages).map(|p| fetch_page(&state, &base, &wd, p))).await;
            for r in extras.into_iter().flatten() {
                if let Ok(mut more) = parse_search_list(&r, &source) {
                    list.append(&mut more);
                }
            }
        }
        Ok::<_, ApiError>(Value::Array(list))
    };

    match tokio::time::timeout(state.cfg.search_source_timeout, fut).await {
        Ok(Ok(list)) => finish(true, list, None, false),
        Ok(Err(e)) => finish(false, json!([]), Some(e.to_string()), false),
        Err(_) => finish(
            false,
            json!([]),
            Some(format!("请求超时（>{}s）", state.cfg.search_source_timeout.as_secs())),
            true,
        ),
    }
}

fn outcomes_to_payload(outcomes: &[Value], wd: &str, filter_adult: bool) -> Value {
    let all: Vec<Value> = outcomes
        .iter()
        .filter_map(|o| o.get("list").and_then(Value::as_array))
        .flatten()
        .cloned()
        .collect();
    let list = aggregate(all, wd, filter_adult);
    let failures: Vec<Value> = outcomes
        .iter()
        .filter(|o| o.get("ok").and_then(Value::as_bool) != Some(true))
        .map(|o| {
            let mut f = json!({
                "sourceKey": o.get("sourceKey").cloned().unwrap_or(Value::Null),
                "error": o.get("error").cloned().unwrap_or(Value::String("请求失败".into())),
            });
            if o.get("timedOut").and_then(Value::as_bool) == Some(true) {
                f["timedOut"] = Value::Bool(true);
            }
            f
        })
        .collect();
    json!({ "list": list, "failures": failures })
}

fn cache_key(wd: &str, sources: &[Value], filter_adult: bool) -> String {
    let mut urls: Vec<&str> = sources
        .iter()
        .map(|s| s.get("url").and_then(Value::as_str).unwrap_or("").trim_end_matches('/'))
        .collect();
    urls.sort_unstable();
    format!("search:{wd}\n{}\n{}", if filter_adult { 1 } else { 0 }, urls.join("|"))
}

pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;

    let parsed: Value =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("请求格式错误".into()))?;
    let wd = parsed.get("wd").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if wd.is_empty() || wd.chars().count() > 100 {
        return Err(ApiError::BadRequest("搜索关键词无效".into()));
    }
    let sources: Vec<Value> = parsed
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(50)
        .collect();
    if sources.is_empty() {
        return Err(ApiError::BadRequest("请至少选择一个点播源".into()));
    }
    let filter_adult = parsed.get("filterAdult").and_then(Value::as_bool) != Some(false);
    let ck = cache_key(&wd, &sources, filter_adult);

    if let Some(cached) = state.cache.get(&ck) {
        if q.get("stream").map(String::as_str) == Some("1") {
            let done = json!({
                "type": "done",
                "list": cached.get("list").cloned().unwrap_or(json!([])),
                "failures": cached.get("failures").cloned().unwrap_or(json!([])),
            });
            let line = format!("{}\n", done);
            let stream = async_stream::stream! { yield Ok::<_, std::io::Error>(BufBytes::from(line)); };
            return Ok(ndjson_response(stream));
        }
        return Ok(json_ok(cached));
    }

    if q.get("stream").map(String::as_str) != Some("1") {
        let outcomes = join_all(
            sources
                .iter()
                .cloned()
                .map(|src| search_source(state.clone(), src, wd.clone())),
        )
        .await;
        let payload = outcomes_to_payload(&outcomes, &wd, filter_adult);
        state.cache.set(ck, payload.clone(), Duration::from_secs(60));
        return Ok(json_ok(payload));
    }

    // NDJSON：完成一个源推一条，健康源不必等坏源超时
    let n = sources.len();
    let cache = state.cache.clone();
    let ck2 = ck.clone();
    let wd2 = wd.clone();
    let stream = async_stream::stream! {
        let mut futures = FuturesUnordered::new();
        for (i, src) in sources.iter().cloned().enumerate() {
            let st = state.clone();
            let w = wd2.clone();
            futures.push(async move { (i, search_source(st, src, w).await) });
        }
        let mut outcomes: Vec<Value> = vec![Value::Null; n];
        while let Some((i, outcome)) = futures.next().await {
            outcomes[i] = outcome.clone();
            let list = outcome.get("list").and_then(Value::as_array).cloned().unwrap_or_default();
            let partial = aggregate(list, &wd2, filter_adult);
            let mut event = outcome;
            if let Some(obj) = event.as_object_mut() {
                obj.insert("type".into(), Value::String("source".into()));
                obj.insert("list".into(), Value::Array(partial.as_array().cloned().unwrap_or_default()));
            }
            yield Ok::<_, std::io::Error>(BufBytes::from(format!("{}\n", event)));
        }
        let payload = outcomes_to_payload(&outcomes, &wd2, filter_adult);
        cache.set(ck2, payload.clone(), Duration::from_secs(60));
        let done = json!({
            "type": "done",
            "list": payload.get("list").cloned().unwrap_or(json!([])),
            "failures": payload.get("failures").cloned().unwrap_or(json!([])),
        });
        yield Ok::<_, std::io::Error>(BufBytes::from(format!("{}\n", done)));
    };
    Ok(ndjson_response(stream))
}

/// 视频详情：优先列表接口 ?ac=videolist&ids=，无播放地址时降级爬详情页 HTML
pub async fn detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;

    let id = q.get("id").map(|s| s.trim()).unwrap_or("").to_string();
    if id.is_empty() || !id.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return Err(ApiError::BadRequest("无效的视频ID".into()));
    }
    let source: Value = q
        .get("source")
        .and_then(|raw| serde_json::from_str(raw).ok())
        .filter(|v: &Value| {
            v.get("url")
                .and_then(Value::as_str)
                .map(|u| u.starts_with("http://") || u.starts_with("https://"))
                == Some(true)
        })
        .ok_or_else(|| ApiError::BadRequest("无效的点播源配置".into()))?;

    let base = s(&source, "url").unwrap_or("").trim_end_matches('/');
    let origin = url::Url::parse(base)
        .ok()
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default();
    let detail_root = source
        .get("detail")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| q.get("baseUrl").map(String::as_str).filter(|s| !s.is_empty()))
        .unwrap_or(&origin)
        .trim_end_matches('/')
        .to_string();
    let ck = format!("detail:{base}|{detail_root}|{id}");
    if let Some(cached) = state.cache.get(&ck) {
        return Ok(json_ok(cached));
    }

    state.upstream.check_url(base, false).await?;
    let api = format!("{base}?ac=videolist&ids={}", encode_component(&id));
    let mut resolved: Option<Value> = None;
    if let Ok(data) = state.upstream.json(&api, &cms_options(&state.cfg).with_timeout(10_000)).await {
        if let Ok(d) = parse_detail(&data, &source) {
            if d.get("episodes").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false) {
                resolved = Some(d);
            }
        }
    }
    if resolved.is_none() && detail_root.starts_with("http") {
        state.upstream.check_url(&detail_root, false).await?;
        let url = format!("{detail_root}/index.php/vod/detail/id/{id}.html");
        let opts = cms_options(&state.cfg).with_timeout(10_000);
        if let Ok(html) = state.upstream.text(&url, &opts).await {
            let d = parse_detail_page_html(&html, &source);
            if d.get("episodes").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false) {
                resolved = Some(d);
            }
        }
    }
    let result = resolved.ok_or_else(|| ApiError::NotFound("未找到播放资源".into()))?;
    state.cache.set(ck, result.clone(), Duration::from_secs(60));
    Ok(json_ok(result))
}

/// 点播源探活：以搜索 "test" 的耗时与结果量衡量可用性
pub async fn source_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let parsed: Value =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("请求格式错误".into()))?;
    let url = parsed
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(json_ok(json!({ "ok": false, "ms": 0, "error": "无效的源地址" })));
    }
    if let Err(e) = state.upstream.check_url(&url, false).await {
        return Ok(json_ok(json!({ "ok": false, "ms": 0, "error": e.to_string() })));
    }
    let start = now_ms();
    let api = format!("{url}?ac=videolist&wd=test");
    let result = state.upstream.json(&api, &cms_opts(&state.cfg)).await;
    let ms = now_ms() - start;
    match result {
        Ok(data) => {
            let src = json!({ "key": "probe", "name": "probe", "url": url });
            let count = parse_search_list(&data, &src).map(|l| l.len()).unwrap_or(0);
            Ok(json_ok(json!({ "ok": true, "ms": ms, "count": count })))
        }
        Err(e) => Ok(json_ok(json!({ "ok": false, "ms": ms, "error": e.to_string() }))),
    }
}


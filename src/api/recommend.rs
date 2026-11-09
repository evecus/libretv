//! 首页推荐：豆瓣、Bangumi 每日放送、60s API 影视热榜。均服务端直连 + TTL 缓存。

use std::{collections::HashMap, time::Duration};

use axum::{extract::{Query, State}, http::HeaderMap, response::Response};
use serde_json::{json, Value};

use crate::{
    error::{json_ok, ApiError, ApiResult},
    net::FetchOptions,
    state::AppState,
    util::encode_component,
};

const UA_BROWSER: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const UA_LIBRETV: &str = "LibreTV (+https://github.com/LibreSpark/LibreTV)";
const HOT_LIST_IDS: &[&str] = &[
    "douban_movie_weekly", "douban_tv_chinese", "douban_tv_global",
    "douban_show_chinese", "douban_show_global", "baidu_teleplay",
];

pub async fn douban(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let ty = if q.get("type").map(String::as_str) == Some("tv") { "tv" } else { "movie" };
    let tag = q.get("tag").map(String::as_str).unwrap_or("热门");
    let tag = tag.chars().take(20).collect::<String>();
    let page_start = q.get("pageStart").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let page_size = q.get("pageSize").and_then(|s| s.parse::<u32>().ok()).unwrap_or(25).clamp(1, 50);
    let ck = format!("douban:{ty}:{tag}:{page_start}:{page_size}");
    if let Some(cached) = state.cache.get(&ck) {
        return Ok(json_ok(json!({ "items": cached })));
    }
    let target = format!(
        "https://movie.douban.com/j/search_subjects?type={ty}&tag={}&sort=recommend&page_limit={page_size}&page_start={page_start}",
        encode_component(&tag)
    );
    let opts = FetchOptions::default()
        .with_timeout(8000)
        .header("user-agent", UA_BROWSER)
        .header("referer", "https://movie.douban.com/")
        .json_accept();
    let mut subjects: Option<Vec<Value>> = None;
    if let Ok(data) = state.upstream.json(&target, &opts).await {
        subjects = data.get("subjects").and_then(Value::as_array).cloned();
    }
    // 降级：FALLBACK_CORS_PROXY
    if subjects.is_none() {
        if let Some(proxy) = state.cfg.fallback_cors_proxy.as_ref() {
            let u = format!("{proxy}{}", encode_component(&target));
            if let Ok(data) = state.upstream.json(&u, &FetchOptions::default().with_timeout(8000).json_accept()).await {
                subjects = data.get("subjects").and_then(Value::as_array).cloned();
            }
        }
    }
    let raw = subjects.ok_or_else(|| ApiError::BadGateway("豆瓣推荐获取失败".into()))?;
    let items: Vec<Value> = raw.iter().filter_map(|s| {
        let mut o = serde_json::Map::new();
        o.insert("id".into(), Value::String(s.get("id").and_then(Value::as_str).unwrap_or("").to_string()));
        o.insert("title".into(), Value::String(s.get("title").and_then(Value::as_str).unwrap_or("").to_string()));
        o.insert("cover".into(), Value::String(s.get("cover").and_then(Value::as_str).unwrap_or("").to_string()));
        let rate = s.get("rate").and_then(Value::as_str).unwrap_or("");
        if !rate.is_empty() && rate != "0" { o.insert("rating".into(), Value::String(rate.to_string())); }
        o.insert("isTv".into(), Value::Bool(ty == "tv"));
        Some(Value::Object(o))
    }).collect();
    state.cache.set(ck, Value::Array(items.clone()), Duration::from_secs(10 * 60));
    Ok(json_ok(json!({ "items": items })))
}

pub async fn bangumi(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    const API: &str = "https://api.bgm.tv/calendar";
    if let Some(cached) = state.cache.get(API) {
        return Ok(json_ok(days_response(&cached)));
    }
    let opts = FetchOptions::default()
        .with_timeout(8000)
        .header("user-agent", UA_LIBRETV)
        .json_accept();
    let data = state.upstream.json(API, &opts).await?;
    let mut days: Vec<Value> = Vec::new();
    if let Some(arr) = data.as_array() {
        for day in arr {
            let weekday = day.get("weekday").and_then(|w| w.get("id")).and_then(Value::as_i64).unwrap_or(-1);
            if !(1..=7).contains(&weekday) { continue; }
            let items: Vec<Value> = day.get("items").and_then(Value::as_array).cloned().unwrap_or_default()
                .iter().filter_map(|sub| {
                    let id = sub.get("id")?;
                    let title = sub.get("name_cn").and_then(Value::as_str).filter(|s| !s.is_empty())
                        .or_else(|| sub.get("name").and_then(Value::as_str))?;
                    let images = sub.get("images")?;
                    let cover = images.get("large").or_else(|| images.get("common")).or_else(|| images.get("medium")).and_then(Value::as_str)?;
                    let mut o = serde_json::Map::new();
                    o.insert("id".into(), Value::String(id.to_string()));
                    o.insert("title".into(), Value::String(title.to_string()));
                    o.insert("cover".into(), Value::String(cover.to_string()));
                    let score = sub.get("rating").and_then(|r| r.get("score")).and_then(Value::as_f64).unwrap_or(0.0);
                    if score > 0.0 { o.insert("rating".into(), Value::String(format!("{score:.1}"))); }
                    o.insert("isTv".into(), Value::Bool(true));
                    Some(Value::Object(o))
                }).collect();
            days.push(json!({ "weekday": weekday, "items": items }));
        }
    }
    if days.is_empty() { return Err(ApiError::BadGateway("Bangumi 放送表数据为空".into())); }
    days.sort_by_key(|d| d.get("weekday").and_then(Value::as_i64).unwrap_or(0));
    let value = Value::Array(days);
    state.cache.set(API, value.clone(), Duration::from_secs(30 * 60));
    Ok(json_ok(days_response(&value)))
}

fn days_response(value: &Value) -> Value {
    json!({ "days": value })
}

pub async fn hot_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let id = q.get("id").map(String::as_str).unwrap_or("");
    if !HOT_LIST_IDS.contains(&id) {
        return Err(ApiError::BadRequest(format!("未知榜单: {}", id.chars().take(30).collect::<String>())));
    }
    let ck = format!("hot-list:{id}");
    if let Some(cached) = state.cache.get(&ck) {
        return Ok(json_ok(json!({ "items": cached })));
    }
    let category = match id {
        "douban_movie_weekly" => Some("movie"),
        "douban_tv_chinese" => Some("tv_chinese"),
        "douban_tv_global" => Some("tv_global"),
        "douban_show_chinese" => Some("show_chinese"),
        "douban_show_global" => Some("show_global"),
        _ => None,
    };
    let path = match category {
        Some(c) => format!("/v2/douban/weekly/{c}"),
        None => "/v2/baidu/teleplay".to_string(),
    };
    let primary = state.cfg.api_60s_base.trim_end_matches('/').to_string();
    let fallback = "https://60s.viki.moe";
    let mut bases = vec![primary.as_str()];
    if primary != fallback { bases.push(fallback); }
    let opts = FetchOptions::default().with_timeout(8000).header("user-agent", UA_LIBRETV).json_accept();
    let mut data: Option<Value> = None;
    for base in bases {
        if let Ok(v) = state.upstream.json(&format!("{base}{path}"), &opts).await {
            data = Some(v);
            break;
        }
    }
    let data = data.ok_or_else(|| ApiError::BadGateway("60s API 请求失败".into()))?;
    let raw = data.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let is_tv = id != "douban_movie_weekly";
    let items: Vec<Value> = raw.iter().enumerate().filter_map(|(i, entry)| {
        let mut o = serde_json::Map::new();
        if category.is_some() {
            let cover = entry.get("cover").and_then(Value::as_str).filter(|s| !s.is_empty())
                .or_else(|| entry.get("cover_proxy").and_then(Value::as_str))?;
            let rid = entry.get("id").cloned()?;
            let title = entry.get("title").and_then(Value::as_str)?;
            o.insert("id".into(), Value::String(match rid { Value::String(s) => s, v => v.to_string() }));
            o.insert("title".into(), Value::String(title.to_string()));
            o.insert("cover".into(), Value::String(cover.to_string()));
            if let Some(r) = entry.get("rating").and_then(Value::as_f64).filter(|r| *r > 0.0) {
                o.insert("rating".into(), Value::String(r.to_string()));
            }
            o.insert("isTv".into(), Value::Bool(is_tv));
        } else {
            let title = entry.get("title").and_then(Value::as_str)?;
            let cover = entry.get("cover").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            o.insert("id".into(), Value::String(format!("baidu_{i}")));
            o.insert("title".into(), Value::String(title.to_string()));
            o.insert("cover".into(), Value::String(cover.to_string()));
            o.insert("isTv".into(), Value::Bool(true));
        }
        Some(Value::Object(o))
    }).collect();
    if items.is_empty() { return Err(ApiError::BadGateway("榜单数据为空".into())); }
    state.cache.set(ck, Value::Array(items.clone()), Duration::from_secs(60 * 60));
    Ok(json_ok(json!({ "items": items })))
}

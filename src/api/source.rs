//! 数据源订阅（LibreTV-SourceList / TVBOX / 接口目录）、源列表发布、画质探测。

use std::{collections::{HashMap, HashSet}, sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, Method},
    response::Response,
};
use bytes::Bytes as BufBytes;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::{
    cms::{cms_options, extract_episodes_from_play_url, parse_detail_page_html},
    error::{json_ok, ndjson_response, ApiError, ApiResult},
    net::FetchOptions,
    state::AppState,
    util::encode_component,
};

const MAX_VOD_SOURCES: usize = 50;
const MAX_LIVE_SOURCES: usize = 50;
const MAX_PUBLISH_BYTES: usize = 256 * 1024;
const MAX_FIELD_LEN: usize = 2048;

// ——————————————————————— 宽容 JSON ———————————————————————

/// 剥离字符串外的 // 行注释、/* */ 块注释与尾随逗号（TVBOX 配置常见）
fn lenient_json(text: &str) -> String {
    let s = text.trim_start_matches('\u{feff}');
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped { escaped = false; }
            else if c == '\\' { escaped = true; }
            else if c == '"' { in_string = false; }
            i += 1;
            continue;
        }
        if c == '"' { in_string = true; out.push(c); i += 1; continue; }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' { i += 1; }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') { i += 1; }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    let re = Regex::new(r",(\s*[\]}])").unwrap();
    re.replace_all(&out, "$1").to_string()
}

fn valid_http(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn short(s: &str) -> String {
    s.trim().chars().take(MAX_FIELD_LEN).collect()
}

/// 解析三种订阅格式，返回 {name?, sources, liveSources, stats}
fn parse_subscription(text: &str) -> Result<Value, String> {
    let cleaned = lenient_json(text);
    let json: Value = serde_json::from_str(&cleaned).map_err(|_| "订阅内容不是合法的 JSON".to_string())?;
    let mut sources: Vec<Value> = Vec::new();
    let mut live_sources: Vec<Value> = Vec::new();
    let mut skipped: HashMap<String, usize> = HashMap::new();
    let mut samples: Vec<String> = Vec::new();
    let mut truncated = 0usize;
    let mut format = "libretv";
    let mut name: Option<String> = None;
    let mut seen = HashSet::new();

    let skip = |reason: &str, label: &str, skipped: &mut HashMap<String, usize>, samples: &mut Vec<String>| {
        *skipped.entry(reason.to_string()).or_insert(0) += 1;
        if samples.len() < 3 && !label.is_empty() { samples.push(label.chars().take(40).collect()); }
    };

    // TVBOX 配置
    let is_tvbox = json.get("sites").is_some() || json.get("lives").is_some();
    // 接口目录：裸数组且元素含 baseUrl
    let is_catalog = json.as_array().map(|a| a.iter().any(|v| v.get("baseUrl").is_some())).unwrap_or(false);

    if is_tvbox {
        format = "tvbox";
        if let Some(sites) = json.get("sites").and_then(Value::as_array) {
            for raw in sites {
                let label = raw.get("name").and_then(Value::as_str).unwrap_or("");
                if raw.get("enabled").and_then(Value::as_bool) == Some(false) {
                    skip("disabled", label, &mut skipped, &mut samples); continue;
                }
                // type=1 为 Apple CMS 直连接口；其他为 Spider / XML，需 TVBOX 引擎
                let ty = raw.get("type").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(1);
                if ty != 0 && ty != 1 { skip("spider", label, &mut skipped, &mut samples); continue; }
                let api = raw.get("api").or_else(|| raw.get("url")).and_then(Value::as_str).unwrap_or("").trim();
                if !valid_http(api) || raw.get("searchable").and_then(Value::as_bool) == Some(false) {
                    skip("invalidUrl", label, &mut skipped, &mut samples); continue;
                }
                if sources.len() >= MAX_VOD_SOURCES { truncated += 1; continue; }
                if seen.insert(api.to_string()) {
                    sources.push(json!({ "name": short(if label.is_empty() { api } else { label }), "url": short(api) }));
                }
            }
        }
        if let Some(lives) = json.get("lives").and_then(Value::as_array) {
            for raw in lives {
                let label = raw.get("name").and_then(Value::as_str).unwrap_or("");
                let u = raw.get("url").and_then(Value::as_str).unwrap_or("").trim();
                if !valid_http(u) || u.ends_with(".txt") { skip("nonM3uLive", label, &mut skipped, &mut samples); continue; }
                if live_sources.len() >= MAX_LIVE_SOURCES { truncated += 1; continue; }
                if seen.insert(u.to_string()) {
                    live_sources.push(json!({ "name": short(if label.is_empty() { u } else { label }), "url": short(u) }));
                }
            }
        }
    } else if is_catalog {
        format = "source-catalog";
        if let Some(arr) = json.as_array() {
            for raw in arr {
                let label = raw.get("name").and_then(Value::as_str).unwrap_or("");
                if raw.get("enabled").and_then(Value::as_bool) == Some(false) { skip("disabled", label, &mut skipped, &mut samples); continue; }
                let u = raw.get("baseUrl").or_else(|| raw.get("url")).and_then(Value::as_str).unwrap_or("").trim();
                if !valid_http(u) { skip("invalidUrl", label, &mut skipped, &mut samples); continue; }
                if sources.len() >= MAX_VOD_SOURCES { truncated += 1; continue; }
                if seen.insert(u.to_string()) {
                    sources.push(json!({ "name": short(if label.is_empty() { u } else { label }), "url": short(u) }));
                }
            }
        }
    } else {
        // LibreTV-SourceList：对象或裸数组（老格式）
        name = json.get("name").and_then(Value::as_str).map(short).filter(|s| !s.is_empty());
        let raw_sources = json.get("sources").and_then(Value::as_array).cloned()
            .or_else(|| json.as_array().cloned()).unwrap_or_default();
        for raw in &raw_sources {
            let label = raw.get("name").and_then(Value::as_str).unwrap_or("");
            let u = raw.get("url").and_then(Value::as_str).unwrap_or("").trim();
            if !valid_http(u) { skip("invalidUrl", label, &mut skipped, &mut samples); continue; }
            if sources.len() >= MAX_VOD_SOURCES { truncated += 1; continue; }
            let mut o = serde_json::Map::new();
            o.insert("name".into(), Value::String(short(if label.is_empty() { u } else { label })));
            o.insert("url".into(), Value::String(short(u)));
            if let Some(d) = raw.get("detail").and_then(Value::as_str).filter(|s| !s.is_empty()) { o.insert("detail".into(), Value::String(short(d))); }
            if let Some(b) = raw.get("isAdult").and_then(Value::as_bool) { o.insert("isAdult".into(), Value::Bool(b)); }
            sources.push(Value::Object(o));
        }
        if let Some(arr) = json.get("liveSources").and_then(Value::as_array) {
            for raw in arr {
                let label = raw.get("name").and_then(Value::as_str).unwrap_or("");
                let u = raw.get("url").and_then(Value::as_str).unwrap_or("").trim();
                if !valid_http(u) { skip("invalidUrl", label, &mut skipped, &mut samples); continue; }
                if live_sources.len() >= MAX_LIVE_SOURCES { truncated += 1; continue; }
                let mut o = serde_json::Map::new();
                o.insert("name".into(), Value::String(short(if label.is_empty() { u } else { label })));
                o.insert("url".into(), Value::String(short(u)));
                if let Some(e) = raw.get("epg").and_then(Value::as_str).filter(|s| !s.is_empty()) { o.insert("epg".into(), Value::String(short(e))); }
                live_sources.push(Value::Object(o));
            }
        }
    }

    let total_skipped: usize = skipped.values().sum();
    let mut out = serde_json::Map::new();
    if let Some(n) = name { out.insert("name".into(), Value::String(n)); }
    out.insert("sources".into(), Value::Array(sources));
    out.insert("liveSources".into(), Value::Array(live_sources));
    out.insert("stats".into(), json!({
        "format": format,
        "skipped": total_skipped,
        "skippedByReason": skipped,
        "skippedSamples": samples,
        "truncated": truncated,
    }));
    Ok(Value::Object(out))
}

pub async fn source_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let target = q.get("url").map(|s| s.trim()).unwrap_or("").to_string();
    if !valid_http(&target) { return Err(ApiError::BadRequest("无效的订阅地址".into())); }
    state.upstream.check_url(&target, false).await?;
    let opts = FetchOptions::default().with_timeout(8000).json_accept();
    let text = state.upstream.text(&target, &opts).await?;
    let mut payload = parse_subscription(&text).map_err(ApiError::BadRequest)?;

    // 点播源一律拒绝非公网；直播源按 LIVE_ALLOW_PRIVATE 策略；EPG 非法时丢弃字段
    let mut valid_sources = Vec::new();
    let mut valid_live = Vec::new();
    let mut dropped = 0;
    for s in payload["sources"].as_array().cloned().unwrap_or_default() {
        let u = s.get("url").and_then(Value::as_str).unwrap_or("");
        if state.upstream.check_url(u, false).await.is_ok() { valid_sources.push(s); } else { dropped += 1; }
    }
    for s in payload["liveSources"].as_array().cloned().unwrap_or_default() {
        let u = s.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        if state.upstream.check_url(&u, state.cfg.live_allow_private).await.is_err() { dropped += 1; continue; }
        let mut item = s;
        if let Some(epg) = item.get("epg").and_then(Value::as_str).map(str::to_string) {
            if state.upstream.check_url(&epg, state.cfg.live_allow_private).await.is_err() {
                item.as_object_mut().map(|o| o.remove("epg"));
            }
        }
        valid_live.push(item);
    }
    payload["sources"] = Value::Array(valid_sources);
    payload["liveSources"] = Value::Array(valid_live);
    if dropped > 0 {
        payload["stats"]["skipped"] = Value::from(payload["stats"]["skipped"].as_u64().unwrap_or(0) + dropped as u64);
        let by = payload["stats"]["skippedByReason"].as_object_mut();
        if let Some(m) = by { *m.entry("invalidUrl".to_string()).or_insert(Value::from(0)) = Value::from(m.get("invalidUrl").and_then(Value::as_u64).unwrap_or(0) + dropped as u64); }
    }
    let empty = payload["sources"].as_array().map(|a| a.is_empty()).unwrap_or(true)
        && payload["liveSources"].as_array().map(|a| a.is_empty()).unwrap_or(true);
    if empty { return Err(ApiError::BadRequest("订阅未导入任何可用源：内容为空或地址未通过校验".into())); }
    Ok(json_ok(payload))
}

// ——————————————————————— 发布 ———————————————————————

fn normalize_publish(raw: &Value) -> Option<Value> {
    if !raw.is_object() { return None; }
    let sources: Vec<Value> = raw.get("sources").and_then(Value::as_array).cloned().unwrap_or_default()
        .iter().take(MAX_VOD_SOURCES).filter_map(|v| {
            let name = short(v.get("name").and_then(Value::as_str).unwrap_or(""));
            let url = short(v.get("url").and_then(Value::as_str).unwrap_or(""));
            if name.is_empty() || !valid_http(&url) { return None; }
            Some(json!({ "name": name, "url": url }))
        }).collect();
    let live: Vec<Value> = raw.get("liveSources").and_then(Value::as_array).cloned().unwrap_or_default()
        .iter().take(MAX_LIVE_SOURCES).filter_map(|v| {
            let name = short(v.get("name").and_then(Value::as_str).unwrap_or(""));
            let url = short(v.get("url").and_then(Value::as_str).unwrap_or(""));
            if name.is_empty() || !valid_http(&url) { return None; }
            let mut o = json!({ "name": name, "url": url });
            if let Some(e) = v.get("epg").and_then(Value::as_str).filter(|s| valid_http(s)) { o["epg"] = Value::String(short(e)); }
            Some(o)
        }).collect();
    if sources.is_empty() && live.is_empty() { return None; }
    Some(json!({
        "name": raw.get("name").and_then(Value::as_str).map(short).filter(|s| !s.is_empty()).unwrap_or_else(|| "LibreTV-SourceList".into()),
        "version": 2,
        "exportedAt": crate::util::now_ms(),
        "sources": sources,
        "liveSources": live,
    }))
}

pub async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let raw: Value = serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("请求内容不是合法 JSON".into()))?;
    let payload = normalize_publish(&raw)
        .ok_or_else(|| ApiError::BadRequest("没有可发布的源（当前没有已勾选启用的点播源或直播源）".into()))?;
    let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
    if text.len() > MAX_PUBLISH_BYTES {
        return Err(ApiError::BadRequest("源列表体积超出公开粘贴板的限制，无法发布".into()));
    }
    let (url, provider) = publish_to_paste(&state, &text).await?;
    Ok(json_ok(json!({
        "url": url,
        "provider": provider,
        "sources": payload.get("sources").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        "liveSources": payload.get("liveSources").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
    })))
}

async fn publish_to_paste(state: &AppState, text: &str) -> Result<(String, String), ApiError> {
    // 主：paste.rs（纯文本 POST）；备：0x0.st（multipart）
    let ua = "LibreTV/2.11 (+https://github.com/LibreSpark/LibreTV)";
    let opts = FetchOptions {
        headers: {
            let mut h = HeaderMap::new();
            if let Ok(v) = axum::http::HeaderValue::from_str("text/plain; charset=utf-8") { h.insert("content-type", v); }
            if let Ok(v) = axum::http::HeaderValue::from_str(ua) { h.insert("user-agent", v); }
            h
        },
        timeout: Duration::from_secs(15),
        allow_private: false,
        method: Method::POST,
        body: Some(text.as_bytes().to_vec()),
    };
    if let Ok(r) = state.upstream.fetch("https://paste.rs/", &opts).await {
        if r.status.as_u16() == 201 {
            if let Ok(b) = r.response.bytes().await {
                let url = String::from_utf8_lossy(&b).trim().to_string();
                if Regex::new(r"^https://paste\.rs/[\w-]+$").unwrap().is_match(&url) {
                    return Ok((url, "paste.rs".into()));
                }
            }
        }
    }
    // 0x0.st：手工构造 multipart/form-data
    let boundary = format!("----libretv{}", crate::util::now_ms());
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"libretv-source-list.json\"\r\nContent-Type: application/json\r\n\r\n").as_bytes());
    body.extend_from_slice(text.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let opts2 = FetchOptions {
        headers: {
            let mut h = HeaderMap::new();
            if let Ok(v) = axum::http::HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")) { h.insert("content-type", v); }
            if let Ok(v) = axum::http::HeaderValue::from_str(ua) { h.insert("user-agent", v); }
            h
        },
        timeout: Duration::from_secs(15),
        allow_private: false,
        method: Method::POST,
        body: Some(body),
    };
    if let Ok(r) = state.upstream.fetch("https://0x0.st", &opts2).await {
        if r.status.is_success() {
            if let Ok(b) = r.response.bytes().await {
                let url = String::from_utf8_lossy(&b).trim().to_string();
                if Regex::new(r"^https://0x0\.st/[\w.-]+$").unwrap().is_match(&url) {
                    return Ok((url, "0x0.st".into()));
                }
            }
        }
    }
    Err(ApiError::BadGateway("发布失败：paste.rs（不可用）、0x0.st（不可用）".into()))
}

// ——————————————————————— 画质探测 ———————————————————————

fn best_height(m3u8: &str) -> Option<u64> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"RESOLUTION=(\d+)x(\d+)").unwrap());
    let mut best: Option<u64> = None;
    for c in re.captures_iter(m3u8) {
        let w = c[1].parse::<u64>().unwrap_or(0);
        let h = c[2].parse::<u64>().unwrap_or(0);
        // 采集站常把宽高写反，取较小值作为 height，避免 720x1280 被标成 2K
        let height = w.min(h);
        if height > 0 && best.map_or(true, |b| height > b) { best = Some(height); }
    }
    best
}

async fn quality_one(state: AppState, source: Value, id: String) -> Value {
    let key = format!("quality:{}|{}|{}", source.get("url").and_then(Value::as_str).unwrap_or(""), source.get("detail").and_then(Value::as_str).unwrap_or(""), id);
    let base = source.get("url").and_then(Value::as_str).unwrap_or("").trim_end_matches('/');
    let source_key = source.get("key").and_then(Value::as_str).unwrap_or("").to_string();
    let result = async {
        let _permit = state.host_gate.acquire(base).await;
        let api = format!("{base}?ac=videolist&ids={}", encode_component(&id));
        let data = state.upstream.json(&api, &cms_options(&state.cfg).with_timeout(6000)).await?;
        let mut episodes: Vec<String> = Vec::new();
        if let Some(vod) = data.get("list").and_then(Value::as_array).and_then(|a| a.first()) {
            episodes = extract_episodes_from_play_url(vod.get("vod_play_url").and_then(Value::as_str).unwrap_or(""));
        }
        if episodes.is_empty() {
            let root = source.get("detail").and_then(Value::as_str)
                .or_else(|| url::Url::parse(base).ok().map(|_| base)).unwrap_or(base);
            let u = format!("{}/index.php/vod/detail/id/{id}.html", root.trim_end_matches('/'));
            if let Ok(html) = state.upstream.text(&u, &cms_options(&state.cfg).with_timeout(6000)).await {
                let d = parse_detail_page_html(&html, &source);
                episodes = d.get("episodes").and_then(Value::as_array).cloned().unwrap_or_default()
                    .iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
            }
        }
        for ep in episodes.iter().take(3) {
            let opts = FetchOptions::default().with_timeout(6000).header("user-agent", &state.cfg.user_agent);
            if let Ok(text) = state.upstream.text(ep, &opts).await {
                if let Some(h) = best_height(&text) { return Ok::<_, ApiError>(Some(h)); }
            }
        }
        Ok(None)
    };
    let (height, error) = match tokio::time::timeout(Duration::from_secs(10), result).await {
        Ok(Ok(h)) => (h, None),
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(_) => (None, Some("探测超时".into())),
    };
    let value = json!({ "height": height, "error": error });
    let ttl = if error.is_some() { Duration::from_secs(2 * 60) } else { Duration::from_secs(30 * 60) };
    state.cache.set(key, value, ttl);
    let mut out = json!({ "sourceKey": source_key, "vodId": id });
    if let Some(h) = height { out["height"] = Value::from(h); }
    if let Some(e) = error { out["error"] = Value::String(e); }
    out
}

fn quality_items(parsed: &Value) -> Vec<(Value, String)> {
    parsed.get("items").and_then(Value::as_array).cloned().unwrap_or_default()
        .iter().take(50).filter_map(|v| {
            let src = v.get("source")?.clone();
            let id = v.get("id")?.as_str().unwrap_or("").to_string();
            if id.is_empty() || src.get("url").and_then(Value::as_str).map(|u| !valid_http(u)).unwrap_or(true) { return None; }
            Some((src, id))
        }).collect()
}

pub async fn quality(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("请求格式错误".into()))?;
    let items = quality_items(&parsed);
    if items.is_empty() { return Err(ApiError::BadRequest("条目格式无效".into())); }
    let is_stream = q.get("stream").map(String::as_str) == Some("1");
    let gate = Arc::new(Semaphore::new(3));

    if !is_stream {
        let mut futures = FuturesUnordered::new();
        for (src, id) in items {
            let st = state.clone(); let g = gate.clone();
            futures.push(async move { let _p = g.acquire().await; quality_one(st, src, id).await });
        }
        let mut results = Vec::new();
        while let Some(r) = futures.next().await { results.push(r); }
        return Ok(json_ok(json!({ "results": results })));
    }

    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(BufBytes::from_static(b"{\"ping\":true}\n"));
        let mut futures = FuturesUnordered::new();
        for (src, id) in items {
            let st = state.clone(); let g = gate.clone();
            futures.push(async move { let _p = g.acquire().await; quality_one(st, src, id).await });
        }
        while let Some(r) = futures.next().await {
            yield Ok::<_, std::io::Error>(BufBytes::from(format!("{}\n", r)));
        }
    };
    Ok(ndjson_response(stream))
}

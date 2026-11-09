//! 直播 / IPTV：M3U 播放列表、XMLTV 节目单（含 gzip）、分片级批量测活。
//! 全部支持 NDJSON 流式推送与按 host 并发闸门。

use std::{collections::{HashMap, HashSet}, io::Read, sync::OnceLock, time::Duration};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use bytes::Bytes as BufBytes;
use flate2::read::GzDecoder;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use url::Url;

use crate::{
    error::{json_ok, ndjson_response, ApiError, ApiResult},
    m3u8::{codecs_of, has_m3u8_header, master_variants, segments},
    net::FetchOptions,
    state::AppState,
    util::{decode_component, now_ms},
};

const PLAYLIST_TTL: Duration = Duration::from_secs(10 * 60);
const EPG_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const PROBE_OK_TTL: Duration = Duration::from_secs(30 * 60);
const PROBE_FAIL_TTL: Duration = Duration::from_secs(2 * 60);
const PROBE_BUDGET: Duration = Duration::from_millis(8000);
const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

fn attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"([a-zA-Z0-9_-]+)="([^"]*)""#).unwrap())
}

fn attrs_of(line: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for c in attr_re().captures_iter(line) {
        map.insert(c[1].to_lowercase(), c[2].trim().to_string());
    }
    map
}

fn stable_id(input: &str) -> String {
    // djb2 变体，与前端一致（base36）
    let mut h: u64 = 5381;
    for b in input.bytes() {
        h = ((h << 5) + h + b as u64) & 0xffff_ffff;
    }
    let mut n = h;
    let mut out = Vec::new();
    loop {
        let d = (n % 36) as u8;
        out.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        n /= 36;
        if n == 0 { break; }
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

fn normalize_stream_url(raw: &str, base: &str) -> Option<String> {
    let u = Url::parse(raw).or_else(|_| Url::parse(base).and_then(|b| b.join(raw))).ok()?;
    if u.scheme() != "http" && u.scheme() != "https" { return None; }
    Some(u.to_string())
}

fn name_from_url(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|s| s.filter(|p| !p.is_empty()).last().map(str::to_string))
                .and_then(|last| {
                    let d = decode_component(&last);
                    let d = Regex::new(r"(?i)\.(m3u8?|flv|ts|mp4)$").unwrap().replace(&d, "").to_string();
                    if d.is_empty() { None } else { Some(d) }
                })
                .or_else(|| u.host_str().map(str::to_string))
        })
        .unwrap_or_else(|| "未命名频道".into())
}

/// M3U / M3U8 播放列表解析（兼容非标准属性位置与 #EXTGRP）
pub fn parse_m3u(content: &str, base: &str) -> Value {
    let mut channels: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    let mut ctx: Option<(HashMap<String, String>, String, String)> = None;
    let strip = Regex::new(r#"[a-zA-Z0-9_-]+="[^"]*""#).unwrap();
    let extinf_prefix = Regex::new(r"(?i)^#EXTINF\s*:\s*").unwrap();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() { continue; }
        let upper = line.to_uppercase();
        if upper.starts_with("#EXTINF") {
            let attrs = attrs_of(line);
            let stripped_line = strip.replace_all(line, "").to_string();
            let stripped = extinf_prefix.replace(&stripped_line, "");
            let title = match stripped.find(',') {
                Some(i) => stripped[i + 1..].trim(),
                None => stripped.trim(),
            }
            .trim_matches('"')
            .to_string();
            ctx = Some((attrs, title, String::new()));
            continue;
        }
        if upper.starts_with("#EXTGRP:") {
            let g = line["#EXTGRP:".len()..].trim().to_string();
            if let Some((attrs, _, ext)) = ctx.as_mut() {
                if !g.is_empty() && !attrs.contains_key("group-title") { *ext = g; }
            }
            continue;
        }
        if line.starts_with('#') { continue; }

        let url = normalize_stream_url(line, base);
        let cur = ctx.take();
        let url = match url { Some(u) if seen.insert(u.clone()) => u, _ => continue };
        let (attrs, title, ext_group) = cur.unwrap_or_default();
        let tvg_id = attrs.get("tvg-id").cloned().filter(|s| !s.is_empty());
        let logo = attrs.get("tvg-logo").and_then(|l| normalize_stream_url(l, base));
        let group = attrs.get("group-title").cloned().filter(|s| !s.is_empty()).or(Some(ext_group).filter(|s| !s.is_empty()));
        let raw_name = attrs.get("tvg-name").cloned().filter(|s| !s.is_empty());
        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), Value::String(tvg_id.clone().unwrap_or_else(|| stable_id(&url))));
        obj.insert("name".into(), Value::String(if title.is_empty() { name_from_url(&url) } else { title }));
        obj.insert("url".into(), Value::String(url));
        if let Some(l) = logo { obj.insert("logo".into(), Value::String(l)); }
        if let Some(g) = group { obj.insert("group".into(), Value::String(g)); }
        if let Some(t) = tvg_id { obj.insert("tvgId".into(), Value::String(t)); }
        if let Some(r) = raw_name { obj.insert("rawName".into(), Value::String(r)); }
        channels.push(Value::Object(obj));
    }

    let mut groups: Vec<String> = channels
        .iter()
        .filter_map(|c| c.get("group").and_then(Value::as_str).map(str::to_string))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    groups.sort();
    let name = Regex::new(r"(?m)^#PLAYLIST:(.+)$")
        .unwrap()
        .captures(content)
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|s| !s.is_empty());
    let mut out = serde_json::Map::new();
    if let Some(n) = name { out.insert("name".into(), Value::String(n)); }
    out.insert("channels".into(), Value::Array(channels));
    out.insert("groups".into(), Value::Array(groups.into_iter().map(Value::String).collect()));
    Value::Object(out)
}

fn to_m3u_text(playlist: &Value) -> String {
    let mut lines = vec!["#EXTM3U".to_string()];
    if let Some(n) = playlist.get("name").and_then(Value::as_str) {
        lines.push(format!("#PLAYLIST:{n}"));
    }
    if let Some(channels) = playlist.get("channels").and_then(Value::as_array) {
        for c in channels {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("");
            let url = c.get("url").and_then(Value::as_str).unwrap_or("");
            let mut attrs = vec![
                format!("tvg-id=\"{}\"", c.get("tvgId").and_then(Value::as_str).or_else(|| c.get("id").and_then(Value::as_str)).unwrap_or("").replace('"', "'")),
            ];
            if let Some(g) = c.get("group").and_then(Value::as_str) { attrs.push(format!("group-title=\"{}\"", g.replace('"', "'"))); }
            if let Some(l) = c.get("logo").and_then(Value::as_str) { attrs.push(format!("tvg-logo=\"{}\"", l.replace('"', "'"))); }
            if let Some(r) = c.get("rawName").and_then(Value::as_str) { attrs.push(format!("tvg-name=\"{}\"", r.replace('"', "'"))); }
            lines.push(format!("#EXTINF:-1 {},{}", attrs.join(" "), name.replace(',', " ")));
            lines.push(url.to_string());
        }
    }
    lines.join("\n") + "\n"
}

// ——————————————————————— XMLTV ———————————————————————

fn decode_xml_text(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    if t.starts_with("<![CDATA[") && t.ends_with("]]>") {
        t = t[9..t.len() - 3].to_string();
    }
    t.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn parse_xmltv_time(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let main: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    if main.len() < 8 { return None; }
    let get = |a: usize, b: usize| main.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let y = get(0, 4)? as i32;
    let mo = get(4, 6)? as u32;
    let d = get(6, 8)? as u32;
    let h = get(8, 10).unwrap_or(0) as u32;
    let mi = get(10, 12).unwrap_or(0) as u32;
    let se = get(12, 14).unwrap_or(0) as u32;
    let ms = digits.split('.').nth(1).and_then(|f| format!("{:0<3}", &f[..f.len().min(3)]).parse::<u32>().ok()).unwrap_or(0);
    let naive = chrono::NaiveDate::from_ymd_opt(y, mo, d)?
        .and_hms_milli_opt(h, mi, se, ms)?;
    let utc_ms = naive.and_utc().timestamp_millis();
    // +0800 → UTC = local - 8h
    let offset = if let Some(pos) = s.find(['+', '-']) {
        let sign = if &s[pos..pos + 1] == "+" { 1 } else { -1 };
        let tz: String = s[pos + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
        let hh = tz.get(0..2).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
        let mm = tz.get(2..4).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
        sign * (hh * 60 + mm) * 60 * 1000
    } else { 0 };
    Some(utc_ms - offset)
}

/// 解析 XMLTV（自动识别 gzip），只保留 [now, now+24h] 窗口内的节目
pub fn parse_xmltv(bytes: &[u8], now: i64) -> HashMap<String, Vec<Value>> {
    let text = if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut s = String::new();
        GzDecoder::new(bytes).read_to_string(&mut s).unwrap_or_default();
        s
    } else {
        crate::net::decode_body(bytes)
    };
    let re = Regex::new(r#"(?s)<programme\s+([^>]*?)\s*/>|<programme\s+([^>]*?)>(.*?)</programme>"#).unwrap();
    let title_re = Regex::new(r#"(?is)<title(?:\s[^>]*)?>(.*?)</title>"#).unwrap();
    let desc_re = Regex::new(r#"(?is)<desc(?:\s[^>]*)?>(.*?)</desc>"#).unwrap();
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    for c in re.captures_iter(&text) {
        let attr_text = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
        let body = c.get(3).map(|m| m.as_str()).unwrap_or("");
        let a = attrs_of(attr_text);
        let channel = match a.get("channel") { Some(x) if !x.is_empty() => x.clone(), _ => continue };
        let start = match a.get("start").and_then(|s| parse_xmltv_time(s)) { Some(x) => x, None => continue };
        let stop = a.get("stop").and_then(|s| parse_xmltv_time(s)).unwrap_or(start);
        if stop <= now || start > now + WINDOW_MS { continue; }
        let title = title_re.captures(body).map(|x| decode_xml_text(&x[1])).unwrap_or_default();
        if title.is_empty() { continue; }
        let mut p = serde_json::Map::new();
        p.insert("channelId".into(), Value::String(channel.clone()));
        p.insert("start".into(), Value::from(start));
        p.insert("stop".into(), Value::from(stop));
        p.insert("title".into(), Value::String(title));
        if let Some(d) = desc_re.captures(body).map(|x| decode_xml_text(&x[1])) {
            if !d.is_empty() { p.insert("desc".into(), Value::String(d)); }
        }
        out.entry(channel).or_default().push(Value::Object(p));
    }
    for list in out.values_mut() { list.sort_by_key(|p| p.get("start").and_then(Value::as_i64).unwrap_or(0)); }
    out
}

fn current_and_next(programs: &[Value], now: i64) -> (Option<Value>, Option<Value>) {
    let mut current = None;
    let mut next = None;
    for p in programs {
        let start = p.get("start").and_then(Value::as_i64).unwrap_or(0);
        let stop = p.get("stop").and_then(Value::as_i64).unwrap_or(0);
        if start <= now && stop > now { current = Some(p.clone()); continue; }
        if start > now { next = Some(p.clone()); break; }
    }
    (current, next)
}

// ——————————————————————— handlers ———————————————————————

pub async fn playlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let url = q.get("url").map(|s| s.trim()).unwrap_or("").to_string();
    if url.is_empty() { return Err(ApiError::BadRequest("缺少 url 参数".into())); }
    let format = q.get("format").map(String::as_str).unwrap_or("json");
    let force = q.get("force").map(String::as_str) == Some("1");
    state.upstream.check_url(&url, state.cfg.live_allow_private).await?;

    let ck = format!("live:playlist:{url}");
    let cached = if force { None } else { state.cache.get(&ck) };
    let playlist = match cached {
        Some(v) => v,
        None => {
            let opts = FetchOptions::default()
                .with_timeout(15_000)
                .allow_private(state.cfg.live_allow_private);
            let text = state.upstream.text(&url, &opts).await?;
            let v = parse_m3u(&text, &url);
            state.cache.set(ck, v.clone(), PLAYLIST_TTL);
            v
        }
    };
    if format == "m3u" {
        return Ok(axum::response::Response::builder()
            .header("content-type", "audio/x-mpegurl; charset=utf-8")
            .header("cache-control", "no-store")
            .body(axum::body::Body::from(to_m3u_text(&playlist)))
            .unwrap());
    }
    let mut resp = json_ok(playlist);
    resp.headers_mut().insert("cache-control", axum::http::HeaderValue::from_static("no-store"));
    Ok(resp)
}

pub async fn epg(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let url = q.get("url").map(|s| s.trim()).unwrap_or("").to_string();
    let channel = q.get("channel").map(|s| s.trim()).unwrap_or("").to_string();
    if url.is_empty() || channel.is_empty() { return Err(ApiError::BadRequest("缺少 url 或 channel 参数".into())); }
    let force = q.get("force").map(String::as_str) == Some("1");
    state.upstream.check_url(&url, state.cfg.live_allow_private).await?;

    let ck = format!("live:epg:{url}");
    let cached = if force { None } else { state.cache.get(&ck) };
    let map = match cached {
        Some(Value::Object(m)) => m.into_iter().filter_map(|(k, v)| v.as_array().cloned().map(|a| (k, a))).collect::<HashMap<_, _>>(),
        _ => {
            let opts = FetchOptions::default()
                .with_timeout(30_000)
                .allow_private(state.cfg.live_allow_private);
            let bytes = state.upstream.bytes(&url, &opts).await?;
            let m = parse_xmltv(&bytes, now_ms());
            let stored: Value = json!(m.iter().map(|(k, v)| (k.clone(), Value::Array(v.clone()))).collect::<serde_json::Map<String, Value>>());
            state.cache.set(ck, stored, EPG_TTL);
            m
        }
    };
    let programs = map.get(&channel).cloned().unwrap_or_default();
    let (current, next) = current_and_next(&programs, now_ms());
    let mut resp = json_ok(json!({
        "channelId": channel,
        "current": current,
        "next": next,
        "programs": programs,
    }));
    resp.headers_mut().insert("cache-control", axum::http::HeaderValue::from_static("no-store"));
    Ok(resp)
}

fn probe_headers(state: &AppState, url: &str) -> FetchOptions {
    let mut opts = FetchOptions::default()
        .with_timeout(6000)
        .allow_private(state.cfg.live_allow_private)
        .header("user-agent", &state.cfg.user_agent);
    if let Ok(u) = Url::parse(url) {
        opts = opts.header("referer", &format!("{}://{}/", u.scheme(), u.host_str().unwrap_or("")));
    }
    opts
}

/// 分片级探测：m3u8 → (master → 首个 variant) → media playlist → 最新分片（Range: bytes=0-1）
async fn probe_chain(state: &AppState, url: &str) -> Result<Value, String> {
    state.upstream.check_url(url, state.cfg.live_allow_private).await.map_err(|e| e.to_string())?;
    Url::parse(url).map_err(|_| "无效的直播地址".to_string())?;
    let opts = probe_headers(state, url);
    let result = state.upstream.fetch(url, &opts).await.map_err(|e| e.to_string())?;
    let ct = result.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let final_url = result.final_url.clone();
    let status = result.status.as_u16();
    if !result.status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let is_m3u8 = ct.to_lowercase().contains("mpegurl") || final_url.path().to_lowercase().ends_with(".m3u8");
    if !is_m3u8 {
        // FLV 等直链：单级探测
        let mut o = json!({ "url": url, "ok": true, "status": status, "level": "head", "ms": 0 });
        o["ms"] = Value::from(0);
        return Ok(o);
    }
    let body = result.response.bytes().await.map_err(|e| e.to_string())?;
    let text = crate::net::decode_body(&body);
    if !has_m3u8_header(&text) { return Err("m3u8 内容无效".into()); }
    let codec = codecs_of(&text);
    let media_text = if !master_variants(&text, &final_url).is_empty() {
        let v = &master_variants(&text, &final_url)[0];
        let r = state.upstream.fetch(v, &opts).await.map_err(|e| e.to_string())?;
        let b = r.response.bytes().await.map_err(|e| e.to_string())?;
        crate::net::decode_body(&b)
    } else { text.clone() };
    let segs = segments(&media_text, &final_url);
    let seg = match segs.last() { Some(s) => s.clone(), None => return Err("播放列表无分片".into()) };

    let start = std::time::Instant::now();
    let seg_opts = probe_headers(state, &seg).header("range", "bytes=0-1");
    let r = state.upstream.fetch(&seg, &seg_opts).await.map_err(|e| e.to_string())?;
    let ok = r.status.as_u16() == 200 || r.status.as_u16() == 206;
    let ms = start.elapsed().as_millis() as u64;
    if !ok { return Err(format!("分片 HTTP {}", r.status.as_u16())); }

    // 采样 128KB 估算吞吐（识别「可达但限速」）
    let sample_opts = probe_headers(state, &seg).header("range", "bytes=0-131071");
    let t = std::time::Instant::now();
    let kbps = match state.upstream.fetch(&seg, &sample_opts).await {
        Ok(sr) => match tokio::time::timeout(Duration::from_secs(3), sr.response.bytes()).await {
            Ok(Ok(b)) => {
                let elapsed = t.elapsed().as_millis().max(1) as u64;
                Some((b.len() as u64 * 8) / elapsed)
            }
            _ => None,
        },
        Err(_) => None,
    };
    let mut out = json!({ "url": url, "ok": true, "status": status, "ms": ms, "level": "segment" });
    if let Some(c) = codec { out["codec"] = Value::String(c); }
    if let Some(k) = kbps { out["kbps"] = Value::from(k); }
    Ok(out)
}

async fn probe_one(state: AppState, url: String) -> Value {
    let ck = format!("live:probe:{url}");
    if let Some(cached) = state.cache.get(&ck) { return cached; }
    let _permit = state.host_gate.acquire(&url).await;
    let start = std::time::Instant::now();
    let outcome = match tokio::time::timeout(PROBE_BUDGET, probe_chain(&state, &url)).await {
        Ok(Ok(mut v)) => { v["ms"] = Value::from(start.elapsed().as_millis() as u64); v }
        Ok(Err(e)) => json!({ "url": url, "ok": false, "ms": start.elapsed().as_millis() as u64, "error": e }),
        Err(_) => json!({ "url": url, "ok": false, "ms": start.elapsed().as_millis() as u64, "timedOut": true, "error": "探测超时（8000ms）" }),
    };
    let ttl = if outcome.get("ok").and_then(Value::as_bool) == Some(true) { PROBE_OK_TTL } else { PROBE_FAIL_TTL };
    state.cache.set(ck, outcome.clone(), ttl);
    outcome
}

pub async fn probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Response> {
    crate::api::basic::ensure_login(&state, &headers)?;
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("无效请求体".into()))?;
    let mut urls: Vec<String> = parsed.get("urls").and_then(Value::as_array).cloned().unwrap_or_default()
        .iter().filter_map(|v| v.as_str().map(str::to_string))
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .collect::<HashSet<_>>().into_iter().take(50).collect();
    urls.sort();
    let is_stream = q.get("stream").map(String::as_str) == Some("1");
    if urls.is_empty() {
        if is_stream {
            let s = async_stream::stream! { yield Ok::<_, std::io::Error>(BufBytes::new()); };
            return Ok(ndjson_response(s));
        }
        return Ok(json_ok(json!({ "results": [] })));
    }

    if !is_stream {
        let mut futures = FuturesUnordered::new();
        for u in &urls {
            let st = state.clone();
            let u = u.clone();
            futures.push(async move { probe_one(st, u).await });
        }
        let mut results = Vec::new();
        while let Some(r) = futures.next().await { results.push(r); }
        let mut resp = json_ok(json!({ "results": results }));
        resp.headers_mut().insert("cache-control", axum::http::HeaderValue::from_static("no-store"));
        return Ok(resp);
    }

    let stream = async_stream::stream! {
        let mut futures = FuturesUnordered::new();
        for u in urls.iter() {
            let st = state.clone();
            let u = u.clone();
            futures.push(async move { probe_one(st, u).await });
        }
        while let Some(r) = futures.next().await {
            yield Ok::<_, std::io::Error>(BufBytes::from(format!("{}\n", r)));
        }
    };
    Ok(ndjson_response(stream))
}

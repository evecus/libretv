//! 通用工具函数。

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::Value;
use url::Url;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 与 JS encodeURIComponent 等价：保留 A-Za-z0-9-_.!~*'()，其余（含非 ASCII 字节）百分号编码
const URI_COMPONENT: &AsciiSet = &CONTROLS
    .add(b' ').add(b'"').add(b'#').add(b'$').add(b'%').add(b'&')
    .add(b'+').add(b',').add(b'/').add(b':').add(b';').add(b'<')
    .add(b'=').add(b'>').add(b'?').add(b'@').add(b'[').add(b'\\')
    .add(b']').add(b'^').add(b'`').add(b'{').add(b'|').add(b'}');

pub fn encode_component(s: &str) -> String {
    utf8_percent_encode(s, URI_COMPONENT).to_string()
}

pub fn decode_component(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .unwrap_or_else(|_| s.into())
        .to_string()
}

/// 以重定向后的最终 URL 为 base 解析 m3u8 内的相对地址；失败时原样返回
pub fn make_absolute(uri: &str, base: &Url) -> String {
    match base.join(uri) {
        Ok(u) => u.to_string(),
        Err(_) => uri.to_string(),
    }
}

/// 从 serde_json::Value 里取字符串字段（对齐原版 String(vod.xxx ?? '') 的宽松行为）
pub fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub fn s_owned(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 取 trim 后非空字符串（对应 cms-parser 的 str()）
pub fn non_empty(v: &Value, key: &str) -> Option<String> {
    s(v, key).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string)
}

pub fn as_array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// 从 Referer/UA 等请求头里取客户端 IP（用于登录限流）
pub fn client_ip(headers: &axum::http::HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            let t = first.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    peer.map(|p| p.ip().to_string()).unwrap_or_else(|| "unknown".to_string())
}

/// x-forwarded-proto 推导 cookie Secure 标记
pub fn request_is_https(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim() == "https")
        .unwrap_or(false)
}

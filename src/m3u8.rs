//! m3u8 处理：代理地址重写（服务端部分）与轻量解析（测活/画质探测用）。
//!
//! 前端 hls.js 拉到的分片地址形如 `/api/proxy?url=<encoded>`，是同源请求，
//! 浏览器自动携带 httpOnly 会话 cookie，因此重写时不需要往 URL 上拼 token。

use std::sync::OnceLock;

use regex::Regex;
use url::Url;

use crate::util::{encode_component, make_absolute};

pub const PROXY_BASE: &str = "/api/proxy?url=";
pub const LIVE_STREAM_BASE: &str = "/api/live/stream?url=";

pub fn is_proxied_uri(uri: &str) -> bool {
    uri.starts_with("/api/proxy") || uri.starts_with("/api/live/stream")
}

fn uri_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"URI="([^"]+)""#).unwrap())
}

fn proxied(prefix: &str, uri: &str, base: &Url) -> String {
    format!("{prefix}{}", encode_component(&make_absolute(uri, base)))
}

/// 把 m3u8 内所有分片 / key / map / variant 地址改写为本站代理地址。
/// base 必须是 302 重定向后的最终 URL，否则相对路径会指向错误目录。
pub fn rewrite_m3u8(content: &str, base: &Url, prefix: &str) -> String {
    content
        .split('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            if line.starts_with("#EXT-X-KEY") || line.starts_with("#EXT-X-MAP") {
                uri_re()
                    .replace_all(line, |caps: &regex::Captures| {
                        let uri = &caps[1];
                        if is_proxied_uri(uri) {
                            caps[0].to_string()
                        } else {
                            format!("URI=\"{}\"", proxied(prefix, uri, base))
                        }
                    })
                    .to_string()
            } else if line.starts_with('#') || line.trim().is_empty() || is_proxied_uri(line) {
                line.to_string()
            } else {
                proxied(prefix, line, base)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判断响应是否为 m3u8（content-type 或 URL 后缀）
pub fn is_m3u8(content_type: &str, url: &Url) -> bool {
    let ct = content_type.to_lowercase();
    ct.contains("mpegurl") || url.path().to_lowercase().ends_with(".m3u8")
}

/// media playlist 里的分片地址（绝对化）
pub fn segments(content: &str, base: &Url) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| !is_proxied_uri(l))
        .map(|l| make_absolute(l, base))
        .collect()
}

/// master playlist 的 variant 地址
pub fn master_variants(content: &str, base: &Url) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = false;
    for line in content.lines() {
        let l = line.trim();
        if l.starts_with("#EXT-X-STREAM-INF") {
            pending = true;
            continue;
        }
        if pending && !l.is_empty() && !l.starts_with('#') {
            out.push(make_absolute(l, base));
            pending = false;
        }
    }
    out
}

/// master playlist 首个 CODECS 声明（前端据此提示 H.265 不可解码）
pub fn codecs_of(content: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"CODECS="([^"]+)""#).unwrap());
    re.captures(content).map(|c| c[1].to_string())
}

pub fn has_m3u8_header(content: &str) -> bool {
    content.trim_start().starts_with("#EXTM3U")
}
